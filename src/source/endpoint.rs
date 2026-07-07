//! Resolves how a source is reached: directly, through an ssh tunnel, or via
//! a docker container. Engines call this with the target host/port they
//! parsed from the config (or dsn) and dial whatever comes back.

use anyhow::{Context, Result, anyhow, bail};

use super::ssh::SshTunnel;
use crate::config::SourceConfig;

pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// Keeps the ssh forward alive for the life of the connection.
    pub tunnel: Option<SshTunnel>,
}

pub async fn resolve(cfg: &SourceConfig, target_host: &str, target_port: u16) -> Result<Endpoint> {
    if let Some(docker) = &cfg.docker {
        let (host, port) =
            docker_endpoint(&docker.container, docker.port.unwrap_or(target_port)).await?;
        return Ok(Endpoint {
            host,
            port,
            tunnel: None,
        });
    }
    if let Some(ssh) = &cfg.ssh {
        let tunnel = super::ssh::open(ssh, target_host, target_port).await?;
        return Ok(Endpoint {
            host: tunnel.local_addr.ip().to_string(),
            port: tunnel.local_addr.port(),
            tunnel: Some(tunnel),
        });
    }
    Ok(Endpoint {
        host: target_host.to_string(),
        port: target_port,
        tunnel: None,
    })
}

/// Where to reach `port` of a container: its published host port if there is
/// one, else the container's network IP (reachable on linux).
async fn docker_endpoint(container: &str, port: u16) -> Result<(String, u16)> {
    let out = tokio::process::Command::new("docker")
        .args(["port", container, &format!("{port}/tcp")])
        .output()
        .await
        .context("run docker (is it installed and on PATH?)")?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some((host, p)) = line.trim().rsplit_once(':')
                && let Ok(p) = p.parse::<u16>()
            {
                let host = match host {
                    "0.0.0.0" | "[::]" | "::" => "127.0.0.1",
                    other => other,
                };
                return Ok((host.to_string(), p));
            }
        }
    }

    let out = tokio::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
            container,
        ])
        .output()
        .await?;
    if !out.status.success() {
        bail!(
            "docker inspect {container}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let ip = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| {
            anyhow!(
                "container {container:?}: port {port} is not published and the container \
                 has no network IP"
            )
        })?
        .to_string();
    Ok((ip, port))
}
