//! MMR result diversification tests (Memora pickup .6 acceptance gate).
//!
//! Memora's reward penalizes redundancy (mean pairwise cosine of the result
//! set). Here: optional MMR re-rank after RRF+recency. Greedy selection
//! maximizes λ·relevance − (1−λ)·max_cosine_to_already_selected over a
//! 2×limit candidate pool.
//!
//! Invariants:
//!   1. mmr_lambda=0.0 → byte-identical ordering to the pre-MMR path.
//!   2. With two near-identical top entries and limit=2, MMR swaps the
//!      redundant twin for the diverse third entry.
//!   3. The top-1 result (highest RRF) is never displaced.
//!   4. score_kind stays "rrf".

use kb::components::db::{apply_event, open_db_memory, search_entries, SearchOptions};
use kb::components::embedder::Embedder;
use serde_json::json;

const DIM: usize = 384;

/// Two-cluster embedder: texts containing "alnote" → e0, others → e1.
struct ClusterEmbedder;

impl Embedder for ClusterEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = vec![0.0f32; DIM];
        if text.contains("alnote") {
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

fn make_entry(id: &str, path: &str, summary: &str) -> serde_json::Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": path,
        "summary": summary,
        "content": format!("body {}", id),
        "tags": ["mmrtest"],
        "kind": "observation",
        "evidence_status": "missing",
        "permanent": false,
        "is_stale": false,
        "ts": "2024-01-01T00:00:00Z",
        "session_id": null,
    })
}

/// Corpus: twins a1/a2 (same embedding cluster "alnote", strong FTS match)
/// and b1 (other cluster, weaker FTS = one shared token).
///
/// All summaries share the token "shared". Query "shared alnote topic" gives:
///   FTS lane: a1, a2 rank above b1 (two matched tokens vs one)
///   semantic lane: a1, a2 cosine 1.0; b1 cosine 0.0
/// → RRF order: a1, a2, b1.
fn setup() -> rusqlite::Connection {
    let conn = open_db_memory().unwrap();
    let emb = ClusterEmbedder;
    apply_event(
        &conn,
        &emb,
        &make_entry("a1", "n/a1", "shared alnote topic one"),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &make_entry("a2", "n/a2", "shared alnote topic two"),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &make_entry("b1", "n/b1", "shared other subject"),
    )
    .unwrap();
    conn
}

fn opts(limit: usize, mmr_lambda: f32) -> SearchOptions {
    SearchOptions {
        limit,
        do_fts: true,
        do_semantic: true,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
        verify_pool_size: None,
        recency_lambda: 0.0,
        mmr_lambda,
    }
}

/// Invariant 1: λ=0.0 keeps the plain RRF ordering (a1 or a2 first, b1 last).
#[test]
fn test_mmr_off_keeps_rrf_order() {
    let conn = setup();
    let results = search_entries(
        &conn,
        &ClusterEmbedder,
        "shared alnote topic",
        &opts(3, 0.0),
    )
    .unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids[2], "b1",
        "without MMR the diverse entry stays last: {ids:?}"
    );
}

/// Invariant 2+3: λ=0.5, limit=2 → top RRF entry kept, redundant twin
/// replaced by the diverse entry.
#[test]
fn test_mmr_swaps_redundant_twin_for_diverse_entry() {
    let conn = setup();

    let baseline = search_entries(
        &conn,
        &ClusterEmbedder,
        "shared alnote topic",
        &opts(2, 0.0),
    )
    .unwrap();
    let base_ids: Vec<&str> = baseline.iter().map(|r| r.id.as_str()).collect();
    assert!(
        base_ids == ["a1", "a2"] || base_ids == ["a2", "a1"],
        "baseline top-2 must be the twins, got {base_ids:?}"
    );
    let top1 = base_ids[0].to_string();

    let diversified = search_entries(
        &conn,
        &ClusterEmbedder,
        "shared alnote topic",
        &opts(2, 0.5),
    )
    .unwrap();
    let ids: Vec<&str> = diversified.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], top1, "MMR must never displace the top-1 result");
    assert_eq!(
        ids[1], "b1",
        "redundant twin must be swapped for the diverse entry, got {ids:?}"
    );
}

/// Invariant 4: score_kind survives the MMR pass.
#[test]
fn test_mmr_score_kind_stays_rrf() {
    let conn = setup();
    let results = search_entries(
        &conn,
        &ClusterEmbedder,
        "shared alnote topic",
        &opts(2, 0.5),
    )
    .unwrap();
    for r in &results {
        assert_eq!(r.score_kind, "rrf");
    }
}
