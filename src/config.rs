use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// SSH aliases of peer machines (e.g. `host-a`, or any name resolvable
    /// from your `~/.ssh/config`). Each peer must have a `~/.diwa/` and
    /// `sqlite3` on PATH.
    pub peers: Vec<String>,

    /// Path to the local diwa data dir. Defaults to `~/.diwa`.
    #[serde(default = "default_diwa_dir")]
    pub diwa_dir: PathBuf,
}

fn default_diwa_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".diwa"))
        .unwrap_or_else(|| PathBuf::from(".diwa"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn write_default(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = r#"# diwa-sync config

# SSH aliases for peer machines. One entry per line of `peers = [...]`.
# These must work passwordlessly: `ssh -o BatchMode=yes <alias> true`.
peers = []

# Local diwa data dir. Default: ~/.diwa
# diwa_dir = "/Users/you/.diwa"
"#;
        std::fs::write(path, body)?;
        Ok(())
    }
}

/// `~/.diwa-sync/`
pub fn sync_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home dir")?
        .join(".diwa-sync"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(sync_dir()?.join("config.toml"))
}

pub fn state_path() -> Result<PathBuf> {
    Ok(sync_dir()?.join("state.json"))
}

pub fn log_dir() -> Result<PathBuf> {
    Ok(sync_dir()?.join("log"))
}
