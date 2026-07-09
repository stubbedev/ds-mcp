//! A source is one named connection from the config. Two families: SQL
//! engines and document stores. New engine = new variant + match arms.

pub mod endpoint;
pub mod mongo;
pub mod redis;
pub mod rest;
pub mod sql;
pub mod ssh;

use serde::Serialize;

use crate::config::{EngineKind, SourceConfig};

pub enum Source {
    Sql(sql::SqlSource),
    Mongo(mongo::MongoSource),
    Redis(redis::RedisSource),
    /// HTTP+JSON engines: elasticsearch, opensearch, qdrant.
    Rest(rest::RestSource),
}

impl Source {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> anyhow::Result<Self> {
        match cfg.engine {
            EngineKind::MongoDb => Ok(Source::Mongo(mongo::MongoSource::new(
                name,
                cfg,
                force_readonly,
            ))),
            // Valkey is Redis-protocol compatible; OpenSearch is ES-API
            // compatible — each rides the same source.
            EngineKind::Redis | EngineKind::Valkey => Ok(Source::Redis(redis::RedisSource::new(
                name,
                cfg,
                force_readonly,
            ))),
            EngineKind::Elasticsearch | EngineKind::OpenSearch | EngineKind::Qdrant => Ok(
                Source::Rest(rest::RestSource::new(name, cfg, force_readonly)),
            ),
            _ => Ok(Source::Sql(sql::SqlSource::new(name, cfg, force_readonly))),
        }
    }

    pub fn info(&self, name: &str) -> SourceInfo {
        let (engine, cfg, readonly) = match self {
            Source::Sql(s) => (s.engine(), s.config(), s.readonly()),
            Source::Mongo(s) => (s.engine(), s.config(), s.readonly()),
            Source::Redis(s) => (s.engine(), s.config(), s.readonly()),
            Source::Rest(s) => (s.engine(), s.config(), s.readonly()),
        };
        SourceInfo {
            name: name.to_string(),
            engine: engine.name(),
            description: cfg.description.clone(),
            readonly,
            remote: cfg.ssh.is_some(),
        }
    }

    pub fn readonly(&self) -> bool {
        match self {
            Source::Sql(s) => s.readonly(),
            Source::Mongo(s) => s.readonly(),
            Source::Redis(s) => s.readonly(),
            Source::Rest(s) => s.readonly(),
        }
    }

    pub async fn close(&self) {
        match self {
            Source::Sql(s) => s.close().await,
            Source::Mongo(s) => s.close().await,
            Source::Redis(s) => s.close().await,
            Source::Rest(s) => s.close().await,
        }
    }

    /// Introspection for the `schema` tool and the `ds://{name}/schema`
    /// resource. Without `table`: list tables/collections (SQL/mongo) or the
    /// keyspace (redis). With `table`: describe columns (SQL), indexes
    /// (mongo), or a key's type + ttl (redis).
    pub async fn schema(
        &self,
        database: Option<&str>,
        table: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        use serde_json::json;
        match self {
            Source::Sql(s) => Ok(match table {
                Some(t) => json!({
                    "engine": s.engine().name(),
                    "table": t,
                    "columns": s.query(&s.describe_table_sql(t, database), 500).await?,
                }),
                None => json!({
                    "engine": s.engine().name(),
                    "tables": s.query(&s.list_tables_sql(database), 1000).await?,
                }),
            }),
            Source::Mongo(m) => Ok(match table {
                Some(c) => json!({
                    "engine": "mongodb",
                    "collection": c,
                    "indexes": m.list_indexes(database, c).await?,
                }),
                None => json!({
                    "engine": "mongodb",
                    "collections": m.list_collections(database).await?,
                }),
            }),
            Source::Redis(r) => Ok(match table {
                Some(key) => json!({
                    "engine": r.engine().name(),
                    "key": key,
                    "type": r.command(&["TYPE".into(), key.into()]).await?,
                    "ttl": r.command(&["TTL".into(), key.into()]).await?,
                }),
                None => json!({
                    "engine": r.engine().name(),
                    "keyspace": r.command(&["INFO".into(), "keyspace".into()]).await?,
                }),
            }),
            // ES/OpenSearch call these "index/indices"; Qdrant
            // "collection/collections".
            Source::Rest(r) => {
                let (one, many) = if r.engine() == EngineKind::Qdrant {
                    ("collection", "collections")
                } else {
                    ("index", "indices")
                };
                let mut obj = serde_json::Map::new();
                obj.insert("engine".into(), r.engine().name().into());
                match table {
                    Some(name) => {
                        obj.insert(one.into(), name.into());
                        obj.insert("detail".into(), r.describe(name).await?);
                    }
                    None => {
                        obj.insert(many.into(), r.list_containers().await?);
                    }
                }
                Ok(serde_json::Value::Object(obj))
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
