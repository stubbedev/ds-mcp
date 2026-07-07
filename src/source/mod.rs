//! A source is one named connection from the config. Two families: SQL
//! engines and document stores. New engine = new variant + match arms.

pub mod mongo;
pub mod redis;
pub mod sql;
pub mod ssh;

use serde::Serialize;

use crate::config::{EngineKind, SourceConfig};

pub enum Source {
    Sql(sql::SqlSource),
    Mongo(mongo::MongoSource),
    Redis(redis::RedisSource),
}

impl Source {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> anyhow::Result<Self> {
        match cfg.engine {
            EngineKind::MongoDb => Ok(Source::Mongo(mongo::MongoSource::new(
                name,
                cfg,
                force_readonly,
            ))),
            EngineKind::Redis => Ok(Source::Redis(redis::RedisSource::new(
                name,
                cfg,
                force_readonly,
            ))),
            _ => Ok(Source::Sql(sql::SqlSource::new(name, cfg, force_readonly))),
        }
    }

    pub fn info(&self, name: &str) -> SourceInfo {
        let (engine, cfg, readonly) = match self {
            Source::Sql(s) => (s.engine(), s.config(), s.readonly()),
            Source::Mongo(s) => (s.engine(), s.config(), s.readonly()),
            Source::Redis(s) => (s.engine(), s.config(), s.readonly()),
        };
        SourceInfo {
            name: name.to_string(),
            engine: engine.name(),
            description: cfg.description.clone(),
            readonly,
            remote: cfg.ssh.is_some(),
        }
    }

    pub fn as_sql(&self, name: &str) -> Result<&sql::SqlSource, String> {
        match self {
            Source::Sql(s) => Ok(s),
            Source::Mongo(_) => Err(format!(
                "source {name:?} is engine mongodb; use the document tools \
                 (find, aggregate, count, ...)"
            )),
            Source::Redis(_) => Err(format!(
                "source {name:?} is engine redis; use the redis_command tool"
            )),
        }
    }

    pub fn as_mongo(&self, name: &str) -> Result<&mongo::MongoSource, String> {
        match self {
            Source::Mongo(s) => Ok(s),
            Source::Sql(s) => Err(format!(
                "source {name:?} is engine {}; use the SQL tools \
                 (read_query, list_tables, ...)",
                s.engine().name()
            )),
            Source::Redis(_) => Err(format!(
                "source {name:?} is engine redis; use the redis_command tool"
            )),
        }
    }

    pub fn as_redis(&self, name: &str) -> Result<&redis::RedisSource, String> {
        match self {
            Source::Redis(s) => Ok(s),
            other => {
                let engine = match other {
                    Source::Sql(s) => s.engine().name(),
                    Source::Mongo(_) => "mongodb",
                    Source::Redis(_) => unreachable!(),
                };
                Err(format!(
                    "source {name:?} is engine {engine}; redis_command only works on redis sources"
                ))
            }
        }
    }

    pub async fn close(&self) {
        match self {
            Source::Sql(s) => s.close().await,
            Source::Mongo(s) => s.close().await,
            Source::Redis(s) => s.close().await,
        }
    }

    /// Schema overview for the MCP resource `ds://{name}/schema`: tables with
    /// their columns (SQL) or collection names (mongo). Capped at 100 tables.
    pub async fn schema(&self) -> anyhow::Result<serde_json::Value> {
        match self {
            Source::Sql(s) => {
                let tables_rs = s.query(&s.list_tables_sql(None), 1000).await?;
                let mut tables = Vec::new();
                for row in &tables_rs.rows {
                    // Table name is the last column of every engine's listing
                    // (mysql/sqlite: 1 col; pg/mssql: schema, name).
                    let Some(name) = row.last().and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let cols = s.query(&s.describe_table_sql(name, None), 200).await?;
                    tables.push(serde_json::json!({
                        "name": name,
                        "columns": cols.columns,
                        "rows": cols.rows,
                    }));
                    if tables.len() >= 100 {
                        break;
                    }
                }
                Ok(serde_json::json!({
                    "engine": s.engine().name(),
                    "tables": tables,
                }))
            }
            Source::Mongo(m) => {
                let collections = m.list_collections(None).await?;
                Ok(serde_json::json!({
                    "engine": "mongodb",
                    "collections": collections,
                    "hint": "use find/list_indexes tools to inspect documents",
                }))
            }
            Source::Redis(r) => {
                let info = r.command(&["INFO".into(), "keyspace".into()]).await?;
                Ok(serde_json::json!({
                    "engine": "redis",
                    "keyspace": info,
                    "hint": "use the redis_command tool (SCAN, TYPE, ...) to inspect keys",
                }))
            }
        }
    }
}

#[derive(Serialize)]
pub struct SourceInfo {
    pub name: String,
    pub engine: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub readonly: bool,
    pub remote: bool,
}

/// Tabular query result. `truncated` is set when more rows existed than the
/// requested limit.
#[derive(Serialize)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct ExecResult {
    pub rows_affected: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_insert_id: Option<u64>,
}
