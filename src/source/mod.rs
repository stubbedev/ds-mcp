//! A source is one named connection from the config. Two families: SQL
//! engines and document stores. New engine = new variant + match arms.

pub mod sql;

use serde::Serialize;

use crate::config::{EngineKind, SourceConfig};

pub enum Source {
    Sql(sql::SqlSource),
    // Mongo lands in phase 2.
}

impl Source {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> anyhow::Result<Self> {
        match cfg.engine {
            EngineKind::MongoDb => {
                anyhow::bail!("mongodb sources are not supported yet (phase 2)")
            }
            EngineKind::Mssql => anyhow::bail!("mssql sources are not supported yet (phase 2)"),
            _ => Ok(Source::Sql(sql::SqlSource::new(name, cfg, force_readonly))),
        }
    }

    pub fn info(&self, name: &str) -> SourceInfo {
        let (engine, cfg, readonly) = match self {
            Source::Sql(s) => (s.engine(), s.config(), s.readonly()),
        };
        SourceInfo {
            name: name.to_string(),
            engine: engine.name(),
            description: cfg.description.clone(),
            readonly,
            remote: cfg.ssh.is_some(),
        }
    }

    pub fn as_sql(&self, _name: &str) -> Result<&sql::SqlSource, String> {
        // _name feeds the wrong-engine error message once Mongo lands.
        match self {
            Source::Sql(s) => Ok(s),
        }
    }

    pub async fn close(&self) {
        match self {
            Source::Sql(s) => s.close().await,
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
