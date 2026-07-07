//! Named sources built from a config (connections are lazy), plus the roots
//! Resolver that lets each MCP client bring its own sources via a
//! `.ds-mcp.json` at its workspace root.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::config::{self, Config};
use crate::source::{Source, SourceInfo};

pub struct Registry {
    sources: BTreeMap<String, Arc<Source>>,
    pub query_timeout: Duration,
}

impl Registry {
    pub fn new(cfg: Config, force_readonly: bool) -> Result<Self> {
        let query_timeout = cfg.query_timeout();
        let mut sources = BTreeMap::new();
        for (name, src_cfg) in cfg.sources {
            let src = Source::new(&name, src_cfg, force_readonly)
                .with_context(|| format!("source {name:?}"))?;
            sources.insert(name, Arc::new(src));
        }
        Ok(Self {
            sources,
            query_timeout,
        })
    }

    pub fn get(&self, name: &str) -> Result<&Arc<Source>, String> {
        self.sources.get(name).ok_or_else(|| {
            format!(
                "unknown source {name:?} (known: {})",
                self.sources
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }

    pub fn list(&self) -> Vec<SourceInfo> {
        self.sources.iter().map(|(n, s)| s.info(n)).collect()
    }

    pub async fn close(&self) {
        for src in self.sources.values() {
            src.close().await;
        }
    }
}

/// Resolves which Registry a tool call should use. Per-workspace
/// `.ds-mcp.json` files (from MCP client roots) override the global config;
/// registries built from them are cached by (path, mtime) so an edited file
/// is picked up on the next call.
pub struct Resolver {
    global: Option<Arc<Registry>>,
    force_readonly: bool,
    by_path: tokio::sync::Mutex<HashMap<PathBuf, CachedRoot>>,
}

struct CachedRoot {
    mtime: SystemTime,
    registry: Arc<Registry>,
}

impl Resolver {
    pub fn new(global: Option<Arc<Registry>>, force_readonly: bool) -> Self {
        Self {
            global,
            force_readonly,
            by_path: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn global(&self) -> Option<Arc<Registry>> {
        self.global.clone()
    }

    pub async fn close(&self) {
        if let Some(g) = &self.global {
            g.close().await;
        }
        for (_, cached) in self.by_path.lock().await.drain() {
            cached.registry.close().await;
        }
    }

    /// Registry for the first workspace root that contains a `.ds-mcp.json`.
    /// None when no root has one.
    pub async fn for_roots(&self, roots: &[PathBuf]) -> Result<Option<Arc<Registry>>> {
        for root in roots {
            let path = root.join(config::ROOT_CONFIG_NAME);
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let mut cache = self.by_path.lock().await;
            if let Some(cached) = cache.get(&path)
                && cached.mtime == mtime
            {
                return Ok(Some(cached.registry.clone()));
            }
            let cfg = config::load(&path)?;
            let registry = Arc::new(Registry::new(cfg, self.force_readonly)?);
            if let Some(stale) = cache.insert(
                path,
                CachedRoot {
                    mtime,
                    registry: registry.clone(),
                },
            ) {
                // Close the replaced registry off the request path.
                tokio::spawn(async move { stale.registry.close().await });
            }
            return Ok(Some(registry));
        }
        Ok(None)
    }
}

/// Parse a roots header value / root URI list into directory paths.
/// Accepts `file://` URIs and absolute paths, comma-separated.
pub fn parse_root_paths<'a>(values: impl Iterator<Item = &'a str>) -> Vec<PathBuf> {
    values
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .filter_map(|v| {
            let path = v.strip_prefix("file://").unwrap_or(v);
            let path = Path::new(path);
            path.is_absolute().then(|| path.to_path_buf())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_root_uris_and_paths() {
        let paths = parse_root_paths(["file:///work/a,file:///work/b", "/plain/c"].into_iter());
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/work/a"),
                PathBuf::from("/work/b"),
                PathBuf::from("/plain/c")
            ]
        );
    }

    #[test]
    fn ignores_relative_and_empty() {
        let paths = parse_root_paths(["relative/x, ,file://nothost"].into_iter());
        assert!(paths.is_empty(), "{paths:?}");
    }

    #[tokio::test]
    async fn root_config_cache_by_mtime() {
        let dir = std::env::temp_dir().join(format!("ds-mcp-roots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join(config::ROOT_CONFIG_NAME);
        std::fs::write(
            &cfg_path,
            r#"{"sources":{"a":{"engine":"sqlite","path":"/tmp/a.db"}}}"#,
        )
        .unwrap();

        let resolver = Resolver::new(None, false);
        let roots = vec![dir.clone()];
        let first = resolver.for_roots(&roots).await.unwrap().unwrap();
        let second = resolver.for_roots(&roots).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &second), "same mtime must hit cache");

        // Backdate-proof: force a different mtime, expect a rebuild.
        let new_time = SystemTime::now() + Duration::from_secs(10);
        let file = std::fs::File::open(&cfg_path).unwrap();
        file.set_modified(new_time).unwrap();
        let third = resolver.for_roots(&roots).await.unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first, &third), "changed mtime must rebuild");

        // No config in root → None.
        let empty = std::env::temp_dir().join(format!("ds-mcp-noroot-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        assert!(resolver.for_roots(&[empty]).await.unwrap().is_none());
    }
}
