//! The MCP tool surface. All failures are tool results (is_error), never
//! protocol errors, so the model always sees the message.

// Helpers deliberately use Result<T, CallToolResult> so `?`-style early
// returns produce the error tool result; the Err size is irrelevant here.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::registry::{Registry, Resolver};
use crate::source::mongo::MongoSource;
use crate::source::sql::SqlSource;

#[derive(Clone)]
pub struct DsServer {
    resolver: Arc<Resolver>,
    /// Workspace roots fetched from this session's client; cleared on
    /// roots/list_changed. One DsServer instance == one session.
    roots_cache: Arc<tokio::sync::Mutex<Option<Vec<std::path::PathBuf>>>>,
    tool_router: ToolRouter<Self>,
}

/// Text content for humans/older clients + structuredContent for clients
/// that parse it.
fn ok_json<T: serde::Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(v) => {
            let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            result.structured_content = Some(v);
            result
        }
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
    /// Return the query plan instead of executing.
    pub explain: Option<bool>,
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
    /// Return the query plan instead of executing.
    pub explain: Option<bool>,
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

#[derive(Deserialize, JsonSchema)]
pub struct RedisCommandArgs {
    pub source: String,
    /// Command and arguments as an array, e.g. ["HGETALL", "user:1"].
    pub command: Vec<String>,
}

impl DsServer {
    pub fn new(resolver: Arc<Resolver>) -> Self {
        Self {
            resolver,
            roots_cache: Arc::new(tokio::sync::Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// Which registry does this call use? Precedence:
    /// 1. roots injected via HTTP headers (request-scoped, never cached),
    /// 2. the client's roots/list (cached per session, cleared on
    ///    roots/list_changed; per-root registries cached by config mtime),
    /// 3. the global config registry.
    async fn registry(&self, ctx: &RequestContext<RoleServer>) -> Result<Arc<Registry>, String> {
        if let Some(parts) = ctx.extensions.get::<http::request::Parts>() {
            let values = ROOTS_HEADERS.iter().flat_map(|h| {
                parts
                    .headers
                    .get_all(*h)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
            });
            let paths = crate::registry::parse_root_paths(values);
            if !paths.is_empty() {
                // Header roots are authoritative: no fallback to the client's
                // own roots or the global config.
                return match self.resolver.for_roots(&paths).await {
                    Ok(Some(reg)) => Ok(reg),
                    Ok(None) => Err(format!(
                        "no {} found in the roots supplied via headers",
                        crate::config::ROOT_CONFIG_NAME
                    )),
                    Err(e) => Err(format!("{e:#}")),
                };
            }
        }

        let supports_roots = ctx
            .peer
            .peer_info()
            .is_some_and(|info| info.capabilities.roots.is_some());
        if supports_roots {
            let mut cache = self.roots_cache.lock().await;
            if cache.is_none() {
                #[allow(deprecated)] // roots is deprecated in the MCP spec but still widely used
                match ctx.peer.list_roots().await {
                    Ok(result) => {
                        let uris: Vec<String> = result.roots.into_iter().map(|r| r.uri).collect();
                        *cache = Some(crate::registry::parse_root_paths(
                            uris.iter().map(String::as_str),
                        ));
                    }
                    Err(e) => tracing::warn!("roots/list failed: {e}"),
                }
            }
            if let Some(paths) = cache.as_deref() {
                match self.resolver.for_roots(paths).await {
                    Ok(Some(reg)) => return Ok(reg),
                    Ok(None) => {}
                    Err(e) => return Err(format!("{e:#}")),
                }
            }
        }

        self.resolver.global().ok_or_else(|| {
            format!(
                "no sources configured; add a {} to your workspace root or start the server with --config",
                crate::config::ROOT_CONFIG_NAME
            )
        })
    }

    async fn source(
        &self,
        ctx: &RequestContext<RoleServer>,
        name: &str,
    ) -> Result<(Arc<crate::source::Source>, Duration), CallToolResult> {
        let reg = self.registry(ctx).await.map_err(err)?;
        let src = reg.get(name).map(Arc::clone).map_err(err)?;
        Ok((src, reg.query_timeout))
    }

    /// Run a source future under the query timeout, rendering any error as a
    /// tool result.
    async fn run<T: serde::Serialize>(
        &self,
        timeout: Duration,
        fut: impl Future<Output = anyhow::Result<T>>,
    ) -> CallToolResult {
        match tokio::time::timeout(timeout, fut).await {
            Err(_) => err(format!("timed out after {}s", timeout.as_secs())),
            Ok(Err(e)) => err(format!("{e:#}")),
            Ok(Ok(v)) => ok_json(&v),
        }
    }
}

/// Header names a trusted proxy can use to inject workspace roots.
const ROOTS_HEADERS: [&str; 4] = ["x-mcp-roots", "x-mcp-root", "mcp-roots", "mcp-root"];

/// Resolve `$args.source` (plus the query timeout) or return the error result.
macro_rules! src {
    ($self:expr, $ctx:expr, $name:expr) => {
        match $self.source(&$ctx, &$name).await {
            Ok(v) => v,
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
    async fn list_sources(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        match self.registry(&ctx).await {
            Ok(reg) => ok_json(&serde_json::json!({ "sources": reg.list() })),
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Check connectivity to a source. Returns latency in milliseconds; useful to diagnose credentials, tunnels and network before running queries.",
        annotations(read_only_hint = true)
    )]
    async fn ping(
        &self,
        Parameters(args): Parameters<SourceArg>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let start = std::time::Instant::now();
        self.run(timeout, async move {
            match src.as_ref() {
                crate::source::Source::Sql(s) => {
                    s.query("SELECT 1", 1).await?;
                }
                crate::source::Source::Mongo(m) => m.ping().await?,
                crate::source::Source::Redis(r) => r.ping().await?,
            }
            Ok(serde_json::json!({
                "ok": true,
                "latency_ms": start.elapsed().as_millis() as u64,
            }))
        })
        .await
    }

    #[tool(
        description = "List databases/schemas available on a source (any engine).",
        annotations(read_only_hint = true)
    )]
    async fn list_databases(
        &self,
        Parameters(args): Parameters<SourceArg>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        match src.as_ref() {
            crate::source::Source::Sql(sql) => {
                self.run(
                    timeout,
                    sql.query(sql.list_databases_sql(), DEFAULT_ROW_LIMIT),
                )
                .await
            }
            crate::source::Source::Mongo(m) => {
                self.run(timeout, async move {
                    let names = m.list_databases().await?;
                    Ok(serde_json::json!({"values": names}))
                })
                .await
            }
            crate::source::Source::Redis(r) => {
                self.run(timeout, async move {
                    let info = r.command(&["INFO".into(), "keyspace".into()]).await?;
                    Ok(serde_json::json!({"keyspace": info}))
                })
                .await
            }
        }
    }

    #[tool(
        description = "List tables on a SQL source.",
        annotations(read_only_hint = true)
    )]
    async fn list_tables(
        &self,
        Parameters(args): Parameters<ListTablesArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        let stmt = sql.list_tables_sql(args.database.as_deref());
        self.run(timeout, sql.query(&stmt, DEFAULT_ROW_LIMIT)).await
    }

    #[tool(
        description = "Describe the columns of a table on a SQL source.",
        annotations(read_only_hint = true)
    )]
    async fn describe_table(
        &self,
        Parameters(args): Parameters<DescribeTableArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        let stmt = sql.describe_table_sql(&args.table, args.database.as_deref());
        self.run(timeout, sql.query(&stmt, DEFAULT_ROW_LIMIT)).await
    }

    #[tool(
        description = "Run a single read-only SQL statement (SELECT/SHOW/DESCRIBE/EXPLAIN) on a SQL source. Results are capped at `limit` rows with a truncated flag; paginate large results with LIMIT/OFFSET in the SQL.",
        annotations(read_only_hint = true)
    )]
    async fn read_query(
        &self,
        Parameters(args): Parameters<ReadQueryArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        if let Err(e) = crate::sqlguard::ensure_read_only(sql.engine(), &args.sql) {
            return err(e);
        }
        let limit = args.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        self.run(timeout, sql.query(&args.sql, limit)).await
    }

    #[tool(
        description = "Run a write/DDL SQL statement (INSERT/UPDATE/DELETE/CREATE/ALTER/...) on a writable SQL source.",
        annotations(destructive_hint = true)
    )]
    async fn write_query(
        &self,
        Parameters(args): Parameters<SqlArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        if sql.readonly() {
            return err(format!("source {:?} is read-only", args.source));
        }
        self.run(timeout, sql.exec(&args.sql)).await
    }

    #[tool(
        description = "Show the query plan for a SQL statement without executing it.",
        annotations(read_only_hint = true)
    )]
    async fn explain_query(
        &self,
        Parameters(args): Parameters<SqlArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        self.run(timeout, sql.explain(&args.sql, DEFAULT_ROW_LIMIT))
            .await
    }

    // ── Document tools (mongodb) ────────────────────────────────────────

    #[tool(
        description = "Query documents in a MongoDB collection. filter/projection/sort are Extended JSON objects.",
        annotations(read_only_hint = true)
    )]
    async fn find(
        &self,
        Parameters(args): Parameters<FindArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let filter = unwrap_or_return!(opt_doc(args.filter));
        let projection = unwrap_or_return!(opt_doc_opt(args.projection));
        let sort = unwrap_or_return!(opt_doc_opt(args.sort));
        let limit = args.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        if args.explain.unwrap_or(false) {
            let mut cmd =
                bson::doc! {"find": &args.collection, "filter": filter, "limit": limit as i64};
            if let Some(p) = projection {
                cmd.insert("projection", p);
            }
            if let Some(so) = sort {
                cmd.insert("sort", so);
            }
            if let Some(sk) = args.skip {
                cmd.insert("skip", sk as i64);
            }
            return self
                .run(timeout, m.explain(args.database.as_deref(), cmd))
                .await;
        }
        self.run(
            timeout,
            m.find(
                args.database.as_deref(),
                &args.collection,
                filter,
                projection,
                sort,
                limit,
                args.skip,
            ),
        )
        .await
    }

    #[tool(
        description = "Run an aggregation pipeline on a MongoDB collection. pipeline is an Extended JSON array of stages; $out/$merge require a writable source.",
        annotations(read_only_hint = true)
    )]
    async fn aggregate(
        &self,
        Parameters(args): Parameters<AggregateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let pipeline = unwrap_or_return!(doc_array(args.pipeline));
        if crate::source::mongo::MongoSource::pipeline_writes(&pipeline) && m.readonly() {
            return err(format!(
                "pipeline contains $out/$merge but source {:?} is read-only",
                args.source
            ));
        }
        let limit = args.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        if args.explain.unwrap_or(false) {
            let cmd = bson::doc! {
                "aggregate": &args.collection,
                "pipeline": pipeline,
                "cursor": {},
            };
            return self
                .run(timeout, m.explain(args.database.as_deref(), cmd))
                .await;
        }
        self.run(
            timeout,
            m.aggregate(args.database.as_deref(), &args.collection, pipeline, limit),
        )
        .await
    }

    #[tool(
        description = "Count documents in a MongoDB collection matching a filter.",
        annotations(read_only_hint = true)
    )]
    async fn count(
        &self,
        Parameters(args): Parameters<FilterArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let filter = unwrap_or_return!(opt_doc(args.filter));
        self.run(timeout, async move {
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
    async fn distinct(
        &self,
        Parameters(args): Parameters<DistinctArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        let filter = unwrap_or_return!(opt_doc(args.filter));
        self.run(timeout, async move {
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
    async fn list_collections(
        &self,
        Parameters(args): Parameters<DatabaseArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        self.run(timeout, async move {
            let names = m.list_collections(args.database.as_deref()).await?;
            Ok(serde_json::json!({"collections": names}))
        })
        .await
    }

    #[tool(
        description = "List indexes on a MongoDB collection.",
        annotations(read_only_hint = true)
    )]
    async fn list_indexes(
        &self,
        Parameters(args): Parameters<CollectionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(as_mongo(&src, &args.source));
        self.run(timeout, async move {
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
    async fn insert(
        &self,
        Parameters(args): Parameters<InsertArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let documents = unwrap_or_return!(doc_array(args.documents));
        if documents.is_empty() {
            return err("documents is empty");
        }
        self.run(timeout, async move {
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
    async fn update(
        &self,
        Parameters(args): Parameters<UpdateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let filter = unwrap_or_return!(req_doc(args.filter, "filter"));
        let update = unwrap_or_return!(req_doc(args.update, "update"));
        self.run(
            timeout,
            m.update(
                args.database.as_deref(),
                &args.collection,
                filter,
                update,
                args.many.unwrap_or(false),
                args.upsert.unwrap_or(false),
            ),
        )
        .await
    }

    #[tool(
        description = "Delete documents from a MongoDB collection. Deletes one document unless many=true. filter must be non-empty.",
        annotations(destructive_hint = true)
    )]
    async fn delete(
        &self,
        Parameters(args): Parameters<DeleteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let filter = unwrap_or_return!(req_doc(args.filter, "filter"));
        if filter.is_empty() {
            return err(
                "refusing to delete with an empty filter; pass an explicit filter \
                 (or drop_collection to remove everything)",
            );
        }
        self.run(
            timeout,
            m.delete(
                args.database.as_deref(),
                &args.collection,
                filter,
                args.many.unwrap_or(false),
            ),
        )
        .await
    }

    #[tool(description = "Create an index on a MongoDB collection.")]
    async fn create_index(
        &self,
        Parameters(args): Parameters<CreateIndexArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        let keys = unwrap_or_return!(req_doc(args.keys, "keys"));
        self.run(timeout, async move {
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
    async fn drop_index(
        &self,
        Parameters(args): Parameters<DropIndexArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        self.run(timeout, async move {
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
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        self.run(timeout, async move {
            m.create_collection(args.database.as_deref(), &args.collection)
                .await?;
            Ok(serde_json::json!({"created": args.collection}))
        })
        .await
    }

    #[tool(
        description = "Run a Redis command, e.g. [\"HGETALL\", \"user:1\"]. On read-only sources only read commands (GET/SCAN/HGETALL/...) are allowed.",
        annotations(destructive_hint = true)
    )]
    async fn redis_command(
        &self,
        Parameters(args): Parameters<RedisCommandArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let r = match src.as_redis(&args.source) {
            Ok(r) => r,
            Err(e) => return err(e),
        };
        let Some(first) = args.command.first() else {
            return err("command is empty");
        };
        if r.readonly() && !crate::source::redis::is_read_command(first) {
            return err(format!(
                "{first} is not a read command and source {:?} is read-only",
                args.source
            ));
        }
        self.run(timeout, async move {
            let value = r.command(&args.command).await?;
            Ok(serde_json::json!({"result": value}))
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
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let m = unwrap_or_return!(writable_mongo(&src, &args.source));
        self.run(timeout, async move {
            m.drop_collection(args.database.as_deref(), &args.collection)
                .await?;
            Ok(serde_json::json!({"dropped": args.collection}))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DsServer {
    async fn on_roots_list_changed(&self, _context: NotificationContext<RoleServer>) {
        *self.roots_cache.lock().await = None;
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let reg = self
            .registry(&ctx)
            .await
            .map_err(|e| ErrorData::invalid_request(e, None))?;
        let resources = reg
            .list()
            .into_iter()
            .map(|info| {
                Resource::new(
                    format!("ds://{}/schema", info.name),
                    format!("{}-schema", info.name),
                )
                .with_description(format!(
                    "Schema overview of source {:?} ({}): tables and columns",
                    info.name, info.engine
                ))
                .with_mime_type("application/json")
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let name = request
            .uri
            .strip_prefix("ds://")
            .and_then(|rest| rest.strip_suffix("/schema"))
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "unknown resource {:?}; expected ds://<source>/schema",
                        request.uri
                    ),
                    None,
                )
            })?;
        let reg = self
            .registry(&ctx)
            .await
            .map_err(|e| ErrorData::invalid_request(e, None))?;
        let src = reg
            .get(name)
            .map(Arc::clone)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let schema = tokio::time::timeout(reg.query_timeout, src.schema())
            .await
            .map_err(|_| ErrorData::internal_error("schema read timed out", None))?
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;
        let text = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| schema.to_string());
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text,
            request.uri,
        )]))
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(
            "DataStore MCP exposes named database sources (SQL and document engines). \
                 Start with list_sources to see what is available, then use the SQL tools \
                 (read_query, list_tables, ...) against SQL sources.",
        )
    }
}
