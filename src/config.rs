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
#[derive(Clone, Debug, Deserialize, Serialize)]
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
    /// Whether a successful relocation may rewrite `evidence.citation_path`.
    /// Default `false`: `kb stale-check --relocate ...` computes and reports
    /// `RELOCATED` / `UNVERIFIED` findings, but mutates evidence only under
    /// this explicit opt-in (plan P4 "no autonomous mutation of evidence").
    /// A heal writes the path only, never the stored hash, and emits a
    /// `citation_healed` event so it stays auditable and revertible.
    #[serde(default)]
    pub relocation_autoheal: bool,
}

impl Default for KbConfig {
    fn default() -> Self {
        Self {
            embed: EmbedConfig::default(),
            inline_verify_k: default_inline_verify_k(),
            dep_depth: None,
            vacuum: None,
            verify_pool_size: None,
            recency_lambda: 0.0,
            compress_threshold: default_compress_threshold(),
            compress_cosine_cutoff: default_compress_cosine_cutoff(),
            dedup_cosine_cutoff: None,
            mmr_lambda: 0.0,
            relocation_autoheal: false,
        }
    }
}

impl KbConfig {
    /// Returns the effective dep traversal depth (defaults to 1 when unset).
    pub fn dep_depth(&self) -> u8 {
        self.dep_depth.unwrap_or(1)
    }

    /// Effective near-duplicate probe cutoff: unset → 0.85; outside (0.0, 1.0]
    /// → disabled (≤ 0 would flag everything as similar, > 1 can never match).
    /// Defined as an accessor (not a serde default fn) so the value is correct
    /// under BOTH construction paths — `toml::from_str` and `KbConfig::default()`
    /// (derive(Default) ignores serde default fns).
    pub fn dedup_cutoff(&self) -> Option<f32> {
        let c = self.dedup_cosine_cutoff.unwrap_or(0.85);
        (c > 0.0 && c <= 1.0).then_some(c)
    }

    /// Load KbConfig from `kb.toml` relative to the repo root derived from `paths`.
    /// Falls back to `KbConfig::default()` when the file is absent or unparseable.
    ///
    pub fn from_paths(paths: &Paths) -> Self {
        let toml_path = paths.root.join("kb.toml");
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
#[derive(Debug, Clone)]
pub struct Paths {
    /// Repository root against which repository-relative paths are resolved.
    pub root: PathBuf,
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
    /// Best-effort query telemetry database (separate from the rebuildable entries DB).
    pub query_hits: PathBuf,
}

impl Paths {
    /// Walk up from cwd to find a repository database. At each candidate root,
    /// canonical storage wins over legacy storage. With no existing database,
    /// a `.state/` directory remains the marker for canonical first-run setup.
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let (paths, warning) = Self::discover_from(&cwd)?;
        if let Some(warning) = warning {
            eprintln!("{warning}");
        }
        Ok(paths)
    }

    /// Same walk as [`Self::discover`], parameterized on a starting directory
    /// instead of the process cwd, and returning the store-divergence warning
    /// (if any) instead of printing it directly. Split out so tests can assert
    /// on the warning without mutating the process-wide cwd or capturing
    /// stderr.
    pub(crate) fn discover_from(start: &Path) -> Result<(Self, Option<String>)> {
        Self::discover_bounded(start, None)
    }

    /// [`Self::discover_from`] confined to `floor` and its descendants: no
    /// directory outside `floor` is inspected (no `stat`, no `read`), and the
    /// walk stops at the boundary instead of climbing past it. `start` must
    /// already be inside `floor`. The MCP port passes the allowlisted root
    /// here so that authorization precedes every filesystem access (ADR-6).
    pub(crate) fn discover_bounded(
        start: &Path,
        floor: Option<&Path>,
    ) -> Result<(Self, Option<String>)> {
        let mut dir: &Path = start;
        let mut warned_about_inner_state = false;
        loop {
            if floor.is_some_and(|f| !dir.starts_with(f)) {
                bail!(
                    "Could not find repo root (no KB database or .state/ directory in {} up to the allowed boundary {})",
                    start.display(),
                    floor.map(|f| f.display().to_string()).unwrap_or_default()
                );
            }
            let inside_managed = is_inside_managed_state_worktree(dir, floor);
            if inside_managed {
                if dir.join(".state").is_dir() && !warned_about_inner_state {
                    eprintln!(
                        "WARNING: ignored root candidate {} inside a managed .state git worktree; continuing to the outer repo root",
                        dir.display()
                    );
                    warned_about_inner_state = true;
                }
            } else {
                let canonical_db = dir.join(".state/agent-kb/agent-kb.db");
                let legacy_db = dir.join("agent-kb/agent-kb.db");
                // Canonical wins whenever it exists OR its `.state/` marker
                // does (first run, or a legacy db not yet migrated) — checked
                // BEFORE the legacy candidate, so canonical wins at every
                // candidate root as documented in
                // docs/decisions/b3-root-derivation.md.
                let selected_db = if canonical_db.exists() || dir.join(".state").is_dir() {
                    Some(canonical_db)
                } else if legacy_db.exists() {
                    Some(legacy_db)
                } else {
                    None
                };
                if let Some(db) = selected_db {
                    // The store-divergence warning compares the selected root
                    // against its canonical form; that comparison is only
                    // meaningful when canonical was actually selected. A
                    // legacy-only checkout has no canonical db to diverge
                    // from — canonicalizing a nonexistent path always fails,
                    // which would otherwise warn on every legacy-only command.
                    let warning = if is_canonical_db(&db) {
                        divergence_warning(dir, &Paths::from_root(dir))
                    } else {
                        None
                    };
                    let paths = Paths::from_db(&db);
                    return Ok((paths, warning));
                }
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => bail!(
                    "Could not find repo root (no KB database or .state/ directory in {} or any parent)",
                    start.display()
                ),
            }
        }
    }

    /// Build Paths rooted at a specific directory (for testing).
    /// Uses `.state/agent-kb/` consistently (no symlink in test tempdirs).
    pub fn from_root(root: &Path) -> Self {
        Paths {
            root: root.to_path_buf(),
            lock: root.join(".state").join(".lock"),
            events: root
                .join(".state")
                .join("agent-kb")
                .join("agent-kb-events.jsonl"),
            db: root.join(".state").join("agent-kb").join("agent-kb.db"),
            fastembed_cache: model_cache_dir(),
            compact_state: root
                .join(".state")
                .join("agent-kb")
                .join("compact-state.json"),
            query_hits: root.join(".state").join("agent-kb").join("query-hits.db"),
        }
    }

    /// Build paths around an explicitly selected database in either supported layout.
    pub fn from_db(db: &Path) -> Self {
        let root = root_from_db(db);
        let mut paths = Self::from_root(&root);
        paths.db = db.to_path_buf();

        if !is_canonical_db(db) {
            if let Some(db_dir) = db.parent() {
                paths.lock = db_dir.join("agent-kb.lock");
                paths.events = db_dir.join("agent-kb-events.jsonl");
                paths.compact_state = db_dir.join("compact-state.json");
                paths.query_hits = db_dir.join("query-hits.db");
            }
        }
        paths
    }

    /// Resolve an explicitly selected database exactly as the MCP port does.
    ///
    /// Canonical stores use the repository `.state/` layout. For a canonical
    /// database whose event log and lock instead live beside that database,
    /// retain the MCP compatibility fallback so direct rebuild workers operate
    /// on the same store as the long-lived MCP port.
    pub fn from_mcp_db(db: &Path) -> Self {
        let mut paths = Self::from_db(db);

        if let Some(dir) = db.parent() {
            if !paths.events.exists() && dir.join("agent-kb-events.jsonl").exists() {
                paths.events = dir.join("agent-kb-events.jsonl");
                paths.lock = dir.join("agent-kb.lock");
            }
            paths.query_hits = dir.join("query-hits.db");
        }

        paths
    }
}

/// Operator allowlist of directories a long-lived MCP port may serve
/// (ADR-6, `docs/decisions/adr-6-per-request-project-root.md`).
///
/// The port is launched for one store, but it serves sessions working in
/// other checkouts. A request may name its project with `repo_root`; this type
/// decides whether that directory is one the operator agreed to expose and,
/// if so, which store it maps to. Every member is canonical, so membership is
/// a component-wise prefix test on paths that no longer contain `..` or
/// symlinks.
#[derive(Debug, Clone)]
pub struct ProjectRoots {
    allowed: Vec<PathBuf>,
}

/// Why a requested `repo_root` was refused. Refusals happen before any file
/// under the requested directory is opened and before any lock is taken.
#[derive(Debug)]
pub enum ProjectRootError {
    /// Relative paths would be resolved against the server's cwd, which the
    /// caller does not control; only absolute paths are accepted.
    NotAbsolute(String),
    /// The directory does not exist or cannot be canonicalized.
    NotFound(String, std::io::Error),
    /// The canonical directory, or the repository root discovered from it, is
    /// outside every allowlisted root.
    NotAllowlisted(PathBuf),
    /// No `.state/` marker or database between the directory and its
    /// allowlisted ancestor: there is no project store to select.
    NoStore(PathBuf, String),
    /// A store component (`.state`, `agent-kb`, db, lock, event log, …)
    /// resolves outside the selected project root.
    StoreEscapes { root: PathBuf, component: PathBuf },
}

impl std::fmt::Display for ProjectRootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute(p) => write!(f, "repo_root must be an absolute path: {p:?}"),
            Self::NotFound(p, e) => write!(f, "repo_root {p:?} cannot be resolved: {e}"),
            Self::NotAllowlisted(p) => write!(
                f,
                "repo_root {} is outside the project roots this server was started with \
                 (kb mcp --project-root / KB_PROJECT_ROOTS)",
                p.display()
            ),
            Self::NoStore(p, e) => write!(f, "repo_root {} selects no KB store: {e}", p.display()),
            Self::StoreEscapes { root, component } => write!(
                f,
                "store path {} resolves outside the selected project root {}",
                component.display(),
                root.display()
            ),
        }
    }
}

impl std::error::Error for ProjectRootError {}

/// A project a request selected: its canonical root and the store under it.
#[derive(Debug, Clone)]
pub struct SelectedProject {
    pub root: PathBuf,
    pub paths: Paths,
}

/// Environment variable merged into the allowlist: colon-separated directories.
pub const PROJECT_ROOTS_ENV: &str = "KB_PROJECT_ROOTS";

impl ProjectRoots {
    /// Build the allowlist. The server's own root is always a member; every
    /// `extra` entry is canonicalized and entries that do not exist are
    /// dropped with a warning (fail closed: fewer roots, never more).
    pub fn new(server_root: &Path, extra: &[PathBuf]) -> Self {
        let mut allowed = Vec::with_capacity(extra.len() + 1);
        match std::fs::canonicalize(server_root) {
            Ok(root) => allowed.push(root),
            Err(error) => {
                eprintln!(
                    "warn: server root {} cannot be canonicalized ({error}); using it as given",
                    server_root.display()
                );
                allowed.push(server_root.to_path_buf());
            }
        }
        for entry in extra {
            match std::fs::canonicalize(entry) {
                Ok(root) if root.is_dir() => {
                    if !allowed.contains(&root) {
                        allowed.push(root);
                    }
                }
                Ok(_) => eprintln!(
                    "warn: ignoring project root {} (not a directory)",
                    entry.display()
                ),
                Err(error) => {
                    eprintln!("warn: ignoring project root {} ({error})", entry.display())
                }
            }
        }
        Self { allowed }
    }

    /// `new`, plus the directories named by [`PROJECT_ROOTS_ENV`].
    pub fn from_env_and_args(server_root: &Path, args: &[PathBuf]) -> Self {
        let mut extra: Vec<PathBuf> = args.to_vec();
        if let Some(env) = std::env::var_os(PROJECT_ROOTS_ENV) {
            extra.extend(std::env::split_paths(&env).filter(|p| !p.as_os_str().is_empty()));
        }
        Self::new(server_root, &extra)
    }

    /// Canonical allowlisted roots, server root first.
    pub fn allowed(&self) -> &[PathBuf] {
        &self.allowed
    }

    fn contains(&self, canonical: &Path) -> bool {
        self.allowed.iter().any(|root| canonical.starts_with(root))
    }

    /// Map a requested directory to the project store it selects.
    ///
    /// The directory must be absolute, exist, and canonicalize to a path
    /// inside the allowlist; the same discovery walk the CLI uses then finds
    /// the enclosing repository (a subdirectory or a `.state/worktrees/`
    /// checkout selects the enclosing project), and that root must itself be
    /// inside the allowlist. Nothing under the directory is opened until both
    /// checks pass.
    pub fn resolve(
        &self,
        requested: &str,
    ) -> std::result::Result<SelectedProject, ProjectRootError> {
        let requested_path = Path::new(requested);
        if requested.is_empty() || !requested_path.is_absolute() {
            return Err(ProjectRootError::NotAbsolute(requested.to_string()));
        }
        let canonical = std::fs::canonicalize(requested_path)
            .map_err(|e| ProjectRootError::NotFound(requested.to_string(), e))?;
        if !canonical.is_dir() {
            return Err(ProjectRootError::NotFound(
                requested.to_string(),
                std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory"),
            ));
        }
        if !self.contains(&canonical) {
            return Err(ProjectRootError::NotAllowlisted(canonical));
        }
        // Walk only inside the widest allowlisted root that contains the
        // request: ancestors beyond it are never touched (ADR-6 decision 2).
        let floor = self
            .allowed
            .iter()
            .filter(|root| canonical.starts_with(root))
            .min_by_key(|root| root.components().count())
            .cloned();
        let (paths, warning) = Paths::discover_bounded(&canonical, floor.as_deref())
            .map_err(|e| ProjectRootError::NoStore(canonical.clone(), e.to_string()))?;
        if let Some(warning) = warning {
            eprintln!("{warning}");
        }
        let root = std::fs::canonicalize(&paths.root).unwrap_or_else(|_| paths.root.clone());
        if !self.contains(&root) {
            return Err(ProjectRootError::NotAllowlisted(root));
        }
        verify_store_containment(&root, &paths)?;
        Ok(SelectedProject { root, paths })
    }
}

/// Refuse a store whose physical location leaves the selected project root.
///
/// Two directory-level rules, so no list of file names has to track the
/// sidecars the store code creates (`.tmp`, `.generation`, `.deadletter`,
/// rebuild locks, `.bak`, ...):
///
/// 1. each known store path (database, query-hit log, lock, event log,
///    compact state and the SQLite sidecars of the first two) is walked
///    component by component from the canonical root with `lstat` semantics,
///    so every directory on the way is a real directory or an in-root link;
/// 2. every existing entry of each store directory (the parents of those
///    paths) is `lstat`ed, and a symlink entry is refused unless it
///    canonicalizes inside the root. A dangling link, a link leaving the
///    root, or an uninspectable entry is a refusal.
///
/// Symlinks that stay inside the root are tolerated (legacy checkouts alias
/// `agent-kb`). Nothing is created or opened here (directories are listed),
/// so a refusal happens before recovery and before the lock is taken. Temp
/// files are additionally created `O_EXCL` by the writers
/// (`cursor::create_tmp_exclusive`), so a link planted after this check fails
/// instead of being followed.
///
/// This is a check-then-use pathname policy, not an fd-pinned one: a
/// concurrent writer that swaps a component after this returns can defeat it
/// (ADR-6, "Limits").
fn verify_store_containment(
    root: &Path,
    paths: &Paths,
) -> std::result::Result<(), ProjectRootError> {
    let escapes = |component: &Path| ProjectRootError::StoreEscapes {
        root: root.to_path_buf(),
        component: component.to_path_buf(),
    };
    // Rule for one path that is itself a symlink.
    let check_link = |link: &Path| -> std::result::Result<(), ProjectRootError> {
        let target = std::fs::canonicalize(link).map_err(|_| escapes(link))?;
        if target.starts_with(root) {
            Ok(())
        } else {
            Err(escapes(link))
        }
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    for base in [&paths.db, &paths.query_hits] {
        candidates.push(base.clone());
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = base.as_os_str().to_os_string();
            sidecar.push(suffix);
            candidates.push(PathBuf::from(sidecar));
        }
    }
    candidates.extend([
        paths.lock.clone(),
        paths.events.clone(),
        paths.compact_state.clone(),
    ]);
    let mut store_dirs: Vec<PathBuf> = Vec::new();
    for candidate in &candidates {
        let relative = candidate
            .strip_prefix(root)
            .map_err(|_| escapes(candidate))?;
        let mut current = root.to_path_buf();
        for component in relative.components() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => check_link(&current)?,
                Ok(_) => {}
                // Absent: so is everything below it; it will be created
                // under the components verified above.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => return Err(escapes(&current)),
            }
        }
        if let Some(dir) = candidate.parent() {
            if !store_dirs.iter().any(|d| d == dir) {
                store_dirs.push(dir.to_path_buf());
            }
        }
    }
    for dir in &store_dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // Absent store directory: nothing planted in it yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(escapes(dir)),
        };
        for entry in entries {
            let entry = entry.map_err(|_| escapes(dir))?;
            let path = entry.path();
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => check_link(&path)?,
                Ok(_) => {}
                Err(_) => return Err(escapes(&path)),
            }
        }
    }
    Ok(())
}

/// Derive a repository root from a canonical or legacy database path.
pub fn root_from_db(db: &Path) -> PathBuf {
    let Some(db_dir) = db.parent() else {
        return PathBuf::from(".");
    };
    if is_canonical_db(db) {
        db_dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        db_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    }
}

fn is_canonical_db(db: &Path) -> bool {
    let Some(db_dir) = db.parent() else {
        return false;
    };
    db_dir.file_name().is_some_and(|name| name == "agent-kb")
        && db_dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".state")
}

fn is_inside_managed_state_worktree(candidate: &Path, floor: Option<&Path>) -> bool {
    candidate.ancestors().any(|ancestor| {
        // Never inspect an ancestor above the authorization boundary.
        if floor.is_some_and(|f| !ancestor.starts_with(f)) {
            return false;
        }
        let state_dir = ancestor.join(".state");
        if !candidate.starts_with(&state_dir) {
            return false;
        }

        // `lstat` + `O_NOFOLLOW`: a symlinked `.git` is never a gitlink and
        // its target is never opened, whatever it points at.
        let gitlink = state_dir.join(".git");
        std::fs::symlink_metadata(&gitlink)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
            && read_gitlink_prefix_nofollow(&gitlink)
    })
}

fn read_gitlink_prefix_nofollow(path: &Path) -> bool {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return false;
    };
    let mut head = [0u8; 7];
    file.take(7).read_exact(&mut head).is_ok() && &head == b"gitdir:"
}

/// Report stores at known non-canonical sibling paths without selecting or modifying them.
fn divergence_warning(root: &Path, canonical: &Paths) -> Option<String> {
    let candidates = [
        (
            root.join("agent-kb").join("agent-kb.db"),
            canonical.db.as_path(),
        ),
        (
            root.join(".state")
                .join(".state")
                .join("agent-kb")
                .join("agent-kb-events.jsonl"),
            canonical.events.as_path(),
        ),
    ];
    let divergences: Vec<String> = candidates
        .into_iter()
        .filter(|(noncanonical, canonical)| {
            std::fs::symlink_metadata(noncanonical).is_ok()
                && match (
                    std::fs::canonicalize(noncanonical),
                    std::fs::canonicalize(canonical),
                ) {
                    (Ok(noncanonical), Ok(canonical)) => noncanonical != canonical,
                    _ => true,
                }
        })
        .map(|(noncanonical, canonical)| {
            format!(
                "non-canonical {} diverges from canonical {}",
                noncanonical.display(),
                canonical.display()
            )
        })
        .collect();

    (!divergences.is_empty()).then(|| {
        format!(
            "WARNING: KB STORE DIVERGENCE: {}; reconcile per bd-xw99",
            divergences.join("; ")
        )
    })
}

/// Get the git HEAD SHA.
pub fn git_head_sha() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| git_head_sha_at(&cwd))
}

/// Get the git HEAD SHA for a specific working directory.
pub fn git_head_sha_at(dir: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
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
    use proptest::prelude::*;
    use std::fs;
    use std::path::PathBuf;

    /// RAII guard that restores cwd on drop (same pattern as add.rs tests).
    ///
    /// Safety: `std::env::set_current_dir` mutates process-wide state, so this
    /// is only sound when each test runs in its own process — true under
    /// `cargo nextest run` (the project's test runner), but NOT under plain
    /// `cargo test`, which runs tests as threads within one process and could
    /// race two `CwdGuard`-using tests against each other. Prefer
    /// `Paths::discover_from(&explicit_dir)` over a `CwdGuard` in new tests
    /// where possible — it takes the starting directory as an argument and
    /// never touches process cwd at all.
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
        assert_eq!(paths.root, root);
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
    fn test_layout_candidate_precedence_and_root_match_elixir_rule() {
        for (canonical, legacy, expected_legacy) in [
            (true, false, false),
            (false, true, true),
            (true, true, false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            let canonical_db = root.join(".state/agent-kb/agent-kb.db");
            let legacy_db = root.join("agent-kb/agent-kb.db");
            if canonical {
                fs::create_dir_all(canonical_db.parent().unwrap()).unwrap();
                fs::write(&canonical_db, b"").unwrap();
            }
            if legacy {
                fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
                fs::write(&legacy_db, b"").unwrap();
            }

            let _guard = CwdGuard::set(root);
            let paths = Paths::discover().unwrap();
            drop(_guard);
            let expected_db = if expected_legacy {
                &legacy_db
            } else {
                &canonical_db
            };
            assert_eq!(&paths.db, expected_db);
            assert_eq!(paths.root, root);
            assert_eq!(root_from_db(expected_db), root);
        }
    }

    /// br-<mid-migration>: a candidate root with a `.state/` marker but no
    /// canonical db file yet (first run, or migration in progress) must still
    /// select the canonical path over a legacy db that already exists at the
    /// same root — matching the decision note's "canonical wins at every
    /// candidate root", including before the canonical db's first write.
    #[test]
    fn test_discover_prefers_canonical_state_marker_over_legacy_db_mid_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // `.state/` exists (the canonical marker) but nothing has been written
        // to the canonical db yet.
        fs::create_dir_all(root.join(".state")).unwrap();
        // A legacy db already exists at this same root (not yet migrated).
        let legacy_db = root.join("agent-kb/agent-kb.db");
        fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
        fs::write(&legacy_db, b"legacy").unwrap();

        let _guard = CwdGuard::set(root);
        let discovered = Paths::discover().expect("discover should succeed");
        drop(_guard);

        assert_eq!(
            discovered.db,
            root.join(".state/agent-kb/agent-kb.db"),
            "a `.state/` marker must select the canonical path even when a \
             legacy db exists and the canonical db has not been written yet"
        );
    }

    /// br-<nested-checkout-parity>: Rust and Elixir discovery must agree on
    /// where a nested checkout's own `.state/` marker stops the upward walk,
    /// even when it has no db file yet. Otherwise the MCP port (Elixir) keeps
    /// walking to an outer ancestor's real db while the CLI (Rust) stops at
    /// the nested marker and would create a brand new store there — silently
    /// splitting one logical repo into two stores. See the matching Elixir
    /// test in mcp/test/agentic_kb_mcp_test.exs.
    #[test]
    fn test_discover_nested_state_marker_stops_before_outer_canonical_db() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        let outer_db = outer.join(".state/agent-kb/agent-kb.db");
        fs::create_dir_all(outer_db.parent().unwrap()).unwrap();
        fs::write(&outer_db, b"outer").unwrap();

        // A nested directory has its own bare `.state/` marker (e.g. an
        // in-progress `kb init`) but no db file of its own yet.
        let nested = outer.join("nested");
        fs::create_dir_all(nested.join(".state")).unwrap();

        let _guard = CwdGuard::set(&nested);
        let discovered = Paths::discover().expect("discover should succeed");
        drop(_guard);

        assert_eq!(
            discovered.db,
            nested.join(".state/agent-kb/agent-kb.db"),
            "discovery must stop at the nested `.state/` marker, not continue \
             up to the outer repo's real db"
        );
    }

    /// br-<divergence-warning-scope>: the store-divergence warning fires only
    /// when the *selected* candidate is canonical. A legacy-only checkout has
    /// no canonical db to diverge from (canonicalize() of a nonexistent path
    /// always fails), so it must stay silent.
    #[test]
    fn test_discover_from_is_silent_on_a_legacy_only_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let legacy_db = root.join("agent-kb/agent-kb.db");
        fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
        fs::write(&legacy_db, b"legacy").unwrap();

        let (paths, warning) = Paths::discover_from(root).expect("discover should succeed");
        assert_eq!(paths.db, legacy_db);
        assert!(
            warning.is_none(),
            "a legacy-only checkout must not warn about store divergence, got: {warning:?}"
        );
    }

    /// The converse of the above: when both a canonical db and a diverging
    /// legacy db exist at the same root, canonical is selected (per the
    /// candidate-precedence tests above) and the warning must still fire.
    #[test]
    fn test_discover_from_still_warns_when_both_candidates_present_and_diverge() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let canonical_db = root.join(".state/agent-kb/agent-kb.db");
        fs::create_dir_all(canonical_db.parent().unwrap()).unwrap();
        fs::write(&canonical_db, b"canonical").unwrap();
        let legacy_db = root.join("agent-kb/agent-kb.db");
        fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
        fs::write(&legacy_db, b"distinct legacy").unwrap();

        let (paths, warning) = Paths::discover_from(root).expect("discover should succeed");
        assert_eq!(paths.db, canonical_db);
        assert!(
            warning.is_some(),
            "a diverging legacy db next to a selected canonical db must still warn"
        );
    }

    proptest! {
        #[test]
        fn root_from_db_is_stable_across_supported_layout_reconstruction(
            components in proptest::collection::vec("[a-z][a-z0-9]{0,7}", 1..6)
        ) {
            let root = components.iter().fold(PathBuf::from("/"), |path, component| path.join(component));
            let canonical = root.join(".state/agent-kb/agent-kb.db");
            let legacy = root.join("agent-kb/agent-kb.db");
            proptest::prop_assert_eq!(root_from_db(&canonical), root.clone());
            proptest::prop_assert_eq!(root_from_db(&legacy), root.clone());
        }

        #[test]
        fn root_from_db_handles_the_filesystem_root_boundary_and_relative_paths(
            components in proptest::collection::vec("[a-z][a-z0-9]{0,7}", 0..6)
        ) {
            // Root-level checkout: the repository root itself is "/", i.e. the
            // last parent-walk step in either layout runs out of directory
            // components. Decision: root_from_db resolves this to "/", not "."
            // — "." would misleadingly imply an unresolved/relative path.
            proptest::prop_assert_eq!(
                root_from_db(Path::new("/.state/agent-kb/agent-kb.db")),
                PathBuf::from("/")
            );
            proptest::prop_assert_eq!(
                root_from_db(Path::new("/agent-kb/agent-kb.db")),
                PathBuf::from("/")
            );

            // A relative (non-absolute) db path must still resolve using the
            // same parent-walking rule, anchored at "." instead of "/".
            let rel_root = components
                .iter()
                .fold(PathBuf::from("."), |path, component| path.join(component));
            let rel_canonical = rel_root.join(".state/agent-kb/agent-kb.db");
            let rel_legacy = rel_root.join("agent-kb/agent-kb.db");
            proptest::prop_assert_eq!(root_from_db(&rel_canonical), rel_root.clone());
            proptest::prop_assert_eq!(root_from_db(&rel_legacy), rel_root);
        }
    }

    /// P4: relocation never mutates evidence unless the operator opts in, and
    /// the opt-in must survive both construction paths (`Default` and TOML).
    #[test]
    fn test_relocation_autoheal_defaults_off() {
        assert!(!KbConfig::default().relocation_autoheal);
        let parsed: KbConfig = toml::from_str("inline_verify_k = 3\n").unwrap();
        assert!(!parsed.relocation_autoheal);
        let opted_in: KbConfig = toml::from_str("relocation_autoheal = true\n").unwrap();
        assert!(opted_in.relocation_autoheal);
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
    fn test_discover_inside_managed_state_worktree_returns_outer_repo_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let inner = root.join(".state").join("somewhere");
        fs::create_dir_all(inner.join(".state")).unwrap();
        fs::create_dir_all(root.join(".git/worktrees")).unwrap();
        fs::write(
            root.join(".state/.git"),
            format!("gitdir: {}\n", root.join(".git/worktrees/state").display()),
        )
        .unwrap();

        let _guard = CwdGuard::set(&inner);
        let discovered = Paths::discover().expect("discover should find the outer repo root");
        drop(_guard);

        assert_eq!(discovered.db, root.join(".state/agent-kb/agent-kb.db"));
    }

    #[test]
    fn test_discover_standalone_repo_under_plain_state_named_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let container = tmp.path().join("x");
        let project = container.join(".state/project");
        fs::create_dir_all(project.join(".state/agent-kb")).unwrap();

        let _guard = CwdGuard::set(&project);
        let discovered = Paths::discover().expect("discover should find the standalone repo");
        drop(_guard);

        assert_eq!(discovered.db, project.join(".state/agent-kb/agent-kb.db"));
    }

    #[test]
    fn test_discover_never_constructs_doubled_state_events_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let inner = root.join(".state").join("nested");
        fs::create_dir_all(inner.join(".state")).unwrap();
        fs::write(root.join(".state/.git"), b"gitdir: /external/gitdir\n").unwrap();

        let _guard = CwdGuard::set(&inner);
        let discovered = Paths::discover().expect("discover should find the outer repo root");
        drop(_guard);

        assert!(
            !discovered
                .events
                .components()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|parts| parts[0].as_os_str() == ".state" && parts[1].as_os_str() == ".state"),
            "events path must not contain adjacent .state components: {}",
            discovered.events.display()
        );
    }

    #[test]
    fn test_divergence_warning_detects_bare_legacy_db() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let paths = Paths::from_root(root);
        let legacy_db = root.join("agent-kb/agent-kb.db");
        fs::create_dir_all(paths.db.parent().unwrap()).unwrap();
        fs::write(&paths.db, b"canonical").unwrap();
        fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
        fs::write(&legacy_db, b"distinct legacy").unwrap();

        let warning = divergence_warning(root, &paths).expect("legacy DB must trigger warning");
        assert!(warning.contains(&paths.db.display().to_string()));
        assert!(warning.contains(&legacy_db.display().to_string()));
        assert!(warning.contains("bd-xw99"));
    }

    #[cfg(unix)]
    #[test]
    fn test_divergence_warning_ignores_symlink_to_canonical_db() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let paths = Paths::from_root(root);
        fs::create_dir_all(paths.db.parent().unwrap()).unwrap();
        fs::write(&paths.db, b"").unwrap();
        let legacy_db = root.join("agent-kb/agent-kb.db");
        fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&paths.db, &legacy_db).unwrap();

        assert!(
            divergence_warning(root, &paths).is_none(),
            "a legacy symlink resolving to the canonical DB must not warn"
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

        let symlink_path = root
            .join("agent-kb")
            .join("agent-kb.db")
            .to_string_lossy()
            .to_string();
        assert_ne!(
            discover_paths.db.to_string_lossy().as_ref(),
            symlink_path.as_str(),
            "discover() must not return the symlink-based path, got: {db_str}"
        );
    }
}
