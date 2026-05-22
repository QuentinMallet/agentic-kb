//! kb configuration and path discovery

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// kb configuration
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KbConfig {
    /// Embedding configuration
    pub embed: EmbedConfig,
}

/// Embedding configuration
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbedConfig {
    /// Whether embedding is enabled
    pub enabled: bool,
    /// Cache directory for model files
    pub cache_dir: Option<PathBuf>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            enabled: std::env::var("KB_NO_EMBED").is_err(),
            cache_dir: None,
        }
    }
}

/// Resolved paths for the kb workspace
pub struct Paths {
    /// Lock file path
    pub lock: PathBuf,
    /// Event log JSONL path
    pub events: PathBuf,
    /// SQLite database path
    pub db: PathBuf,
    /// Model cache directory
    pub fastembed_cache: PathBuf,
}

impl Paths {
    /// Walk up from cwd to find the repo root (has a .state/ directory).
    /// Prefers `agent-kb/agent-kb.db` (symlink convention) but falls back to
    /// `.state/agent-kb/agent-kb.db` when the symlink is absent (e.g. worktrees).
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let mut dir: &Path = &cwd;
        loop {
            if dir.join(".state").is_dir() {
                let symlink_db = dir.join("agent-kb").join("agent-kb.db");
                let state_db = dir.join(".state").join("agent-kb").join("agent-kb.db");
                let db = if symlink_db.exists() {
                    symlink_db
                } else {
                    state_db
                };
                return Ok(Paths {
                    lock: dir.join(".state").join(".lock"),
                    events: dir
                        .join(".state")
                        .join("agent-kb")
                        .join("agent-kb-events.jsonl"),
                    db,
                    fastembed_cache: model_cache_dir(),
                });
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => bail!(
                    "Could not find repo root (no .state/ directory in {} or any parent)",
                    cwd.display()
                ),
            }
        }
    }

    /// Build Paths rooted at a specific directory (for testing).
    /// Uses `.state/agent-kb/` consistently (no symlink in test tempdirs).
    pub fn from_root(root: &Path) -> Self {
        Paths {
            lock: root.join(".state").join(".lock"),
            events: root
                .join(".state")
                .join("agent-kb")
                .join("agent-kb-events.jsonl"),
            db: root
                .join(".state")
                .join("agent-kb")
                .join("agent-kb.db"),
            fastembed_cache: model_cache_dir(),
        }
    }
}

/// Get the git HEAD SHA.
pub fn git_head_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Get the git repo root.
pub fn git_repo_root() -> Option<PathBuf> {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
}

/// Resolve the model cache directory.
fn model_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("FASTEMBED_CACHE_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cache").join("fastembed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kb_config_default_fields() {
        let config = KbConfig::default();
        // embed.enabled depends on KB_NO_EMBED env var at runtime;
        // just verify we get a valid config with cache_dir = None.
        assert!(config.embed.cache_dir.is_none());
    }

    #[test]
    fn test_paths_from_root() {
        let root = Path::new("/tmp/test-repo");
        let paths = Paths::from_root(root);
        assert_eq!(paths.lock, PathBuf::from("/tmp/test-repo/.state/.lock"));
        assert_eq!(
            paths.events,
            PathBuf::from("/tmp/test-repo/.state/agent-kb/agent-kb-events.jsonl")
        );
        assert_eq!(
            paths.db,
            PathBuf::from("/tmp/test-repo/.state/agent-kb/agent-kb.db")
        );
    }
}
