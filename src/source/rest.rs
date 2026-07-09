//! REST sources: a thin pass-through over an HTTP+JSON API. Elasticsearch,
//! OpenSearch (ES-API compatible) and Qdrant all share this — the tool layer
//! sends a request document ({"method": "GET", "path": "/idx/_search",
//! "body": {...}}). Read-only sources are gated by a method+path classifier so
//! mutating endpoints are refused and pointed at `execute`.

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::config::{EngineKind, SourceConfig};

pub struct RestSource {
    name: String,
    cfg: SourceConfig,
    readonly: bool,
    /// (client, base url without trailing slash).
    conn: OnceCell<(reqwest::Client, String)>,
    tunnel: OnceCell<super::ssh::SshTunnel>,
}

/// Elasticsearch/OpenSearch read actions reachable by POST. The "action" is
/// the first `_`-prefixed path segment (`/{index}/_search` → `_search`); write
/// actions (`_doc`, `_bulk`, `_update`, `_update_by_query`, ...) are absent, so
/// they fall through to a write. Matching the segment — not a substring —
/// keeps an index literally named `my_search` from being taken for a read.
const ES_READ_ACTIONS: &[&str] = &[
    "_search",
    "_msearch",
    "_count",
    "_field_caps",
    "_mget",
    "_termvectors",
    "_mtermvectors",
    "_analyze",
    "_validate",
    "_explain",
    "_rank_eval",
    "_search_shards",
    "_render",
    "_sql",
    "_pit",
    "_terms_enum",
    "_knn_search",
    "_mvt",
];

/// Qdrant read operations reachable by POST. The "operation" is the path after
/// the collection name (`/collections/{c}/points/search` → `points/search`),
/// so a collection literally named `search` cannot make a payload/vector write
/// look like a read. Write ops (`payload`, `vectors`, `batch`, `index`,
/// `delete`, ...) are absent and fall through to a write.
const QDRANT_READ_OPS: &[&str] = &[
    "search",
    "query",
    "scroll",
    "count",
    "recommend",
    "discover",
    "facet",
    "matrix",
    "exists",
];

/// Normalize a path exactly as the HTTP client will before it hits the wire:
/// `reqwest` parses the URL with the `url` crate, which resolves `.`/`..` (and
/// their percent-encoded forms) as dot segments. The read gate MUST classify
/// that same normalized path — otherwise `POST /idx/_search/../_doc` is gated
/// on `_search` (read) but sent as `/idx/_doc` (write). Returns None if the
/// path will not parse, which the gate treats as a non-read (default-deny).
fn normalized_path(path: &str) -> Option<String> {
    let joined = format!("http://x/{}", path.trim_start_matches('/'));
    url::Url::parse(&joined).ok().map(|u| u.path().to_string())
}

/// Is this request a read? GET/HEAD always are; POST depends on the endpoint;
/// PUT/DELETE/PATCH always mutate. The POST classifier is the read-only
/// boundary for the `query` tool, so it (a) classifies the *normalized* path
/// the server will actually receive, and (b) matches path *segments*, never
/// substrings — neither a `..` segment nor a container name can spoof a read.
pub fn is_read_request(engine: EngineKind, method: &str, path: &str) -> bool {
    match method.to_ascii_uppercase().as_str() {
        "GET" | "HEAD" => true,
        "POST" => {
            let Some(p) = normalized_path(path) else {
                return false;
            };
            let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
            match engine {
                EngineKind::Qdrant => {
                    // Operation = segments after `/collections/{name}`.
                    let op: &[&str] = match segs.as_slice() {
                        ["collections", _collection, rest @ ..] => rest,
                        other => other,
                    };
                    // Retrieve-points-by-id is POST /collections/{c}/points.
                    if op == ["points"] {
                        return true;
                    }
                    // A delete op is a write even next to a read verb.
                    if op.contains(&"delete") {
                        return false;
                    }
                    op.iter().any(|s| QDRANT_READ_OPS.contains(s))
                }
                // Elasticsearch / OpenSearch: the first `_`-prefixed segment.
                _ => segs
                    .iter()
                    .find(|s| s.starts_with('_'))
                    .is_some_and(|a| ES_READ_ACTIONS.contains(a)),
            }
        }
        _ => false,
    }
}

impl RestSource {
    pub fn new(name: &str, cfg: SourceConfig, force_readonly: bool) -> Self {
        let readonly = force_readonly || cfg.readonly;
        Self {
            name: name.to_string(),
            cfg,
            readonly,
            conn: OnceCell::new(),
            tunnel: OnceCell::new(),
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

    fn default_port(&self) -> u16 {
        match self.cfg.engine {
            EngineKind::Qdrant => 6333,
            _ => 9200, // elasticsearch / opensearch
        }
    }

    async fn conn(&self) -> Result<(reqwest::Client, String)> {
        let (client, base) = self
            .conn
            .get_or_try_init(|| async {
                // Parse scheme/host/port from the dsn, else default to http
                // over discrete fields + the engine's default port.
                let (scheme, host, port) = match &self.cfg.dsn {
                    Some(dsn) => {
                        let u = url::Url::parse(dsn).context("parse dsn")?;
                        let scheme = u.scheme().to_string();
                        let host = u.host_str().unwrap_or("127.0.0.1").to_string();
                        let port = u.port_or_known_default().unwrap_or(self.default_port());
                        (scheme, host, port)
                    }
                    None => (
                        "http".to_string(),
                        self.cfg.host.as_deref().unwrap_or("127.0.0.1").to_string(),
                        self.cfg.port.unwrap_or(self.default_port()),
                    ),
                };
                let ep = super::endpoint::resolve(&self.cfg, &host, port).await?;
                if let Some(t) = ep.tunnel {
                    let _ = self.tunnel.set(t);
                }
                // ponytail: through an ssh tunnel we dial the local forward
                // (127.0.0.1:port), so an `https` dsn would verify the cert
                // against 127.0.0.1 and fail — tunnelled sources should reach
                // the cluster over http. Upgrade path: carry the original host
                // as a TLS SNI/domain override on the reqwest client.
                let base = format!("{scheme}://{}:{}", ep.host, ep.port);
                let client = reqwest::Client::builder()
                    .connect_timeout(self.cfg.connect_timeout())
                    .build()
                    .context("build http client")?;
                Ok::<_, anyhow::Error>((client, base))
            })
            .await
            .with_context(|| format!("connect to source {:?}", self.name))?;
        Ok((client.clone(), base.clone()))
    }

    /// Send a raw request. The caller enforces the read-only gate.
    pub async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let (client, base) = self.conn().await?;
        let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes())
            .with_context(|| format!("invalid HTTP method {method:?}"))?;
        let url = format!("{base}/{}", path.trim_start_matches('/'));
        let mut req = client.request(method, &url);
        // API key auth (header style differs per engine); basic auth otherwise.
        if let Some(key) = &self.cfg.api_key {
            req = match self.cfg.engine {
                // Qdrant reads the raw key from the `api-key` header.
                EngineKind::Qdrant => req.header("api-key", key),
                // Elasticsearch/OpenSearch: base64 id:key in `ApiKey <key>`.
                _ => req.header(reqwest::header::AUTHORIZATION, format!("ApiKey {key}")),
            };
        } else if let Some(user) = &self.cfg.user {
            req = req.basic_auth(user, self.cfg.password.as_deref());
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.context("send request")?;
        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .unwrap_or_else(|_| Value::String("(non-JSON response body)".into()));
        if !status.is_success() {
            bail!("{} {status}: {value}", self.cfg.engine.name());
        }
        Ok(value)
    }

    pub async fn ping(&self) -> Result<()> {
        // Root returns cluster/build info on both ES and Qdrant.
        self.request("GET", "/", None).await.map(|_| ())
    }

    /// List the source's containers: ES/OS indices, Qdrant collections.
    pub async fn list_containers(&self) -> Result<Value> {
        match self.cfg.engine {
            EngineKind::Qdrant => self.request("GET", "/collections", None).await,
            _ => {
                self.request(
                    "GET",
                    "/_cat/indices?format=json&h=index,health,status,docs.count,store.size",
                    None,
                )
                .await
            }
        }
    }

    /// Describe one container: ES/OS field mappings, Qdrant collection info.
    pub async fn describe(&self, name: &str) -> Result<Value> {
        match self.cfg.engine {
            EngineKind::Qdrant => {
                self.request("GET", &format!("/collections/{name}"), None)
                    .await
            }
            _ => {
                self.request("GET", &format!("/{name}/_mapping"), None)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EngineKind::{Elasticsearch, Qdrant};

    fn cfg(json: &str) -> SourceConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn readonly_is_forced_and_honored() {
        // `--read-only` forces it even when the config says writable.
        assert!(RestSource::new("s", cfg(r#"{"engine":"qdrant"}"#), true).readonly());
        // A config `readonly` sticks without the global flag.
        assert!(
            RestSource::new(
                "s",
                cfg(r#"{"engine":"elasticsearch","readonly":true}"#),
                false
            )
            .readonly()
        );
        // A plain source stays writable.
        assert!(!RestSource::new("s", cfg(r#"{"engine":"qdrant"}"#), false).readonly());
    }

    #[test]
    fn path_traversal_cannot_spoof_a_read() {
        // `..` (and its percent-encoded form) is resolved by the HTTP client
        // before the request is sent, so the gate must classify the resolved
        // path — a read verb followed by `../<write>` is a write.
        for p in [
            "/idx/_search/../_doc",
            "/_search/../_bulk",
            "/idx/_search/%2e%2e/_doc",
            "/idx/_search/../_update_by_query",
        ] {
            assert!(!is_read_request(Elasticsearch, "POST", p), "ES {p}");
        }
        for p in [
            "/collections/c/points/search/../payload",
            "/collections/c/points/count/../../c/points/delete",
            "/collections/c/points/query/%2e%2e/payload",
        ] {
            assert!(!is_read_request(Qdrant, "POST", p), "Qdrant {p}");
        }
        // A genuine read with no traversal still passes.
        assert!(is_read_request(Elasticsearch, "POST", "/idx/_search"));
        assert!(is_read_request(
            Qdrant,
            "POST",
            "/collections/c/points/search"
        ));
    }

    #[test]
    fn post_read_gate_is_default_deny() {
        // An unknown POST action/op must be treated as a write, so a novel
        // endpoint can never slip past a read-only source via `query`.
        assert!(!is_read_request(Elasticsearch, "POST", "/idx/_frobnicate"));
        assert!(!is_read_request(
            Qdrant,
            "POST",
            "/collections/c/points/frobnicate"
        ));
        assert!(!is_read_request(Elasticsearch, "POST", "/idx")); // no action segment
        // Unknown/odd methods are writes too.
        assert!(!is_read_request(Elasticsearch, "PATCH", "/idx/_search"));
        assert!(!is_read_request(
            Qdrant,
            "FROB",
            "/collections/c/points/search"
        ));
    }

    #[test]
    fn es_read_gate() {
        for (m, p) in [
            ("GET", "/idx/_search"),
            ("get", "/_cat/indices"),
            ("HEAD", "/idx"),
            ("POST", "/idx/_search"),
            ("POST", "/_search/scroll"),
            ("POST", "/idx/_count"),
        ] {
            assert!(is_read_request(Elasticsearch, m, p), "{m} {p}");
        }
        // Read action reached via a query string, and under a multi-segment
        // index name — still a read.
        assert!(is_read_request(
            Elasticsearch,
            "POST",
            "/idx/_search?size=1"
        ));
        assert!(is_read_request(Elasticsearch, "POST", "/a,b/_msearch"));
        for (m, p) in [
            ("POST", "/idx/_doc"),
            ("POST", "/idx/_bulk"),
            ("POST", "/idx/_update_by_query"),
            ("POST", "/idx/_delete_by_query"),
            ("PUT", "/idx/_doc/1"),
            ("DELETE", "/idx/_doc/1"),
            ("DELETE", "/idx"),
            // Bypass guard: an index named to embed a read verb must NOT let a
            // write slip through the read gate.
            ("POST", "/my_search/_doc"),
            ("POST", "/scroll_data/_bulk"),
        ] {
            assert!(!is_read_request(Elasticsearch, m, p), "{m} {p}");
        }
    }

    #[test]
    fn qdrant_read_gate() {
        for (m, p) in [
            ("GET", "/collections"),
            ("GET", "/collections/c"),
            ("POST", "/collections/c/points/search"),
            ("POST", "/collections/c/points/scroll"),
            ("POST", "/collections/c/points/count"),
            ("POST", "/collections/c/points/query"),
            ("POST", "/collections/c/points"), // retrieve by id
        ] {
            assert!(is_read_request(Qdrant, m, p), "{m} {p}");
        }
        // Search under a query string, and the batch/matrix read variants.
        assert!(is_read_request(
            Qdrant,
            "POST",
            "/collections/c/points/query?x=1"
        ));
        assert!(is_read_request(
            Qdrant,
            "POST",
            "/collections/c/points/search/batch"
        ));
        for (m, p) in [
            ("PUT", "/collections/c"),                 // create collection
            ("PUT", "/collections/c/points"),          // upsert
            ("POST", "/collections/c/points/delete"),  // delete points
            ("POST", "/collections/c/points/payload"), // set payload
            ("POST", "/collections/c/points/vectors"), // update vectors
            ("POST", "/collections/c/index"),          // build field index
            ("DELETE", "/collections/c"),
            // Bypass guard: a collection named after a read verb must NOT let a
            // payload write slip through the read gate.
            ("POST", "/collections/search/points/payload"),
            ("POST", "/collections/query/points/delete"),
        ] {
            assert!(!is_read_request(Qdrant, m, p), "{m} {p}");
        }
    }
}
