//! Config types. One set of structs drives JSON parsing, validation and the
//! generated JSON Schema (`ds-mcp gen-schema`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// File name of the per-workspace config discovered from MCP client roots.
pub const ROOT_CONFIG_NAME: &str = ".ds-mcp.json";

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// JSON Schema reference; ignored at runtime.
    #[serde(rename = "$schema", default)]
    #[allow(dead_code)] // parse-only: accepts the "$schema" key
    pub schema: Option<String>,
    /// HTTP transport settings; only used with `--transport http`.
    #[serde(default)]
    pub http: HttpConfig,
    /// Per-query timeout in seconds. Default 30.
    pub query_timeout_seconds: Option<u64>,
    /// Named data sources. The map key is the `source` argument MCP clients
    /// pass on every tool call.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Listen address. Default 127.0.0.1:7100.
    #[serde(default = "default_addr")]
    pub addr: String,
    /// Path the MCP endpoint is mounted at. Default /mcp.
    #[serde(default = "default_path")]
    pub path: String,
    /// Run without server-side sessions (no server->client requests, so
    /// per-workspace roots configs do not work in this mode).
    #[serde(default)]
    pub stateless: bool,
    /// Return plain JSON responses instead of SSE streams (stateless only).
    #[serde(default)]
    pub json_response: bool,
    /// Extra Origin/Host values to accept besides localhost. `["*"]` disables
    /// the check entirely (put a trusted proxy in front).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            addr: default_addr(),
            path: default_path(),
            stateless: false,
            json_response: false,
            allowed_origins: Vec::new(),
        }
    }
}

fn default_addr() -> String {
    "127.0.0.1:7100".into()
}

fn default_path() -> String {
    "/mcp".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    MySql,
    MariaDb,
    Postgres,
    Sqlite,
    DuckDb,
    Mssql,
    ClickHouse,
    Redis,
    Valkey,
    MongoDb,
    Elasticsearch,
    OpenSearch,
    Qdrant,
}

impl EngineKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::MySql => "mysql",
            Self::MariaDb => "mariadb",
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
            Self::DuckDb => "duckdb",
            Self::Mssql => "mssql",
            Self::ClickHouse => "clickhouse",
            Self::Redis => "redis",
            Self::Valkey => "valkey",
            Self::MongoDb => "mongodb",
            Self::Elasticsearch => "elasticsearch",
            Self::OpenSearch => "opensearch",
            Self::Qdrant => "qdrant",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Database engine.
    pub engine: EngineKind,
    /// Free-text description surfaced via `list_sources` so a model can pick
    /// the right source unaided.
    pub description: Option<String>,
    /// Refuse all write tools on this source.
    #[serde(default)]
    pub readonly: bool,
    /// Redact sensitive column/field values on the way out. `true` turns on
    /// the default pattern set; the object form overrides patterns and mode.
    pub pii: Option<Pii>,
    /// Full connection string (engine-native format). Mutually exclusive
    /// with host/port/user/password/database. `uri` is accepted as an alias.
    #[serde(alias = "uri")]
    pub dsn: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    /// Password. Supports `${ENV_VAR}` expansion.
    pub password: Option<String>,
    /// API key auth (elasticsearch/opensearch/qdrant). Supports `${ENV_VAR}`
    /// expansion.
    pub api_key: Option<String>,
    pub database: Option<String>,
    /// Database file path (sqlite/duckdb only). Supports `~` expansion.
    pub path: Option<String>,
    /// Default database for document tools (mongodb only).
    pub default_database: Option<String>,
    /// Connect timeout in seconds. Default 10.
    pub connect_timeout_seconds: Option<u64>,
    /// SSH tunnel: the database is dialed through this host.
    pub ssh: Option<SshConfig>,
    /// Docker container the database runs in: the published port (or the
    /// container IP) is dialed instead of host/port.
    pub docker: Option<DockerConfig>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    /// Container name or id.
    pub container: String,
    /// Port inside the container; defaults to the engine's default port.
    pub port: Option<u16>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SshConfig {
    pub host: String,
    /// Default 22.
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    /// Password auth. Supports `${ENV_VAR}` expansion.
    pub password: Option<String>,
    /// Private key file. Supports `~` expansion.
    pub identity_file: Option<String>,
    /// Passphrase for the private key. Supports `${ENV_VAR}` expansion.
    pub passphrase: Option<String>,
    /// Use the running ssh-agent (`SSH_AUTH_SOCK`). With no `identity_file`,
    /// password or `use_agent` configured, the agent and then ~/.ssh default
    /// keys are tried automatically.
    #[serde(default)]
    pub use_agent: bool,
    /// `known_hosts` file used for host-key verification. Default ~/.`ssh/known_hosts`.
    pub known_hosts_file: Option<String>,
    /// Skip host-key verification. Do not use outside throwaway environments.
    #[serde(default)]
    pub insecure_ignore_host_key: bool,
}

const fn default_ssh_port() -> u16 {
    22
}

/// Outbound redaction of sensitive values. `"pii": true` is the short form:
/// the default column patterns below plus every value detector, in `redact`
/// mode. The object form overrides any part of that. Absent (or `false`)
/// leaves results untouched.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Pii {
    /// `true` = default patterns + all value detectors + redact; `false` = off.
    Enabled(bool),
    Rules(PiiRules),
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PiiRules {
    /// Case-insensitive globs (`*` = any run) matched against the returned
    /// column/field name, optionally qualified `table.column`. Omitted = the
    /// default set.
    pub columns: Option<Vec<String>>,
    /// Value detectors to run over string values, catching what an innocent
    /// column name hides: `email`, `credit_card`, `iban`, `ssn`, `phone`,
    /// `jwt`, `private_key`, `aws_key`. Omitted = all of them; `[]` = column
    /// names only. An unknown name is a config error.
    pub values: Option<Vec<String>>,
    /// What happens to a matching value. Default `redact`.
    #[serde(default)]
    pub mode: PiiMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PiiMode {
    /// Value becomes `"[redacted]"`.
    #[default]
    Redact,
    /// Value becomes a stable `sha256:<prefix>`, so equal values still group
    /// and join.
    Hash,
    /// Column/field is removed from the result entirely.
    Drop,
}

/// Applied when `pii` is on but `columns` is omitted. Deliberately broad —
/// over-redacting a `token_count` is cheaper than leaking an address — and
/// deliberately not `*name*`, which would swallow `table_name`.
const DEFAULT_PII_COLUMNS: &[&str] = &[
    "*password*",
    "*passwd*",
    "*secret*",
    "*token*",
    "*api_key*",
    "*apikey*",
    "*private_key*",
    "*email*",
    "*phone*",
    "*mobile*",
    "*ssn*",
    "*social_security*",
    "*national_id*",
    "*passport*",
    "*credit_card*",
    "*card_number*",
    "*cvv*",
    "*iban*",
    "*date_of_birth*",
    "dob",
    "*birth_date*",
    "*first_name*",
    "*last_name*",
    "*full_name*",
    "*address*",
];

fn default_pii_columns() -> &'static [String] {
    static COLUMNS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        DEFAULT_PII_COLUMNS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    });
    &COLUMNS
}

/// What a `pii` block resolves to: the column patterns, the enabled value
/// detectors (None = all of them) and what to do with a match.
pub type PiiRuleset<'a> = (&'a [String], Option<&'a [String]>, PiiMode);

impl Pii {
    /// The rules to apply, or None when this source redacts nothing.
    pub fn resolve(&self) -> Option<PiiRuleset<'_>> {
        match self {
            Self::Enabled(false) => None,
            Self::Enabled(true) => Some((default_pii_columns(), None, PiiMode::Redact)),
            Self::Rules(r) => Some((
                match r.columns.as_deref() {
                    Some(columns) => columns,
                    None => default_pii_columns(),
                },
                r.values.as_deref(),
                r.mode,
            )),
        }
    }
}
impl Config {
    pub fn query_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.query_timeout_seconds.unwrap_or(30))
    }
}

impl SourceConfig {
    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.connect_timeout_seconds.unwrap_or(10))
    }
}

/// Default global config path: `$XDG_CONFIG_HOME/ds-mcp/config.json`.
pub fn default_path_global() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ds-mcp").join("config.json"))
}

/// Load and validate a config file. A `.env` next to the config supplies
/// `${VAR}` values (real environment variables still win), so a per-repo
/// `.ds-mcp.json` + `.env` keep secrets out of the committed config. Relative
/// paths inside the config (sqlite/duckdb `path`, ssh key files) resolve
/// against the config file's directory.
pub fn load(path: &Path) -> Result<Config> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let dotenv = load_dotenv(&dir.join(".env"));
    let mut cfg =
        parse_with_env(&raw, &dotenv).with_context(|| format!("config {}", path.display()))?;
    resolve_relative_paths(&mut cfg, dir);
    Ok(cfg)
}

/// Read `.env` into a map (best effort — a missing file is fine). Values are
/// a fallback: process environment variables take precedence.
fn load_dotenv(path: &Path) -> BTreeMap<String, String> {
    match dotenvy::from_path_iter(path) {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => BTreeMap::new(),
    }
}

fn resolve_relative_paths(cfg: &mut Config, dir: &Path) {
    let resolve = |v: &mut Option<String>| {
        if let Some(p) = v
            && !Path::new(p.as_str()).is_absolute()
        {
            *v = Some(dir.join(p.as_str()).to_string_lossy().into_owned());
        }
    };
    for src in cfg.sources.values_mut() {
        resolve(&mut src.path);
        if let Some(ssh) = &mut src.ssh {
            resolve(&mut ssh.identity_file);
            resolve(&mut ssh.known_hosts_file);
        }
    }
}

/// Parse + validate without a `.env` (env vars come from the process only).
/// Used by the tests; `load` is the real entry point.
#[cfg(test)]
pub fn parse(raw: &str) -> Result<Config> {
    parse_with_env(raw, &BTreeMap::new())
}

fn parse_with_env(raw: &str, dotenv: &BTreeMap<String, String>) -> Result<Config> {
    let mut cfg: Config = serde_json::from_str(raw)?;
    expand(&mut cfg, dotenv)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// `${ENV_VAR}` expansion on secret-bearing fields, `~` on path fields.
fn expand(cfg: &mut Config, dotenv: &BTreeMap<String, String>) -> Result<()> {
    for (name, src) in &mut cfg.sources {
        let ctx = |field: &str| format!("source {name:?}: {field}");
        if let Some(v) = &src.password {
            src.password = Some(expand_env(v, dotenv).with_context(|| ctx("password"))?);
        }
        if let Some(v) = &src.dsn {
            src.dsn = Some(expand_env(v, dotenv).with_context(|| ctx("dsn"))?);
        }
        if let Some(v) = &src.api_key {
            src.api_key = Some(expand_env(v, dotenv).with_context(|| ctx("api_key"))?);
        }
        if let Some(v) = &src.path {
            src.path = Some(expand_tilde(v));
        }
        if let Some(ssh) = &mut src.ssh {
            if let Some(v) = &ssh.password {
                ssh.password = Some(expand_env(v, dotenv).with_context(|| ctx("ssh.password"))?);
            }
            if let Some(v) = &ssh.passphrase {
                ssh.passphrase =
                    Some(expand_env(v, dotenv).with_context(|| ctx("ssh.passphrase"))?);
            }
            if let Some(v) = &ssh.identity_file {
                ssh.identity_file = Some(expand_tilde(v));
            }
            if let Some(v) = &ssh.known_hosts_file {
                ssh.known_hosts_file = Some(expand_tilde(v));
            }
        }
    }
    Ok(())
}

/// Expand `${VAR}` references, preferring the process environment and falling
/// back to the config's sibling `.env`. Unset variables are an error (fail
/// loud rather than connecting with an empty password).
fn expand_env(s: &str, dotenv: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            bail!("unterminated ${{ in {s:?}");
        };
        let var = &after[..end];
        let value = std::env::var(var)
            .ok()
            .or_else(|| dotenv.get(var).cloned())
            .with_context(|| format!("environment variable {var} not set (and not in .env)"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    s.to_string()
}

fn validate(cfg: &Config) -> Result<()> {
    if cfg.sources.is_empty() {
        bail!("config has no sources");
    }
    for (name, src) in &cfg.sources {
        validate_source(src).with_context(|| format!("source {name:?}"))?;
    }
    Ok(())
}

fn validate_source(src: &SourceConfig) -> Result<()> {
    match src.engine {
        EngineKind::Sqlite | EngineKind::DuckDb => {
            if src.path.is_none() && src.dsn.is_none() {
                bail!("{} needs `path`", src.engine.name());
            }
            if src.ssh.is_some() || src.docker.is_some() {
                bail!(
                    "{} is a local file; ssh/docker make no sense",
                    src.engine.name()
                );
            }
        }
        // Everything else defaults to localhost + the engine's default port,
        // so a bare {"engine": "..."} is valid.
        _ => {
            if src.dsn.is_some() && src.host.is_some() {
                bail!("`dsn` and `host` are mutually exclusive; put the host in the dsn");
            }
        }
    }
    if !matches!(src.engine, EngineKind::Sqlite | EngineKind::DuckDb) && src.path.is_some() {
        bail!("`path` is only for file engines (sqlite, duckdb)");
    }
    if src.engine != EngineKind::MongoDb && src.default_database.is_some() {
        bail!("`default_database` is mongodb-only; use `database`");
    }
    if !matches!(
        src.engine,
        EngineKind::Elasticsearch | EngineKind::OpenSearch | EngineKind::Qdrant
    ) && src.api_key.is_some()
    {
        bail!("`api_key` is for elasticsearch/opensearch/qdrant only");
    }
    if src.ssh.is_some() && src.docker.is_some() {
        bail!("`ssh` and `docker` are mutually exclusive");
    }
    if let Some(Pii::Rules(rules)) = &src.pii
        && let Some(values) = &rules.values
        && let Some(unknown) = values.iter().find(|v| !crate::detect::is_known(v))
    {
        bail!(
            "unknown pii value detector {unknown:?}; known: {}",
            crate::detect::names().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal(engine: &str, extra: &str) -> String {
        format!(r#"{{"sources":{{"s":{{"engine":"{engine}"{extra}}}}}}}"#)
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = parse(r#"{"sources":{},"nope":1}"#).unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "{err}");
    }

    #[test]
    fn rejects_empty_sources() {
        assert!(parse(r#"{"sources":{}}"#).is_err());
    }

    #[test]
    fn accepts_minimal_mysql_host() {
        let cfg = parse(&minimal("mysql", r#","host":"localhost""#)).unwrap();
        assert_eq!(cfg.sources["s"].engine, EngineKind::MySql);
        assert_eq!(cfg.http.addr, "127.0.0.1:7100");
    }

    #[test]
    fn uri_is_dsn_alias() {
        let cfg = parse(&minimal("mongodb", r#","uri":"mongodb://localhost""#)).unwrap();
        assert_eq!(cfg.sources["s"].dsn.as_deref(), Some("mongodb://localhost"));
    }

    #[test]
    fn dsn_and_host_conflict() {
        assert!(parse(&minimal("mysql", r#","dsn":"mysql://x","host":"y""#)).is_err());
        // Bare engine works: localhost + default port.
        assert!(parse(&minimal("mysql", "")).is_ok());
        assert!(parse(&minimal("mongodb", "")).is_ok());
    }

    #[test]
    fn dsn_with_ssh_allowed() {
        assert!(
            parse(&minimal(
                "postgres",
                r#","dsn":"postgres://x","ssh":{"host":"b","user":"u","use_agent":true}"#,
            ))
            .is_ok()
        );
    }

    #[test]
    fn ssh_and_docker_conflict() {
        let e = parse(&minimal(
            "mysql",
            r#","ssh":{"host":"b","user":"u"},"docker":{"container":"db"}"#,
        ))
        .unwrap_err();
        assert!(format!("{e:#}").contains("mutually exclusive"), "{e}");
    }

    #[test]
    fn dotenv_beside_config_supplies_vars() {
        let dir = std::env::temp_dir().join(format!("ds-mcp-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "DS_MCP_DOTENV_PW=fromdotenv\n").unwrap();
        let cfg_path = dir.join("config.json");
        std::fs::write(
            &cfg_path,
            r#"{"sources":{"s":{"engine":"mysql","host":"h","password":"${DS_MCP_DOTENV_PW}"}}}"#,
        )
        .unwrap();
        let cfg = load(&cfg_path).unwrap();
        assert_eq!(cfg.sources["s"].password.as_deref(), Some("fromdotenv"));
    }

    #[test]
    fn relative_paths_resolve_against_config_dir() {
        let dir = std::env::temp_dir().join(format!("ds-mcp-relcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.json");
        std::fs::write(
            &cfg_path,
            r#"{"sources":{"s":{"engine":"sqlite","path":"data/dev.db"}}}"#,
        )
        .unwrap();
        let cfg = load(&cfg_path).unwrap();
        assert_eq!(
            cfg.sources["s"].path.as_deref(),
            dir.join("data/dev.db").to_str(),
        );
    }

    #[test]
    fn sqlite_needs_path() {
        assert!(parse(&minimal("sqlite", "")).is_err());
        assert!(parse(&minimal("sqlite", r#","path":"/tmp/x.db""#)).is_ok());
    }

    #[test]
    fn env_expansion() {
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("DS_MCP_TEST_PW", "sekret") };
        let cfg = parse(&minimal(
            "mysql",
            r#","host":"h","password":"${DS_MCP_TEST_PW}""#,
        ))
        .unwrap();
        assert_eq!(cfg.sources["s"].password.as_deref(), Some("sekret"));
    }

    #[test]
    fn unset_env_is_error() {
        let e = parse(&minimal(
            "mysql",
            r#","host":"h","password":"${DS_MCP_TEST_UNSET_VAR}""#,
        ))
        .unwrap_err();
        assert!(format!("{e:#}").contains("DS_MCP_TEST_UNSET_VAR"), "{e}");
    }

    #[test]
    fn pii_short_and_long_form() {
        let short = parse(&minimal("mysql", r#","pii":true"#)).unwrap();
        let (columns, values, mode) = short.sources["s"].pii.as_ref().unwrap().resolve().unwrap();
        assert_eq!(mode, PiiMode::Redact);
        assert!(columns.iter().any(|c| c == "*email*"));
        // None = every value detector.
        assert!(values.is_none());

        let long = parse(&minimal(
            "mysql",
            r#","pii":{"columns":["email"],"values":["iban"],"mode":"hash"}"#,
        ))
        .unwrap();
        let (columns, values, mode) = long.sources["s"].pii.as_ref().unwrap().resolve().unwrap();
        assert_eq!(columns, ["email".to_string()].as_slice());
        assert_eq!(values, Some(["iban".to_string()].as_slice()));
        assert_eq!(mode, PiiMode::Hash);

        // Object form without `columns`/`values` still gets both defaults.
        let defaulted = parse(&minimal("mysql", r#","pii":{"mode":"drop"}"#)).unwrap();
        let (columns, values, mode) = defaulted.sources["s"]
            .pii
            .as_ref()
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(mode, PiiMode::Drop);
        assert!(columns.len() > 1);
        assert!(values.is_none());

        // An unknown detector is a config error, not a silent no-op.
        let e = parse(&minimal("mysql", r#","pii":{"values":["nope"]}"#)).unwrap_err();
        assert!(
            format!("{e:#}").contains("unknown pii value detector"),
            "{e}"
        );

        // Off is off.
        let off = parse(&minimal("mysql", r#","pii":false"#)).unwrap();
        assert!(off.sources["s"].pii.as_ref().unwrap().resolve().is_none());
        assert!(
            parse(&minimal("mysql", "")).unwrap().sources["s"]
                .pii
                .is_none()
        );

        assert!(parse(&minimal("mysql", r#","pii":{"mode":"nonsense"}"#)).is_err());
    }

    #[test]
    fn ssh_without_auth_config_is_valid() {
        // Auth falls back to the agent / default keys at connect time.
        assert!(
            parse(&minimal(
                "mysql",
                r#","host":"h","ssh":{"host":"b","user":"u"}"#
            ))
            .is_ok()
        );
    }
}
