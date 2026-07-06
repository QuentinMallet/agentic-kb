//! Recency-bias post-RRF multiplier tests (T6 acceptance gate).
//!
//! Three invariants:
//!   1. λ=0.0 produces identical ordering to baseline (byte-identical skip path).
//!   2. λ>0 causes a newer entry to rank above an identical-RRF older entry.
//!   3. score_kind field remains "rrf" after decay multiplication.

use kb::components::db::{apply_event, open_db_memory, search_entries, SearchOptions};
use kb::components::embedder::Embedder;
use serde_json::json;

const EMBED_DIM: usize = 384;

/// Minimal deterministic embedder for tests.
/// Returns a fixed 384-dim unit vector so the semantic lane is active.
/// All entries get the same vector → identical cosine similarity → identical
/// semantic rank → RRF scores differ only by FTS rank, making decay the
/// only discriminant after equal-RRF.
struct FixedEmbedder;

impl Embedder for FixedEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        // Constant unit-norm 384-dim vector: each element = 1/sqrt(384).
        let v = 1.0 / (EMBED_DIM as f32).sqrt();
        Ok(vec![v; EMBED_DIM])
    }

    fn is_noop(&self) -> bool {
        false
    }
}

fn make_entry(id: &str, path: &str, summary: &str, tags: &[&str]) -> serde_json::Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": path,
        "summary": summary,
        "content": format!("content for {}", id),
        "tags": tags,
        "kind": "observation",
        "evidence_status": "missing",
        "permanent": false,
        "is_stale": false,
        "ts": "2024-01-01T00:00:00Z",
        "session_id": null,
    })
}

/// Insert two entries then manually update updated_at so we control recency.
///
/// `old-entry`: updated 200 days ago.
/// `new-entry`: updated 1 day ago.
///
/// Both have the same FTS-matchable summary token "recencytest", so they
/// receive identical FTS+semantic rank contributions → identical RRF scores
/// before decay.  After λ>0 decay the newer entry must rank first.
fn setup_db_with_dated_entries() -> rusqlite::Connection {
    let conn = open_db_memory().unwrap();
    let emb = FixedEmbedder;

    apply_event(
        &conn,
        &emb,
        &make_entry("old-entry", "bench/old", "recencytest old observation", &["recencytest"]),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &make_entry("new-entry", "bench/new", "recencytest new observation", &["recencytest"]),
    )
    .unwrap();

    // Override updated_at to control recency.
    conn.execute(
        "UPDATE entries SET updated_at = datetime('now', '-200 days') WHERE id = 'old-entry'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE entries SET updated_at = datetime('now', '-1 day') WHERE id = 'new-entry'",
        [],
    )
    .unwrap();

    conn
}

fn hybrid_opts(recency_lambda: f32) -> SearchOptions {
    SearchOptions {
        limit: 10,
        do_fts: true,
        do_semantic: true,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
        verify_pool_size: None,
        recency_lambda,
        mmr_lambda: 0.0,
    }
}

/// Test 1: λ=0.0 produces identical ordering across two identical calls.
///
/// Confirms the skip-path is stable — no non-determinism introduced.
#[test]
fn test_lambda_zero_stable_ordering() {
    let conn = setup_db_with_dated_entries();
    let emb = FixedEmbedder;

    let opts = hybrid_opts(0.0);
    let results_a = search_entries(&conn, &emb, "recencytest", &opts).unwrap();
    let results_b = search_entries(&conn, &emb, "recencytest", &opts).unwrap();

    assert!(!results_a.is_empty(), "must return results");
    let ids_a: Vec<&str> = results_a.iter().map(|r| r.id.as_str()).collect();
    let ids_b: Vec<&str> = results_b.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids_a, ids_b, "λ=0.0 must produce identical ordering across calls");
}

/// Test 2: λ>0 causes the newer entry to rank above the identical-RRF older entry.
///
/// With λ=0.1:
///   exp(-0.1 * 1)   ≈ 0.905  (new-entry decay factor)
///   exp(-0.1 * 200) ≈ 2e-9   (old-entry decay factor)
///
/// new-entry score >> old-entry score → new-entry must appear first.
#[test]
fn test_lambda_positive_newer_ranks_higher() {
    let conn = setup_db_with_dated_entries();
    let emb = FixedEmbedder;

    let opts = hybrid_opts(0.1);
    let results = search_entries(&conn, &emb, "recencytest", &opts).unwrap();

    assert!(results.len() >= 2, "must return at least 2 entries");
    assert_eq!(
        results[0].id, "new-entry",
        "with λ=0.1 the newer entry (1 day old) must outrank the older entry (200 days old); \
         order was: {:?}",
        results.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
}

/// Test 3: score_kind field remains "rrf" after decay multiplication.
#[test]
fn test_score_kind_stays_rrf_after_decay() {
    let conn = setup_db_with_dated_entries();
    let emb = FixedEmbedder;

    let opts = hybrid_opts(0.1);
    let results = search_entries(&conn, &emb, "recencytest", &opts).unwrap();

    assert!(!results.is_empty(), "must return results");
    for r in &results {
        assert_eq!(
            r.score_kind, "rrf",
            "score_kind must remain 'rrf' after recency-bias decay; got '{}' for entry '{}'",
            r.score_kind, r.id
        );
    }
}
