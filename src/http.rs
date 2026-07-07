//! Streamable HTTP transport: rmcp service mounted at the configured path,
//! plus /healthz. No auth layer — bind loopback (default) or put a proxy in
//! front.

use anyhow::{Context, Result};
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use tokio_util::sync::CancellationToken;

use crate::config::HttpConfig;
use crate::tools::DsServer;

pub async fn serve(
    server: DsServer,
    cfg: &HttpConfig,
    addr_override: Option<String>,
) -> Result<()> {
    let ct = CancellationToken::new();

    let mut config = StreamableHttpServerConfig::default()
        .with_stateful_mode(!cfg.stateless)
        .with_json_response(cfg.json_response)
        .with_cancellation_token(ct.child_token());
    // Default: rmcp only accepts localhost Host headers (DNS-rebind guard).
    // `allowed_origins: ["*"]` disables the checks (trusted proxy in front);
    // otherwise listed origins are allowed and their hosts added to the
    // Host allowlist.
    if cfg.allowed_origins.iter().any(|o| o == "*") {
        config = config.disable_allowed_hosts();
    } else if !cfg.allowed_origins.is_empty() {
        let extra_hosts = cfg
            .allowed_origins
            .iter()
            .filter_map(|o| o.split_once("://").map(|(_, host)| host.to_string()));
        let hosts: Vec<String> = ["localhost".into(), "127.0.0.1".into(), "::1".into()]
            .into_iter()
            .chain(extra_hosts)
            .collect();
        config = config
            .with_allowed_origins(cfg.allowed_origins.clone())
            .with_allowed_hosts(hosts);
    }

    let service: StreamableHttpService<DsServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Default::default(), config);

    let router = axum::Router::new()
        .nest_service(cfg.path.as_str(), service)
        .route("/healthz", axum::routing::get(|| async { "ok" }));

    let addr = addr_override.unwrap_or_else(|| cfg.addr.clone());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("serving http on {addr}{}", cfg.path);

    let shutdown_ct = ct.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            shutdown_ct.cancel();
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}
