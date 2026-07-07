//! The MCP tool surface. All failures are tool results (is_error), never
//! protocol errors, so the model always sees the message.

// Helpers deliberately use Result<T, CallToolResult> so `?`-style early
// returns produce the error tool result; the Err size is irrelevant here.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::registry::Registry;
use crate::source::mongo::MongoSource;
use crate::source::sql::SqlSource;

#[derive(Clone)]
pub struct DsServer {
    registry: Arc<Registry>,
    query_timeout: Duration,
    tool_router: ToolRouter<Self>,
}

fn ok_json<T: serde::Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(s) => CallToolResult::success(vec![ContentBlock::text(s)]),
        Err(e) => err(format!("serialize result: {e}")),
    }
}

fn err(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

const DEFAULT_ROW_LIMIT: usize = 1000;

#[derive(Deserialize, JsonSchema)]
pub struct SourceArg {
    /// Name of the configured source (see list_sources).
    pub source: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListTablesArgs {
    pub source: String,
    /// Database/schema to list; defaults to the source's configured database.
    pub database: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DescribeTableArgs {
    pub source: String,
    pub table: String,
    /// Database/schema of the table; defaults to the source's configured database.
    pub database: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadQueryArgs {
    pub source: String,
    /// A single read-only SQL statement (SELECT/SHOW/DESCRIBE/EXPLAIN).
    pub sql: String,
    /// Maximum rows to return. Default 1000.
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SqlArgs {
    pub source: String,
    pub sql: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct DatabaseArgs {
    pub source: String,
    /// Database; defaults to the source's default_database.
    pub database: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CollectionArgs {
    pub source: String,
    pub collection: String,
    /// Database; defaults to the source's default_database.
    pub database: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FilterArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Extended JSON filter object. Omit to match everything.
    pub filter: Option<serde_json::Value>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FindArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Extended JSON filter object. Omit to match everything.
    pub filter: Option<serde_json::Value>,
    /// Extended JSON projection object.
    pub projection: Option<serde_json::Value>,
    /// Extended JSON sort object, e.g. {"created_at": -1}.
    pub sort: Option<serde_json::Value>,
    /// Maximum documents to return. Default 1000.
    pub limit: Option<usize>,
    pub skip: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AggregateArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Extended JSON array of pipeline stages.
    pub pipeline: serde_json::Value,
    /// Maximum documents to return (read pipelines only). Default 1000.
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DistinctArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Field to collect distinct values of.
    pub field: String,
    pub filter: Option<serde_json::Value>,
}

#[derive(Deserialize, JsonSchema)]
pub struct InsertArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Extended JSON array of documents to insert.
    pub documents: serde_json::Value,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Extended JSON filter selecting the documents to update.
    pub filter: serde_json::Value,
    /// Extended JSON update document (update operators like $set).
    pub update: serde_json::Value,
    /// Update all matching documents instead of the first. Default false.
    pub many: Option<bool>,
    /// Insert if nothing matches. Default false.
    pub upsert: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeleteArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Extended JSON filter selecting the documents to delete. Must be non-empty.
    pub filter: serde_json::Value,
    /// Delete all matching documents instead of the first. Default false.
    pub many: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CreateIndexArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Extended JSON index keys, e.g. {"email": 1}.
    pub keys: serde_json::Value,
    pub unique: Option<bool>,
    pub name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DropIndexArgs {
    pub source: String,
    pub collection: String,
    pub database: Option<String>,
    /// Index name to drop.
    pub name: String,
}

impl DsServer {
    pub fn new(registry: Arc<Registry>, query_timeout: Duration) -> Self {
        Self {
            registry,
            query_timeout,
            tool_router: Self::tool_router(),
        }
    }

    fn sql_source(&self, name: &str) -> Result<Arc<crate::source::Source>, CallToolResult> {
        self.registry.get(name).map(Arc::clone).map_err(err)
    }

    /// Run a source future under the configured query timeout, rendering any
    /// error as a tool result.
    async fn run<T: serde::Serialize>(
        &self,
        fut: impl Future<Output = anyhow::Result<T>>,
    ) -> CallToolResult {
        match tokio::time::timeout(self.query_timeout, fut).await {
            Err(_) => err(format!("timed out after {}s", self.query_timeout.as_secs())),
            Ok(Err(e)) => err(format!("{e:#}")),
            Ok(Ok(v)) => ok_json(&v),
        }
    }
}

/// Resolve `$args.source` as a SQL-family source or return the error result.
macro_rules! sql_src {
    ($self:expr, $name:expr) => {
        match $self.sql_source(&$name) {
            Ok(src) => src,
            Err(e) => return e,
        }
    };
}

fn as_sql<'a>(src: &'a crate::source::Source, name: &str) -> Result<&'a SqlSource, CallToolResult> {
    src.as_sql(name).map_err(err)
}

fn as_mongo<'a>(
    src: &'a crate::source::Source,
    name: &str,
) -> Result<&'a MongoSource, CallToolResult> {
    src.as_mongo(name).map_err(err)
}

/// A mongo source that also accepts writes.
fn writable_mongo<'a>(
    src: &'a crate::source::Source,
    name: &str,
) -> Result<&'a MongoSource, CallToolResult> {
    let m = as_mongo(src, name)?;
    if m.readonly() {
        return Err(err(format!("source {name:?} is read-only")));
    }
    Ok(m)
}

/// Optional Extended JSON object argument; None means empty document.
fn opt_doc(v: Option<serde_json::Value>) -> Result<bson::Document, CallToolResult> {
    match v {
        None => Ok(bson::Document::new()),
        Some(v) => crate::source::mongo::to_doc(v).map_err(|e| err(format!("{e:#}"))),
    }
}

/// Optional Extended JSON object argument, kept optional.
fn opt_doc_opt(v: Option<serde_json::Value>) -> Result<Option<bson::Document>, CallToolResult> {
    v.map(|v| crate::source::mongo::to_doc(v).map_err(|e| err(format!("{e:#}"))))
        .transpose()
}

fn req_doc(v: serde_json::Value, what: &str) -> Result<bson::Document, CallToolResult> {
    crate::source::mongo::to_doc(v).map_err(|e| err(format!("{what}: {e:#}")))
}

/// Extended JSON array of objects (pipeline stages, documents to insert).
fn doc_array(v: serde_json::Value) -> Result<Vec<bson::Document>, CallToolResult> {
    let serde_json::Value::Array(items) = v else {
        return Err(err("expected a JSON array"));
    };
    items
        .into_iter()
        .map(|item| crate::source::mongo::to_doc(item).map_err(|e| err(format!("{e:#}"))))
        .collect()
}

macro_rules! unwrap_or_return {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

#[tool_router]
impl DsServer {
    #[tool(
        description = "List the configured data sources: name, engine, description, readonly and remote flags. Call this first to pick the right source.",
        annotations(read_only_hint = true)
    )]
    async fn list_sources(&self) -> CallToolResult {
        ok_json(&serde_json::json!({ "sources": self.registry.list() }))
    }

    #[tool(
        description = "List databases/schemas available on a source (any engine).",
        annotations(read_only_hint = true)
    )]
    async fn list_databases(&self, Parameters(args): Parameters<SourceArg>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        match src.as_ref() {
            crate::source::Source::Sql(sql) => {
                self.run(sql.query(sql.list_databases_sql(), DEFAULT_ROW_LIMIT))
                    .await
            }
            crate::source::Source::Mongo(m) => {
                self.run(async move {
                    let names = m.list_databases().await?;
                    Ok(serde_json::json!({"values": names}))
                })
                .await
            }
        }
    }

    #[tool(
        description = "List tables on a SQL source.",
        annotations(read_only_hint = true)
    )]
    async fn list_tables(&self, Parameters(args): Parameters<ListTablesArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        let stmt = sql.list_tables_sql(args.database.as_deref());
        self.run(sql.query(&stmt, DEFAULT_ROW_LIMIT)).await
    }

    #[tool(
        description = "Describe the columns of a table on a SQL source.",
        annotations(read_only_hint = true)
    )]
    async fn describe_table(
        &self,
        Parameters(args): Parameters<DescribeTableArgs>,
    ) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        let stmt = sql.describe_table_sql(&args.table, args.database.as_deref());
        self.run(sql.query(&stmt, DEFAULT_ROW_LIMIT)).await
    }

    #[tool(
        description = "Run a single read-only SQL statement (SELECT/SHOW/DESCRIBE/EXPLAIN) on a SQL source. Results are capped at `limit` rows with a truncated flag.",
        annotations(read_only_hint = true)
    )]
    async fn read_query(&self, Parameters(args): Parameters<ReadQueryArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        if let Err(e) = crate::sqlguard::ensure_read_only(sql.engine(), &args.sql) {
            return err(e);
        }
        let limit = args.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        self.run(sql.query(&args.sql, limit)).await
    }

    #[tool(
        description = "Run a write/DDL SQL statement (INSERT/UPDATE/DELETE/CREATE/ALTER/...) on a writable SQL source.",
        annotations(destructive_hint = true)
    )]
    async fn write_query(&self, Parameters(args): Parameters<SqlArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        if sql.readonly() {
            return err(format!("source {:?} is read-only", args.source));
        }
        self.run(sql.exec(&args.sql)).await
    }

    #[tool(
        description = "Show the query plan for a SQL statement without executing it.",
        annotations(read_only_hint = true)
    )]
    async fn explain_query(&self, Parameters(args): Parameters<SqlArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        self.run(sql.explain(&args.sql, DEFAULT_ROW_LIMIT)).await
    }

    // ── Document tools (mongodb) ────────────────────────────────────────

    #[tool(
        description = "Query documents in a MongoDB collection. filter/projection/sort are Extended JSON objects.",
        annotations(read_only_hint = true)
    )]
    async fn find(&self, Parameters(args): Parameters<FindArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let filter = unwrap_or_return!(opt_doc(args.filter));
        let projection = unwrap_or_return!(opt_doc_opt(args.projection));
        let sort = unwrap_or_return!(opt_doc_opt(args.sort));
        let limit = args.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        self.run(m.find(
            args.database.as_deref(),
            &args.collection,
            filter,
            projection,
            sort,
            limit,
            args.skip,
        ))
        .await
    }

    #[tool(
        description = "Run an aggregation pipeline on a MongoDB collection. pipeline is an Extended JSON array of stages; $out/$merge require a writable source.",
        annotations(read_only_hint = true)
    )]
    async fn aggregate(&self, Parameters(args): Parameters<AggregateArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let pipeline = unwrap_or_return!(doc_array(args.pipeline));
        if crate::source::mongo::MongoSource::pipeline_writes(&pipeline) && m.readonly() {
            return err(format!(
                "pipeline contains $out/$merge but source {:?} is read-only",
                args.source
            ));
        }
        let limit = args.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        self.run(m.aggregate(args.database.as_deref(), &args.collection, pipeline, limit))
            .await
    }

    #[tool(
        description = "Count documents in a MongoDB collection matching a filter.",
        annotations(read_only_hint = true)
    )]
    async fn count(&self, Parameters(args): Parameters<FilterArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let filter = unwrap_or_return!(opt_doc(args.filter));
        self.run(async move {
            let count = m
                .count(args.database.as_deref(), &args.collection, filter)
                .await?;
            Ok(serde_json::json!({"count": count}))
        })
        .await
    }

    #[tool(
        description = "List distinct values of a field in a MongoDB collection.",
        annotations(read_only_hint = true)
    )]
    async fn distinct(&self, Parameters(args): Parameters<DistinctArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let filter = unwrap_or_return!(opt_doc(args.filter));
        self.run(async move {
            let values = m
                .distinct(
                    args.database.as_deref(),
                    &args.collection,
                    &args.field,
                    filter,
                )
                .await?;
            Ok(serde_json::json!({"values": values}))
        })
        .await
    }

    #[tool(
        description = "List collections in a MongoDB database.",
        annotations(read_only_hint = true)
    )]
    async fn list_collections(&self, Parameters(args): Parameters<DatabaseArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        self.run(async move {
            let names = m.list_collections(args.database.as_deref()).await?;
            Ok(serde_json::json!({"collections": names}))
        })
        .await
    }

    #[tool(
        description = "List indexes on a MongoDB collection.",
        annotations(read_only_hint = true)
    )]
    async fn list_indexes(&self, Parameters(args): Parameters<CollectionArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        self.run(async move {
            let indexes = m
                .list_indexes(args.database.as_deref(), &args.collection)
                .await?;
            Ok(serde_json::json!({"indexes": indexes}))
        })
        .await
    }

    #[tool(
        description = "Insert one or more documents into a MongoDB collection. documents is an Extended JSON array."
    )]
    async fn insert(&self, Parameters(args): Parameters<InsertArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let documents = unwrap_or_return!(doc_array(args.documents));
        if documents.is_empty() {
            return err("documents is empty");
        }
        self.run(async move {
            let ids = m
                .insert(args.database.as_deref(), &args.collection, documents)
                .await?;
            Ok(serde_json::json!({"inserted_ids": ids}))
        })
        .await
    }

    #[tool(
        description = "Update documents in a MongoDB collection. Updates one document unless many=true.",
        annotations(destructive_hint = true)
    )]
    async fn update(&self, Parameters(args): Parameters<UpdateArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let filter = unwrap_or_return!(req_doc(args.filter, "filter"));
        let update = unwrap_or_return!(req_doc(args.update, "update"));
        self.run(m.update(
            args.database.as_deref(),
            &args.collection,
            filter,
            update,
            args.many.unwrap_or(false),
            args.upsert.unwrap_or(false),
        ))
        .await
    }

    #[tool(
        description = "Delete documents from a MongoDB collection. Deletes one document unless many=true. filter must be non-empty.",
        annotations(destructive_hint = true)
    )]
    async fn delete(&self, Parameters(args): Parameters<DeleteArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let filter = unwrap_or_return!(req_doc(args.filter, "filter"));
        if filter.is_empty() {
            return err(
                "refusing to delete with an empty filter; pass an explicit filter \
                 (or drop_collection to remove everything)",
            );
        }
        self.run(m.delete(
            args.database.as_deref(),
            &args.collection,
            filter,
            args.many.unwrap_or(false),
        ))
        .await
    }

    #[tool(description = "Create an index on a MongoDB collection.")]
    async fn create_index(&self, Parameters(args): Parameters<CreateIndexArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let keys = unwrap_or_return!(req_doc(args.keys, "keys"));
        self.run(async move {
            let name = m
                .create_index(
                    args.database.as_deref(),
                    &args.collection,
                    keys,
                    args.unique.unwrap_or(false),
                    args.name,
                )
                .await?;
            Ok(serde_json::json!({"name": name}))
        })
        .await
    }

    #[tool(
        description = "Drop an index from a MongoDB collection.",
        annotations(destructive_hint = true)
    )]
    async fn drop_index(&self, Parameters(args): Parameters<DropIndexArgs>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        self.run(async move {
            m.drop_index(args.database.as_deref(), &args.collection, &args.name)
                .await?;
            Ok(serde_json::json!({"dropped": args.name}))
        })
        .await
    }

    #[tool(description = "Create a collection in a MongoDB database.")]
    async fn create_collection(
        &self,
        Parameters(args): Parameters<CollectionArgs>,
    ) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        self.run(async move {
            m.create_collection(args.database.as_deref(), &args.collection)
                .await?;
            Ok(serde_json::json!({"created": args.collection}))
        })
        .await
    }

    #[tool(
        description = "Drop a MongoDB collection and all its documents.",
        annotations(destructive_hint = true)
    )]
    async fn drop_collection(
        &self,
        Parameters(args): Parameters<CollectionArgs>,
    ) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        self.run(async move {
            m.drop_collection(args.database.as_deref(), &args.collection)
                .await?;
            Ok(serde_json::json!({"dropped": args.collection}))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "DataStore MCP exposes named database sources (SQL and document engines). \
                 Start with list_sources to see what is available, then use the SQL tools \
                 (read_query, list_tables, ...) against SQL sources.",
        )
    }
}
