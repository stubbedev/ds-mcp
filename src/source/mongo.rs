//! MongoDB sources. The tool layer sends runCommand-style command documents
//! (e.g. {"find": "c", "filter": {...}}); they are interpreted as MongoDB
//! Extended JSON so {"$oid": ...} etc. work.

use anyhow::{Context, Result, bail};
use bson::{Bson, Document};
use futures_util::TryStreamExt;
use mongodb::options::ClientOptions;
use mongodb::{Client, Collection, Database, IndexModel};
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::config::{EngineKind, SourceConfig};

pub struct MongoSource {
    name: String,
    cfg: SourceConfig,
    readonly: bool,
    client: OnceCell<Client>,
    /// Keeps the ssh forward alive for the life of the client.
    tunnel: OnceCell<super::ssh::SshTunnel>,
}

/// Commands that only read. The first key of a command document names it.
/// `aggregate` is read unless its pipeline writes ($out/$merge).
const READ_COMMANDS: &[&str] = &[
    "find",
    "aggregate",
    "count",
    "distinct",
    "listcollections",
    "listindexes",
    "listdatabases",
    "dbstats",
    "collstats",
    "estimateddocumentcount",
    "explain",
    "ping",
    "hello",
    "ismaster",
    "buildinfo",
    "serverstatus",
    "connectionstatus",
    "getmore",
    "geosearch",
];

/// Convert a JSON tool argument to a BSON document (Extended JSON aware).
pub fn to_doc(v: Value) -> Result<Document> {
    match Bson::try_from(v).context("invalid Extended JSON")? {
        Bson::Document(d) => Ok(d),
        _ => bail!("expected a JSON object"),
    }
}

fn doc_to_json(d: Document) -> Value {
    Bson::Document(d).into_relaxed_extjson()
}

/// Is this command document a read? Err on an empty document.
pub fn command_is_read(cmd: &Document) -> Result<bool> {
    let name = cmd
        .keys()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty command document"))?
        .to_ascii_lowercase();
    if name == "aggregate" {
        if let Ok(pipeline) = cmd.get_array("pipeline")
            && pipeline.iter().any(stage_writes)
        {
            return Ok(false);
        }
        return Ok(true);
    }
    Ok(READ_COMMANDS.contains(&name.as_str()))
}

fn stage_writes(stage: &Bson) -> bool {
    stage
        .as_document()
        .is_some_and(|d| d.contains_key("$out") || d.contains_key("$merge"))
}

impl MongoSource {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> Self {
        let readonly = force_readonly || cfg.readonly;
        Self {
            name: name.to_string(),
            cfg,
            readonly,
            client: OnceCell::new(),
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
        if let Some(client) = self.client.get() {
            client.clone().shutdown().await;
        }
    }

    /// Connection string from the config: the dsn verbatim, or one built
    /// from host/port/user/password/database (defaults: localhost:27017).
    fn uri(&self) -> String {
        if let Some(dsn) = &self.cfg.dsn {
            return dsn.clone();
        }
        let auth = match (&self.cfg.user, &self.cfg.password) {
            (Some(u), Some(p)) => format!("{u}:{p}@"),
            (Some(u), None) => format!("{u}@"),
            _ => String::new(),
        };
        let host = self.cfg.host.as_deref().unwrap_or("127.0.0.1");
        let port = self.cfg.port.unwrap_or(27017);
        let db = self.cfg.database.as_deref().unwrap_or("");
        format!("mongodb://{auth}{host}:{port}/{db}")
    }

    async fn client(&self) -> Result<&Client> {
        self.client
            .get_or_try_init(|| async {
                let mut opts = ClientOptions::parse(self.uri()).await?;
                opts.connect_timeout = Some(self.cfg.connect_timeout());
                opts.server_selection_timeout = Some(self.cfg.connect_timeout());
                if self.cfg.ssh.is_some() || self.cfg.docker.is_some() {
                    // Reroute the first URI host through the tunnel/container.
                    // Replica-set discovery cannot cross either, so force a
                    // direct connection.
                    let Some(mongodb::options::ServerAddress::Tcp { host, port }) =
                        opts.hosts.first().cloned()
                    else {
                        anyhow::bail!("ssh/docker access needs a tcp host in the mongodb uri");
                    };
                    let ep =
                        super::endpoint::resolve(&self.cfg, &host, port.unwrap_or(27017)).await?;
                    opts.hosts = vec![mongodb::options::ServerAddress::Tcp {
                        host: ep.host,
                        port: Some(ep.port),
                    }];
                    opts.direct_connection = Some(true);
                    if let Some(t) = ep.tunnel {
                        let _ = self.tunnel.set(t);
                    }
                }
                Ok::<_, anyhow::Error>(Client::with_options(opts)?)
            })
            .await
            .with_context(|| format!("connect to source {:?}", self.name))
    }

    async fn db(&self, database: Option<&str>) -> Result<Database> {
        let client = self.client().await?;
        match database.or(self.cfg.default_database.as_deref()) {
            Some(name) => Ok(client.database(name)),
            None => client.default_database().ok_or_else(|| {
                anyhow::anyhow!(
                    "no database given; pass `database` or set default_database on source {:?}",
                    self.name
                )
            }),
        }
    }

    async fn coll(&self, database: Option<&str>, collection: &str) -> Result<Collection<Document>> {
        Ok(self.db(database).await?.collection(collection))
    }

    /// Run a command document. When `cap` is Some and the command is
    /// find/aggregate, a limit is injected and the cursor is normalized to
    /// {documents, count, has_more}; every other command returns its raw
    /// result document.
    pub async fn run_command(
        &self,
        database: Option<&str>,
        mut cmd: Document,
        cap: Option<usize>,
    ) -> Result<Value> {
        let db = self.db(database).await?;
        let name = cmd
            .keys()
            .next()
            .map(|k| k.to_ascii_lowercase())
            .unwrap_or_default();
        match (cap, name.as_str()) {
            (Some(limit), "find") => {
                cmd.insert("limit", (limit + 1) as i64);
                Ok(cursor_docs(db.run_command(cmd).await?, limit))
            }
            (Some(limit), "aggregate") => {
                if !cmd.contains_key("cursor") {
                    cmd.insert("cursor", Document::new());
                }
                if let Ok(pipeline) = cmd.get_array_mut("pipeline") {
                    pipeline.push(Bson::Document(bson::doc! {"$limit": (limit + 1) as i64}));
                }
                Ok(cursor_docs(db.run_command(cmd).await?, limit))
            }
            _ => Ok(doc_to_json(db.run_command(cmd).await?)),
        }
    }

    pub async fn ping(&self) -> Result<()> {
        self.client()
            .await?
            .database("admin")
            .run_command(bson::doc! {"ping": 1})
            .await?;
        Ok(())
    }

    pub async fn list_collections(&self, database: Option<&str>) -> Result<Vec<String>> {
        let mut names = self.db(database).await?.list_collection_names().await?;
        names.sort();
        Ok(names)
    }

    pub async fn list_indexes(
        &self,
        database: Option<&str>,
        collection: &str,
    ) -> Result<Vec<Value>> {
        let indexes: Vec<IndexModel> = self
            .coll(database, collection)
            .await?
            .list_indexes()
            .await?
            .try_collect()
            .await?;
        Ok(indexes
            .into_iter()
            .map(|m| {
                bson::to_bson(&m)
                    .map(Bson::into_relaxed_extjson)
                    .unwrap_or(Value::Null)
            })
            .collect())
    }
}

/// Extract `cursor.firstBatch` from a find/aggregate result, applying the
/// row cap (the command fetched limit+1 to detect more).
fn cursor_docs(result: Document, limit: usize) -> Value {
    let batch = result
        .get_document("cursor")
        .ok()
        .and_then(|c| c.get_array("firstBatch").ok())
        .cloned()
        .unwrap_or_default();
    let mut docs: Vec<Value> = batch.into_iter().map(Bson::into_relaxed_extjson).collect();
    let has_more = docs.len() > limit;
    docs.truncate(limit);
    serde_json::json!({
        "documents": docs,
        "count": docs.len(),
        "has_more": has_more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_read_vs_write() {
        let read = |d: Document| command_is_read(&d).unwrap();
        assert!(read(bson::doc! {"find": "t", "filter": {}}));
        assert!(read(bson::doc! {"count": "t"}));
        assert!(read(
            bson::doc! {"aggregate": "t", "pipeline": [{"$match": {}}]}
        ));
        assert!(!read(bson::doc! {"insert": "t", "documents": []}));
        assert!(!read(bson::doc! {"update": "t"}));
        assert!(!read(bson::doc! {"delete": "t"}));
        assert!(!read(bson::doc! {"createIndexes": "t"}));
        assert!(!read(
            bson::doc! {"aggregate": "t", "pipeline": [{"$out": "dest"}]}
        ));
        assert!(command_is_read(&Document::new()).is_err());
    }

    #[test]
    fn extended_json_parses() {
        let doc = to_doc(serde_json::json!({"_id": {"$oid": "507f1f77bcf86cd799439011"}})).unwrap();
        assert!(matches!(doc.get("_id"), Some(Bson::ObjectId(_))));
    }

    #[test]
    fn non_object_rejected() {
        assert!(to_doc(serde_json::json!([1, 2])).is_err());
    }
}
