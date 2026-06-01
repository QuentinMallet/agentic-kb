//! `add` subcommand

use crate::commands::add_validation::{compute_evidence_status_write, validate_kb_add_inputs};
use crate::components::db;
use crate::components::embedder;
use crate::components::events;
use crate::config;
use crate::models::Evidence;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::params;
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
        let tags_json: Value = serde_json::json!(
            self.tags.split(',').map(|t| t.trim()).collect::<Vec<_>>()
        );

        // Compute id early so the self-loop provenance check in validate_kb_add_inputs
        // can compare evidence.derived_from against the entry's own id.
        let id = self
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Validate kind, tags, and evidence before acquiring the lock.
        validate_kb_add_inputs(&id, &self.kind, &tags_json, &evidence_rows)?;

        // Compute write-time evidence_status and emit soft-mandate warning.
        let evidence_status = compute_evidence_status_write(&self.kind, &evidence_rows);

        let _lock = acquire_lock(&paths.lock)?;
        let version_ref = self.version_ref.clone().or_else(config::git_head_sha);
        let ts = chrono::Utc::now().to_rfc3339();
        let session =
            std::env::var("OMC_SESSION_ID").unwrap_or_else(|_| "cli".to_string());

        // Soft-mandate warning (AC10).
        if evidence_status == "missing" {
            eprintln!("kb: entry {id} kind={} has no evidence; evidence_status=missing", self.kind);
        }

        // Open DB once; used for both the optional path-replace step and the upsert.
        let conn = db::open_db(&paths.db)?;

        // --replace-path: expire all existing non-stale entries at this path before
        // inserting. Bypasses the permanent guard in expire.rs (the user is explicitly
        // replacing the entry via kb add --replace-path).
        if self.replace_path {
            let existing_ids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT id FROM entries WHERE path=?1 AND is_stale=0",
                )?;
                let ids: Vec<String> = stmt
                    .query_map(params![self.path], |r| r.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                ids
            };
            for old_id in existing_ids {
                let expire_ev = serde_json::json!({
                    "action": "expire",
                    "table": "entries",
                    "id": old_id,
                    "reason": "replaced by --replace-path",
                    "ts": ts,
                    "session": session,
                });
                events::append_event(&paths.events, &expire_ev)?;
                db::apply_event(&conn, embedder, &expire_ev)?;
            }
        }

        // Build Add event (carries kind + evidence_status).
        let add_event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": id,
            "path": self.path,
            "summary": self.summary,
            "content": self.content,
            "tags": tags_json,
            "version_ref": version_ref,
            "permanent": self.permanent,
            "kind": self.kind,
            "evidence_status": evidence_status,
            "ts": ts,
            "session": session,
        });

        // Build EvidenceAdd events (one per evidence row).
        let evidence_events: Vec<Value> = evidence_rows
            .iter()
            .map(|ev| {
                let evidence = Evidence {
                    id: uuid::Uuid::new_v4().to_string(),
                    entry_id: id.clone(),
                    kind: ev.get("kind").and_then(|v| v.as_str()).unwrap_or("code").to_string(),
                    citation_path: ev.get("citation_path").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    citation_sha: ev.get("citation_sha").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    citation_hash: ev.get("citation_hash").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    citation_excerpt: ev.get("citation_excerpt").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    derived_from: ev.get("derived_from").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    recorded_at: Some(ts.clone()),
                };
                events::evidence_add_event(&id, &evidence, version_ref.as_deref())
            })
            .collect();

        // Atomic batch: Add event + N EvidenceAdd events under the held lock (AC12).
        let mut batch = vec![add_event.clone()];
        batch.extend(evidence_events.iter().cloned());
        events::append_events_batch(&paths.events, &batch)?;

        // Apply each event to the DB (also under lock).
        db::apply_event(&conn, embedder, &add_event)?;
        for ev in &evidence_events {
            db::apply_event(&conn, embedder, ev)?;
        }

        println!("added  {} ({})", self.path, id);
        Ok(())
    }
}

/// Build the appropriate embedder based on KB_NO_EMBED env var.
pub fn make_embedder(paths: &config::Paths) -> Box<dyn embedder::Embedder> {
    if std::env::var("KB_NO_EMBED").is_ok() {
        Box::new(embedder::NoopEmbedder)
    } else {
        Box::new(embedder::CandleEmbedder::new(&paths.fastembed_cache))
    }
}

/// Acquire the agentic lock.
pub fn acquire_lock(lock_path: &std::path::Path) -> anyhow::Result<Lock> {
    use anyhow::Context;
    use fs2::FileExt;
    use std::fs::{self, OpenOptions};

    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    f.lock_exclusive()
        .with_context(|| format!("acquire lock {}", lock_path.display()))?;
    Ok(Lock(f))
}

/// RAII lock guard — holds the file lock until dropped.
pub struct Lock(#[allow(dead_code)] std::fs::File);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use rusqlite::Connection;
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
            evidence: vec![],
            evidence_file: None,
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
        let conn = Connection::open(&paths.db).unwrap();
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
            kind: "belief".to_string(),
            evidence: vec![],
            evidence_file: None,
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
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
            kind: "belief".to_string(),
            evidence: vec![],
            evidence_file: None,
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

        let conn = db::open_db(&paths.db).unwrap();
        db::apply_event(&conn, &embedder, &old_event).unwrap();

        let permanent: i64 = conn
            .query_row(
                "SELECT permanent FROM entries WHERE id='old-event-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(permanent, 0, "old events without permanent field must default to 0");
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
            kind: "belief".to_string(),
            evidence: vec![],
            evidence_file: None,
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
            kind: "belief".to_string(),
            evidence: vec![],
            evidence_file: None,
        };
        cmd2.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
        let old_stale: i64 = conn
            .query_row(
                "SELECT is_stale FROM entries WHERE id='rp-old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_stale, 1, "old entry must be stale after --replace-path");
        let new_stale: i64 = conn
            .query_row(
                "SELECT is_stale FROM entries WHERE id='rp-new'",
                [],
                |r| r.get(0),
            )
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
            kind: "belief".to_string(),
            evidence: vec![],
            evidence_file: None,
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
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

        let conn = Connection::open(&paths.db).unwrap();
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
        };
        let err = cmd.execute_with(&paths, &embedder).unwrap_err();
        assert!(err.to_string().contains("Phase 1 ships evidence.kind=code|derived only"));
    }

    #[test]
    fn test_kb_add_soft_mandate_warns_on_missing_evidence() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // observation with no evidence → evidence_status="missing" in DB
        let cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test".to_string(),
            content: "content".to_string(),
            tags: "test".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("soft-mandate-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "observation".to_string(),
            evidence: vec![],
            evidence_file: None,
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
        let evidence_status: String = conn
            .query_row(
                "SELECT evidence_status FROM entries WHERE id='soft-mandate-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(evidence_status, "missing");
    }

    #[test]
    fn test_kb_add_with_evidence_writes_atomic_batch() {
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
            id: Some("batch-ev-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "observation".to_string(),
            evidence: vec![
                r#"{"kind":"code","citation_path":"src/foo.rs:1-10","citation_sha":"abc","citation_hash":"sha256:aaa","citation_excerpt":"fn foo() {}"}"#.to_string(),
                r#"{"kind":"code","citation_path":"src/bar.rs:5-15","citation_sha":"abc","citation_hash":"sha256:bbb","citation_excerpt":"fn bar() {}"}"#.to_string(),
            ],
            evidence_file: None,
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        // Verify events.jsonl has Add followed by 2 EvidenceAdd events
        let events_content = fs::read_to_string(&paths.events).unwrap();
        let lines: Vec<&str> = events_content.lines().collect();
        // Should have 3 lines: 1 upsert + 2 evidence_add
        assert_eq!(lines.len(), 3, "expected 3 event lines (1 add + 2 evidence_add)");

        let ev0: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(ev0["action"], "upsert");
        assert_eq!(ev0["table"], "entries");
        assert_eq!(ev0["id"], "batch-ev-1");
        assert_eq!(ev0["kind"], "observation");
        assert_eq!(ev0["evidence_status"], "present");

        let ev1: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ev1["action"], "evidence_add");
        assert_eq!(ev1["entry_id"], "batch-ev-1");

        let ev2: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(ev2["action"], "evidence_add");
        assert_eq!(ev2["entry_id"], "batch-ev-1");

        // Verify evidence rows in DB
        let conn = Connection::open(&paths.db).unwrap();
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
}
