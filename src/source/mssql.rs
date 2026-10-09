//! SQL Server connection pool: `deadpool`'s generic managed pool over
//! tiberius. Sizing, timeouts and recycling come from deadpool; this module
//! only says how to open a connection and how to health-check one.
//!
//! Replaces `deadpool-tiberius`, whose last release pins tiberius 0.12 (and
//! with it rustls 0.21 / rustls-webpki 0.101, which carry open advisories).

use std::time::Duration;

use deadpool::managed::{self, Metrics, RecycleResult};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub type Client = tiberius::Client<Compat<TcpStream>>;
pub type Pool = managed::Pool<Manager>;

pub struct Manager {
    config: tiberius::Config,
}

impl Manager {
    pub fn new(mut config: tiberius::Config) -> Self {
        // tiberius 0.13 bounds the login handshake (15s) and every command
        // round-trip (30s) by default; 0.12 waited indefinitely. The pool's
        // create timeout already bounds connecting and the registry's
        // per-query timeout bounds queries, both user-configurable — a fixed
        // 30s here would silently cap a longer configured query timeout.
        config.handshake_timeout(None);
        config.command_timeout(None);
        Self { config }
    }

    pub fn into_pool(
        self,
        max_size: usize,
        create_timeout: Duration,
    ) -> Result<Pool, managed::BuildError> {
        Pool::builder(self)
            .max_size(max_size)
            .create_timeout(Some(create_timeout))
            .runtime(deadpool::Runtime::Tokio1)
            .build()
    }

    async fn dial(config: tiberius::Config) -> tiberius::Result<Client> {
        let tcp = TcpStream::connect(config.get_addr()).await?;
        tcp.set_nodelay(true)?;
        // Boxed: tiberius' login future is ~18KB.
        Box::pin(Client::connect(config, tcp.compat_write())).await
    }
}

impl managed::Manager for Manager {
    type Type = Client;
    type Error = tiberius::error::Error;

    async fn create(&self) -> Result<Client, Self::Error> {
        match Self::dial(self.config.clone()).await {
            // Azure SQL gateways answer the login with a redirect to the node
            // that actually serves the database.
            Err(tiberius::error::Error::Routing { host, port }) => {
                let mut config = self.config.clone();
                config.host(host);
                config.port(port);
                Self::dial(config).await
            }
            other => other,
        }
    }

    async fn recycle(&self, conn: &mut Client, _: &Metrics) -> RecycleResult<Self::Error> {
        // Drain the result so the connection is idle again before reuse.
        conn.simple_query("SELECT 1").await?.into_results().await?;
        Ok(())
    }
}

/// Parse an ADO.NET connection string, keeping tiberius 0.12's encryption
/// default. 0.13 changed an omitted `Encrypt` from `Off` (only the login is
/// encrypted) to `Required` (full TLS with certificate validation), which
/// would break existing DSNs that point at servers with self-signed
/// certificates. Opting into full TLS stays explicit: `Encrypt=true`.
pub fn config_from_ado(dsn: &str) -> anyhow::Result<tiberius::Config> {
    let mut config = tiberius::Config::from_ado_string(dsn)?;
    if !sets_encrypt(dsn)? {
        config.encryption(tiberius::EncryptionLevel::Off);
    }
    Ok(config)
}

/// Whether the connection string sets `Encrypt` itself (keys are
/// case-insensitive).
fn sets_encrypt(dsn: &str) -> anyhow::Result<bool> {
    let keys: connection_string::AdoNetString = dsn.parse()?;
    Ok(keys.contains_key("encrypt"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool::managed::{Manager as _, PoolError};

    #[test]
    fn detects_explicit_encrypt() {
        assert!(!sets_encrypt("Server=tcp:db,1433;User Id=u;Password=p").unwrap());
        assert!(sets_encrypt("Server=tcp:db,1433;Encrypt=true").unwrap());
        assert!(sets_encrypt("server=tcp:db,1433;ENCRYPT=false").unwrap());
        assert!(sets_encrypt("Server=db;encrypt=DANGER_PLAINTEXT").unwrap());
        // A value merely mentioning the word is not the key.
        assert!(!sets_encrypt("Server=db;Application Name=encrypt").unwrap());
    }

    #[test]
    fn manager_lifts_tiberius_default_timeouts() {
        let m = Manager::new(config_from_ado("Server=tcp:db,1433").unwrap());
        assert_eq!(m.config.get_handshake_timeout(), None);
        assert_eq!(m.config.get_command_timeout(), None);
    }

    /// Nothing listens on the port: `create` must surface the dial error and
    /// the pool must report it as a backend error rather than hang.
    #[tokio::test]
    async fn create_reports_unreachable_server() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut config = tiberius::Config::new();
        config.host("127.0.0.1");
        config.port(port);
        let manager = Manager::new(config);
        assert!(Box::pin(manager.create()).await.is_err());

        let pool = manager.into_pool(8, Duration::from_secs(5)).unwrap();
        assert_eq!(pool.status().max_size, 8);
        match Box::pin(pool.get()).await {
            Err(PoolError::Backend(_)) => {}
            Err(e) => panic!("expected a backend error, got {e}"),
            Ok(_) => panic!("connected to a closed port"),
        }
    }

    /// Full create -> query -> recycle round-trip against a real server.
    /// `DSMCP_MSSQL_DSN` is an ADO string, e.g.
    /// `Server=tcp:127.0.0.1,11433;User Id=sa;Password=...;TrustServerCertificate=true`.
    #[tokio::test]
    #[ignore = "requires a SQL Server (DSMCP_MSSQL_DSN)"]
    async fn pool_recycles_live_connection() {
        let dsn = std::env::var("DSMCP_MSSQL_DSN").expect("DSMCP_MSSQL_DSN");
        let pool = Manager::new(config_from_ado(&dsn).unwrap())
            .into_pool(1, Duration::from_secs(10))
            .unwrap();
        for _ in 0..3 {
            // max_size 1: the second and third checkouts reuse the same
            // connection, so they go through `recycle`.
            let mut conn = Box::pin(pool.get()).await.unwrap();
            let row = conn
                .simple_query("SELECT 42")
                .await
                .unwrap()
                .into_row()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.get::<i32, _>(0), Some(42));
        }
        assert_eq!(pool.status().size, 1);
    }
}
