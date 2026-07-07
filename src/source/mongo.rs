//! MongoDB sources. Filters/documents arrive as JSON tool arguments and are
//! interpreted as MongoDB Extended JSON (so {"$oid": ...} etc. work).

use anyhow::{Context, Result, bail};
use bson::{Bson, Document};
use futures_util::TryStreamExt;
use mongodb::options::{ClientOptions, IndexOptions};
use mongodb::{Client, Collection, Database, IndexModel};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::config::{EngineKind, SourceConfig};

pub struct MongoSource {
    name: String,
    cfg: SourceConfig,
    readonly: bool,
    client: OnceCell<Client>,
}

#[derive(Serialize)]
pub struct DocsOut {
    pub documents: Vec<Value>,
    pub count: usize,
    pub has_more: bool,
}

/// Convert a JSON tool argument to a BSON document (Extended JSON aware).
pub fn to_doc(v: Value) -> Result<Document> {
    match Bson::try_from(v).context("invalid Extended JSON")? {
        Bson::Document(d) => Ok(d),
        other => bail!("expected a JSON object, got {}", other.element_type() as u8),
    }
}

fn doc_to_json(d: Document) -> Value {
    Bson::Document(d).into_relaxed_extjson()
}

impl MongoSource {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> Self {
        let readonly = force_readonly || cfg.readonly;
        Self {
            name: name.to_string(),
            cfg,
            readonly,
            client: OnceCell::new(),
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

    async fn client(&self) -> Result<&Client> {
        self.client
            .get_or_try_init(|| async {
                let uri = self.cfg.dsn.as_deref().expect("validated: mongodb has uri");
                let mut opts = ClientOptions::parse(uri).await?;
                opts.connect_timeout = Some(self.cfg.connect_timeout());
                opts.server_selection_timeout = Some(self.cfg.connect_timeout());
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

    #[allow(clippy::too_many_arguments)]
    pub async fn find(
        &self,
        database: Option<&str>,
        collection: &str,
        filter: Document,
        projection: Option<Document>,
        sort: Option<Document>,
        limit: usize,
        skip: Option<u64>,
    ) -> Result<DocsOut> {
        let coll = self.coll(database, collection).await?;
        let mut find = coll.find(filter).limit((limit + 1) as i64);
        if let Some(p) = projection {
            find = find.projection(p);
        }
        if let Some(s) = sort {
            find = find.sort(s);
        }
        if let Some(s) = skip {
            find = find.skip(s);
        }
        let docs: Vec<Document> = find.await?.try_collect().await?;
        Ok(docs_out(docs, limit))
    }

    /// Pipelines containing $out/$merge write; the caller gates those on
    /// readonly sources.
    pub fn pipeline_writes(pipeline: &[Document]) -> bool {
        pipeline
            .iter()
            .any(|stage| stage.contains_key("$out") || stage.contains_key("$merge"))
    }

    pub async fn aggregate(
        &self,
        database: Option<&str>,
        collection: &str,
        mut pipeline: Vec<Document>,
        limit: usize,
    ) -> Result<DocsOut> {
        let coll = self.coll(database, collection).await?;
        let writes = Self::pipeline_writes(&pipeline);
        if !writes {
            pipeline.push(bson::doc! {"$limit": (limit + 1) as i64});
        }
        let docs: Vec<Document> = coll.aggregate(pipeline).await?.try_collect().await?;
        Ok(docs_out(docs, limit))
    }

    pub async fn count(
        &self,
        database: Option<&str>,
        collection: &str,
        filter: Document,
    ) -> Result<u64> {
        Ok(self
            .coll(database, collection)
            .await?
            .count_documents(filter)
            .await?)
    }

    pub async fn distinct(
        &self,
        database: Option<&str>,
        collection: &str,
        field: &str,
        filter: Document,
    ) -> Result<Vec<Value>> {
        let values = self
            .coll(database, collection)
            .await?
            .distinct(field, filter)
            .await?;
        Ok(values.into_iter().map(Bson::into_relaxed_extjson).collect())
    }

    pub async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(self.client().await?.list_database_names().await?)
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

    pub async fn insert(
        &self,
        database: Option<&str>,
        collection: &str,
        documents: Vec<Document>,
    ) -> Result<Vec<Value>> {
        let result = self
            .coll(database, collection)
            .await?
            .insert_many(documents)
            .await?;
        let mut ids: Vec<_> = result.inserted_ids.into_iter().collect();
        ids.sort_by_key(|(i, _)| *i);
        Ok(ids
            .into_iter()
            .map(|(_, id)| id.into_relaxed_extjson())
            .collect())
    }

    pub async fn update(
        &self,
        database: Option<&str>,
        collection: &str,
        filter: Document,
        update: Document,
        many: bool,
        upsert: bool,
    ) -> Result<Value> {
        let coll = self.coll(database, collection).await?;
        let result = if many {
            coll.update_many(filter, update).upsert(upsert).await?
        } else {
            coll.update_one(filter, update).upsert(upsert).await?
        };
        Ok(serde_json::json!({
            "matched": result.matched_count,
            "modified": result.modified_count,
            "upserted_id": result.upserted_id.map(Bson::into_relaxed_extjson),
        }))
    }

    pub async fn delete(
        &self,
        database: Option<&str>,
        collection: &str,
        filter: Document,
        many: bool,
    ) -> Result<Value> {
        let coll = self.coll(database, collection).await?;
        let result = if many {
            coll.delete_many(filter).await?
        } else {
            coll.delete_one(filter).await?
        };
        Ok(serde_json::json!({"deleted": result.deleted_count}))
    }

    pub async fn create_index(
        &self,
        database: Option<&str>,
        collection: &str,
        keys: Document,
        unique: bool,
        name: Option<String>,
    ) -> Result<String> {
        let options = IndexOptions::builder().unique(unique).name(name).build();
        let model = IndexModel::builder().keys(keys).options(options).build();
        let result = self
            .coll(database, collection)
            .await?
            .create_index(model)
            .await?;
        Ok(result.index_name)
    }

    pub async fn drop_index(
        &self,
        database: Option<&str>,
        collection: &str,
        name: &str,
    ) -> Result<()> {
        Ok(self
            .coll(database, collection)
            .await?
            .drop_index(name)
            .await?)
    }

    pub async fn create_collection(&self, database: Option<&str>, name: &str) -> Result<()> {
        Ok(self.db(database).await?.create_collection(name).await?)
    }

    pub async fn drop_collection(&self, database: Option<&str>, collection: &str) -> Result<()> {
        Ok(self.coll(database, collection).await?.drop().await?)
    }
}

fn docs_out(mut docs: Vec<Document>, limit: usize) -> DocsOut {
    let has_more = docs.len() > limit;
    docs.truncate(limit);
    DocsOut {
        count: docs.len(),
        documents: docs.into_iter().map(doc_to_json).collect(),
        has_more,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_write_detection() {
        let read: Vec<Document> = vec![bson::doc! {"$match": {"a": 1}}];
        assert!(!MongoSource::pipeline_writes(&read));
        let out: Vec<Document> = vec![bson::doc! {"$match": {}}, bson::doc! {"$out": "target"}];
        assert!(MongoSource::pipeline_writes(&out));
        let merge: Vec<Document> = vec![bson::doc! {"$merge": {"into": "t"}}];
        assert!(MongoSource::pipeline_writes(&merge));
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
