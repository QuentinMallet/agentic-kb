//! `add` subcommand
//!
//! This command constructs validated inputs and delegates all event-writing and
//! DB-apply work to `kb_core::add`.  It does NOT contain any direct calls to
//! `events::append_event` / `events::append_events_batch` / `db::apply_event`
//! inside `execute_with` — those are the sole responsibility of `kb_core::add`.
//!
//! The `"session"` field written to events is `$OMC_SESSION_ID` when set, else `"cli"`.
//! The `"session_id"` field is `$OMC_SESSION_ID` when set, else absent (NULL in DB).
//! The `expire_reason` is `"replaced by --replace-path"`.

use crate::commands::add_validation::{
    compute_evidence_status_write, validate_kb_add_inputs, warn_nested_worktree_citations,
};
use crate::components::embedder;
use crate::components::kb_core;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use serde_json::Value;

/// Add or update a knowledge entry
#[derive(Command, Debug, Parser)]
pub struct Add {
    /// Category/file path
    #[arg(long)]
    pub path: String,
    /// Short summary
    #[arg(long)]
    pub summary: String,
    /// Full content
    #[arg(long, allow_hyphen_values = true)]
    pub content: String,
    /// Comma-separated tags
    #[arg(long)]
    pub tags: String,
    /// Git commit SHA (auto-populated from HEAD if omitted)
    #[arg(long)]
    pub version_ref: Option<String>,
    /// Entry ID (auto-generated UUID if omitted)
    #[arg(long)]
    pub id: Option<String>,
    /// Mark entry as permanent (survives compact and resists expire)
    #[arg(long, default_value_t = false)]
    pub permanent: bool,
    /// Expire all existing non-stale entries at this path before inserting.
    /// Useful for idempotent re-ingestion (e.g. kb-ingest chunk updates).
    #[arg(long, default_value_t = false)]
    pub replace_path: bool,
    /// Entry kind: observation | belief | procedure | convention | memory
    #[arg(long, default_value = "belief")]
    pub kind: String,
    /// Evidence row as a JSON object (repeatable; mutually exclusive with --evidence-file)
    #[arg(long, conflicts_with = "evidence_file")]
    pub evidence: Vec<String>,
    /// Path to a JSON file containing an array of evidence objects (mutually exclusive with --evidence)
    #[arg(long, conflicts_with = "evidence")]
    pub evidence_file: Option<String>,
    /// Cue anchor "[Main Entity] + [Key Aspect]" (repeatable, max 8).
    /// Semantic entry points embedded separately from the entry.
    #[arg(long = "cue")]
    pub cues: Vec<String>,
}

impl Runnable for Add {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Add {
    /// Execute the add command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let embedder = make_embedder(&paths);
        crate::commands::rebuild::recover_if_needed(&paths, embedder.as_ref())?;
        self.execute_with(&paths, embedder.as_ref())
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        // Parse evidence rows from --evidence flags or --evidence-file.
        let evidence_rows: Vec<Value> = if let Some(ref file_path) = self.evidence_file {
            let raw = std::fs::read_to_string(file_path)
                .map_err(|e| anyhow::anyhow!("read --evidence-file '{file_path}': {e}"))?;
            serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("parse --evidence-file '{file_path}': {e}"))?
        } else {
            self.evidence
                .iter()
                .map(|s| {
                    serde_json::from_str(s)
                        .map_err(|e| anyhow::anyhow!("parse --evidence JSON: {e}"))
                })
                .collect::<anyhow::Result<Vec<Value>>>()?
        };

        // Build tags JSON before validation so validate_kb_add_inputs can check shape.
        let tags_json: Value =
            serde_json::json!(self.tags.split(',').map(|t| t.trim()).collect::<Vec<_>>());

        // Compute id early so the self-loop provenance check in validate_kb_add_inputs
        // can compare evidence.derived_from against the entry's own id.
        let id = self
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Validate kind, tags, and evidence before acquiring the lock.
        validate_kb_add_inputs(&id, &self.kind, &tags_json, &evidence_rows)?;
        warn_nested_worktree_citations(&evidence_rows);

        let evidence_status = compute_evidence_status_write(&self.kind, &evidence_rows);
        let version_ref = self.version_ref.clone().or_else(config::git_head_sha);
        let ts = chrono::Utc::now().to_rfc3339();
        // Read OMC_SESSION_ID once: used for both the audit "session" label and
        // the per-entry session_id column (Phase-5 per-session confidence weighting).
        let (session, omc_session_id) = read_omc_session();

        // Delegate to kb_core::add — all event-writing and DB-apply logic lives there.
        let outcome = kb_core::add(
            paths,
            embedder,
            kb_core::AddArgs {
                id: id.clone(),
                path: self.path.clone(),
                summary: self.summary.clone(),
                content: self.content.clone(),
                tags: tags_json,
                version_ref,
                permanent: self.permanent,
                replace_path: self.replace_path,
                kind: self.kind.clone(),
                evidence_status: evidence_status.to_string(),
                evidence_rows,
                ts,
                session,
                session_id: omc_session_id,
                expire_reason: "replaced by --replace-path".to_string(),
                dedup_cutoff: config::KbConfig::from_paths(paths).dedup_cutoff(),
                cues: self.cues.clone(),
            },
        )?;

        println!("added  {} ({})", self.path, outcome.entry_id);
        for s in &outcome.similar_existing {
            eprintln!(
                "warn: similar existing entry (cosine {:.2}): [{}] {} (id={})",
                s.score, s.path, s.summary, s.id
            );
        }
        Ok(())
    }
}

/// Read `OMC_SESSION_ID` and derive the `(session, session_id)` pair used by
/// every CLI subcommand that emits an event.
///
/// Returns `(session, session_id)` where:
/// - `session_id` is `Some(value)` when the env var is set and non-empty, else `None`.
/// - `session` mirrors `session_id` when present, else falls back to the literal `"cli"`.
///
/// Centralising the read avoids the divergent `unwrap_or_else` / `ok().filter()`
/// patterns that previously appeared in `add.rs`, `expire.rs`, `run.rs`, and `test_add.rs`.
pub fn read_omc_session() -> (String, Option<String>) {
    let session_id = std::env::var("OMC_SESSION_ID")
        .ok()
        .filter(|v| !v.is_empty());
    let session = session_id.clone().unwrap_or_else(|| "cli".to_string());
    (session, session_id)
}

/// Build the appropriate embedder based on KB_NO_EMBED env var.
///
/// Prefer `make_embedder_with_opts` at call sites that control `no_embed` explicitly
/// (e.g. `ingest`, `import`). This wrapper exists for callers that rely on the env var
/// (e.g. the top-level `add` command invoked via the shell).
pub fn make_embedder(paths: &config::Paths) -> Box<dyn embedder::Embedder> {
    make_embedder_with_opts(paths, std::env::var("KB_NO_EMBED").is_ok())
}

/// Build an embedder with an explicit no-embed flag.
///
/// Pass `no_embed=true` to get a `NoopEmbedder` without touching `std::env`.
/// This avoids `env::set_var` (unsafe in multi-threaded contexts, forbidden by
/// Rust 2024 Edition) and eliminates cross-talk between parallel test threads.
///
/// Constraint: must not break the MCP `make_embedder` path — `make_embedder`
///             remains as a backwards-compatible wrapper that reads the env var.
/// Directive: never reintroduce `env::set_var("KB_NO_EMBED", …)` to flip embedder
///            behaviour; pass `no_embed` explicitly via this function instead.
pub fn make_embedder_with_opts(
    paths: &config::Paths,
    no_embed: bool,
) -> Box<dyn embedder::Embedder> {
    if no_embed {
        Box::new(embedder::NoopEmbedder)
    } else {
        Box::new(embedder::CandleEmbedder::new(&paths.fastembed_cache))
    }
}

/// Lock files each thread currently holds, keyed on `(ThreadId, CANONICAL path)`
/// and valued by the source location that acquired the lock.
///
/// `fs2::lock_exclusive` is `flock(2)`, associated with the open file
/// description: a thread that acquires a lock it already holds opens a second
/// description and blocks on itself forever. The type system cannot see that —
/// `&Lock` proves a live guard exists at a mutating open, not that the same
/// call chain did not take the lock twice — so the registry converts that
/// self-deadlock into an immediate error naming the first acquisition site.
///
/// **Scoped to the acquiring thread, not the process.** A *different* thread
/// waiting on the same flock is ordinary mutual exclusion, not a deadlock, and
/// the codebase depends on it: `rebuild`'s schema-upgrade single-flight and its
/// Phase 2 concurrent-writer guarantee are both exercised by in-process threads
/// that must serialize on the flock rather than fail. Every self-deadlock ADR-1
/// names — `rebuild.rs`'s documented case, `handle_import` under `L2` — is one
/// thread re-entering its own lock.
///
/// Keying on the canonical path is load-bearing: two spellings of one lock file
/// (a relative path, a `..` component, a symlinked repo root) must collapse to
/// one entry or a re-entrant acquire slips through under an alias and hangs.
/// See `.state/agent-kb/tla/decisions/lock-contract-no-spec.md`, re-entrancy row.
type LockRegistryKey = (std::thread::ThreadId, std::path::PathBuf);

static HELD_LOCKS: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<LockRegistryKey, String>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn held_locks() -> std::sync::MutexGuard<'static, std::collections::HashMap<LockRegistryKey, String>>
{
    // A panic while the registry is held would otherwise poison every later
    // acquire; the map itself is always left consistent, so recover in place.
    HELD_LOCKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Acquire the agentic lock.
///
/// Blocking and exclusive, released when the returned [`Lock`] is dropped.
/// A second acquire of the same file *on the same thread* is rejected rather
/// than deadlocked — callers that already hold the lock must pass the guard
/// down (see [`crate::components::kb_core::add_locked`]). Other threads still
/// block on the flock, which is real mutual exclusion.
#[track_caller]
pub fn acquire_lock(lock_path: &std::path::Path) -> anyhow::Result<Lock> {
    use anyhow::Context;
    use fs2::FileExt;
    use std::fs::{self, OpenOptions};

    let caller = std::panic::Location::caller();
    let site = format!("{}:{}", caller.file(), caller.line());

    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    // Canonicalize only after the file exists, so a first-ever acquire resolves.
    let canonical = fs::canonicalize(lock_path)
        .with_context(|| format!("canonicalize lock {}", lock_path.display()))?;

    let owner = std::thread::current().id();
    let key: LockRegistryKey = (owner, canonical.clone());
    {
        let mut held = held_locks();
        if let Some(first) = held.get(&key) {
            anyhow::bail!(
                "re-entrant acquire of {}: this thread already holds it (acquired at {first}). \
                 Pass the existing Lock down instead of re-acquiring — e.g. kb_core::add_locked \
                 or db::open_rw(&paths, &lock).",
                canonical.display()
            );
        }
        held.insert(key.clone(), site);
    }

    if let Err(e) = f.lock_exclusive() {
        held_locks().remove(&key);
        return Err(anyhow::Error::new(e).context(format!("acquire lock {}", lock_path.display())));
    }
    crate::components::events::note_log_lock_acquired();
    Ok(Lock {
        file: f,
        path: canonical,
        owner,
    })
}

/// RAII lock guard — holds the file lock until dropped.
///
/// Carries the canonicalized path of the file it locks so a mutating open can
/// assert it was handed the *right* lock, not merely *a* lock (ADR-1).
/// Construction and drop are the only places the event-log lock depth moves, so
/// the append path's destructive span repair can assert the flock structurally
/// instead of documenting it.
pub struct Lock {
    #[allow(dead_code)]
    file: std::fs::File,
    path: std::path::PathBuf,
    /// Thread that acquired it. Recorded so the registry entry is cleared
    /// under its owner's key even when the guard is dropped on another thread.
    owner: std::thread::ThreadId,
}

impl Lock {
    /// Canonicalized path of the locked file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        held_locks().remove(&(self.owner, self.path.clone()));
        crate::components::events::note_log_lock_released();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{db, embedder::NoopEmbedder, events};
    use crate::config::Paths;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command as Cmd;
    use tempfile::tempdir;

    /// RAII guard that restores cwd on drop.
    struct CwdGuard(PathBuf);
    impl CwdGuard {
        fn set(dir: &std::path::Path) -> Self {
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

    fn setup_test_repo() -> (tempfile::TempDir, Paths) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        Cmd::new("git")
            .args(["init", "-b", "master"])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.email", "test@test"])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::write(root.join("README"), "init").unwrap();
        Cmd::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();

        let paths = Paths::from_root(root);
        (dir, paths)
    }

    fn make_add(path: &str, id: &str) -> Add {
        Add {
            path: path.to_string(),
            summary: "test entry".to_string(),
            content: "content".to_string(),
            tags: "rust,test".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some(id.to_string()),
            permanent: false,
            replace_path: false,
            kind: "belief".to_string(),
            evidence: vec![r#"{"kind":"code","citation_hash":"sha256:abc123"}"#.to_string()],
            evidence_file: None,
            cues: vec![],
        }
    }

    #[test]
    fn test_cmd_add_writes_event_and_db_row() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let cmd = make_add("src/lib.rs", "test-id-1");
        cmd.execute_with(&paths, &embedder).unwrap();

        // Verify JSONL event was written
        let events_content = fs::read_to_string(&paths.events).unwrap();
        assert!(events_content.contains("test-id-1"));

        // Verify DB row
        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let (path, summary): (String, String) = conn
            .query_row(
                "SELECT path, summary FROM entries WHERE id='test-id-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path, "src/lib.rs");
        assert_eq!(summary, "test entry");
    }

    #[test]
    fn test_cmd_add_permanent_flag_stored_in_db() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // permanent=true
        let cmd = Add {
            path: "skills/new-module".to_string(),
            summary: "how to add a NixOS module".to_string(),
            content: "content here".to_string(),
            tags: "nixos,skill".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("perm-test-1".to_string()),
            permanent: true,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let permanent: i64 = conn
            .query_row(
                "SELECT permanent FROM entries WHERE id='perm-test-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(permanent, 1);

        // permanent=false (default)
        let cmd2 = Add {
            path: "skills/other".to_string(),
            summary: "non-permanent".to_string(),
            content: "content".to_string(),
            tags: "test".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("perm-test-2".to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        cmd2.execute_with(&paths, &embedder).unwrap();

        let permanent2: i64 = conn
            .query_row(
                "SELECT permanent FROM entries WHERE id='perm-test-2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(permanent2, 0);

        // JSONL event includes permanent field
        let events_content = fs::read_to_string(&paths.events).unwrap();
        assert!(events_content.contains("\"permanent\":true"));
        assert!(events_content.contains("\"permanent\":false"));
    }

    #[test]
    fn test_cmd_add_old_event_without_permanent_replays() {
        // Old JSONL events without the `permanent` field should deserialize with permanent=false
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Write a raw event without `permanent` field (simulates pre-permanent JSONL)
        let old_event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "old-event-1",
            "path": "src/old.rs",
            "summary": "old entry",
            "content": "old content",
            "tags": ["old"],
            "ts": "2024-01-01T00:00:00Z"
        });
        events::append_event(&paths.events, &old_event).unwrap();

        let (_paths, conn) = db::test_db(root);
        db::apply_event(&conn, &embedder, &old_event).unwrap();

        let permanent: i64 = conn
            .query_row(
                "SELECT permanent FROM entries WHERE id='old-event-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            permanent, 0,
            "old events without permanent field must default to 0"
        );
    }

    #[test]
    fn test_cmd_add_replace_path_expires_old_entries() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Add initial entry at path
        let cmd1 = Add {
            path: "docs/guide.md".to_string(),
            summary: "original guide".to_string(),
            content: "original content".to_string(),
            tags: "docs".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("rp-old".to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        cmd1.execute_with(&paths, &embedder).unwrap();

        // Re-add same path with --replace-path
        let cmd2 = Add {
            path: "docs/guide.md".to_string(),
            summary: "updated guide".to_string(),
            content: "updated content".to_string(),
            tags: "docs".to_string(),
            version_ref: Some("def".to_string()),
            id: Some("rp-new".to_string()),
            permanent: false,
            replace_path: true,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        cmd2.execute_with(&paths, &embedder).unwrap();

        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let old_stale: i64 = conn
            .query_row("SELECT is_stale FROM entries WHERE id='rp-old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(old_stale, 1, "old entry must be stale after --replace-path");
        let new_stale: i64 = conn
            .query_row("SELECT is_stale FROM entries WHERE id='rp-new'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(new_stale, 0, "new entry must be active");
    }

    #[test]
    fn test_cmd_add_auto_populates_version_ref() {
        let (dir, paths) = setup_test_repo();
        let embedder = NoopEmbedder;

        let expected_sha = String::from_utf8(
            Cmd::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let _guard = CwdGuard::set(dir.path());

        let cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test entry".to_string(),
            content: "content".to_string(),
            tags: "test".to_string(),
            version_ref: None,
            id: None,
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let version_ref: Option<String> = conn
            .query_row(
                "SELECT version_ref FROM entries WHERE path = 'src/lib.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version_ref, Some(expected_sha));
    }

    // ---- New tests for L-write lane ----

    #[test]
    fn test_kb_add_default_kind_is_belief() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let cmd = make_add("src/lib.rs", "kind-default-1");
        cmd.execute_with(&paths, &embedder).unwrap();

        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let kind: String = conn
            .query_row(
                "SELECT kind FROM entries WHERE id='kind-default-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "belief");
    }

    #[test]
    fn test_kb_add_rejects_invalid_kind() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test".to_string(),
            content: "content".to_string(),
            tags: "test".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("bad-kind-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "fact".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        let err = cmd.execute_with(&paths, &embedder).unwrap_err();
        assert!(err.to_string().contains("invalid kind 'fact'"));
    }

    #[test]
    fn test_kb_add_rejects_non_code_evidence() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test".to_string(),
            content: "content".to_string(),
            tags: "test".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("bad-ev-kind-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "observation".to_string(),
            evidence: vec![r#"{"kind":"test","citation_hash":"sha256:abc"}"#.to_string()],
            evidence_file: None,
            cues: vec![],
        };
        let err = cmd.execute_with(&paths, &embedder).unwrap_err();
        assert!(err
            .to_string()
            .contains("Phase 1 ships evidence.kind=code|derived only"));
    }

    #[test]
    fn test_kb_add_soft_mandate_stores_evidence_less_entries_with_derived_status() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // bd-r05y.3: mandated kinds are accepted with "missing"; the other
        // kinds retain their "n/a" status when no evidence is supplied.
        for (kind, expected_status) in [
            ("observation", "missing"),
            ("belief", "missing"),
            ("procedure", "missing"),
            ("convention", "n/a"),
            ("memory", "n/a"),
        ] {
            let id = format!("soft-mandate-{kind}");
            let cmd = Add {
                path: format!("test/{kind}"),
                summary: "test".to_string(),
                content: "content".to_string(),
                tags: "test".to_string(),
                version_ref: Some("abc".to_string()),
                id: Some(id.clone()),
                permanent: false,
                replace_path: false,
                kind: kind.to_string(),
                evidence: vec![],
                evidence_file: None,
                cues: vec![],
            };
            cmd.execute_with(&paths, &embedder).unwrap();

            let conn = crate::components::db::open_unchecked_for_test(&paths.db).unwrap();
            let stored_status: String = conn
                .query_row(
                    "SELECT evidence_status FROM entries WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stored_status, expected_status, "kind={kind}");
        }
    }

    #[test]
    fn test_kb_add_with_evidence_writes_atomic_batch() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        // add_locked resolves + re-verifies citation_path against a real repo
        // file under the flock, so the cited files must actually exist.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/foo.rs"), b"fn foo() {}\nfn foo2() {}\n").unwrap();
        fs::write(root.join("src/bar.rs"), b"fn bar() {}\nfn bar2() {}\n").unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test".to_string(),
            content: "content".to_string(),
            tags: "test".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("batch-ev-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "observation".to_string(),
            evidence: vec![
                r#"{"kind":"code","citation_path":"src/foo.rs:1-10","citation_excerpt":"fn foo() {}"}"#.to_string(),
                r#"{"kind":"code","citation_path":"src/bar.rs:5-15","citation_excerpt":"fn bar() {}"}"#.to_string(),
            ],
            evidence_file: None,
            cues: vec![],
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        // Verify events.jsonl has Add followed by 2 EvidenceAdd events
        let lines = events::read_events(&paths.events).unwrap().events;
        // Should have 3 events: 1 upsert + 2 evidence_add (markers are not events)
        assert_eq!(lines.len(), 3, "expected 3 events (1 add + 2 evidence_add)");

        let ev0 = &lines[0];
        assert_eq!(ev0["action"], "upsert");
        assert_eq!(ev0["table"], "entries");
        assert_eq!(ev0["id"], "batch-ev-1");
        assert_eq!(ev0["kind"], "observation");
        assert_eq!(ev0["evidence_status"], "present");

        let ev1 = &lines[1];
        assert_eq!(ev1["action"], "evidence_add");
        assert_eq!(ev1["entry_id"], "batch-ev-1");

        let ev2 = &lines[2];
        assert_eq!(ev2["action"], "evidence_add");
        assert_eq!(ev2["entry_id"], "batch-ev-1");

        // Verify evidence rows in DB
        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let ev_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM evidence WHERE entry_id='batch-ev-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ev_count, 2);

        let evidence_status: String = conn
            .query_row(
                "SELECT evidence_status FROM entries WHERE id='batch-ev-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(evidence_status, "present");
    }

    // ── TDD: session_id propagation (Lane A, A2) ────────────────────────────
    //
    // FAILING TEST (pre-implementation):
    // With OMC_SESSION_ID set, `kb add` must write that value into
    // entries.session_id.  Before the fix, add.rs passes `session_id: None`
    // to kb_core::AddArgs, so the column stays NULL.
    //
    // Confirmed failing on HEAD: `session_id: None` is hard-coded in
    // Add::execute_with.

    #[test]
    fn test_cli_add_propagates_session_id_to_db() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Set OMC_SESSION_ID in the environment for the duration of this test.
        std::env::set_var("OMC_SESSION_ID", "test123");

        let cmd = Add {
            path: "src/session_test.rs".to_string(),
            summary: "session propagation test".to_string(),
            content: "content".to_string(),
            tags: "test,session".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("sess-prop-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        // Clean up env var.
        std::env::remove_var("OMC_SESSION_ID");

        // AC2: entries.session_id must be "test123" (NOT NULL).
        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let session_id: Option<String> = conn
            .query_row(
                "SELECT session_id FROM entries WHERE id='sess-prop-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            session_id,
            Some("test123".to_string()),
            "entries.session_id must be populated from OMC_SESSION_ID"
        );

        // AC1: the upsert event in the JSONL must also carry session_id.
        // Read through the span-aware reader: commit markers are not events.
        let ev = events::read_events(&paths.events).unwrap().events.remove(0);
        assert_eq!(
            ev["session_id"],
            serde_json::json!("test123"),
            "upsert event must carry session_id field"
        );
    }
}
