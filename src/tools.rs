//! The MCP tool surface. All failures are tool results (is_error), never
//! protocol errors, so the model always sees the message.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::registry::Registry;
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
        description = "List databases/schemas available on a source.",
        annotations(read_only_hint = true)
    )]
    async fn list_databases(&self, Parameters(args): Parameters<SourceArg>) -> CallToolResult {
        let src = sql_src!(self, args.source);
        let sql = unwrap_or_return!(as_sql(&src, &args.source));
        self.run(sql.query(sql.list_databases_sql(), DEFAULT_ROW_LIMIT))
            .await
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
        let stmt = sql.explain_sql(&args.sql);
        self.run(sql.query(&stmt, DEFAULT_ROW_LIMIT)).await
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
