use criterion::{criterion_group, criterion_main, Criterion};
use kb::components::{db, embedder::NoopEmbedder};
use kb::config;
use rusqlite::params;
use serde_json::json;
use std::fs;
use tempfile::tempdir;

fn setup_bench_db() -> (tempfile::TempDir, config::Paths) {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
    let paths = config::Paths::from_root(root);
    let emb = NoopEmbedder;

    let conn = db::open_db(&paths.db).unwrap();

    // Seed 50 entries via apply_event
    for i in 0..50 {
        let ev = json!({
            "action": "upsert", "table": "entries",
            "id": format!("bench-{:03}", i),
            "path": format!("bench/entry-{}", i),
            "summary": format!("bench entry number {}", i),
            "content": format!("content for entry {}", i),
            "tags": ["bench"],
            "kind": "observation",
            "evidence_status": if i < 10 { "present" } else { "missing" },
            "permanent": false,
            "is_stale": false,
            "ts": "2024-01-01T00:00:00Z",
            "session_id": null,
        });
        db::apply_event(&conn, &emb, &ev).unwrap();
    }

    // Add audit history for 10 entries (varies successes/failures)
    for i in 0..10 {
        let successes = i as i64 + 1;
        let failures = (10 - i) as i64;
        conn.execute(
            "INSERT INTO source_weights(kind, session_id, successes, failures)
             VALUES('observation', '__GLOBAL__', ?1, ?2)
             ON CONFLICT(kind, session_id) DO UPDATE SET successes=successes+?1, failures=failures+?2",
            params![successes, failures],
        ).unwrap();
    }

    (dir, paths)
}

fn bench_search_confidence(c: &mut Criterion) {
    let (_dir, paths) = setup_bench_db();
    let emb = NoopEmbedder;
    let conn = db::open_db(&paths.db).unwrap();

    let opts_with = db::SearchOptions {
        limit: 20,
        do_fts: true,
        do_semantic: false,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
    };

    // Baseline: same query, no source_weights rows (would be identical structure)
    // We measure the query including the confidence prefetch
    c.bench_function("kb_search_confidence_enabled_50entries", |b| {
        b.iter(|| {
            db::search_entries(&conn, &emb, "bench entry", &opts_with).unwrap()
        });
    });
}

criterion_group!(benches, bench_search_confidence);
criterion_main!(benches);
