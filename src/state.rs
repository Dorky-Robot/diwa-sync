use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Per-(peer, repo) bookkeeping. Used today only to attribute "when did we
/// last touch this" in logs; future versions may use it to skip unchanged
/// peers via a hash, but a hash check requires reading the peer's DB which
/// roughly costs the same as snapshotting it, so v1 just merges every time.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub peers: HashMap<String, PeerState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PeerState {
    #[serde(default)]
    pub repos: HashMap<String, RepoState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RepoState {
    pub last_merged_at: Option<String>,
    pub last_local_after: Option<i64>,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read state {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let state: State = serde_json::from_str(&raw)
            .with_context(|| format!("parse state {}", path.display()))?;
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn record(&mut self, peer: &str, repo: &str, local_after: i64) {
        let now = chrono::Utc::now().to_rfc3339();
        let p = self.peers.entry(peer.to_string()).or_default();
        let r = p.repos.entry(repo.to_string()).or_default();
        r.last_merged_at = Some(now);
        r.last_local_after = Some(local_after);
    }
}
