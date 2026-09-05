//! A source is one named connection from the config. Two families: SQL
//! engines and document stores. New engine = new variant + match arms.

pub mod endpoint;
pub mod mongo;
pub mod redis;
pub mod rest;
pub mod sql;
pub mod ssh;

use serde::Serialize;
use serde_json::Value;

use crate::config::{EngineKind, SourceConfig};

pub enum Source {
    Sql(sql::SqlSource),
    Mongo(mongo::MongoSource),
    Redis(redis::RedisSource),
    /// HTTP+JSON engines: elasticsearch, opensearch, qdrant.
    Rest(rest::RestSource),
}

impl Source {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> Self {
        match cfg.engine {
            EngineKind::MongoDb => Self::Mongo(mongo::MongoSource::new(name, cfg, force_readonly)),
            // Valkey is Redis-protocol compatible; OpenSearch is ES-API
            // compatible — each rides the same source.
            EngineKind::Redis | EngineKind::Valkey => {
                Self::Redis(redis::RedisSource::new(name, cfg, force_readonly))
            }
            EngineKind::Elasticsearch | EngineKind::OpenSearch | EngineKind::Qdrant => {
                Self::Rest(rest::RestSource::new(name, cfg, force_readonly))
            }
            _ => Self::Sql(sql::SqlSource::new(name, cfg, force_readonly)),
        }
    }

    pub const fn config(&self) -> &SourceConfig {
        match self {
            Self::Sql(s) => s.config(),
            Self::Mongo(s) => s.config(),
            Self::Redis(s) => s.config(),
            Self::Rest(s) => s.config(),
        }
    }

    /// This source's outbound redaction rules, if any.
    pub const fn pii(&self) -> Option<&crate::config::Pii> {
        self.config().pii.as_ref()
    }

    pub fn info(&self, name: &str) -> SourceInfo {
        let cfg = self.config();
        SourceInfo {
            name: name.to_string(),
            engine: cfg.engine.name(),
            description: cfg.description.clone(),
            readonly: self.readonly(),
            remote: cfg.ssh.is_some(),
            pii: crate::pii::Filter::new(self.pii(), &[]).is_some(),
        }
    }

    pub const fn readonly(&self) -> bool {
        match self {
            Self::Sql(s) => s.readonly(),
            Self::Mongo(s) => s.readonly(),
            Self::Redis(s) => s.readonly(),
            Self::Rest(s) => s.readonly(),
        }
    }

    pub async fn close(&self) {
        match self {
            Self::Sql(s) => s.close().await,
            Self::Mongo(s) => s.close().await,
            // Redis' multiplexed connection and reqwest's pool are dropped
            // with the source; there is nothing to close.
            Self::Redis(_) | Self::Rest(_) => {}
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
            Self::Sql(s) => Ok(match table {
                Some(t) => {
                    let mut columns = s.query(&s.describe_table_sql(t, database), 500).await?;
                    let tables = vec![t.to_string()];
                    if let Some(filter) = crate::pii::Filter::new(self.pii(), &tables) {
                        filter.mark_described_columns(&mut columns);
                    }
                    json!({ "engine": s.engine().name(), "table": t, "columns": columns })
                }
                None => json!({
                    "engine": s.engine().name(),
                    "tables": s.query(&s.list_tables_sql(database), 1000).await?,
                }),
            }),
            Self::Mongo(m) => Ok(match table {
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
            Self::Redis(r) => Ok(match table {
                Some(key) => json!({
                    "engine": r.engine().name(),
                    "key": key,
                    "type": r.command(&["TYPE".into(), key.into()], database).await?,
                    "ttl": r.command(&["TTL".into(), key.into()], database).await?,
                }),
                None => json!({
                    "engine": r.engine().name(),
                    "keyspace": r.command(&["INFO".into(), "keyspace".into()], database).await?,
                }),
            }),
            // ES/OpenSearch call these "index/indices"; Qdrant
            // "collection/collections".
            Self::Rest(r) => {
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
    /// Sensitive column/field values are redacted on the way out of this
    /// source; querying them again will not reveal more.
    pub pii: bool,
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

/// Lowercase hex, no separators.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            // Writing to a String cannot fail.
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Decode a byte string to JSON: text when it is valid UTF-8, a `0x…` hex dump
/// when it is not. Shared by the SQL and Redis paths, which both hand back raw
/// bytes an engine never promised were text.
pub fn bytes_value(v: Vec<u8>) -> Value {
    match String::from_utf8(v) {
        Ok(s) => Value::String(s),
        Err(e) => Value::String(format!("0x{}", hex(&e.into_bytes()))),
    }
}

#[derive(Serialize)]
pub struct ExecResult {
    pub rows_affected: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_insert_id: Option<u64>,
}
