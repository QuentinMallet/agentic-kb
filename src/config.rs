//! kb configuration and path discovery

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_inline_verify_k() -> usize {
    10
}

/// kb configuration
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KbConfig {
    /// Embedding configuration
    pub embed: EmbedConfig,
    /// Maximum number of search results to verify inline (AC18 narrow-K fallback).
    /// Results beyond this count get verified=null in search responses.
    /// Default: 10. Set lower to cap verification latency (e.g. inline_verify_k=3).
    #[serde(default = "default_inline_verify_k")]
    pub inline_verify_k: usize,
    /// Default peer traversal depth for dep-type edges. Default: 1.
    pub dep_depth: Option<u8>,
}

impl KbConfig {
    /// Returns the effective dep traversal depth (defaults to 1 when unset).
    pub fn dep_depth(&self) -> u8 {
        self.dep_depth.unwrap_or(1)
    }
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
    /// Always returns the canonical `.state/agent-kb/agent-kb.db` form, consistent
    /// with `from_root()`. The `agent-kb/` symlink convention at the repo root is
    /// intentionally ignored here — symlink-fallback removal is a separate follow-up
    /// chore (see plan §paths-discover-canonical-form-regression, revision #11).
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let mut dir: &Path = &cwd;
        loop {
            if dir.join(".state").is_dir() {
                return Ok(Paths {
                    lock: dir.join(".state").join(".lock"),
                    events: dir
                        .join(".state")
                        .join("agent-kb")
                        .join("agent-kb-events.jsonl"),
                    db: dir.join(".state").join("agent-kb").join("agent-kb.db"),
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
    use std::fs;
    use std::path::PathBuf;

    /// RAII guard that restores cwd on drop (same pattern as add.rs tests).
    struct CwdGuard(PathBuf);
    impl CwdGuard {
        fn set(dir: &Path) -> Self {
            let orig = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            CwdGuard(orig)
        }
    }
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

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

    /// Regression test (br-bhg): both `Paths::discover` and `Paths::from_root` must
    /// return identical canonical `.state/agent-kb/...` form for the same root input.
    ///
    /// Acceptance criteria (plan §paths-discover-canonical-form-regression):
    /// 1. `discover()` and `from_root()` return the same `.db` path.
    /// 2. Neither path contains `/.agent-kb/` or `/agent-kb/` without the `.state/` prefix.
    /// 3. The invariant holds even when a symlink `<root>/agent-kb/agent-kb.db` exists
    ///    (the historically divergent branch in `discover()`).
    #[test]
    fn test_canonical_form_discover_matches_from_root_without_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Create the `.state/agent-kb/` structure that `discover()` looks for.
        let state_db_dir = root.join(".state").join("agent-kb");
        fs::create_dir_all(&state_db_dir).unwrap();
        fs::write(state_db_dir.join("agent-kb.db"), b"").unwrap();

        // Change into root so `discover()` finds it.
        let _guard = CwdGuard::set(root);
        let discover_paths = Paths::discover().expect("discover should succeed");
        drop(_guard);

        let from_root_paths = Paths::from_root(root);

        // Both must agree on the db path.
        assert_eq!(
            discover_paths.db, from_root_paths.db,
            "discover() and from_root() must return the same canonical db path"
        );

        // The db path must be rooted at `.state/agent-kb/`.
        let db_str = discover_paths.db.to_string_lossy();
        assert!(
            db_str.contains("/.state/agent-kb/"),
            "db path must contain '/.state/agent-kb/', got: {db_str}"
        );

        // Reject non-canonical variants.
        assert!(
            !db_str.contains("/.agent-kb/"),
            "db path must not contain '/.agent-kb/', got: {db_str}"
        );
        // The path segment `/agent-kb/` must always be preceded by `/.state`.
        // `/.state/agent-kb/` starts at position P; the `/agent-kb/` inside it
        // starts at P + len("/.state") = P + 7.
        let agent_kb_pos = db_str.find("/agent-kb/");
        let state_agent_kb_pos = db_str.find("/.state/agent-kb/");
        assert_eq!(
            agent_kb_pos,
            state_agent_kb_pos.map(|p| p + "/.state".len()),
            "'/agent-kb/' in db path must be preceded by '.state', got: {db_str}"
        );
    }

    /// Regression test (br-bhg): `discover()` must return the canonical `.state/...` form
    /// even when a `<root>/agent-kb/agent-kb.db` symlink is present (the historically
    /// divergent branch).
    #[test]
    fn test_canonical_form_discover_with_symlink_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        // Create the `.state/agent-kb/` structure.
        let state_db_dir = root.join(".state").join("agent-kb");
        fs::create_dir_all(&state_db_dir).unwrap();
        let state_db = state_db_dir.join("agent-kb.db");
        fs::write(&state_db, b"").unwrap();

        // Create the symlink `<root>/agent-kb/agent-kb.db` that triggers the old branch.
        let symlink_dir = root.join("agent-kb");
        fs::create_dir_all(&symlink_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&state_db, symlink_dir.join("agent-kb.db")).unwrap();

        let _guard = CwdGuard::set(root);
        let discover_paths = Paths::discover().expect("discover should succeed");
        drop(_guard);

        let from_root_paths = Paths::from_root(root);

        // Both must agree (canonical form), even with symlink present.
        assert_eq!(
            discover_paths.db, from_root_paths.db,
            "discover() and from_root() must return the same canonical db path even when symlink exists"
        );

        let db_str = discover_paths.db.to_string_lossy();
        assert!(
            db_str.contains("/.state/agent-kb/"),
            "db path must contain '/.state/agent-kb/' even with symlink, got: {db_str}"
        );

        // Explicitly reject the symlink-based path form.
        let symlink_path = root.join("agent-kb").join("agent-kb.db").to_string_lossy().to_string();
        assert_ne!(
            discover_paths.db.to_string_lossy().as_ref(),
            symlink_path.as_str(),
            "discover() must not return the symlink-based path, got: {db_str}"
        );
    }
}
