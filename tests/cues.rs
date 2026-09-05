//! Cue anchor tests (Memora pickup .4 acceptance gate).
//!
//! Cue anchors are agent-supplied semantic entry points ("[Main Entity] +
//! [Key Aspect]", e.g. "recency bias decay") stored as embedded rows linked
//! to their entry. Design verified by .state/agent-kb/tla/CueBatch.tla:
//!   * cues ride the upsert event (no separate cue events)
//!   * apply_event replaces the entry's cue rows transactionally
//!   * expire removes entry + cue rows together
//!
//! Invariants:
//!   1. Upsert with cues → cue rows exist with embeddings.
//!   2. Re-upsert with a different cue set → old rows replaced (TLA S3).
//!   3. Expire → cue rows gone (TLA S2, no orphans).
//!   4. Hybrid search reaches an entry via cue similarity alone (third RRF
//!      lane) when neither FTS nor entry-embedding lanes match.
//!   5. Events without cues field (legacy) behave exactly as before.
//!   6. kb_core::add propagates cues into the upsert event and the DB.

use kb::components::db::{apply_event, open_db_memory, search_entries, SearchOptions};
use kb::components::embedder::Embedder;
use serde_json::json;

const DIM: usize = 384;

/// Deterministic keyword embedder: "cuetok" → e0, everything else → e1.
struct KeywordEmbedder;

impl Embedder for KeywordEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = vec![0.0f32; DIM];
        if text.contains("cuetok") {
            v[0] = 1.0;
        } else {
            v[1] = 1.0;
        }
        Ok(v)
    }
    fn is_noop(&self) -> bool {
        false
    }
}

fn entry_event(id: &str, cues: serde_json::Value) -> serde_json::Value {
    let mut ev = json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": format!("notes/{id}"),
        "summary": "plain summary words",
        "content": "plain body words",
        "tags": ["cuetest"],
        "kind": "observation",
        "evidence_status": "missing",
        "permanent": false,
        "is_stale": false,
        "ts": "2024-01-01T00:00:00Z",
        "session_id": null,
    });
    if !cues.is_null() {
        ev["cues"] = cues;
    }
    ev
}

fn count_cues(conn: &rusqlite::Connection, entry_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM cues WHERE entry_id = ?1",
        rusqlite::params![entry_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// Invariant 1: cue rows land with embeddings.
#[test]
fn test_upsert_with_cues_creates_embedded_rows() {
    let conn = open_db_memory().unwrap();
    let emb = KeywordEmbedder;
    apply_event(
        &conn,
        &emb,
        &entry_event("c1", json!(["cuetok anchor", "other anchor"])),
    )
    .unwrap();

    assert_eq!(count_cues(&conn, "c1"), 2);
    let with_emb: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cues WHERE entry_id='c1' AND embedding IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(with_emb, 2, "cue rows must carry embeddings");
}

/// Invariant 2 (TLA S3): re-upsert replaces the cue set wholesale.
#[test]
fn test_reupsert_replaces_cue_rows() {
    let conn = open_db_memory().unwrap();
    let emb = KeywordEmbedder;
    apply_event(
        &conn,
        &emb,
        &entry_event("c2", json!(["old anchor one", "old anchor two"])),
    )
    .unwrap();
    apply_event(&conn, &emb, &entry_event("c2", json!(["new anchor"]))).unwrap();

    assert_eq!(count_cues(&conn, "c2"), 1, "old cue rows must be replaced");
    let cue: String = conn
        .query_row("SELECT cue FROM cues WHERE entry_id='c2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cue, "new anchor");
}

/// Invariant 3 (TLA S2): expire removes cue rows with the entry.
#[test]
fn test_expire_removes_cue_rows() {
    let conn = open_db_memory().unwrap();
    let emb = KeywordEmbedder;
    apply_event(&conn, &emb, &entry_event("c3", json!(["cuetok anchor"]))).unwrap();
    assert_eq!(count_cues(&conn, "c3"), 1);

    apply_event(
        &conn,
        &emb,
        &json!({"action": "expire", "table": "entries", "id": "c3",
                "reason": "test", "ts": "2024-01-02T00:00:00Z"}),
    )
    .unwrap();
    assert_eq!(
        count_cues(&conn, "c3"),
        0,
        "expire must remove cue rows (no orphans)"
    );
}

/// Invariant 4: the cue lane alone can surface an entry in hybrid search.
///
/// Entry summary/content/tags share no token with the query ("cuetok probe"),
/// and the entry embedding is e1 while the query embeds to e0 — so FTS and
/// the entry-semantic lane both miss. Only the cue "cuetok anchor" (e0)
/// matches. The entry must still be returned.
#[test]
fn test_cue_lane_reaches_entry_hybrid() {
    let conn = open_db_memory().unwrap();
    let emb = KeywordEmbedder;
    apply_event(&conn, &emb, &entry_event("c4", json!(["cuetok anchor"]))).unwrap();
    // Distractor without cues.
    apply_event(&conn, &emb, &entry_event("c5", json!(null))).unwrap();

    let opts = SearchOptions {
        limit: 10,
        do_fts: true,
        do_semantic: true,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
        verify_pool_size: None,
        recency_lambda: 0.0,
        mmr_lambda: 0.0,
    };
    let results = search_entries(&conn, &emb, "cuetok probe", &opts).unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"c4"), "cue lane must surface c4, got {ids:?}");
    assert_eq!(
        ids[0], "c4",
        "cue-matched entry must rank first, got {ids:?}"
    );
}

/// Invariant 5: legacy events without cues field — no rows, no errors.
#[test]
fn test_legacy_event_without_cues() {
    let conn = open_db_memory().unwrap();
    let emb = KeywordEmbedder;
    apply_event(&conn, &emb, &entry_event("c6", json!(null))).unwrap();
    assert_eq!(count_cues(&conn, "c6"), 0);
}

/// Spec Rebuild action (CueBatch.tla): replaying the full JSONL into a fresh
/// DB rematerializes cue rows exactly — after upsert, cue-set replace, and
/// expire, the rebuilt cues table equals the live one.
#[test]
fn test_rebuild_replays_cue_rows() {
    use kb::commands::rebuild::Rebuild;
    use kb::components::kb_core::{add, AddArgs};
    use kb::config::Paths;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());
    let emb = KeywordEmbedder;

    let mk = |id: &str, cues: Vec<String>| AddArgs {
        id: id.into(),
        path: format!("notes/{id}"),
        summary: "summary".into(),
        content: "content".into(),
        tags: serde_json::json!([]),
        version_ref: None,
        permanent: false,
        replace_path: false,
        kind: "belief".into(),
        evidence_status: "missing".into(),
        evidence_rows: vec![],
        ts: "2024-01-01T00:00:00Z".into(),
        session: "test".into(),
        session_id: None,
        expire_reason: String::new(),
        dedup_cutoff: None,
        cues,
    };

    // History: r1 gets cues then a replacing cue set; r2 gets cues then expires.
    add(&paths, &emb, mk("r1", vec!["old anchor".into()])).unwrap();
    add(
        &paths,
        &emb,
        mk("r1", vec!["cuetok kept".into(), "second kept".into()]),
    )
    .unwrap();
    add(&paths, &emb, mk("r2", vec!["doomed anchor".into()])).unwrap();
    {
        let conn = kb::components::db::open_unchecked_for_test(&paths.db).unwrap();
        apply_event(
            &conn,
            &emb,
            &json!({"action": "expire", "table": "entries", "id": "r2",
                    "reason": "test", "ts": "2024-01-02T00:00:00Z"}),
        )
        .unwrap();
        // The expire above bypassed the event log (DB-only) — append it so the
        // log is the source of truth the rebuild will replay.
        kb::components::events::append_event(
            &paths.events,
            &json!({"action": "expire", "table": "entries", "id": "r2",
                    "reason": "test", "ts": "2024-01-02T00:00:00Z"}),
        )
        .unwrap();
    }

    let live_cues = |paths: &Paths| -> Vec<(String, String)> {
        let conn = rusqlite::Connection::open(&paths.db).unwrap();
        let mut stmt = conn
            .prepare("SELECT entry_id, cue FROM cues ORDER BY entry_id, cue")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    let before = live_cues(&paths);
    assert_eq!(
        before,
        vec![
            ("r1".to_string(), "cuetok kept".to_string()),
            ("r1".to_string(), "second kept".to_string()),
        ],
        "pre-rebuild state must be the replaced cue set only"
    );

    // Rebuild from the event log into a fresh DB (spec Rebuild action).
    (Rebuild).execute_with(&paths, &emb).unwrap();

    let after = live_cues(&paths);
    assert_eq!(after, before, "rebuild must rematerialize cue rows exactly");
}

/// Invariant 6: kb_core::add carries cues end-to-end (event + DB).
#[test]
fn test_kb_core_add_propagates_cues() {
    use kb::components::kb_core::{add, AddArgs};
    use kb::config::Paths;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());
    let emb = KeywordEmbedder;

    add(
        &paths,
        &emb,
        AddArgs {
            id: "cue-add-1".into(),
            path: "notes/cue-add".into(),
            summary: "summary".into(),
            content: "content".into(),
            tags: json!([]),
            version_ref: None,
            permanent: false,
            replace_path: false,
            kind: "belief".into(),
            evidence_status: "missing".into(),
            evidence_rows: vec![],
            ts: "2024-01-01T00:00:00Z".into(),
            session: "test".into(),
            session_id: None,
            expire_reason: String::new(),
            dedup_cutoff: None,
            cues: vec!["cuetok anchor".into()],
        },
    )
    .unwrap();

    // Event log carries the cues field.
    let log = kb::components::events::read_events(&paths.events).unwrap();
    let last = log.events.last().unwrap();
    assert_eq!(last["cues"], json!(["cuetok anchor"]));

    // DB has the cue row.
    let conn = rusqlite::Connection::open(&paths.db).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cues WHERE entry_id='cue-add-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}
