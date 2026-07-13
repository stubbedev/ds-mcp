//! Redis sources: one tool surface (`redis_command`) running raw commands.
//! Read-only sources are gated by a read-command allowlist.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::{EngineKind, SourceConfig};

/// A cached connection plus the tunnel (if any) that keeps it reachable.
type Conn = (
    redis::aio::MultiplexedConnection,
    Option<super::ssh::SshTunnel>,
);

pub struct RedisSource {
    name: String,
    cfg: SourceConfig,
    readonly: bool,
    /// One connection per database index, opened on first use and reused.
    conns: Mutex<HashMap<String, Conn>>,
}

/// Commands allowed on read-only sources. Uppercase, no side effects. Only the
/// non-writing variant of each family is here — e.g. `GEOSEARCH` not
/// `GEOSEARCHSTORE`, `SINTER` not `SINTERSTORE`, the `*_RO` script/sort forms
/// not `EVAL`/`SORT`. Container commands with mixed read/write subcommands
/// (`CONFIG`, `MEMORY`) are NOT here — they are gated per-subcommand below.
/// `PFCOUNT` is deliberately absent: Redis flags it a write (it may rewrite the
/// HyperLogLog's cached cardinality and replicates), so it belongs on `execute`.
const READ_COMMANDS: &[&str] = &[
    "GET",
    "MGET",
    "GETRANGE",
    "STRLEN",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "KEYS",
    "SCAN",
    "RANDOMKEY",
    "DBSIZE",
    "HGET",
    "HGETALL",
    "HMGET",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HSCAN",
    "HEXISTS",
    "HSTRLEN",
    "HRANDFIELD",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "LPOS",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SRANDMEMBER",
    "SSCAN",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZSCORE",
    "ZMSCORE",
    "ZRANK",
    "ZREVRANK",
    "ZSCAN",
    "ZRANDMEMBER",
    "ZDIFF",
    "ZINTER",
    "ZUNION",
    "ZINTERCARD",
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XREAD",
    "XINFO",
    "XPENDING",
    "BITCOUNT",
    "BITPOS",
    "GETBIT",
    "BITFIELD_RO",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEOSEARCH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "SORT_RO",
    "EVAL_RO",
    "EVALSHA_RO",
    "FCALL_RO",
    "OBJECT",
    "INFO",
    "PING",
    "ECHO",
    "TIME",
    "COMMAND",
    "DUMP",
    "TOUCH",
    "LCS",
];

/// Read-only subcommands of container commands whose siblings mutate. The gate
/// is otherwise first-token-only, so these must be matched on the second token.
const READ_SUBCOMMANDS: &[(&str, &[&str])] = &[
    // CONFIG SET/REWRITE/RESETSTAT mutate the server; only GET/HELP read.
    ("CONFIG", &["GET", "HELP"]),
    // MEMORY PURGE has an allocator side effect; the rest are introspection.
    (
        "MEMORY",
        &["USAGE", "STATS", "DOCTOR", "MALLOC-STATS", "HELP"],
    ),
];

/// Is this command a read (safe on a read-only source)? First token must be in
/// the allowlist; for container commands (`CONFIG`/`MEMORY`) the second token
/// must be a read subcommand too, since the raw command runs verbatim.
pub fn is_read_command(parts: &[String]) -> bool {
    let Some(name) = parts.first() else {
        return false;
    };
    let name = name.to_ascii_uppercase();
    if let Some((_, subs)) = READ_SUBCOMMANDS.iter().find(|(c, _)| *c == name) {
        return parts
            .get(1)
            .is_some_and(|s| subs.contains(&s.to_ascii_uppercase().as_str()));
    }
    READ_COMMANDS.contains(&name.as_str())
}

impl RedisSource {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> Self {
        let readonly = force_readonly || cfg.readonly;
        Self {
            name: name.to_string(),
            cfg,
            readonly,
            conns: Mutex::new(HashMap::new()),
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

    pub async fn close(&self) {}

    /// The source's configured database (the `/N` in the URL). Defaults to `0`.
    fn configured_db(&self) -> &str {
        self.cfg.database.as_deref().unwrap_or("0")
    }

    /// Build the redis URL for `db`, resolving the endpoint (and opening a
    /// tunnel if configured). Returns the URL and the tunnel to keep alive.
    async fn build_url(&self, db: &str) -> Result<(String, Option<super::ssh::SshTunnel>)> {
        match &self.cfg.dsn {
            Some(dsn) => {
                let mut parsed = url::Url::parse(dsn).context("parse redis dsn")?;
                let host = parsed.host_str().unwrap_or("127.0.0.1").to_string();
                let port = parsed.port().unwrap_or(6379);
                let ep = super::endpoint::resolve(&self.cfg, &host, port).await?;
                let _ = parsed.set_host(Some(&ep.host));
                let _ = parsed.set_port(Some(ep.port));
                parsed.set_path(&format!("/{db}"));
                Ok((parsed.to_string(), ep.tunnel))
            }
            None => {
                let target = self.cfg.host.as_deref().unwrap_or("127.0.0.1");
                let ep = super::endpoint::resolve(&self.cfg, target, self.cfg.port.unwrap_or(6379))
                    .await?;
                let auth = self
                    .cfg
                    .password
                    .as_ref()
                    .map(|p| format!(":{p}@"))
                    .unwrap_or_default();
                Ok((
                    format!("redis://{auth}{}:{}/{db}", ep.host, ep.port),
                    ep.tunnel,
                ))
            }
        }
    }

    async fn open(
        &self,
        db: &str,
    ) -> Result<(
        redis::aio::MultiplexedConnection,
        Option<super::ssh::SshTunnel>,
    )> {
        let (url, tunnel) = self.build_url(db).await?;
        let client = redis::Client::open(url.as_str())?;
        let conn = tokio::time::timeout(
            self.cfg.connect_timeout(),
            client.get_multiplexed_async_connection(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("connect timed out"))??;
        Ok((conn, tunnel))
    }

    /// A connection to `db`, opened on first use and cached for reuse. SELECT is
    /// a write (blocked, and unsafe on a multiplexed connection), so each db
    /// index gets its own connection instead.
    async fn conn(&self, db: &str) -> Result<redis::aio::MultiplexedConnection> {
        if let Some((conn, _)) = self.conns.lock().await.get(db) {
            return Ok(conn.clone());
        }
        // Open outside the lock; a concurrent opener for the same db just loses
        // its connection (dropped here), which is fine.
        let entry = self
            .open(db)
            .await
            .with_context(|| format!("connect to source {:?} db {db}", self.name))?;
        let mut map = self.conns.lock().await;
        Ok(map.entry(db.to_string()).or_insert(entry).0.clone())
    }

    /// Run a raw command. The caller enforces the read-only gate. `db` overrides
    /// the source's configured database for this call; defaults to it.
    pub async fn command(&self, parts: &[String], db: Option<&str>) -> Result<Value> {
        let Some((name, args)) = parts.split_first() else {
            bail!("empty command");
        };
        let mut cmd = redis::cmd(name);
        for arg in args {
            cmd.arg(arg.as_str());
        }
        let mut conn = self.conn(db.unwrap_or(self.configured_db())).await?;
        Ok(redis_to_json(cmd.query_async(&mut conn).await?))
    }

    pub async fn ping(&self) -> Result<()> {
        self.command(&["PING".into()], None).await.map(|_| ())
    }
}

fn redis_to_json(v: redis::Value) -> Value {
    use redis::Value as R;
    match v {
        R::Nil => Value::Null,
        R::Int(n) => Value::from(n),
        R::BulkString(bytes) => bytes_to_json(bytes),
        R::Array(items) | R::Set(items) => {
            Value::Array(items.into_iter().map(redis_to_json).collect())
        }
        R::SimpleString(s) => Value::String(s),
        R::Okay => Value::String("OK".into()),
        R::Map(pairs) => Value::Object(
            pairs
                .into_iter()
                .map(|(k, v)| {
                    let key = match redis_to_json(k) {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (key, redis_to_json(v))
                })
                .collect(),
        ),
        R::Attribute { data, .. } => redis_to_json(*data),
        R::Double(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        R::Boolean(b) => Value::Bool(b),
        R::VerbatimString { text, .. } => Value::String(text),
        R::BigNumber(n) => Value::String(format!("{n:?}")),
        R::Push { .. } => Value::Null,
        R::ServerError(e) => Value::String(format!("error: {e:?}")),
        other => Value::String(format!("{other:?}")),
    }
}

fn bytes_to_json(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(s) => Value::String(s),
        Err(e) => Value::String(format!(
            "0x{}",
            e.into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn read_allowlist() {
        for c in [
            "GET",
            "get",
            "Scan",
            "HGETALL",
            "INFO",
            "SORT_RO",
            "EVAL_RO",
            "GEOHASH",
            "ZDIFF",
            "EXPIRETIME",
        ] {
            assert!(is_read_command(&cmd(&[c])), "{c}");
        }
        for c in [
            "SET",
            "DEL",
            "FLUSHALL",
            "HSET",
            "EXPIRE",
            "EVAL",
            "SUBSCRIBE",
            "SORT",
            "PFCOUNT",
            "GETDEL",
            "BITFIELD",
        ] {
            assert!(!is_read_command(&cmd(&[c])), "{c}");
        }
    }

    #[test]
    fn container_subcommands_gated() {
        // Reads pass, write subcommands are refused — the CONFIG SET bypass.
        assert!(is_read_command(&cmd(&["CONFIG", "GET", "maxmemory"])));
        assert!(is_read_command(&cmd(&["config", "get", "*"])));
        assert!(is_read_command(&cmd(&["MEMORY", "USAGE", "k"])));
        for parts in [
            vec!["CONFIG", "SET", "requirepass", "x"],
            vec!["CONFIG", "REWRITE"],
            vec!["CONFIG", "RESETSTAT"],
            vec!["CONFIG"], // no subcommand → not a read
            vec!["MEMORY", "PURGE"],
        ] {
            assert!(!is_read_command(&cmd(&parts)), "{parts:?}");
        }
    }

    #[test]
    fn empty_is_not_read() {
        assert!(!is_read_command(&[]));
    }
}
