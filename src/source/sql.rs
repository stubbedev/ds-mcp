//! SQL-family sources: mysql/mariadb, postgres, sqlite, mssql.
//! One lazily-connected pool per source; rows are decoded to JSON with a
//! per-engine try-decode chain (sqlx has no generic runtime decode).

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use serde_json::Value;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlRow};
use sqlx::pool::PoolOptions;
use sqlx::postgres::{PgConnectOptions, PgPool, PgRow};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqliteRow};
use sqlx::{AssertSqlSafe, Column, Row};
use tokio::sync::OnceCell;

use super::{ExecResult, ResultSet};
use crate::config::{EngineKind, SourceConfig};

pub struct SqlSource {
    name: String,
    cfg: SourceConfig,
    readonly: bool,
    pool: OnceCell<SqlPool>,
    /// Keeps the ssh forward alive for the life of the pool.
    tunnel: OnceCell<super::ssh::SshTunnel>,
}

pub enum SqlPool {
    MySql(MySqlPool),
    Pg(PgPool),
    Sqlite(SqlitePool),
    Mssql(deadpool_tiberius::Pool),
}

impl SqlSource {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> Self {
        let readonly = force_readonly || cfg.readonly;
        Self {
            name: name.to_string(),
            cfg,
            readonly,
            pool: OnceCell::new(),
            tunnel: OnceCell::new(),
        }
    }

    pub fn engine(&self) -> EngineKind {
        self.cfg.engine
    }

    pub fn config(&self) -> &SourceConfig {
        &self.cfg
    }

    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub async fn close(&self) {
        match self.pool.get() {
            Some(SqlPool::MySql(p)) => p.close().await,
            Some(SqlPool::Pg(p)) => p.close().await,
            Some(SqlPool::Sqlite(p)) => p.close().await,
            Some(SqlPool::Mssql(p)) => p.close(),
            None => {}
        }
    }

    async fn pool(&self) -> Result<&SqlPool> {
        self.pool
            .get_or_try_init(|| self.connect())
            .await
            .with_context(|| format!("connect to source {:?}", self.name))
    }

    async fn connect(&self) -> Result<SqlPool> {
        let cfg = &self.cfg;
        fn opts<DB: sqlx::Database>(cfg: &SourceConfig) -> PoolOptions<DB> {
            PoolOptions::new()
                .max_connections(8)
                .min_connections(0)
                .idle_timeout(std::time::Duration::from_secs(300))
                .acquire_timeout(cfg.connect_timeout())
        }
        // With ssh configured, everything dials the local forward instead.
        let default_port = match cfg.engine {
            EngineKind::MySql | EngineKind::MariaDb => 3306,
            EngineKind::Postgres => 5432,
            EngineKind::Mssql => 1433,
            _ => 0,
        };
        let (host, port) = match &cfg.ssh {
            Some(ssh) => {
                let target = cfg.host.as_deref().unwrap_or("127.0.0.1");
                let tunnel =
                    super::ssh::open(ssh, target, cfg.port.unwrap_or(default_port)).await?;
                let addr = tunnel.local_addr;
                let _ = self.tunnel.set(tunnel);
                (addr.ip().to_string(), addr.port())
            }
            None => (
                cfg.host.clone().unwrap_or_else(|| "127.0.0.1".into()),
                cfg.port.unwrap_or(default_port),
            ),
        };
        Ok(match cfg.engine {
            EngineKind::MySql | EngineKind::MariaDb => {
                let o = match &cfg.dsn {
                    Some(dsn) => MySqlConnectOptions::from_str(dsn)?,
                    None => {
                        let mut o = MySqlConnectOptions::new().host(&host).port(port);
                        if let Some(u) = &cfg.user {
                            o = o.username(u);
                        }
                        if let Some(p) = &cfg.password {
                            o = o.password(p);
                        }
                        if let Some(d) = &cfg.database {
                            o = o.database(d);
                        }
                        o
                    }
                };
                SqlPool::MySql(opts(cfg).connect_lazy_with(o))
            }
            EngineKind::Postgres => {
                let o = match &cfg.dsn {
                    Some(dsn) => PgConnectOptions::from_str(dsn)?,
                    None => {
                        let mut o = PgConnectOptions::new().host(&host).port(port);
                        if let Some(u) = &cfg.user {
                            o = o.username(u);
                        }
                        if let Some(p) = &cfg.password {
                            o = o.password(p);
                        }
                        if let Some(d) = &cfg.database {
                            o = o.database(d);
                        }
                        o
                    }
                };
                SqlPool::Pg(opts(cfg).connect_lazy_with(o))
            }
            EngineKind::Sqlite => {
                let o = match (&cfg.dsn, &cfg.path) {
                    (Some(dsn), _) => SqliteConnectOptions::from_str(dsn)?,
                    (None, Some(path)) => SqliteConnectOptions::new().filename(path),
                    (None, None) => bail!("sqlite needs path or dsn"),
                };
                // Belt and braces: a readonly sqlite source is opened read-only
                // at the file level too.
                let o = o.read_only(self.readonly);
                SqlPool::Sqlite(opts(cfg).connect_lazy_with(o))
            }
            EngineKind::Mssql => {
                let mut m = match &cfg.dsn {
                    // ADO connection string; TrustServerCertificate etc. go here.
                    Some(dsn) => deadpool_tiberius::Manager::from_ado_string(dsn)?,
                    None => {
                        let mut m = deadpool_tiberius::Manager::new().host(&host).port(port);
                        if let (Some(u), Some(p)) = (&cfg.user, &cfg.password) {
                            m = m.basic_authentication(u, p);
                        }
                        if let Some(d) = &cfg.database {
                            m = m.database(d);
                        }
                        m
                    }
                };
                m = m.max_size(8).create_timeout(cfg.connect_timeout());
                SqlPool::Mssql(m.create_pool()?)
            }
            EngineKind::MongoDb => {
                bail!("engine {} not handled by SqlSource", cfg.engine.name())
            }
        })
    }

    /// Run a query, returning at most `limit` rows (+ a truncated flag).
    /// The SQL is user-provided by design (this server's whole job); the
    /// read-only guard lives in sqlguard, not here.
    pub async fn query(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let fetch = limit + 1;
        let safe = || AssertSqlSafe(sql.to_owned());
        let (columns, mut rows) = match self.pool().await? {
            SqlPool::MySql(p) => {
                collect(
                    sqlx::query(safe()).fetch(p),
                    fetch,
                    columns_of_mysql,
                    mysql_value,
                )
                .await?
            }
            SqlPool::Pg(p) => {
                collect(sqlx::query(safe()).fetch(p), fetch, columns_of_pg, pg_value).await?
            }
            SqlPool::Sqlite(p) => {
                collect(
                    sqlx::query(safe()).fetch(p),
                    fetch,
                    columns_of_sqlite,
                    sqlite_value,
                )
                .await?
            }
            SqlPool::Mssql(p) => {
                let mut conn = p.get().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                mssql_collect(&mut conn, sql, fetch).await?
            }
        };
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        Ok(ResultSet {
            columns,
            row_count: rows.len(),
            rows,
            truncated,
        })
    }

    pub async fn exec(&self, sql: &str) -> Result<ExecResult> {
        let safe = || AssertSqlSafe(sql.to_owned());
        Ok(match self.pool().await? {
            SqlPool::MySql(p) => {
                let r = sqlx::query(safe()).execute(p).await?;
                ExecResult {
                    rows_affected: r.rows_affected(),
                    last_insert_id: Some(r.last_insert_id()),
                }
            }
            SqlPool::Pg(p) => {
                let r = sqlx::query(safe()).execute(p).await?;
                ExecResult {
                    rows_affected: r.rows_affected(),
                    last_insert_id: None,
                }
            }
            SqlPool::Sqlite(p) => {
                let r = sqlx::query(safe()).execute(p).await?;
                ExecResult {
                    rows_affected: r.rows_affected(),
                    last_insert_id: u64::try_from(r.last_insert_rowid()).ok(),
                }
            }
            SqlPool::Mssql(p) => {
                let mut conn = p.get().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let r = conn.execute(sql.to_owned(), &[]).await?;
                ExecResult {
                    rows_affected: r.total(),
                    last_insert_id: None,
                }
            }
        })
    }

    /// EXPLAIN. MSSQL has no EXPLAIN prefix; it needs SHOWPLAN_ALL toggled
    /// around the statement on one connection.
    pub async fn explain(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        if self.engine() != EngineKind::Mssql {
            return self.query(&self.explain_sql(sql), limit).await;
        }
        let SqlPool::Mssql(p) = self.pool().await? else {
            unreachable!()
        };
        let mut conn = p.get().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.simple_query("SET SHOWPLAN_ALL ON")
            .await?
            .into_results()
            .await?;
        let collected = mssql_collect(&mut conn, sql, limit + 1).await;
        // Always turn SHOWPLAN back off — the connection returns to the pool.
        let off = conn.simple_query("SET SHOWPLAN_ALL OFF").await;
        if let Ok(stream) = off {
            let _ = stream.into_results().await;
        }
        let (columns, mut rows) = collected?;
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        Ok(ResultSet {
            columns,
            row_count: rows.len(),
            rows,
            truncated,
        })
    }

    pub fn list_databases_sql(&self) -> &'static str {
        match self.engine() {
            EngineKind::MySql | EngineKind::MariaDb => "SHOW DATABASES",
            EngineKind::Postgres => {
                "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname"
            }
            EngineKind::Sqlite => "PRAGMA database_list",
            EngineKind::Mssql => "SELECT name FROM sys.databases ORDER BY name",
            EngineKind::MongoDb => unreachable!(),
        }
    }

    pub fn list_tables_sql(&self, database: Option<&str>) -> String {
        let db = database.or(self.cfg.database.as_deref());
        match self.engine() {
            EngineKind::MySql | EngineKind::MariaDb => match db {
                Some(db) => format!("SHOW TABLES FROM {}", quote_ident_mysql(db)),
                None => "SHOW TABLES".into(),
            },
            // Postgres cannot switch databases per query; an explicit
            // `database` argument filters the schema instead. The source's
            // configured database is a catalog, not a schema — never use it
            // as a filter here.
            EngineKind::Postgres => {
                let filter = match database {
                    Some(schema) => format!("AND table_schema = {}", quote_literal(schema)),
                    None => "AND table_schema NOT IN ('pg_catalog','information_schema')".into(),
                };
                format!(
                    "SELECT table_schema, table_name FROM information_schema.tables \
                     WHERE table_type = 'BASE TABLE' {filter} ORDER BY 1, 2"
                )
            }
            EngineKind::Sqlite => {
                "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name".into()
            }
            EngineKind::Mssql => {
                let prefix = db
                    .map(|d| format!("{}.", quote_ident_bracket(d)))
                    .unwrap_or_default();
                format!(
                    "SELECT table_schema, table_name FROM {prefix}information_schema.tables \
                     WHERE table_type = 'BASE TABLE' ORDER BY 1, 2"
                )
            }
            EngineKind::MongoDb => unreachable!(),
        }
    }

    pub fn describe_table_sql(&self, table: &str, database: Option<&str>) -> String {
        let db = database.or(self.cfg.database.as_deref());
        match self.engine() {
            EngineKind::MySql | EngineKind::MariaDb => match db {
                Some(db) => format!(
                    "DESCRIBE {}.{}",
                    quote_ident_mysql(db),
                    quote_ident_mysql(table)
                ),
                None => format!("DESCRIBE {}", quote_ident_mysql(table)),
            },
            // Same schema-vs-catalog distinction as list_tables_sql.
            EngineKind::Postgres => {
                let schema = match database {
                    Some(schema) => format!("AND table_schema = {}", quote_literal(schema)),
                    None => String::new(),
                };
                format!(
                    "SELECT column_name, data_type, is_nullable, column_default \
                     FROM information_schema.columns WHERE table_name = {} {schema} \
                     ORDER BY ordinal_position",
                    quote_literal(table)
                )
            }
            EngineKind::Sqlite => format!("PRAGMA table_info({})", quote_ident_dq(table)),
            EngineKind::Mssql => {
                let prefix = db
                    .map(|d| format!("{}.", quote_ident_bracket(d)))
                    .unwrap_or_default();
                format!(
                    "SELECT column_name, data_type, is_nullable, column_default \
                     FROM {prefix}information_schema.columns WHERE table_name = {} \
                     ORDER BY ordinal_position",
                    quote_literal(table)
                )
            }
            EngineKind::MongoDb => unreachable!(),
        }
    }

    pub fn explain_sql(&self, sql: &str) -> String {
        match self.engine() {
            EngineKind::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
            _ => format!("EXPLAIN {sql}"),
        }
    }
}

fn quote_ident_mysql(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

fn quote_ident_dq(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn quote_ident_bracket(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

async fn collect<R>(
    mut stream: futures_util::stream::BoxStream<'_, sqlx::Result<R>>,
    fetch: usize,
    columns_of: fn(&R) -> Vec<String>,
    value_of: fn(&R, usize) -> Value,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    while let Some(row) = stream.try_next().await? {
        if columns.is_empty() {
            columns = columns_of(&row);
        }
        rows.push((0..columns.len()).map(|i| value_of(&row, i)).collect());
        if rows.len() >= fetch {
            break;
        }
    }
    Ok((columns, rows))
}

async fn mssql_collect(
    conn: &mut deadpool_tiberius::Client,
    sql: &str,
    fetch: usize,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let stream = conn.simple_query(sql.to_owned()).await?;
    let mut row_stream = stream.into_row_stream();
    let mut columns: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    while let Some(row) = row_stream.try_next().await? {
        if columns.is_empty() {
            columns = row.columns().iter().map(|c| c.name().to_string()).collect();
        }
        rows.push(mssql_row_values(&row));
        if rows.len() >= fetch {
            break;
        }
    }
    Ok((columns, rows))
}

fn mssql_row_values(row: &tiberius::Row) -> Vec<Value> {
    use tiberius::ColumnData as C;
    row.cells()
        .enumerate()
        .map(|(i, (_, data))| match data {
            C::U8(v) => v.map(Value::from).unwrap_or(Value::Null),
            C::I16(v) => v.map(Value::from).unwrap_or(Value::Null),
            C::I32(v) => v.map(Value::from).unwrap_or(Value::Null),
            C::I64(v) => v.map(Value::from).unwrap_or(Value::Null),
            C::F32(v) => v.map(|v| num_f64(v as f64)).unwrap_or(Value::Null),
            C::F64(v) => v.map(num_f64).unwrap_or(Value::Null),
            C::Bit(v) => v.map(Value::Bool).unwrap_or(Value::Null),
            C::String(v) => v
                .as_ref()
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null),
            C::Guid(v) => v
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null),
            C::Binary(v) => v
                .as_ref()
                .map(|v| bytes_value(v.to_vec()))
                .unwrap_or(Value::Null),
            C::Numeric(v) => v
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null),
            C::Xml(v) => v
                .as_ref()
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null),
            // Temporal TDS types: re-decode through chrono (feature-gated
            // FromSql impls) instead of converting raw wire structs by hand.
            C::Date(_) => decode_time::<chrono::NaiveDate>(row, i),
            C::Time(_) => decode_time::<chrono::NaiveTime>(row, i),
            C::DateTime(_) | C::SmallDateTime(_) | C::DateTime2(_) => {
                decode_time::<chrono::NaiveDateTime>(row, i)
            }
            C::DateTimeOffset(_) => row
                .try_get::<chrono::DateTime<chrono::Utc>, _>(i)
                .ok()
                .flatten()
                .map(|v| Value::String(v.to_rfc3339()))
                .unwrap_or(Value::Null),
        })
        .collect()
}

fn decode_time<'a, T: tiberius::FromSql<'a> + ToString>(row: &'a tiberius::Row, i: usize) -> Value {
    row.try_get::<T, _>(i)
        .ok()
        .flatten()
        .map(|v| Value::String(v.to_string()))
        .unwrap_or(Value::Null)
}

fn columns_of_mysql(row: &MySqlRow) -> Vec<String> {
    row.columns().iter().map(|c| c.name().to_string()).collect()
}

fn columns_of_pg(row: &PgRow) -> Vec<String> {
    row.columns().iter().map(|c| c.name().to_string()).collect()
}

fn columns_of_sqlite(row: &SqliteRow) -> Vec<String> {
    row.columns().iter().map(|c| c.name().to_string()).collect()
}

/// Try decoding a cell as each type in turn; sqlx rejects incompatible
/// decodes, so the first success is the right one.
macro_rules! try_decode {
    ($row:expr, $i:expr, [$($ty:ty => $conv:expr),+ $(,)?]) => {
        $(
            if let Ok(v) = $row.try_get::<Option<$ty>, _>($i) {
                return match v {
                    None => Value::Null,
                    #[allow(clippy::redundant_closure_call)]
                    Some(v) => ($conv)(v),
                };
            }
        )+
    };
}

fn num_f64(v: f64) -> Value {
    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
}

fn bytes_value(v: Vec<u8>) -> Value {
    match String::from_utf8(v) {
        Ok(s) => Value::String(s),
        Err(e) => Value::String(format!(
            "0x{}",
            e.into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )),
    }
}

fn mysql_value(row: &MySqlRow, i: usize) -> Value {
    try_decode!(row, i, [
        i64 => |v: i64| Value::from(v),
        u64 => |v: u64| Value::from(v),
        f64 => num_f64,
        rust_decimal::Decimal => |v: rust_decimal::Decimal| Value::String(v.to_string()),
        chrono::DateTime<chrono::Utc> => |v: chrono::DateTime<chrono::Utc>| Value::String(v.to_rfc3339()),
        chrono::NaiveDateTime => |v: chrono::NaiveDateTime| Value::String(v.to_string()),
        chrono::NaiveDate => |v: chrono::NaiveDate| Value::String(v.to_string()),
        chrono::NaiveTime => |v: chrono::NaiveTime| Value::String(v.to_string()),
        String => Value::String,
        bool => Value::Bool,
        serde_json::Value => |v| v,
        Vec<u8> => bytes_value,
    ]);
    Value::String(format!("<undecodable: {}>", row.column(i).type_info()))
}

fn pg_value(row: &PgRow, i: usize) -> Value {
    try_decode!(row, i, [
        i16 => |v: i16| Value::from(v),
        i32 => |v: i32| Value::from(v),
        i64 => |v: i64| Value::from(v),
        f32 => |v: f32| num_f64(v as f64),
        f64 => num_f64,
        rust_decimal::Decimal => |v: rust_decimal::Decimal| Value::String(v.to_string()),
        bool => Value::Bool,
        uuid::Uuid => |v: uuid::Uuid| Value::String(v.to_string()),
        chrono::DateTime<chrono::Utc> => |v: chrono::DateTime<chrono::Utc>| Value::String(v.to_rfc3339()),
        chrono::NaiveDateTime => |v: chrono::NaiveDateTime| Value::String(v.to_string()),
        chrono::NaiveDate => |v: chrono::NaiveDate| Value::String(v.to_string()),
        chrono::NaiveTime => |v: chrono::NaiveTime| Value::String(v.to_string()),
        String => Value::String,
        serde_json::Value => |v| v,
        Vec<String> => |v: Vec<String>| Value::Array(v.into_iter().map(Value::String).collect()),
        Vec<i64> => |v: Vec<i64>| Value::Array(v.into_iter().map(Value::from).collect()),
        Vec<u8> => bytes_value,
    ]);
    Value::String(format!("<undecodable: {}>", row.column(i).type_info()))
}

fn sqlite_value(row: &SqliteRow, i: usize) -> Value {
    try_decode!(row, i, [
        i64 => |v: i64| Value::from(v),
        f64 => num_f64,
        String => Value::String,
        bool => Value::Bool,
        Vec<u8> => bytes_value,
    ]);
    Value::Null
}
