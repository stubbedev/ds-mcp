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

use crate::config::SourceConfig;

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
        // Fail closed: an aggregate whose pipeline we cannot inspect as an
        // array is treated as a write (kept off the read path).
        let Ok(pipeline) = cmd.get_array("pipeline") else {
            return Ok(false);
        };
        return Ok(!pipeline.iter().any(stage_writes));
    }
    Ok(READ_COMMANDS.contains(&name.as_str()))
}

/// Does this pipeline stage (or anything nested in it) write? `$out`/`$merge`
/// only appear as stage operators — field names in stored docs cannot start
/// with `$` — so scanning for those keys anywhere is safe and catches them
/// inside `$facet`, `$unionWith`/`$lookup` sub-pipelines, etc. Genuine MongoDB
/// rejects a writing stage in those positions, but Mongo-compatible backends
/// (`FerretDB`, `CosmosDB`, `DocumentDB`) may not — so we do not rely on the server.
fn stage_writes(stage: &Bson) -> bool {
    match stage {
        Bson::Document(d) => {
            d.contains_key("$out") || d.contains_key("$merge") || d.values().any(stage_writes)
        }
        Bson::Array(a) => a.iter().any(stage_writes),
        _ => false,
    }
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

    pub const fn config(&self) -> &SourceConfig {
        &self.cfg
    }

    pub const fn readonly(&self) -> bool {
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
    /// {documents, count, `has_more`}; every other command returns its raw
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
                cmd.insert("limit", fetch_limit(limit));
                Ok(cursor_docs(&db.run_command(cmd).await?, limit))
            }
            (Some(limit), "aggregate") => {
                if !cmd.contains_key("cursor") {
                    cmd.insert("cursor", Document::new());
                }
                if let Ok(pipeline) = cmd.get_array_mut("pipeline") {
                    pipeline.push(Bson::Document(bson::doc! {"$limit": fetch_limit(limit)}));
                }
                Ok(cursor_docs(&db.run_command(cmd).await?, limit))
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
            .map(|m| bson::to_bson(&m).map_or(Value::Null, Bson::into_relaxed_extjson))
            .collect())
    }
}

/// One more than the requested cap, as the i64 the wire protocol wants — that
/// extra document is what tells us more exist. A cap too large for an i64 is a
/// caller asking for more than the protocol can express, so saturate.
fn fetch_limit(limit: usize) -> i64 {
    i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)
}

/// Extract `cursor.firstBatch` from a find/aggregate result, applying the
/// row cap (the command fetched limit+1 to detect more).
fn cursor_docs(result: &Document, limit: usize) -> Value {
    let cursor = result.get_document("cursor").ok();
    let batch = cursor
        .and_then(|c| c.get_array("firstBatch").ok())
        .cloned()
        .unwrap_or_default();
    // A live cursor id means more batches exist server-side. Length alone is
    // not enough: a user-set batchSize smaller than the cap keeps firstBatch
    // short no matter how many documents match.
    let id_open = cursor
        .and_then(|c| match c.get("id") {
            Some(Bson::Int64(id)) => Some(*id != 0),
            Some(Bson::Int32(id)) => Some(*id != 0),
            _ => None,
        })
        .unwrap_or(false);
    let mut docs: Vec<Value> = batch.into_iter().map(Bson::into_relaxed_extjson).collect();
    let has_more = docs.len() > limit || id_open;
    docs.truncate(limit);
    tabulate(&docs, has_more)
}

/// Uniform documents project to the SQL result shape — field names declared
/// once in `columns`, one positional row per document — which is both the
/// token-lean encoding (no key repeated per document) and the same tabular
/// shape the PII filter already redacts positionally. Anything mixed keeps
/// the per-document `documents` array.
fn tabulate(docs: &[Value], truncated: bool) -> Value {
    let Some(columns) = uniform_columns(docs) else {
        return serde_json::json!({
            "documents": docs,
            "count": docs.len(),
            "has_more": truncated,
        });
    };
    let rows: Vec<Vec<Value>> = docs
        .iter()
        .map(|doc| {
            let obj = doc
                .as_object()
                .expect("uniform_columns checked every document is an object");
            columns
                .iter()
                .map(|k| obj.get(k).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    serde_json::json!({
        "columns": columns,
        "rows": rows,
        "row_count": rows.len(),
        "truncated": truncated,
    })
}

/// The shared field list when every document is an object with the same key
/// set (in the first document's order), else None. Empty key sets and empty
/// batches do not qualify.
fn uniform_columns(docs: &[Value]) -> Option<Vec<String>> {
    let first = docs.first()?.as_object()?;
    if first.is_empty() {
        return None;
    }
    let columns: Vec<String> = first.keys().cloned().collect();
    let uniform = docs[1..].iter().all(|doc| {
        doc.as_object().is_some_and(|map| {
            map.len() == columns.len() && columns.iter().all(|k| map.contains_key(k))
        })
    });
    uniform.then_some(columns)
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
    fn nested_write_stages_are_caught() {
        let read = |d: Document| command_is_read(&d).unwrap();
        // $merge/$out nested inside $unionWith / $lookup / $facet sub-pipelines
        // must be treated as writes (genuine MongoDB rejects them, but
        // Mongo-compatible backends may not — the gate cannot trust the server).
        assert!(!read(bson::doc! {"aggregate": "t", "pipeline": [
            {"$unionWith": {"coll": "y", "pipeline": [{"$merge": "victim"}]}}
        ]}));
        assert!(!read(bson::doc! {"aggregate": "t", "pipeline": [
            {"$lookup": {"from": "y", "pipeline": [{"$out": "victim"}], "as": "j"}}
        ]}));
        assert!(!read(bson::doc! {"aggregate": "t", "pipeline": [
            {"$facet": {"a": [{"$merge": "victim"}]}}
        ]}));
        // A non-array pipeline fails closed (treated as write).
        assert!(!read(bson::doc! {"aggregate": "t", "pipeline": "nope"}));
        // A genuinely nested read pipeline still classifies as read.
        assert!(read(bson::doc! {"aggregate": "t", "pipeline": [
            {"$unionWith": {"coll": "y", "pipeline": [{"$match": {"a": 1}}]}}
        ]}));
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

    #[test]
    fn has_more_survives_a_small_batch_size() {
        // A firstBatch shorter than the cap with a live cursor id must still
        // report truncation: the server simply has not sent the rest yet.
        // Uniform documents, so the result is the tabular shape.
        let result = bson::doc! {
            "cursor": {
                "id": 42,
                "firstBatch": [ {"a": 1}, {"a": 2} ],
            },
            "ok": 1,
        };
        let v = cursor_docs(&result, 100);
        assert_eq!(v["truncated"], serde_json::json!(true));
        assert_eq!(v["row_count"], serde_json::json!(2));

        // Exhausted cursor, batch within the cap: no more.
        let result = bson::doc! {
            "cursor": {
                "id": 0,
                "firstBatch": [ {"a": 1}, {"a": 2} ],
            },
            "ok": 1,
        };
        let v = cursor_docs(&result, 100);
        assert_eq!(v["truncated"], serde_json::json!(false));

        // Over-cap batch (the limit+1 probe): truncated via length.
        let result = bson::doc! {
            "cursor": {
                "id": 0,
                "firstBatch": [ {"a": 1}, {"a": 2}, {"a": 3} ],
            },
            "ok": 1,
        };
        let v = cursor_docs(&result, 2);
        assert_eq!(v["truncated"], serde_json::json!(true));
        assert_eq!(v["row_count"], serde_json::json!(2));
    }

    #[test]
    fn uniform_documents_project_to_the_tabular_shape() {
        let result = bson::doc! {
            "cursor": {
                "id": 0,
                "firstBatch": [
                    {"_id": 1, "sku": "A1", "nested": {"qty": 2}},
                    {"_id": 2, "sku": "B2", "nested": {"qty": 5}},
                ],
            },
            "ok": 1,
        };
        let v = cursor_docs(&result, 10);
        assert_eq!(v["columns"], serde_json::json!(["_id", "sku", "nested"]));
        // Field order follows the first document; values are positional,
        // nested values ride along as cells.
        assert_eq!(
            v["rows"],
            serde_json::json!([
                [1, "A1", {"qty": 2}],
                [2, "B2", {"qty": 5}],
            ])
        );
        assert_eq!(v["row_count"], serde_json::json!(2));
        // Key order differing between documents does not break uniformity.
        let result = bson::doc! {
            "cursor": {
                "id": 0,
                "firstBatch": [
                    {"_id": 1, "sku": "A1"},
                    {"sku": "B2", "_id": 2},
                ],
            },
            "ok": 1,
        };
        let v = cursor_docs(&result, 10);
        assert_eq!(v["rows"][1], serde_json::json!([2, "B2"]));
    }

    #[test]
    fn mixed_documents_keep_the_per_document_shape() {
        for batch in [
            // A missing key in one document.
            vec![
                bson::doc! {"a": 1, "b": 2}.into(),
                bson::doc! {"a": 3}.into(),
            ],
            // An extra key in one document.
            vec![
                bson::doc! {"a": 1}.into(),
                bson::doc! {"a": 2, "b": 3}.into(),
            ],
            // A non-object element.
            vec![bson::doc! {"a": 1}.into(), bson::Bson::Int32(7)],
        ] {
            let result = bson::doc! {
                "cursor": {"id": 0, "firstBatch": batch},
                "ok": 1,
            };
            let v = cursor_docs(&result, 10);
            assert_eq!(v["count"], serde_json::json!(2), "{v}");
            assert!(v["documents"].is_array(), "{v}");
            assert!(v.get("columns").is_none(), "{v}");
        }

        // An empty batch has no columns to declare.
        let result = bson::doc! {
            "cursor": {"id": 0, "firstBatch": []},
            "ok": 1,
        };
        let v = cursor_docs(&result, 10);
        assert_eq!(v["documents"], serde_json::json!([]));
    }
}
