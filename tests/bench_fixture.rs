use kb::bench_fixture::{seed_fixture, BenchEmbedder, DEFAULT_SEED};
use kb::commands::add::acquire_lock;
use kb::components::cursor::{self, Decision};
use kb::components::db;
use kb::components::events;
use kb::components::kb_core::{self, AddArgs};
use serde_json::json;
use std::fs;

#[test]
fn seeded_benchmark_fixture_is_converged_and_accepts_add() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let (paths, unlocked_conn) = db::test_db(dir.path());
    drop(unlocked_conn);
    let lock = acquire_lock(&paths.lock).unwrap();
    let conn = db::open_rw(&paths, &lock).unwrap();
    let embedder = BenchEmbedder::new(DEFAULT_SEED);

    seed_fixture(&lock, &conn, &paths, &embedder, 50, DEFAULT_SEED).unwrap();

    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
    assert!(paths.events.is_file());
    let log = fs::read_to_string(&paths.events).unwrap();
    assert!(log.contains("\"action\":\"batch_begin\""));
    assert!(log.contains("\"action\":\"batch_commit\""));
    let committed_len = events::read_events(&paths.events).unwrap().committed_len;
    assert_eq!(cursor::read(&conn).unwrap().unwrap().offset, committed_len);

    drop(conn);
    drop(lock);
    kb_core::add(
        &paths,
        &embedder,
        AddArgs {
            id: "after-fixture".to_string(),
            path: "bench/after-fixture".to_string(),
            summary: "write after fixture seed".to_string(),
            content: "the converged fixture accepts a sanctioned write".to_string(),
            tags: json!(["bench"]),
            version_ref: None,
            permanent: false,
            replace_path: false,
            kind: "observation".to_string(),
            evidence_status: "missing".to_string(),
            evidence_rows: vec![],
            ts: "2024-01-02T00:00:00Z".to_string(),
            session: "test".to_string(),
            session_id: None,
            expire_reason: "replaced by test".to_string(),
            dedup_cutoff: None,
            cues: vec![],
        },
    )
    .unwrap();
}
