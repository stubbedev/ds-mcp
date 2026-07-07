//! SSH tunnels: one session per source, a local listener forwarding each
//! connection through a direct-tcpip channel. Engines just dial the local
//! forward address — no per-driver dialer plumbing.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use russh::client;
use russh::keys::PrivateKeyWithHashAlg;
use tokio::net::TcpListener;

use crate::config::SshConfig;

pub struct SshTunnel {
    pub local_addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct HostKeyCheck {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
    insecure: bool,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        if self.insecure {
            return Ok(true);
        }
        let checked = match &self.known_hosts {
            Some(path) => russh::keys::check_known_hosts_path(&self.host, self.port, key, path),
            None => russh::keys::check_known_hosts(&self.host, self.port, key),
        };
        match checked {
            Ok(true) => Ok(true),
            Ok(false) => {
                tracing::error!(
                    "ssh host {}:{} is not in known_hosts; add it with \
                     `ssh-keyscan -p {} {} >> ~/.ssh/known_hosts` after verifying the key",
                    self.host,
                    self.port,
                    self.port,
                    self.host
                );
                Ok(false)
            }
            Err(e) => {
                // Includes KeyChanged — never connect through a changed key.
                tracing::error!(
                    "ssh host key verification for {}:{} failed: {e}",
                    self.host,
                    self.port
                );
                Ok(false)
            }
        }
    }
}

/// Open a tunnel: local listener on 127.0.0.1:0 forwarding every connection
/// to `target_host:target_port` as seen from the SSH host.
pub async fn open(cfg: &SshConfig, target_host: &str, target_port: u16) -> Result<SshTunnel> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    });
    let handler = HostKeyCheck {
        host: cfg.host.clone(),
        port: cfg.port,
        known_hosts: cfg.known_hosts_file.clone().map(PathBuf::from),
        insecure: cfg.insecure_ignore_host_key,
    };
    let mut session = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
        .await
        .with_context(|| format!("ssh connect to {}:{}", cfg.host, cfg.port))?;
    authenticate(&mut session, cfg).await?;

    let session = Arc::new(session);
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let local_addr = listener.local_addr()?;
    let target_host = target_host.to_string();
    tracing::info!(
        "ssh tunnel {} -> {}:{} via {}@{}:{}",
        local_addr,
        target_host,
        target_port,
        cfg.user,
        cfg.host,
        cfg.port
    );

    let task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, peer)) = listener.accept().await else {
                break;
            };
            let handle = Arc::clone(&session);
            let target_host = target_host.clone();
            tokio::spawn(async move {
                match handle
                    .channel_open_direct_tcpip(
                        target_host,
                        u32::from(target_port),
                        peer.ip().to_string(),
                        u32::from(peer.port()),
                    )
                    .await
                {
                    Ok(channel) => {
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
                    }
                    Err(e) => tracing::warn!("ssh forward failed: {e}"),
                }
            });
        }
    });

    Ok(SshTunnel { local_addr, task })
}

/// Try identity file, then agent, then password — first success wins.
async fn authenticate(session: &mut client::Handle<HostKeyCheck>, cfg: &SshConfig) -> Result<()> {
    if let Some(file) = &cfg.identity_file {
        let key = russh::keys::load_secret_key(file, cfg.passphrase.as_deref())
            .with_context(|| format!("load ssh key {file}"))?;
        let hash = session.best_supported_rsa_hash().await?.flatten();
        if session
            .authenticate_publickey(
                cfg.user.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await?
            .success()
        {
            return Ok(());
        }
    }
    if cfg.use_agent {
        let mut agent = russh::keys::agent::client::AgentClient::connect_env()
            .await
            .context("connect to ssh-agent (SSH_AUTH_SOCK)")?;
        for identity in agent.request_identities().await? {
            let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity else {
                continue;
            };
            let hash = session.best_supported_rsa_hash().await?.flatten();
            let ok = session
                .authenticate_publickey_with(cfg.user.clone(), key, hash, &mut agent)
                .await
                .map(|r| r.success())
                .unwrap_or(false);
            if ok {
                return Ok(());
            }
        }
    }
    if let Some(password) = &cfg.password
        && session
            .authenticate_password(cfg.user.clone(), password.clone())
            .await?
            .success()
    {
        return Ok(());
    }
    bail!(
        "ssh authentication as {}@{} failed (tried: {})",
        cfg.user,
        cfg.host,
        [
            cfg.identity_file.as_ref().map(|_| "identity_file"),
            cfg.use_agent.then_some("agent"),
            cfg.password.as_ref().map(|_| "password"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs docker; run with `cargo test ssh_tunnel -- --ignored`.
    /// Spins up an sshd container and tunnels back to the sshd itself, then
    /// reads the SSH banner through the forward — proves connect, auth and
    /// direct-tcpip forwarding without needing a database.
    #[tokio::test]
    #[ignore = "requires docker"]
    async fn ssh_tunnel_forwards() {
        use tokio::io::AsyncReadExt;

        let name = "dsmcp-ssh-test";
        let docker = |args: &[&str]| {
            std::process::Command::new("docker")
                .args(args)
                .output()
                .expect("docker")
        };
        docker(&["rm", "-f", name]);
        let run = docker(&[
            "run",
            "-d",
            "--name",
            name,
            "-p",
            "127.0.0.1:2223:2222",
            "-e",
            "PASSWORD_ACCESS=true",
            "-e",
            "USER_PASSWORD=testpw",
            "-e",
            "USER_NAME=test",
            "linuxserver/openssh-server",
        ]);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );

        // Wait for sshd to accept connections.
        let mut ready = false;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if tokio::net::TcpStream::connect("127.0.0.1:2223")
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
        }
        assert!(ready, "sshd container did not come up");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // The image ships AllowTcpForwarding no; flip it and restart sshd.
        docker(&[
            "exec",
            name,
            "sed",
            "-i",
            "s/AllowTcpForwarding no/AllowTcpForwarding yes/",
            "/config/sshd/sshd_config",
        ]);
        docker(&["restart", name]);
        let mut ready = false;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if tokio::net::TcpStream::connect("127.0.0.1:2223")
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
        }
        assert!(ready, "sshd did not come back after restart");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let cfg = SshConfig {
            host: "127.0.0.1".into(),
            port: 2223,
            user: "test".into(),
            password: Some("testpw".into()),
            identity_file: None,
            passphrase: None,
            use_agent: false,
            known_hosts_file: None,
            insecure_ignore_host_key: true,
        };
        // Tunnel to the sshd's own port as seen from inside the container.
        let tunnel = open(&cfg, "127.0.0.1", 2222).await.expect("open tunnel");

        let mut stream = tokio::net::TcpStream::connect(tunnel.local_addr)
            .await
            .expect("connect local forward");
        let mut banner = [0u8; 7];
        stream.read_exact(&mut banner).await.expect("read banner");
        assert_eq!(&banner, b"SSH-2.0", "expected ssh banner through tunnel");

        docker(&["rm", "-f", name]);
    }
}
