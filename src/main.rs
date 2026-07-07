mod config;
mod http;
mod registry;
mod source;
mod sqlguard;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(
    name = "ds-mcp",
    version,
    about = "DataStore MCP — multi-engine data-source MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server.
    Serve {
        /// Config file path. Default: ~/.config/ds-mcp/config.json.
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(short, long, value_enum, default_value_t = Transport::Stdio)]
        transport: Transport,
        /// Override http.addr from the config.
        #[arg(long)]
        http_addr: Option<String>,
        /// Force every source read-only regardless of config.
        #[arg(long)]
        read_only: bool,
    },
    /// Write the config JSON Schema.
    GenSchema {
        #[arg(short, long, default_value = "config.schema.json")]
        output: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs to stderr only; stdout belongs to the JSON-RPC stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::GenSchema { output } => gen_schema(&output),
        Command::Serve {
            config,
            transport,
            http_addr,
            read_only,
        } => serve(config, transport, http_addr, read_only).await,
    }
}

fn gen_schema(output: &std::path::Path) -> Result<()> {
    let schema = schemars::schema_for!(config::Config);
    let mut json = serde_json::to_string_pretty(&schema)?;
    json.push('\n');
    std::fs::write(output, json).with_context(|| format!("write {}", output.display()))?;
    eprintln!("wrote {}", output.display());
    Ok(())
}

async fn serve(
    config_path: Option<PathBuf>,
    transport: Transport,
    http_addr: Option<String>,
    read_only: bool,
) -> Result<()> {
    let cfg = match &config_path {
        // An explicit --config that fails to load is fatal.
        Some(p) => Some(config::load(p)?),
        None => match config::default_path_global().filter(|p| p.exists()) {
            Some(p) => Some(config::load(&p)?),
            // Roots-only mode: clients supply sources via a .ds-mcp.json at
            // their workspace root.
            None => {
                tracing::warn!(
                    "no global config found; running in roots-only mode \
                     (clients need a {} in a workspace root)",
                    config::ROOT_CONFIG_NAME
                );
                None
            }
        },
    };
    let http_cfg = cfg.as_ref().map(|c| c.http.clone()).unwrap_or_default();
    let global = cfg
        .map(|c| registry::Registry::new(c, read_only).map(Arc::new))
        .transpose()?;
    let resolver = Arc::new(registry::Resolver::new(global, read_only));
    let server = tools::DsServer::new(Arc::clone(&resolver));

    match transport {
        Transport::Stdio => {
            tracing::info!("serving over stdio");
            let running = server.serve(rmcp::transport::stdio()).await?;
            running.waiting().await?;
        }
        Transport::Http => {
            http::serve(server, &http_cfg, http_addr).await?;
        }
    }
    resolver.close().await;
    Ok(())
}
