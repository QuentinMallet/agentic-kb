//! kb configuration and path discovery

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_inline_verify_k() -> usize {
    10
}

fn default_compress_threshold() -> usize {
    4000
}

fn default_compress_cosine_cutoff() -> f32 {
    0.85
}

fn default_vacuum_after_compacts() -> u64 {
    8
}

fn default_vacuum_min_free_pages() -> u64 {
    1024
}

/// VACUUM trigger configuration for the compact command.
///
/// VACUUM fires only when BOTH conditions hold:
/// - `vacuum_after_compacts`: compaction counter has reached this value since the last VACUUM.
/// - `vacuum_min_free_pages`: SQLite `freelist_count` is at least this value.
///
/// Both thresholds are config knobs; the AND-gating is hard-coded.
/// Defaults: `vacuum_after_compacts = 8`, `vacuum_min_free_pages = 1024`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VacuumConfig {
    /// Number of compact runs since last VACUUM before triggering. Default: 8.
    #[serde(default = "default_vacuum_after_compacts")]
    pub vacuum_after_compacts: u64,
    /// Minimum SQLite freelist_count required to trigger VACUUM. Default: 1024.
    #[serde(default = "default_vacuum_min_free_pages")]
    pub vacuum_min_free_pages: u64,
}

impl Default for VacuumConfig {
    fn default() -> Self {
        Self {
            vacuum_after_compacts: default_vacuum_after_compacts(),
            vacuum_min_free_pages: default_vacuum_min_free_pages(),
        }
    }
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
    /// Optional VACUUM configuration for the compact command.
    /// When absent, defaults are used (vacuum_after_compacts=8, vacuum_min_free_pages=1024).
    pub vacuum: Option<VacuumConfig>,
    /// Number of worker threads in the bounded verification pool used by
    /// `search_entries`. Defaults to `num_cpus::get_physical()` when absent.
    /// Set lower to cap verification CPU usage (e.g. `verify_pool_size=2`).
    pub verify_pool_size: Option<usize>,
    /// Recency-bias decay factor for hybrid search (λ in exp(-λ·days)).
    /// Set to 0.0 to disable (byte-identical to pre-recency-bias behavior).
    /// A value around 0.01 gives half-life of ~70 days.
    #[serde(default)]
    pub recency_lambda: f32,
    /// Minimum body size (chars) before kb compress considers an entry bloated.
    #[serde(default = "default_compress_threshold")]
    pub compress_threshold: usize,
    /// Cosine similarity cutoff for paragraph deduplication in kb compress.
    #[serde(default = "default_compress_cosine_cutoff")]
    pub compress_cosine_cutoff: f32,
    /// Cosine cutoff for the near-duplicate probe on kb_add / MCP kb_add.
    /// Unset → 0.85. Values > 1.0 disable the probe (cosine never exceeds 1).
    #[serde(default)]
    pub dedup_cosine_cutoff: Option<f32>,
    /// MMR diversification strength for hybrid search (λ in
    /// λ·relevance − (1−λ)·max_cosine_to_selected). 0.0 disables (default).
    #[serde(default)]
    pub mmr_lambda: f32,
}

impl KbConfig {
    /// Returns the effective dep traversal depth (defaults to 1 when unset).
    pub fn dep_depth(&self) -> u8 {
        self.dep_depth.unwrap_or(1)
    }

    /// Load KbConfig from `kb.toml` relative to the repo root derived from `paths`.
    /// Falls back to `KbConfig::default()` when the file is absent or unparseable.
    ///
    /// The repo root is inferred as `paths.db.parent().parent().parent()` (i.e. the
    /// directory above `.state/agent-kb/`).
    pub fn from_paths(paths: &Paths) -> Self {
        let root = paths
            .db
            .parent() // agent-kb/
            .and_then(|p| p.parent()) // .state/
            .and_then(|p| p.parent()); // repo root
        let toml_path = match root {
            Some(r) => r.join("kb.toml"),
            None => return Self::default(),
        };
        let content = match std::fs::read_to_string(&toml_path) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        toml::from_str(&content).unwrap_or_default()
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
    /// Compact state file path (persists VACUUM counter across invocations).
    pub compact_state: PathBuf,
}

impl Paths {
    /// Walk up from cwd to find the repo root (has a .state/ directory).
    /// Always returns the canonical `.state/agent-kb/agent-kb.db` form, consistent
    /// with `from_root()`. The `agent-kb/` symlink convention at the repo root is
    /// intentionally ignored here -- symlink-fallback removal is a separate follow-up
    /// chore (see plan sec-paths-discover-canonical-form-regression, revision #11).
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
                    compact_state: dir
                        .join(".state")
                        .join("agent-kb")
                        .join("compact-state.json"),
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
            compact_state: root
                .join(".state")
                .join("agent-kb")
                .join("compact-state.json"),
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
        assert_eq!(
            paths.compact_state,
            PathBuf::from("/tmp/test-repo/.state/agent-kb/compact-state.json")
        );
    }

    #[test]
    fn test_vacuum_config_defaults() {
        let vcfg = VacuumConfig::default();
        assert_eq!(vcfg.vacuum_after_compacts, 8);
        assert_eq!(vcfg.vacuum_min_free_pages, 1024);
    }

    /// Regression test (br-bhg): both `Paths::discover` and `Paths::from_root` must
    /// return identical canonical `.state/agent-kb/...` form for the same root input.
    #[test]
    fn test_canonical_form_discover_matches_from_root_without_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let state_db_dir = root.join(".state").join("agent-kb");
        fs::create_dir_all(&state_db_dir).unwrap();
        fs::write(state_db_dir.join("agent-kb.db"), b"").unwrap();

        let _guard = CwdGuard::set(root);
        let discover_paths = Paths::discover().expect("discover should succeed");
        drop(_guard);

        let from_root_paths = Paths::from_root(root);

        assert_eq!(
            discover_paths.db, from_root_paths.db,
            "discover() and from_root() must return the same canonical db path"
        );

        let db_str = discover_paths.db.to_string_lossy();
        assert!(
            db_str.contains("/.state/agent-kb/"),
            "db path must contain '/.state/agent-kb/', got: {db_str}"
        );
        assert!(
            !db_str.contains("/.agent-kb/"),
            "db path must not contain '/.agent-kb/', got: {db_str}"
        );
        let agent_kb_pos = db_str.find("/agent-kb/");
        let state_agent_kb_pos = db_str.find("/.state/agent-kb/");
        assert_eq!(
            agent_kb_pos,
            state_agent_kb_pos.map(|p| p + "/.state".len()),
            "'/agent-kb/' in db path must be preceded by '.state', got: {db_str}"
        );
    }

    #[test]
    fn test_canonical_form_discover_with_symlink_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let state_db_dir = root.join(".state").join("agent-kb");
        fs::create_dir_all(&state_db_dir).unwrap();
        let state_db = state_db_dir.join("agent-kb.db");
        fs::write(&state_db, b"").unwrap();

        let symlink_dir = root.join("agent-kb");
        fs::create_dir_all(&symlink_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&state_db, symlink_dir.join("agent-kb.db")).unwrap();

        let _guard = CwdGuard::set(root);
        let discover_paths = Paths::discover().expect("discover should succeed");
        drop(_guard);

        let from_root_paths = Paths::from_root(root);

        assert_eq!(
            discover_paths.db, from_root_paths.db,
            "discover() and from_root() must return the same canonical db path even when symlink exists"
        );

        let db_str = discover_paths.db.to_string_lossy();
        assert!(
            db_str.contains("/.state/agent-kb/"),
            "db path must contain '/.state/agent-kb/' even with symlink, got: {db_str}"
        );

        let symlink_path = root.join("agent-kb").join("agent-kb.db").to_string_lossy().to_string();
        assert_ne!(
            discover_paths.db.to_string_lossy().as_ref(),
            symlink_path.as_str(),
            "discover() must not return the symlink-based path, got: {db_str}"
        );
    }
}
