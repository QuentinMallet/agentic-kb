//! Criterion benchmark: kb_search end-to-end with inline evidence verification.
//!
//! Measures p50/p95/p99 latency of `search_entries` over N=50 entries where
//! each entry has one "code" evidence row pointing to a real file in the repo.
//! Uses `NoopEmbedder` — no model download, FTS-only search.
//!
//! AC23-AC25 (br-jwe.22, T-C2).

use criterion::{criterion_group, criterion_main, Criterion};
use kb::components::db::{apply_event, open_db_memory, search_entries, SearchOptions};
use kb::components::embedder::NoopEmbedder;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tempfile::TempDir;

/// Number of entries inserted into the bench DB.
const N: usize = 50;

/// A stable file in the repo with a known byte range and its sha256.
/// Cargo.toml bytes 0..200 — stable enough for a bench reference.
const CITATION_FILE: &str = "Cargo.toml";
const CITATION_START: usize = 0;
const CITATION_END: usize = 200;
// Pre-computed: sha256(Cargo.toml[0:200]).
// Recompute with: python3 -c "import hashlib; d=open('Cargo.toml','rb').read(); print(hashlib.sha256(d[0:200]).hexdigest())"
const CITATION_HASH: &str =
    "c2b77be7fb1c2b48453aa7cc8e3fedffdbf9bc595f778590d0d9d40a2971021c";

/// Resolve the repo root (worktree root = dir containing Cargo.toml).
/// The bench binary is always run from the worktree root via `cargo bench`.
fn repo_root() -> PathBuf {
    std::env::current_dir().expect("cwd unavailable")
}

/// Build a temporary DB with N entries, each with one code-evidence row
/// pointing to CITATION_FILE[CITATION_START..CITATION_END].
fn build_bench_db() -> (rusqlite::Connection, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open_db_memory().expect("open in-memory DB");
    let embedder = NoopEmbedder;

    // Verify the citation file is present and the hash matches so the bench
    // result is meaningful (wrong hash → all verified=false, still valid perf test).
    let root = repo_root();
    let citation_abs = root.join(CITATION_FILE);
    if citation_abs.exists() {
        let bytes = std::fs::read(&citation_abs).unwrap_or_default();
        if CITATION_END <= bytes.len() {
            let mut h = Sha256::new();
            h.update(&bytes[CITATION_START..CITATION_END]);
            let actual = format!("{:x}", h.finalize());
            assert_eq!(
                actual, CITATION_HASH,
                "CITATION_HASH is stale — recompute with python3 snippet in bench header"
            );
        }
    }

    for i in 0..N {
        // Upsert entry
        let entry_id = format!("bench-entry-{i:03}");
        let ev = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": entry_id,
            "path": format!("gotchas/bench-{i:03}"),
            "summary": format!("bench entry number {i} for search verification test"),
            "content": format!("content for bench entry {i}: test bench verification criterion search"),
            "tags": ["bench", "test"],
            "kind": "observation",
            "evidence_status": "present",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &ev).expect("apply upsert");

        // Attach one code-evidence row
        let ev_id = format!("bench-ev-{i:03}");
        let citation_path = format!("{CITATION_FILE}:{CITATION_START}-{CITATION_END}");
        let evidence_ev = serde_json::json!({
            "action": "evidence_add",
            "table": "evidence",
            "entry_id": entry_id,
            "evidence": {
                "id": ev_id,
                "entry_id": entry_id,
                "kind": "code",
                "citation_path": citation_path,
                "citation_sha": null,
                "citation_hash": CITATION_HASH,
                "citation_excerpt": null,
                "derived_from": null,
                "recorded_at": "2024-01-01T00:00:00Z"
            },
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &evidence_ev).expect("apply evidence_add");
    }

    (conn, dir)
}

fn bench_kb_search_with_verification(c: &mut Criterion) {
    // Build DB once outside the iter loop — we re-use it across iterations.
    // criterion's `.iter()` closure must be Send, so we pre-build here.
    let (conn, _dir) = build_bench_db();
    let embedder = NoopEmbedder;
    let opts = SearchOptions {
        limit: 10,
        do_fts: true,
        do_semantic: false, // NoopEmbedder skips semantic path anyway
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 10,
        repo_root: None,
        verify_pool_size: None,
        recency_lambda: 0.0,
        mmr_lambda: 0.0,
    };

    c.bench_function("bench_kb_search_with_verification", |b| {
        b.iter(|| {
            let results =
                search_entries(&conn, &embedder, "bench test", &opts).expect("search_entries");
            // Prevent optimizer from eliding the call.
            criterion::black_box(results);
        });
    });
}

criterion_group! {
    name = verification_benches;
    config = Criterion::default().sample_size(10);
    targets = bench_kb_search_with_verification
}
criterion_main!(verification_benches);
