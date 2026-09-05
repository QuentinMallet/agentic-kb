//! C1/T4 — the applied cursor, automatic recovery, and the poison policy.
//!
//! Spec: `.state/agent-kb/tla/DurableBatch.tla` (`RecTarget`,
//! `CursorAgreesWithDB`, CE3, CE8) and `CompactMaterialize.tla` (CE7).
//! Plan: `.state/.omc/plans/c1-log-durability.md` §4 D3 and task T4.
//!
//! The crash tests live in `src/components/cursor.rs`: `kill_point` compiles
//! to a no-op outside `cfg(test)` of the library itself, so an integration
//! test could never reach one.

#![allow(deprecated)] // db::open_db (ADR-1) — the fixtures follow the repo default

use kb::commands::add::acquire_lock;
use kb::commands::rebuild::{recover_if_needed, Rebuild};
use kb::components::cursor::{
    self, Cursor, DeadLetter, Decision, RebuildReason, POISON_MAX_ATTEMPTS,
};
use kb::components::db::{self, open_db, open_ro};
use kb::components::embedder::{Embedder, NoopEmbedder};
use kb::components::events;
use kb::components::kb_core::{self, AddArgs};
use kb::config::Paths;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A deterministic non-noop embedder: recovery's full-rebuild rows defer under
/// `KB_NO_EMBED` on purpose, so tests that must see a rebuild need a real one.
struct FixedEmbedder;
impl Embedder for FixedEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![0.1; 384])
    }
}

fn repo() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let (paths, _conn) = db::test_db(dir.path());
    (dir, paths)
}

fn upsert(id: &str, summary: &str) -> Value {
    json!({
        "action": "upsert", "table": "entries",
        "id": id, "path": format!("p/{id}"), "summary": summary,
        "content": format!("content of {id}"), "tags": ["t"],
        "version_ref": "abc123", "kind": "belief", "evidence_status": "n/a",
        "ts": "2026-09-05T00:00:00Z",
    })
}

fn add_args(id: &str) -> AddArgs {
    AddArgs {
        id: id.to_string(),
        path: format!("p/{id}"),
        summary: format!("summary {id}"),
        content: format!("content {id}"),
        tags: json!(["t"]),
        version_ref: Some("abc123".to_string()),
        permanent: false,
        replace_path: false,
        kind: "belief".to_string(),
        evidence_status: "n/a".to_string(),
        evidence_rows: vec![],
        cues: vec![],
        ts: "2026-09-05T00:00:00Z".to_string(),
        session: "test".to_string(),
        session_id: None,
        expire_reason: "replaced".to_string(),
        dedup_cutoff: None,
    }
}

fn cursor_of(paths: &Paths) -> Option<Cursor> {
    let conn = open_db(&paths.db).unwrap();
    cursor::read(&conn).unwrap()
}

fn live_ids(paths: &Paths) -> Vec<String> {
    let conn = open_db(&paths.db).unwrap();
    let mut stmt = conn
        .prepare("SELECT id FROM entries WHERE is_stale=0 ORDER BY id")
        .unwrap();
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    ids
}

/// Materialized state, exactly the tables the plan's invariant names.
fn materialized(conn: &Connection) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for (table, sql) in [
        ("entries", "SELECT id,path,summary,content,tags,permanent,is_stale,kind,evidence_status,created_at,updated_at FROM entries ORDER BY id"),
        ("test_cases", "SELECT id,app,name,protocol,config,is_stale,created_at,updated_at FROM test_cases ORDER BY id"),
        ("evidence", "SELECT id,entry_id,kind,citation_path,citation_hash FROM evidence ORDER BY id"),
        ("cues", "SELECT entry_id,cue FROM cues ORDER BY entry_id,cue"),
        ("entries_emb", "SELECT rowid,hex(embedding) FROM entries_emb ORDER BY rowid"),
        ("run_history", "SELECT test_id,result,adapter,detail,ts,run_id FROM run_history ORDER BY run_id"),
    ] {
        let mut stmt = conn.prepare(sql).unwrap();
        let cols = stmt.column_count();
        let rows: Vec<String> = stmt
            .query_map([], |r| {
                let mut cells = Vec::new();
                for i in 0..cols {
                    cells.push(format!("{:?}", r.get::<_, rusqlite::types::Value>(i)?));
                }
                Ok(cells.join("|"))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        out.push((table.to_string(), rows));
    }
    out
}

// ---------------------------------------------------------------------------
// 1. The cursor exists, is transactional, and every writer maintains it
// ---------------------------------------------------------------------------

#[test]
fn test_add_writes_all_three_cursor_rows_in_the_apply_transaction() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &NoopEmbedder, add_args("e1")).unwrap();

    let cursor = cursor_of(&paths).expect("add must leave an applied cursor");
    let committed_len = events::read_events(&paths.events).unwrap().committed_len;
    assert_eq!(cursor.offset, committed_len);
    assert_eq!(cursor.generation, cursor::read_generation(&paths.events));
    assert_eq!(
        cursor.tail_sha,
        cursor::tail_sha(&paths.events, committed_len).unwrap()
    );
    // The offset is the boundary AFTER the batch_commit newline, not before it.
    assert_eq!(
        cursor.offset,
        fs::metadata(&paths.events).unwrap().len(),
        "the cursor must sit immediately after the last committed byte"
    );
}

/// Enumerated, not sampled: every production writer must leave the cursor
/// caught up. One that appends and applies without touching it leaves the
/// cursor permanently behind, and then every subsequent open replays its
/// events — a rare crash gap turned into a guaranteed loop.
///
/// Each row drives the production entry point, not the helper it delegates to,
/// so a regression inside one of those commands is visible here. The MCP
/// handlers are private to their module and are covered by the equivalent
/// assertions in `src/commands/mcp.rs`; `stale_check`'s and
/// `migrate_citations`' heal writers likewise, in their own modules, because
/// both need a git repository and a relocated file to reach.
#[test]
fn test_every_writer_leaves_the_cursor_caught_up() {
    type Writer = Box<dyn Fn(&Paths)>;
    let writers: Vec<(&str, Writer)> = vec![
        (
            "kb_core::add / add_locked (kb add, kb ingest, MCP kb_add)",
            Box::new(|paths: &Paths| {
                kb_core::add(paths, &NoopEmbedder, add_args("w-add")).unwrap();
            }),
        ),
        (
            "expire.rs (kb expire)",
            Box::new(|paths: &Paths| {
                kb::commands::expire::Expire {
                    id: "w-add".to_string(),
                    reason: Some("because".to_string()),
                    force: false,
                }
                .execute_with(paths, &NoopEmbedder)
                .unwrap();
            }),
        ),
        (
            "test_add.rs (kb test-add)",
            Box::new(|paths: &Paths| {
                kb::commands::test_add::TestAdd {
                    app: "a".to_string(),
                    name: "n".to_string(),
                    protocol: "browser".to_string(),
                    config: "{}".to_string(),
                    id: Some("tc-1".to_string()),
                    version_ref: None,
                }
                .execute_with_paths(paths)
                .unwrap();
            }),
        ),
        (
            "run.rs (kb run)",
            Box::new(|paths: &Paths| {
                kb::commands::test_add::TestAdd {
                    app: "a".to_string(),
                    name: "n".to_string(),
                    protocol: "browser".to_string(),
                    config: "{}".to_string(),
                    id: Some("tc-1".to_string()),
                    version_ref: None,
                }
                .execute_with_paths(paths)
                .unwrap();
                kb::commands::run::Run {
                    test_id: "tc-1".to_string(),
                    result: "pass".to_string(),
                    adapter: None,
                    detail: None,
                }
                .execute_with_paths(paths, &NoopEmbedder)
                .unwrap();
            }),
        ),
    ];

    for (name, writer) in writers {
        let (_dir, paths) = repo();
        // A live seed so the expire/heal writers have a parent entry.
        kb_core::add(&paths, &NoopEmbedder, add_args("w-add")).unwrap();
        writer(&paths);

        let conn = open_db(&paths.db).unwrap();
        assert_eq!(
            cursor::inspect(&conn, &paths),
            Decision::NoOp,
            "{name} left the applied cursor behind the log"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. A write may only start from a converged database
// ---------------------------------------------------------------------------

/// Overwriting the cursor without reading it first re-baselines a diverged
/// database to converged, and the divergence becomes unrecoverable: the log is
/// the only other record of the gap and the new cursor claims it is applied.
#[test]
fn test_write_refuses_when_the_cursor_says_a_rebuild_is_due() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    let before_cursor = cursor_of(&paths).unwrap();
    let before_log = fs::read(&paths.events).unwrap();

    // An external `kb compact` in another process bumps the generation.
    cursor::bump_generation(&paths.events).unwrap();

    let conn = open_db(&paths.db).unwrap();
    let lock = acquire_lock(&paths.lock).unwrap();
    let error = cursor::append_and_apply(
        &lock,
        &conn,
        &paths,
        &FixedEmbedder,
        &[upsert("e2", "two")],
    )
    .unwrap_err();

    assert!(
        cursor::is_not_converged(&error),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fs::read(&paths.events).unwrap(),
        before_log,
        "a refused write must not append"
    );
    assert_eq!(
        cursor::read(&conn).unwrap().unwrap(),
        before_cursor,
        "a refused write must not touch the cursor"
    );
    // The repair is still available, which is the whole point of refusing.
    drop(lock);
    assert!(recover_if_needed(&paths, &FixedEmbedder).unwrap());
}

/// The same guard for a tail-replay gap rather than a rebuild.
#[test]
fn test_write_refuses_when_the_cursor_is_behind_the_log() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    // Another process appended and was killed before its apply.
    events::append_event(&paths.events, &upsert("orphan", "unapplied")).unwrap();
    let before_len = fs::metadata(&paths.events).unwrap().len();

    let conn = open_db(&paths.db).unwrap();
    let lock = acquire_lock(&paths.lock).unwrap();
    let error = cursor::append_and_apply(
        &lock,
        &conn,
        &paths,
        &FixedEmbedder,
        &[upsert("e2", "two")],
    )
    .unwrap_err();

    assert!(cursor::is_not_converged(&error), "unexpected error: {error:#}");
    assert_eq!(fs::metadata(&paths.events).unwrap().len(), before_len);
    drop(lock);

    // Recovery converges it, and the write then succeeds.
    recover_if_needed(&paths, &FixedEmbedder).unwrap();
    kb_core::add(&paths, &FixedEmbedder, add_args("e2")).unwrap();
    let conn = open_db(&paths.db).unwrap();
    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
    assert_eq!(
        live_ids(&paths),
        vec!["e1".to_string(), "e2".to_string(), "orphan".to_string()]
    );
}

/// Route (c): the no-op-embedder rebuild defer returns Ok(false), so without
/// the guard the very next write re-baselines the database it declined to
/// repair.
#[test]
fn test_write_refuses_after_a_deferred_rebuild_under_a_noop_embedder() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &NoopEmbedder, add_args("e1")).unwrap();
    cursor::bump_generation(&paths.events).unwrap();

    // The defer: nothing was rebuilt, and nothing was re-baselined either.
    assert!(!recover_if_needed(&paths, &NoopEmbedder).unwrap());
    let error = kb_core::add(&paths, &NoopEmbedder, add_args("e2")).unwrap_err();
    assert!(cursor::is_not_converged(&error), "unexpected error: {error:#}");

    let conn = open_db(&paths.db).unwrap();
    assert_eq!(
        cursor::inspect(&conn, &paths),
        Decision::FullRebuild(RebuildReason::GenerationMismatch),
        "the divergence must still be visible after the refusal"
    );
}

/// The empty-batch path takes the guard too: an audit verdict with no expire
/// still writes audit_runs rows, and those must not land on a diverged
/// database either.
#[test]
fn test_empty_batch_write_takes_the_guard() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    cursor::bump_generation(&paths.events).unwrap();

    let conn = open_db(&paths.db).unwrap();
    let lock = acquire_lock(&paths.lock).unwrap();
    let error =
        cursor::append_and_apply_with(&lock, &conn, &paths, &FixedEmbedder, &[], |_| Ok(()))
            .unwrap_err();
    assert!(cursor::is_not_converged(&error), "unexpected error: {error:#}");
}

// ---------------------------------------------------------------------------
// 3. Idempotent replay of every apply_event arm
// ---------------------------------------------------------------------------

/// Enumerated, not sampled. A second non-idempotent arm must not slip through
/// behind `run_history`: recovery re-applies a whole tail, and a downgraded
/// binary that appends-and-applies without the cursor makes replay of
/// already-applied events the normal case, not the exception.
#[test]
fn test_every_apply_event_arm_is_idempotent_under_replay() {
    let arms: Vec<(&str, Vec<Value>)> = vec![
        ("(upsert, entries)", vec![upsert("a1", "one")]),
        (
            "(upsert, entries) with cues",
            vec![{
                let mut e = upsert("a2", "two");
                e["cues"] = json!(["cue one", "cue two"]);
                e
            }],
        ),
        (
            "(expire, entries)",
            vec![
                upsert("a3", "three"),
                json!({"action":"expire","table":"entries","id":"a3","reason":"r","ts":"2026-09-05T00:00:00Z"}),
            ],
        ),
        (
            "(upsert, test_cases)",
            vec![json!({
                "action":"upsert","table":"test_cases","id":"tc","app":"app","name":"n",
                "protocol":"browser","config":"{}","version_ref":null,"ts":"2026-09-05T00:00:00Z"
            })],
        ),
        (
            "(insert, run_history) keyed",
            vec![
                json!({
                    "action":"upsert","table":"test_cases","id":"tc","app":"app","name":"n",
                    "protocol":"browser","config":"{}","version_ref":null,"ts":"2026-09-05T00:00:00Z"
                }),
                json!({
                    "action":"insert","table":"run_history","test_id":"tc","result":"pass",
                    "adapter":"rust_tool","detail":"d","ts":"2026-09-05T00:00:00Z","run_id":"r-1"
                }),
            ],
        ),
        (
            "(insert, run_history) legacy, run_id-less",
            vec![
                json!({
                    "action":"upsert","table":"test_cases","id":"tc","app":"app","name":"n",
                    "protocol":"browser","config":"{}","version_ref":null,"ts":"2026-09-05T00:00:00Z"
                }),
                json!({
                    "action":"insert","table":"run_history","test_id":"tc","result":"fail",
                    "adapter":null,"detail":null,"ts":"2026-09-05T00:00:00Z"
                }),
            ],
        ),
        (
            "(evidence_add, evidence)",
            vec![upsert("a4", "four"), {
                let ev = kb::models::Evidence {
                    id: "ev-1".to_string(),
                    entry_id: "a4".to_string(),
                    kind: "code".to_string(),
                    citation_path: Some("src/lib.rs".to_string()),
                    citation_sha: None,
                    citation_hash: "hash".to_string(),
                    citation_excerpt: None,
                    derived_from: None,
                    recorded_at: Some("2026-09-05T00:00:00Z".to_string()),
                };
                events::evidence_add_event("a4", &ev, None)
            }],
        ),
        (
            "(citation_healed, evidence)",
            vec![upsert("a5", "five"), {
                let ev = kb::models::Evidence {
                    id: "ev-2".to_string(),
                    entry_id: "a5".to_string(),
                    kind: "code".to_string(),
                    citation_path: Some("src/old.rs".to_string()),
                    citation_sha: None,
                    citation_hash: "hash".to_string(),
                    citation_excerpt: None,
                    derived_from: None,
                    recorded_at: Some("2026-09-05T00:00:00Z".to_string()),
                };
                events::evidence_add_event("a5", &ev, None)
            },
            events::citation_healed_event("a5", "ev-2", "src/old.rs", "src/new.rs", "hash", None)],
        ),
        (
            "(evidence_expire, evidence)",
            vec![upsert("a6", "six"), {
                let ev = kb::models::Evidence {
                    id: "ev-3".to_string(),
                    entry_id: "a6".to_string(),
                    kind: "code".to_string(),
                    citation_path: Some("src/lib.rs".to_string()),
                    citation_sha: None,
                    citation_hash: "hash".to_string(),
                    citation_excerpt: None,
                    derived_from: None,
                    recorded_at: Some("2026-09-05T00:00:00Z".to_string()),
                };
                events::evidence_add_event("a6", &ev, None)
            },
            events::evidence_expire_event("a6", "ev-3", "gone")],
        ),
        (
            "unknown action (the `_ => {}` arm)",
            vec![json!({"action":"no_such_action","table":"nowhere","id":"x"})],
        ),
    ];

    for (name, events_of_arm) in arms {
        let dir = tempfile::tempdir().unwrap();
        let (_paths, conn) = db::test_db(dir.path());
        // A replay driver supplies each event's occurrence index in the log —
        // the same value on every pass, because it is a function of the log.
        let replay = || {
            let mut seen: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
            for event in &events_of_arm {
                let occurrence = db::legacy_run_content_hash(event).map(|hash| {
                    let slot = seen.entry(hash).or_default();
                    let n = *slot;
                    *slot += 1;
                    n
                });
                db::apply_event_at(&conn, &FixedEmbedder, event, occurrence).unwrap();
            }
        };
        replay();
        let once = materialized(&conn);
        replay();
        let twice = materialized(&conn);
        assert_eq!(once, twice, "{name} is not idempotent under replay");
    }
}

/// The same property end-to-end through recovery: rewinding the cursor and
/// recovering again must not duplicate rows, including the run_id-less legacy
/// `run_history` events T3 handed to T4.
#[test]
fn test_replaying_the_same_tail_twice_through_recovery_is_a_no_op() {
    let (_dir, paths) = repo();
    let legacy_run = json!({
        "action":"insert","table":"run_history","test_id":"tc","result":"pass",
        "adapter":null,"detail":null,"ts":"2026-09-05T00:00:00Z"
    });
    events::append_events_batch(
        &paths.events,
        &[
            json!({
                "action":"upsert","table":"test_cases","id":"tc","app":"app","name":"n",
                "protocol":"browser","config":"{}","version_ref":null,"ts":"2026-09-05T00:00:00Z"
            }),
            upsert("e1", "one"),
            // Two byte-identical legacy run events must NOT collapse ...
            legacy_run.clone(),
            legacy_run.clone(),
        ],
    )
    .unwrap();
    recover_if_needed(&paths, &FixedEmbedder).unwrap();
    let after_first = {
        let conn = open_db(&paths.db).unwrap();
        materialized(&conn)
    };
    let runs: i64 = open_db(&paths.db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM run_history", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs, 2, "distinct legacy occurrences must not collapse");

    // Rewind the cursor to before the batch: the whole tail replays again.
    {
        let conn = open_db(&paths.db).unwrap();
        cursor::write(
            &conn,
            &Cursor {
                generation: cursor::read_generation(&paths.events),
                offset: 0,
                tail_sha: cursor::tail_sha(&paths.events, 0).unwrap(),
            },
        )
        .unwrap();
    }
    recover_if_needed(&paths, &FixedEmbedder).unwrap();

    let conn = open_db(&paths.db).unwrap();
    assert_eq!(
        materialized(&conn),
        after_first,
        "... and replaying them must not duplicate them either"
    );
}

// ---------------------------------------------------------------------------
// 4. Reads detect, warn, and never take the write lock
// ---------------------------------------------------------------------------

/// The read path must serve stale data rather than block. The write lock is
/// held by a second thread for the duration, so a read that tried to acquire
/// it would never return — `flock` conflicts across open file descriptions
/// even within one process.
#[test]
fn test_read_path_serves_stale_data_and_never_takes_the_write_lock() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &NoopEmbedder, add_args("visible")).unwrap();
    // Append an event the database has never seen.
    events::append_event(&paths.events, &upsert("invisible", "not applied")).unwrap();

    {
        let conn = open_ro(&paths.db).unwrap();
        assert!(
            matches!(cursor::inspect(&conn, &paths), Decision::ReplayTail { .. }),
            "the read path must detect that it is behind"
        );
    }

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let lock_path = paths.lock.clone();
    let holder = std::thread::spawn(move || {
        let _lock = acquire_lock(&lock_path).unwrap();
        tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    rx.recv().unwrap();

    // With the write lock held elsewhere, the read still completes.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let read_paths = paths.clone();
    std::thread::spawn(move || {
        let conn = open_ro(&read_paths.db).unwrap();
        cursor::warn_if_behind(&conn, &read_paths);
        let mut stmt = conn
            .prepare("SELECT id FROM entries WHERE is_stale=0 ORDER BY id")
            .unwrap();
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        done_tx.send(ids).unwrap();
    });
    let ids = done_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the read path blocked on the write lock");
    assert_eq!(ids, vec!["visible".to_string()], "reads serve what they have");

    release_tx.send(()).unwrap();
    holder.join().unwrap();
}

// ---------------------------------------------------------------------------
// 5. CE7 — a compacted log bumps the generation and forces a full rebuild
// ---------------------------------------------------------------------------

/// `CompactMaterialize.tla` CE7, reconstructed literally: no crash, and no
/// writer that skips the cursor. The generation bump is the whole mechanism.
#[test]
fn test_compaction_bumps_the_generation_and_forces_a_full_rebuild() {
    let (_dir, paths) = repo();

    // Append upsert A twice; apply through the cursor.
    events::append_events_batch(&paths.events, &[upsert("A", "a1"), upsert("A", "a2")]).unwrap();
    recover_if_needed(&paths, &FixedEmbedder).unwrap();
    assert_eq!(live_ids(&paths), vec!["A".to_string()]);
    let cursor_before = cursor_of(&paths).unwrap();

    // Append upsert B and a third upsert A. Do NOT apply them.
    events::append_events_batch(&paths.events, &[upsert("B", "b1"), upsert("A", "a3")]).unwrap();

    // Compact: the superseded A upserts are dropped, so the surviving log is
    // [B, A] — shorter than the cursor's offset is long, and rewritten under it.
    let generation_before = cursor::read_generation(&paths.events);
    kb::commands::compact::Compact
        .execute_with_paths(&paths)
        .unwrap();
    assert_eq!(
        cursor::read_generation(&paths.events),
        generation_before + 1,
        "compaction must bump the log generation under its rename lock"
    );

    // Reopen. The generation check — not the tail hash, and not a crash —
    // is what makes this a full rebuild.
    {
        let conn = open_db(&paths.db).unwrap();
        assert_eq!(
            cursor::inspect(&conn, &paths),
            Decision::FullRebuild(RebuildReason::GenerationMismatch),
        );
        assert_ne!(cursor_before.generation, cursor::read_generation(&paths.events));
    }
    assert!(recover_if_needed(&paths, &FixedEmbedder).unwrap());

    // Without the generation check, recovery would have replayed the compacted
    // tail onto a database that already held A, and B would be missing.
    assert_eq!(live_ids(&paths), vec!["A".to_string(), "B".to_string()]);
    let conn = open_db(&paths.db).unwrap();
    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
}

/// The generation check fires on its own, with the log's bytes untouched —
/// proof that it is not the tail hash doing the work in CE7.
#[test]
fn test_generation_mismatch_alone_forces_a_full_rebuild() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &NoopEmbedder, add_args("e1")).unwrap();
    let before = fs::read(&paths.events).unwrap();

    cursor::bump_generation(&paths.events).unwrap();
    assert_eq!(fs::read(&paths.events).unwrap(), before, "log bytes unchanged");

    let conn = open_db(&paths.db).unwrap();
    assert_eq!(
        cursor::inspect(&conn, &paths),
        Decision::FullRebuild(RebuildReason::GenerationMismatch)
    );
}

// ---------------------------------------------------------------------------
// 6. The rest of the eight-row table
// ---------------------------------------------------------------------------

#[test]
fn test_cursorless_database_takes_the_full_rebuild_row() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    {
        let conn = open_db(&paths.db).unwrap();
        conn.execute(
            "DELETE FROM kb_meta WHERE key LIKE 'applied_log_%'",
            [],
        )
        .unwrap();
        assert_eq!(
            cursor::inspect(&conn, &paths),
            Decision::FullRebuild(RebuildReason::CursorMissing)
        );
    }
    assert!(recover_if_needed(&paths, &FixedEmbedder).unwrap());
    let conn = open_db(&paths.db).unwrap();
    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
    assert!(
        cursor::read(&conn).unwrap().is_some(),
        "a rebuild must write the cursor rows into the swapped-in database (T5b)"
    );
}

#[test]
fn test_obsolete_schema_stamp_takes_the_full_rebuild_row() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    let conn = open_db(&paths.db).unwrap();
    conn.execute("UPDATE kb_meta SET value='1' WHERE key='schema_version'", [])
        .unwrap();
    assert_eq!(
        cursor::inspect(&conn, &paths),
        Decision::FullRebuild(RebuildReason::SchemaObsolete)
    );
}

/// `DurableBatch-counterexamples.md` §5 obligation 2: the model cannot reach
/// this row, so it is covered with a fabricated cursor instead.
#[test]
fn test_offset_beyond_the_log_takes_the_full_rebuild_row() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    {
        let conn = open_db(&paths.db).unwrap();
        let mut fabricated = cursor::read(&conn).unwrap().unwrap();
        fabricated.offset += 4096;
        cursor::write(&conn, &fabricated).unwrap();
        assert_eq!(
            cursor::inspect(&conn, &paths),
            Decision::FullRebuild(RebuildReason::OffsetBeyondLog)
        );
    }
    assert!(recover_if_needed(&paths, &FixedEmbedder).unwrap());
    let conn = open_db(&paths.db).unwrap();
    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
}

#[test]
fn test_rewritten_bytes_at_the_offset_take_the_full_rebuild_row() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    let conn = open_db(&paths.db).unwrap();
    let mut fabricated = cursor::read(&conn).unwrap().unwrap();
    fabricated.tail_sha = "0".repeat(64);
    cursor::write(&conn, &fabricated).unwrap();
    assert_eq!(
        cursor::inspect(&conn, &paths),
        Decision::FullRebuild(RebuildReason::TailShaMismatch)
    );
}

/// Row 6. A malformed line in the MIDDLE of the log hard-errors in the reader;
/// recovery must defer with a warning rather than take down every entry point.
#[test]
fn test_unreadable_log_defers_with_a_warning() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    let mut raw = fs::read_to_string(&paths.events).unwrap();
    raw.push_str("{ this is not json }\n");
    raw.push_str(&format!("{}\n", upsert("later", "after the damage")));
    fs::write(&paths.events, raw).unwrap();

    {
        let conn = open_db(&paths.db).unwrap();
        assert!(
            matches!(cursor::inspect(&conn, &paths), Decision::Defer(_)),
            "an unreadable log must defer, not fail"
        );
    }
    // Every entry point still works: no error, no rebuild.
    assert!(!recover_if_needed(&paths, &FixedEmbedder).unwrap());
}

/// A cursor at offset 0 against a populated database is the fresh-database and
/// downgraded-binary shape: the log replays from the start and idempotent
/// arms make the already-applied prefix a no-op.
#[test]
fn test_cursor_at_offset_zero_against_a_populated_database() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("e1")).unwrap();
    kb_core::add(&paths, &FixedEmbedder, add_args("e2")).unwrap();
    let expected = {
        let conn = open_db(&paths.db).unwrap();
        materialized(&conn)
    };

    {
        let conn = open_db(&paths.db).unwrap();
        cursor::write(
            &conn,
            &Cursor {
                generation: cursor::read_generation(&paths.events),
                offset: 0,
                tail_sha: cursor::tail_sha(&paths.events, 0).unwrap(),
            },
        )
        .unwrap();
    }
    recover_if_needed(&paths, &FixedEmbedder).unwrap();

    let conn = open_db(&paths.db).unwrap();
    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
    assert_eq!(
        materialized(&conn),
        expected,
        "replaying the whole log onto a populated database must be a no-op"
    );
}

/// A rebuild over an empty log leaves a usable, cursored database rather than
/// looping on the cursorless row.
#[test]
fn test_rebuild_over_an_empty_log_leaves_a_current_cursor() {
    let (_dir, paths) = repo();
    fs::write(&paths.events, b"").unwrap();
    db::open_or_init(&paths).unwrap();
    (Rebuild).execute_with(&paths, &FixedEmbedder).unwrap();

    let conn = open_db(&paths.db).unwrap();
    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
}

// ---------------------------------------------------------------------------
// 7. Poison policy
// ---------------------------------------------------------------------------

/// An embedder that fails on one specific entry's text: a deterministic apply
/// failure, exactly `DurableBatch.tla`'s `PoisonBatch`.
struct PoisonEmbedder;
impl Embedder for PoisonEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        if text.contains("poison") {
            anyhow::bail!("embedder is down for this record");
        }
        Ok(vec![0.1; 384])
    }
}

#[test]
fn test_poison_record_is_quarantined_after_k_attempts_and_the_cursor_advances() {
    let (_dir, paths) = repo();
    kb_core::add(&paths, &FixedEmbedder, add_args("healthy")).unwrap();

    let mut bad = upsert("bad", "poison");
    bad["content"] = json!("poison content");
    events::append_events_batch(&paths.events, &[bad.clone(), upsert("after", "fine")]).unwrap();
    let committed_len = events::read_events(&paths.events).unwrap().committed_len;

    recover_if_needed(&paths, &PoisonEmbedder).unwrap();

    let ledger = DeadLetter::load(&paths.events);
    let record = ledger
        .records
        .get(&cursor::fingerprint(&bad))
        .expect("the failing record must be dead-lettered");
    assert!(record.quarantined, "it must be quarantined");
    assert_eq!(record.attempts, POISON_MAX_ATTEMPTS);

    // The cursor advanced past it and the rest of the tail applied.
    let cursor = cursor_of(&paths).unwrap();
    assert_eq!(cursor.offset, committed_len);
    assert_eq!(
        live_ids(&paths),
        vec!["after".to_string(), "healthy".to_string()],
        "the poison record is skipped; everything else lands"
    );

    // And every entry point stays alive rather than replaying the poison.
    let conn = open_db(&paths.db).unwrap();
    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
}

#[test]
fn test_a_full_rebuild_skips_quarantined_records() {
    let (_dir, paths) = repo();
    let mut bad = upsert("bad", "poison");
    bad["content"] = json!("poison content");
    events::append_events_batch(&paths.events, &[upsert("good", "fine"), bad.clone()]).unwrap();
    recover_if_needed(&paths, &PoisonEmbedder).unwrap();
    assert!(DeadLetter::load(&paths.events)
        .quarantined()
        .contains(&cursor::fingerprint(&bad)));

    // A rebuild that re-applied the dead-lettered event would fail on exactly
    // the record recovery already gave up on.
    (Rebuild).execute_with(&paths, &PoisonEmbedder).unwrap();
    assert_eq!(live_ids(&paths), vec!["good".to_string()]);
}

// ---------------------------------------------------------------------------
// 8. No write transaction is held across an embedder call
// ---------------------------------------------------------------------------

/// Probes for an open write transaction from a SECOND connection: with
/// `busy_timeout` at zero, `BEGIN IMMEDIATE` returns SQLITE_BUSY exactly when
/// another connection holds the write lock on the database.
struct TxProbeEmbedder {
    db_path: std::path::PathBuf,
    calls: std::sync::atomic::AtomicUsize,
    saw_write_transaction: std::sync::atomic::AtomicBool,
}

impl Embedder for TxProbeEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        use std::sync::atomic::Ordering;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let probe = Connection::open(&self.db_path).unwrap();
        probe.busy_timeout(std::time::Duration::from_millis(0)).unwrap();
        match probe.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {
                let _ = probe.execute_batch("ROLLBACK");
            }
            Err(_) => {
                self.saw_write_transaction.store(true, Ordering::SeqCst);
            }
        }
        Ok(vec![0.1; 384])
    }
}

#[test]
fn test_no_write_transaction_is_held_across_an_embedder_call() {
    use std::sync::atomic::Ordering;
    let (_dir, paths) = repo();
    db::open_or_init(&paths).unwrap();
    let probe = TxProbeEmbedder {
        db_path: paths.db.clone(),
        calls: std::sync::atomic::AtomicUsize::new(0),
        saw_write_transaction: std::sync::atomic::AtomicBool::new(false),
    };

    let mut args = add_args("probe");
    args.cues = vec!["cue one".to_string(), "cue two".to_string()];
    kb_core::add(&paths, &probe, args).unwrap();

    assert!(
        probe.calls.load(Ordering::SeqCst) >= 3,
        "the probe must actually have been asked to embed (entry text + 2 cues)"
    );
    assert!(
        !probe.saw_write_transaction.load(Ordering::SeqCst),
        "an embedder call happened while a write transaction was open"
    );
}

/// The prefetch is sealed at BEGIN, so a text the arm needs but the prefetch
/// missed is a loud error instead of a silent model call inside the
/// transaction.
#[test]
fn test_sealed_prefetch_rejects_an_unresolved_text() {
    use kb::components::embedder::PrefetchedEmbedder;
    let prefetched = PrefetchedEmbedder::prefetch(&FixedEmbedder, vec!["known".to_string()]).unwrap();
    assert!(prefetched.embed("known").is_ok());
    assert!(prefetched.embed("unknown").is_ok(), "open before sealing");
    prefetched.seal();
    assert!(prefetched.embed("known").is_ok(), "cached texts still resolve");
    let error = prefetched.embed("unknown").unwrap_err().to_string();
    assert!(
        error.contains("applied-cursor transaction"),
        "unexpected error: {error}"
    );
}
