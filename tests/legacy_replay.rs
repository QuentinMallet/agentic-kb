//! Legacy-log replay tolerance + schema-version force-rebuild
//! (br-23b-handoff-tomorrow-y0a / br-23b-handoff-tomorrow-uob).
//!
//! Invariants:
//!   1. apply_event clamps over-limit content (>10000 chars) instead of
//!      aborting on the SQL CHECK — replay of legacy logs must never fail.
//!   2. Same for summary (>200 chars).
//!   3. Clamping is deterministic: replaying twice yields identical rows.
//!   4. kb_core::add REJECTS oversized input with a clear error — tolerance
//!      is for replay only, never for new writes.
//!   5. Fresh DBs are stamped schema_version=current at creation.
//!   6. A DB without the stamp (legacy schema) is detected obsolete;
//!      rebuild_if_schema_obsolete rebuilds once, preserves entries, stamps.
//!   7. On a current DB, rebuild_if_schema_obsolete is a no-op.
//!   8. End-to-end: kb rebuild over a log containing an oversized event
//!      succeeds and stores the clamped entry.

use kb::commands::rebuild::{rebuild_if_schema_obsolete, Rebuild};
use kb::components::db::{apply_event, open_db, open_db_memory, schema_is_current};
use kb::components::embedder::{Embedder, NoopEmbedder};
use kb::components::events;
use kb::components::kb_core::{add, AddArgs};
use kb::config::Paths;
use serde_json::json;
use std::fs;

/// Deterministic non-noop embedder: the upgrade path refuses to rebuild
/// under a Noop embedder (it would wipe entries_emb).
struct FixedEmbedder;

impl Embedder for FixedEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![1.0 / (384f32).sqrt(); 384])
    }
    fn is_noop(&self) -> bool {
        false
    }
}

fn oversized_event(id: &str, content_len: usize, summary_len: usize) -> serde_json::Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": format!("legacy/{id}"),
        "summary": "s".repeat(summary_len),
        "content": "x".repeat(content_len),
        "tags": ["legacy"],
        "ts": "2024-01-01T00:00:00Z",
    })
}

fn entry_lens(conn: &rusqlite::Connection, id: &str) -> (i64, i64) {
    conn.query_row(
        "SELECT LENGTH(summary), LENGTH(content) FROM entries WHERE id=?1",
        rusqlite::params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn base_args(id: &str) -> AddArgs {
    AddArgs {
        id: id.into(),
        path: format!("t/{id}"),
        summary: "ok".into(),
        content: "ok".into(),
        tags: json!([]),
        version_ref: None,
        permanent: false,
        replace_path: false,
        kind: "memory".into(),
        evidence_status: "n/a".into(),
        evidence_rows: vec![],
        ts: chrono::Utc::now().to_rfc3339(),
        session: "test".into(),
        session_id: None,
        expire_reason: String::new(),
        dedup_cutoff: None,
        cues: vec![],
    }
}

/// Invariants 1+2: over-limit content and summary are clamped, not fatal.
#[test]
fn test_apply_event_clamps_oversized_fields() {
    let conn = open_db_memory().unwrap();
    apply_event(&conn, &NoopEmbedder, &oversized_event("big", 90_000, 300)).unwrap();
    let (s, c) = entry_lens(&conn, "big");
    assert_eq!(s, 200, "summary must be clamped to the schema cap");
    assert_eq!(c, 10_000, "content must be clamped to the schema cap");
}

/// Invariant 3: clamping is deterministic across replays.
#[test]
fn test_clamp_deterministic_across_replays() {
    let get = || {
        let conn = open_db_memory().unwrap();
        apply_event(&conn, &NoopEmbedder, &oversized_event("det", 20_000, 250)).unwrap();
        conn.query_row(
            "SELECT summary || '|' || content FROM entries WHERE id='det'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    assert_eq!(get(), get(), "replay must be a pure function of the event");
}

/// Invariant 4: the write API rejects oversized input loudly.
#[test]
fn test_kb_core_add_rejects_oversized() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());

    let mut a = base_args("too-big-content");
    a.content = "x".repeat(10_001);
    let err = add(&paths, &NoopEmbedder, a);
    assert!(err.is_err());
    assert!(err.err().unwrap().to_string().contains("content exceeds"), "clear error required");

    let mut a = base_args("too-big-summary");
    a.summary = "s".repeat(201);
    let err = add(&paths, &NoopEmbedder, a);
    assert!(err.is_err());
    assert!(err.err().unwrap().to_string().contains("summary exceeds"), "clear error required");
}

/// Invariant 5: fresh DBs are stamped current at creation.
#[test]
fn test_fresh_db_is_current() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_db(&dir.path().join("fresh.db")).unwrap();
    assert!(schema_is_current(&conn), "fresh DB must carry the current schema_version stamp");
}

/// Invariants 6+7: legacy DB detected + force-rebuilt exactly once.
#[test]
fn test_obsolete_schema_forces_one_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());

    // Write real entries through the normal path (events + DB).
    add(&paths, &NoopEmbedder, base_args("keep-1")).unwrap();
    add(&paths, &NoopEmbedder, base_args("keep-2")).unwrap();

    // Simulate a legacy DB: strip the stamp (pre-stamp binaries never wrote one).
    {
        let conn = open_db(&paths.db).unwrap();
        conn.execute("DELETE FROM kb_meta WHERE key='schema_version'", []).unwrap();
        assert!(!schema_is_current(&conn), "stripped DB must read as obsolete");
    }

    // A Noop embedder must DEFER (rebuilding would wipe entries_emb) and
    // must not stamp — the DB stays flagged for the next real interaction.
    let deferred = rebuild_if_schema_obsolete(&paths, &NoopEmbedder).unwrap();
    assert!(!deferred, "noop embedder must defer the upgrade");
    {
        let conn = open_db(&paths.db).unwrap();
        assert!(!schema_is_current(&conn), "deferral must not stamp");
    }

    // First real interaction: forces a rebuild, preserves entries, stamps.
    let rebuilt = rebuild_if_schema_obsolete(&paths, &FixedEmbedder).unwrap();
    assert!(rebuilt, "obsolete schema must trigger a rebuild");
    {
        let conn = open_db(&paths.db).unwrap();
        assert!(schema_is_current(&conn), "rebuilt DB must be stamped current");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "rebuild must preserve entries");
        let emb: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries_emb", [], |r| r.get(0))
            .unwrap();
        assert_eq!(emb, 2, "upgrade rebuild must produce embeddings");
    }

    // Second interaction: no-op.
    let rebuilt_again = rebuild_if_schema_obsolete(&paths, &FixedEmbedder).unwrap();
    assert!(!rebuilt_again, "current schema must not rebuild again");
}

/// Missing event log must NEVER silently stamp a populated DB (scenario-B
/// finding): a legacy DB whose log path failed to resolve stays unstamped
/// (warn + retry next interaction). Only a genuinely empty DB may stamp.
#[test]
fn test_missing_log_does_not_disarm_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());

    add(&paths, &NoopEmbedder, base_args("orphaned")).unwrap();
    {
        let conn = open_db(&paths.db).unwrap();
        conn.execute("DELETE FROM kb_meta WHERE key='schema_version'", []).unwrap();
    }
    // Simulate a layout mismatch: the log is unreachable at paths.events.
    fs::remove_file(&paths.events).unwrap();

    let rebuilt = rebuild_if_schema_obsolete(&paths, &FixedEmbedder).unwrap();
    assert!(!rebuilt, "no log -> no rebuild");
    {
        let conn = open_db(&paths.db).unwrap();
        assert!(
            !schema_is_current(&conn),
            "populated DB without its log must NOT be stamped — the upgrade must retry"
        );
    }

    // Genuinely fresh DB (zero entries, no log): stamping is correct.
    let dir2 = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir2.path().join(".state/agent-kb")).unwrap();
    let paths2 = Paths::from_root(dir2.path());
    {
        let conn = open_db(&paths2.db).unwrap();
        conn.execute("DELETE FROM kb_meta WHERE key='schema_version'", []).unwrap();
    }
    let rebuilt2 = rebuild_if_schema_obsolete(&paths2, &FixedEmbedder).unwrap();
    assert!(!rebuilt2);
    let conn = open_db(&paths2.db).unwrap();
    assert!(schema_is_current(&conn), "empty DB with no log may stamp without rebuild");
}

/// Coverage guard (codex round-4 finding): an obsolete DB whose log does NOT
/// cover its live entries (partial/foreign log created after the original
/// went unreachable) must never auto-rebuild — that would drop the uncovered
/// rows. The gate refuses, warns, and leaves the DB unstamped and intact.
#[test]
fn test_partial_log_refuses_auto_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());

    add(&paths, &NoopEmbedder, base_args("legacy-a")).unwrap();
    add(&paths, &NoopEmbedder, base_args("legacy-b")).unwrap();
    {
        let conn = open_db(&paths.db).unwrap();
        conn.execute("DELETE FROM kb_meta WHERE key='schema_version'", []).unwrap();
    }
    // Simulate the partial-log hazard: the original log is gone; one later
    // write created a fresh log containing only a NEW entry.
    fs::remove_file(&paths.events).unwrap();
    add(&paths, &NoopEmbedder, base_args("only-in-new-log")).unwrap();

    let rebuilt = rebuild_if_schema_obsolete(&paths, &FixedEmbedder).unwrap();
    assert!(!rebuilt, "partial log must refuse auto-rebuild");

    let conn = open_db(&paths.db).unwrap();
    assert!(!schema_is_current(&conn), "refusal must not stamp");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 3, "no entry may be dropped by the refused upgrade");
}

/// Freshness guard (codex round-5 finding): a restored/stale log that covers
/// every live id but whose newest event is far older than the DB's newest row
/// must refuse auto-rebuild — replaying it would roll entries back.
#[test]
fn test_stale_covering_log_refuses_auto_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());

    add(&paths, &NoopEmbedder, base_args("rolled")).unwrap();
    {
        let conn = open_db(&paths.db).unwrap();
        conn.execute("DELETE FROM kb_meta WHERE key='schema_version'", []).unwrap();
    }
    // Replace the log with a STALE one: same id (full coverage) but an old
    // payload and an event ts years behind the DB row's updated_at.
    fs::write(
        &paths.events,
        serde_json::json!({
            "action": "upsert", "table": "entries", "id": "rolled",
            "path": "t/rolled", "summary": "OLD payload", "content": "OLD content",
            "tags": [], "ts": "2020-01-01T00:00:00Z",
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let rebuilt = rebuild_if_schema_obsolete(&paths, &FixedEmbedder).unwrap();
    assert!(!rebuilt, "stale covering log must refuse auto-rebuild");

    let conn = open_db(&paths.db).unwrap();
    assert!(!schema_is_current(&conn), "refusal must not stamp");
    let content: String = conn
        .query_row("SELECT content FROM entries WHERE id='rolled'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(content, "ok", "the newer DB payload must survive (base_args content)");
}

/// Single-flight (codex review finding): concurrent first interactions with
/// an obsolete KB serialize on the upgrade lock — exactly one performs the
/// rebuild, the loser re-checks the stamp and returns without rebuilding.
#[test]
fn test_concurrent_upgrade_single_flight() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());

    add(&paths, &NoopEmbedder, base_args("cc-1")).unwrap();
    {
        let conn = open_db(&paths.db).unwrap();
        conn.execute("DELETE FROM kb_meta WHERE key='schema_version'", []).unwrap();
    }

    let results: Vec<bool> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let paths = &paths;
                s.spawn(move || rebuild_if_schema_obsolete(paths, &FixedEmbedder).unwrap())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let rebuilds = results.iter().filter(|r| **r).count();
    assert_eq!(rebuilds, 1, "exactly one of the racers must rebuild, got {results:?}");

    let conn = open_db(&paths.db).unwrap();
    assert!(schema_is_current(&conn));
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "entries must survive the racing upgrade");
}

/// Invariant 8: end-to-end rebuild over a legacy log with an oversized event.
#[test]
fn test_rebuild_survives_oversized_legacy_event() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());

    add(&paths, &NoopEmbedder, base_args("normal")).unwrap();
    // Inject a legacy oversized event straight into the log (predates caps).
    events::append_event(&paths.events, &oversized_event("legacy-big", 90_000, 300)).unwrap();

    (Rebuild).execute_with(&paths, &NoopEmbedder).unwrap();

    let conn = rusqlite::Connection::open(&paths.db).unwrap();
    let (s, c) = entry_lens(&conn, "legacy-big");
    assert_eq!((s, c), (200, 10_000), "oversized legacy entry must be clamped, not lost");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}
