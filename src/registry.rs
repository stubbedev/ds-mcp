//! Named sources built from a config. Connections are lazy; the registry is
//! cheap to construct.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::source::{Source, SourceInfo};

pub struct Registry {
    sources: BTreeMap<String, Arc<Source>>,
}

impl Registry {
    pub fn new(cfg: Config, force_readonly: bool) -> Result<Self> {
        let mut sources = BTreeMap::new();
        for (name, src_cfg) in cfg.sources {
            let src = Source::new(&name, src_cfg, force_readonly)
                .with_context(|| format!("source {name:?}"))?;
            sources.insert(name, Arc::new(src));
        }
        Ok(Self { sources })
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
