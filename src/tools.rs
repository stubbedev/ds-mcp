//! The MCP tool surface: five engine-agnostic tools. `query`/`execute` take
//! an engine-native payload (SQL string, Mongo command document, Redis
//! command array) and dispatch internally. All failures are tool results
//! (is_error), never protocol errors, so the model always sees the message.

// Helpers use Result<T, CallToolResult> so `?`-style early returns produce
// the error tool result; the Err size is irrelevant here.
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
use serde_json::Value;

use crate::registry::{Registry, Resolver};
use crate::source::Source;

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
pub struct SchemaArgs {
    pub source: String,
    /// Database/schema to inspect; defaults to the source's configured one.
    pub database: Option<String>,
    /// Table/collection (SQL/mongo) or key (redis) to describe. Omit to list
    /// what the source contains.
    pub table: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct QueryArgs {
    pub source: String,
    /// The read to run, in the source's native form:
    /// - SQL engines: a single SELECT/SHOW/DESCRIBE/EXPLAIN string.
    /// - MongoDB: a command document, e.g. {"find": "widgets", "filter": {"qty": {"$gte": 1}}}
    ///   or {"aggregate": "widgets", "pipeline": [...]}. Extended JSON is honored.
    /// - Redis: a command array, e.g. ["GET", "widget:1"].
    pub query: Value,
    /// Database for MongoDB commands; defaults to the source's default_database.
    pub database: Option<String>,
    /// Max rows/documents to return (SQL + Mongo find/aggregate). Default 1000.
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExecuteArgs {
    pub source: String,
    /// The write to run, in the source's native form:
    /// - SQL engines: any statement (INSERT/UPDATE/DELETE/CREATE/ALTER/...).
    /// - MongoDB: a command document, e.g. {"insert": "widgets", "documents": [...]},
    ///   {"update": ...}, {"delete": ...}, {"createIndexes": ...}, {"drop": ...}.
    /// - Redis: a command array, e.g. ["SET", "widget:1", "sprocket"].
    pub query: Value,
    /// Database for MongoDB commands; defaults to the source's default_database.
    pub database: Option<String>,
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
    ) -> Result<(Arc<Source>, Duration), CallToolResult> {
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

/// A SQL payload must be a string.
fn as_sql_string(v: &Value, engine: &str) -> Result<String, CallToolResult> {
    v.as_str().map(str::to_owned).ok_or_else(|| {
        err(format!(
            "{engine} is a SQL source; `query` must be a SQL statement string"
        ))
    })
}

/// A Redis payload must be an array of string/number/bool args.
fn as_redis_parts(v: Value) -> Result<Vec<String>, CallToolResult> {
    let Value::Array(items) = v else {
        return Err(err(
            "redis sources take a command array, e.g. [\"GET\", \"key\"]",
        ));
    };
    items
        .into_iter()
        .map(|e| match e {
            Value::String(s) => Ok(s),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err(err("redis command arguments must be strings or numbers")),
        })
        .collect()
}

/// A Mongo payload must be a command document.
fn as_mongo_command(v: Value) -> Result<bson::Document, CallToolResult> {
    crate::source::mongo::to_doc(v).map_err(|e| {
        err(format!(
            "mongodb sources take a command document, e.g. {{\"find\": \"coll\", \"filter\": {{}}}} ({e:#})"
        ))
    })
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
                Source::Sql(s) => {
                    s.query("SELECT 1", 1).await?;
                }
                Source::Mongo(m) => m.ping().await?,
                Source::Redis(r) => r.ping().await?,
            }
            Ok(serde_json::json!({
                "ok": true,
                "latency_ms": start.elapsed().as_millis() as u64,
            }))
        })
        .await
    }

    #[tool(
        description = "Introspect a source. Without `table`: list tables/collections (SQL/mongo) or the keyspace (redis). With `table`: describe its columns (SQL), indexes (mongo), or a key's type and ttl (redis).",
        annotations(read_only_hint = true)
    )]
    async fn schema(
        &self,
        Parameters(args): Parameters<SchemaArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        self.run(
            timeout,
            src.schema(args.database.as_deref(), args.table.as_deref()),
        )
        .await
    }

    #[tool(
        description = "Run a read against a source. `query` is engine-native: a SQL SELECT/SHOW/DESCRIBE/EXPLAIN string; a MongoDB command document like {\"find\": \"c\", \"filter\": {...}} or {\"aggregate\": \"c\", \"pipeline\": [...]}; or a Redis command array like [\"GET\", \"k\"]. Writes are refused here (use execute). Results are capped at `limit` rows/documents with a truncated/has_more flag; paginate with LIMIT/OFFSET (SQL) or skip/limit (mongo).",
        annotations(read_only_hint = true)
    )]
    async fn query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        let limit = args.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        match src.as_ref() {
            Source::Sql(s) => {
                let sql = match as_sql_string(&args.query, s.engine().name()) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                if let Err(e) = crate::sqlguard::ensure_read_only(s.engine(), &sql) {
                    return err(e);
                }
                self.run(timeout, s.query(&sql, limit)).await
            }
            Source::Mongo(m) => {
                let cmd = match as_mongo_command(args.query) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                match crate::source::mongo::command_is_read(&cmd) {
                    Ok(true) => {}
                    Ok(false) => {
                        return err(
                            "that command writes (or aggregates with $out/$merge); use execute",
                        );
                    }
                    Err(e) => return err(format!("{e:#}")),
                }
                self.run(
                    timeout,
                    m.run_command(args.database.as_deref(), cmd, Some(limit)),
                )
                .await
            }
            Source::Redis(r) => {
                let parts = match as_redis_parts(args.query) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                let Some(first) = parts.first() else {
                    return err("command is empty");
                };
                if !crate::source::redis::is_read_command(first) {
                    return err(format!("{first} is not a read command; use execute"));
                }
                self.run(timeout, async move {
                    Ok(serde_json::json!({"result": r.command(&parts).await?}))
                })
                .await
            }
        }
    }

    #[tool(
        description = "Run a write against a writable source. `query` is engine-native: any SQL statement (INSERT/UPDATE/DELETE/CREATE/ALTER/CREATE INDEX/...); a MongoDB command document like {\"insert\": ...}, {\"update\": ...}, {\"delete\": ...}, {\"createIndexes\": ...}, {\"drop\": ...}; or a Redis command array like [\"SET\", \"k\", \"v\"]. Refused on read-only sources. No implicit guards — a DELETE without a filter deletes everything.",
        annotations(destructive_hint = true)
    )]
    async fn execute(
        &self,
        Parameters(args): Parameters<ExecuteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let (src, timeout) = src!(self, ctx, args.source);
        if src.readonly() {
            return err(format!("source {:?} is read-only", args.source));
        }
        match src.as_ref() {
            Source::Sql(s) => {
                let sql = match as_sql_string(&args.query, s.engine().name()) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                self.run(timeout, s.exec(&sql)).await
            }
            Source::Mongo(m) => {
                let cmd = match as_mongo_command(args.query) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                self.run(timeout, m.run_command(args.database.as_deref(), cmd, None))
                    .await
            }
            Source::Redis(r) => {
                let parts = match as_redis_parts(args.query) {
                    Ok(v) => v,
                    Err(e) => return e,
                };
                if parts.is_empty() {
                    return err("command is empty");
                }
                self.run(timeout, async move {
                    Ok(serde_json::json!({"result": r.command(&parts).await?}))
                })
                .await
            }
        }
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
                    "Schema overview of source {:?} ({})",
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
        let schema = tokio::time::timeout(reg.query_timeout, src.schema(None, None))
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
            "DataStore MCP exposes named database sources across SQL, document and \
             key-value engines through one tool set. Call list_sources first, then \
             schema to introspect, and query/execute to read/write. The query payload \
             is engine-native: a SQL string, a MongoDB command document, or a Redis \
             command array.",
        )
    }
}
