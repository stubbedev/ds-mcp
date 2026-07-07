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
    // ponytail: one connection behind a mutex — duckdb's API is sync and MCP
    // traffic is low; add a real pool if it ever becomes the bottleneck.
    DuckDb(std::sync::Arc<std::sync::Mutex<duckdb::Connection>>),
    Mssql(deadpool_tiberius::Pool),
    // ClickHouse speaks HTTP; no connection to pool.
    ClickHouse(ClickHouseHttp),
}

pub struct ClickHouseHttp {
    client: reqwest::Client,
    /// Base URL including any ?database=/user=/password= params.
    url: String,
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
            Some(SqlPool::DuckDb(_)) => {}
            Some(SqlPool::Mssql(p)) => p.close(),
            Some(SqlPool::ClickHouse(_)) => {}
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
        let default_port = match cfg.engine {
            EngineKind::MySql | EngineKind::MariaDb => 3306,
            EngineKind::Postgres => 5432,
            EngineKind::Mssql => 1433,
            EngineKind::ClickHouse => 8123,
            _ => 0, // file engines
        };
        // The target the database listens on (from host/port fields, or the
        // dsn below); ssh/docker blocks reroute the dial to a forward.
        let target_host = cfg.host.clone().unwrap_or_else(|| "127.0.0.1".into());
        let target_port = cfg.port.unwrap_or(default_port);
        // Resolve for engines whose target comes from discrete fields; the
        // dsn cases re-resolve with the dsn's own host below.
        let resolve = async |host: &str, port: u16| -> Result<(String, u16)> {
            let ep = super::endpoint::resolve(cfg, host, port).await?;
            if let Some(t) = ep.tunnel {
                let _ = self.tunnel.set(t);
            }
            Ok((ep.host, ep.port))
        };
        Ok(match cfg.engine {
            EngineKind::MySql | EngineKind::MariaDb => {
                let mut o = match &cfg.dsn {
                    Some(dsn) => MySqlConnectOptions::from_str(dsn)?,
                    None => {
                        let mut o = MySqlConnectOptions::new();
                        if let Some(u) = &cfg.user {
                            o = o.username(u);
                        }
                        if let Some(p) = &cfg.password {
                            o = o.password(p);
                        }
                        if let Some(d) = &cfg.database {
                            o = o.database(d);
                        }
                        o.host(&target_host).port(target_port)
                    }
                };
                let (host, port) = resolve(o.get_host(), o.get_port()).await?;
                o = o.host(&host).port(port);
                SqlPool::MySql(opts(cfg).connect_lazy_with(o))
            }
            EngineKind::Postgres => {
                let mut o = match &cfg.dsn {
                    Some(dsn) => PgConnectOptions::from_str(dsn)?,
                    None => {
                        let mut o = PgConnectOptions::new();
                        if let Some(u) = &cfg.user {
                            o = o.username(u);
                        }
                        if let Some(p) = &cfg.password {
                            o = o.password(p);
                        }
                        if let Some(d) = &cfg.database {
                            o = o.database(d);
                        }
                        o.host(&target_host).port(target_port)
                    }
                };
                let (host, port) = resolve(o.get_host(), o.get_port()).await?;
                o = o.host(&host).port(port);
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
            EngineKind::DuckDb => {
                let path = cfg.path.clone().or_else(|| cfg.dsn.clone());
                let path = path.ok_or_else(|| anyhow::anyhow!("duckdb needs path"))?;
                let readonly = self.readonly;
                let conn = tokio::task::spawn_blocking(move || {
                    let mut config = duckdb::Config::default();
                    if readonly {
                        config = config.access_mode(duckdb::AccessMode::ReadOnly)?;
                    }
                    duckdb::Connection::open_with_flags(&path, config)
                        .with_context(|| format!("open duckdb {path}"))
                })
                .await??;
                SqlPool::DuckDb(std::sync::Arc::new(std::sync::Mutex::new(conn)))
            }
            EngineKind::Mssql => {
                let (mut m, target) = match &cfg.dsn {
                    // ADO connection string; TrustServerCertificate etc. go here.
                    Some(dsn) => {
                        let addr = tiberius::Config::from_ado_string(dsn)?.get_addr();
                        let (h, p) = addr.rsplit_once(':').unwrap_or((addr.as_str(), "1433"));
                        let target = (h.to_string(), p.parse().unwrap_or(1433));
                        (deadpool_tiberius::Manager::from_ado_string(dsn)?, target)
                    }
                    None => {
                        let mut m = deadpool_tiberius::Manager::new();
                        if let (Some(u), Some(p)) = (&cfg.user, &cfg.password) {
                            m = m.basic_authentication(u, p);
                        }
                        if let Some(d) = &cfg.database {
                            m = m.database(d);
                        }
                        (m, (target_host.clone(), target_port))
                    }
                };
                let (host, port) = resolve(&target.0, target.1).await?;
                m = m
                    .host(host)
                    .port(port)
                    .max_size(8)
                    .create_timeout(cfg.connect_timeout());
                SqlPool::Mssql(m.create_pool()?)
            }
            EngineKind::ClickHouse => {
                // HTTP interface. dsn = full base URL (auth via URL params or
                // https); discrete fields build one.
                let mut url = match &cfg.dsn {
                    Some(dsn) => {
                        let mut parsed = url::Url::parse(dsn).context("parse clickhouse dsn")?;
                        let dsn_host = parsed.host_str().unwrap_or("127.0.0.1").to_string();
                        let dsn_port = parsed.port().unwrap_or(8123);
                        let (host, port) = resolve(&dsn_host, dsn_port).await?;
                        let _ = parsed.set_host(Some(&host));
                        let _ = parsed.set_port(Some(port));
                        parsed.to_string()
                    }
                    None => {
                        let (host, port) = resolve(&target_host, target_port).await?;
                        let mut url = format!("http://{host}:{port}/?");
                        if let Some(u) = &cfg.user {
                            url.push_str(&format!("user={u}&"));
                        }
                        if let Some(p) = &cfg.password {
                            url.push_str(&format!("password={p}&"));
                        }
                        if let Some(d) = &cfg.database {
                            url.push_str(&format!("database={d}&"));
                        }
                        url
                    }
                };
                // collect/exec append query params directly.
                if !url.contains('?') {
                    url.push('?');
                } else if !url.ends_with('?') && !url.ends_with('&') {
                    url.push('&');
                }
                let client = reqwest::Client::builder()
                    .connect_timeout(cfg.connect_timeout())
                    .build()?;
                SqlPool::ClickHouse(ClickHouseHttp { client, url })
            }
            EngineKind::Redis | EngineKind::MongoDb => {
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
            SqlPool::DuckDb(conn) => {
                let conn = std::sync::Arc::clone(conn);
                let sql = sql.to_owned();
                tokio::task::spawn_blocking(move || duckdb_collect(&conn, &sql, fetch)).await??
            }
            SqlPool::Mssql(p) => {
                let mut conn = p.get().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                mssql_collect(&mut conn, sql, fetch).await?
            }
            SqlPool::ClickHouse(ch) => clickhouse_collect(ch, sql, fetch).await?,
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
            SqlPool::DuckDb(conn) => {
                let conn = std::sync::Arc::clone(conn);
                let sql = sql.to_owned();
                let affected = tokio::task::spawn_blocking(move || {
                    let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                    conn.execute(&sql, [])
                })
                .await??;
                ExecResult {
                    rows_affected: affected as u64,
                    last_insert_id: None,
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
            SqlPool::ClickHouse(ch) => {
                clickhouse_exec(ch, sql).await?;
                // The HTTP interface does not report affected rows.
                ExecResult {
                    rows_affected: 0,
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
            EngineKind::Sqlite | EngineKind::DuckDb => "PRAGMA database_list",
            EngineKind::Mssql => "SELECT name FROM sys.databases ORDER BY name",
            EngineKind::ClickHouse => "SHOW DATABASES",
            EngineKind::Redis | EngineKind::MongoDb => unreachable!(),
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
            EngineKind::DuckDb => "SHOW TABLES".into(),
            EngineKind::ClickHouse => match db {
                Some(db) => format!("SHOW TABLES FROM {}", quote_ident_mysql(db)),
                None => "SHOW TABLES".into(),
            },
            EngineKind::Mssql => {
                let prefix = db
                    .map(|d| format!("{}.", quote_ident_bracket(d)))
                    .unwrap_or_default();
                format!(
                    "SELECT table_schema, table_name FROM {prefix}information_schema.tables \
                     WHERE table_type = 'BASE TABLE' ORDER BY 1, 2"
                )
            }
            EngineKind::Redis | EngineKind::MongoDb => unreachable!(),
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
            EngineKind::DuckDb => format!("DESCRIBE {}", quote_ident_dq(table)),
            EngineKind::ClickHouse => match db {
                Some(db) => format!(
                    "DESCRIBE TABLE {}.{}",
                    quote_ident_mysql(db),
                    quote_ident_mysql(table)
                ),
                None => format!("DESCRIBE TABLE {}", quote_ident_mysql(table)),
            },
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
            EngineKind::Redis | EngineKind::MongoDb => unreachable!(),
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

/// ClickHouse HTTP: append FORMAT JSONCompact and parse {meta, data}.
/// max_result_rows/break caps the result server-side.
async fn clickhouse_collect(
    ch: &ClickHouseHttp,
    sql: &str,
    fetch: usize,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let url = format!(
        "{}max_result_rows={fetch}&result_overflow_mode=break&output_format_json_quote_64bit_integers=0",
        ch.url
    );
    let body = format!("{} FORMAT JSONCompact", sql.trim_end_matches(';'));
    let resp = ch.client.post(&url).body(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("clickhouse: {}", text.trim());
    }
    let parsed: serde_json::Value = serde_json::from_str(&text).with_context(
        || "parse clickhouse JSONCompact response (does the query already contain a FORMAT clause?)",
    )?;
    let columns = parsed["meta"]
        .as_array()
        .map(|m| {
            m.iter()
                .map(|c| c["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    let rows = parsed["data"]
        .as_array()
        .map(|d| {
            d.iter()
                .map(|row| row.as_array().cloned().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();
    Ok((columns, rows))
}

async fn clickhouse_exec(ch: &ClickHouseHttp, sql: &str) -> Result<()> {
    let resp = ch.client.post(&ch.url).body(sql.to_owned()).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("clickhouse: {}", text.trim());
    }
    Ok(())
}

fn duckdb_collect(
    conn: &std::sync::Mutex<duckdb::Connection>,
    sql: &str,
    fetch: usize,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = conn.prepare(sql)?;
    let mut rows_iter = stmt.query([])?;
    let mut columns: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    while let Some(row) = rows_iter.next()? {
        if columns.is_empty() {
            columns = row
                .as_ref()
                .column_names()
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        rows.push(
            (0..columns.len())
                .map(|i| {
                    duckdb_value(
                        row.get::<_, duckdb::types::Value>(i)
                            .unwrap_or(duckdb::types::Value::Null),
                    )
                })
                .collect(),
        );
        if rows.len() >= fetch {
            break;
        }
    }
    Ok((columns, rows))
}

fn duckdb_value(v: duckdb::types::Value) -> Value {
    use duckdb::types::Value as D;
    match v {
        D::Null => Value::Null,
        D::Boolean(b) => Value::Bool(b),
        D::TinyInt(n) => Value::from(n),
        D::SmallInt(n) => Value::from(n),
        D::Int(n) => Value::from(n),
        D::BigInt(n) => Value::from(n),
        D::HugeInt(n) => Value::String(n.to_string()),
        D::UTinyInt(n) => Value::from(n),
        D::USmallInt(n) => Value::from(n),
        D::UInt(n) => Value::from(n),
        D::UBigInt(n) => Value::from(n),
        D::Float(f) => num_f64(f as f64),
        D::Double(f) => num_f64(f),
        D::Decimal(d) => Value::String(d.to_string()),
        D::Text(s) | D::Enum(s) => Value::String(s),
        D::Blob(b) => bytes_value(b),
        D::Timestamp(unit, n) => {
            match chrono::DateTime::from_timestamp_micros(to_micros(unit, n)) {
                Some(ts) => Value::String(ts.naive_utc().to_string()),
                None => Value::Null,
            }
        }
        D::Date32(days) => chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163)
            .map(|d| Value::String(d.to_string()))
            .unwrap_or(Value::Null),
        D::Time64(unit, n) => {
            let micros = to_micros(unit, n);
            chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                (micros / 1_000_000) as u32,
                ((micros % 1_000_000) * 1000) as u32,
            )
            .map(|t| Value::String(t.to_string()))
            .unwrap_or(Value::Null)
        }
        D::List(items) | D::Array(items) => {
            Value::Array(items.into_iter().map(duckdb_value).collect())
        }
        D::Struct(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), duckdb_value(v.clone())))
                .collect(),
        ),
        D::Union(inner) => duckdb_value(*inner),
        other => Value::String(format!("{other:?}")),
    }
}

fn to_micros(unit: duckdb::types::TimeUnit, n: i64) -> i64 {
    use duckdb::types::TimeUnit;
    match unit {
        TimeUnit::Second => n * 1_000_000,
        TimeUnit::Millisecond => n * 1_000,
        TimeUnit::Microsecond => n,
        TimeUnit::Nanosecond => n / 1_000,
    }
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
