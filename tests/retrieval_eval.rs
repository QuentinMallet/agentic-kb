//! Retrieval eval harness tests (Memora pickup .2 acceptance gate).
//!
//! Invariants:
//!   1. recall@k and MRR computed correctly on a known corpus (FTS lane, deterministic).
//!   2. Perfect golden set → recall@k = 1.0, MRR = 1.0.
//!   3. Unfindable expected id → recall@k = 0.0 for that case, MRR contribution 0.
//!   4. Golden JSONL parser: skips blank/# lines, rejects empty expected_ids.
//!   5. Metrics always within [0, 1].

use kb::components::db::{apply_event, open_db_memory, SearchOptions};
use kb::components::embedder::NoopEmbedder;
use kb::components::retrieval_eval::{evaluate, parse_golden_jsonl, GoldenCase};
use serde_json::json;

fn make_entry(id: &str, path: &str, summary: &str, content: &str) -> serde_json::Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": path,
        "summary": summary,
        "content": content,
        "tags": ["eval"],
        "kind": "observation",
        "evidence_status": "missing",
        "permanent": false,
        "is_stale": false,
        "ts": "2024-01-01T00:00:00Z",
        "session_id": null,
    })
}

/// Corpus with three distinctly-tokenized entries so FTS matching is exact:
///   e-auth    ↔ "authentication jwt tokens"
///   e-search  ↔ "hybrid search ranking"
///   e-deploy  ↔ "deployment nixos flake"
fn setup_corpus() -> rusqlite::Connection {
    let conn = open_db_memory().unwrap();
    let emb = NoopEmbedder;
    for (id, path, summary, content) in [
        ("e-auth", "src/auth", "authentication jwt tokens", "verifies bearer jwt"),
        ("e-search", "src/search", "hybrid search ranking", "rrf fusion of lanes"),
        ("e-deploy", "ops/deploy", "deployment nixos flake", "nix build pipeline"),
    ] {
        apply_event(&conn, &emb, &make_entry(id, path, summary, content)).unwrap();
    }
    conn
}

fn fts_opts(k: usize) -> SearchOptions {
    SearchOptions {
        limit: k,
        do_fts: true,
        do_semantic: false,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
        verify_pool_size: None,
        recency_lambda: 0.0,
        mmr_lambda: 0.0,
    }
}

/// Test 1+2: every query finds its entry at rank 1 → recall@k = 1.0, MRR = 1.0.
#[test]
fn test_perfect_golden_set_scores_one() {
    let conn = setup_corpus();
    let cases = vec![
        GoldenCase { query: "authentication jwt".into(), expected_ids: vec!["e-auth".into()] },
        GoldenCase { query: "hybrid search ranking".into(), expected_ids: vec!["e-search".into()] },
        GoldenCase { query: "deployment nixos".into(), expected_ids: vec!["e-deploy".into()] },
    ];
    let report = evaluate(&conn, &NoopEmbedder, &cases, &fts_opts(10)).unwrap();

    assert_eq!(report.per_case.len(), 3);
    assert!((report.recall_at_k() - 1.0).abs() < 1e-9, "recall@10 must be 1.0, got {}", report.recall_at_k());
    assert!((report.mrr() - 1.0).abs() < 1e-9, "MRR must be 1.0, got {}", report.mrr());
    for c in &report.per_case {
        assert_eq!(c.first_rank, Some(1), "expected id must be rank 1 for query '{}'", c.query);
    }
}

/// Test 3: one case expects an id that no query token matches → its recall is 0,
/// aggregate recall = mean(1.0, 0.0) = 0.5; MRR = mean(1.0, 0.0) = 0.5.
#[test]
fn test_miss_case_lowers_aggregates() {
    let conn = setup_corpus();
    let cases = vec![
        GoldenCase { query: "authentication jwt".into(), expected_ids: vec!["e-auth".into()] },
        GoldenCase { query: "zzz nonexistent query".into(), expected_ids: vec!["e-deploy".into()] },
    ];
    let report = evaluate(&conn, &NoopEmbedder, &cases, &fts_opts(10)).unwrap();

    assert!((report.recall_at_k() - 0.5).abs() < 1e-9, "recall must be 0.5, got {}", report.recall_at_k());
    assert!((report.mrr() - 0.5).abs() < 1e-9, "MRR must be 0.5, got {}", report.mrr());
    assert_eq!(report.per_case[1].hits, 0);
    assert_eq!(report.per_case[1].first_rank, None);
}

/// Multi-expected case: query matches both e-auth (rank 1 or 2) and e-search.
/// With expected = {e-auth, e-deploy} and only e-auth findable:
/// recall_case = 1/2, first_rank = rank of e-auth.
#[test]
fn test_partial_hit_multi_expected() {
    let conn = setup_corpus();
    let cases = vec![GoldenCase {
        query: "authentication jwt".into(),
        expected_ids: vec!["e-auth".into(), "e-deploy".into()],
    }];
    let report = evaluate(&conn, &NoopEmbedder, &cases, &fts_opts(10)).unwrap();

    let c = &report.per_case[0];
    assert_eq!(c.expected, 2);
    assert_eq!(c.hits, 1);
    assert!((report.recall_at_k() - 0.5).abs() < 1e-9);
    assert_eq!(c.first_rank, Some(1));
}

/// Test 4: parser skips blank lines and # comments; rejects empty expected_ids.
#[test]
fn test_parse_golden_jsonl() {
    let good = r#"
# retrieval golden set
{"query": "authentication jwt", "expected_ids": ["e-auth"]}

{"query": "hybrid search", "expected_ids": ["e-search", "e-auth"]}
"#;
    let cases = parse_golden_jsonl(good).unwrap();
    assert_eq!(cases.len(), 2);
    assert_eq!(cases[0].query, "authentication jwt");
    assert_eq!(cases[1].expected_ids.len(), 2);

    let bad = r#"{"query": "no expectations", "expected_ids": []}"#;
    assert!(parse_golden_jsonl(bad).is_err(), "empty expected_ids must be rejected");

    let empty = "\n# only comments\n";
    assert!(parse_golden_jsonl(empty).is_err(), "golden set with zero cases must be rejected");
}

/// Test 5 (property): metrics bounded in [0,1] for arbitrary case mixes.
mod prop {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]
        #[test]
        fn prop_metrics_bounded(
            // Random subset of queries, some matching, some not.
            picks in proptest::collection::vec(0usize..4, 1..6),
        ) {
            let conn = setup_corpus();
            let pool = [
                ("authentication jwt", "e-auth"),
                ("hybrid search ranking", "e-search"),
                ("deployment nixos", "e-deploy"),
                ("zzz unfindable", "e-auth"),
            ];
            let cases: Vec<GoldenCase> = picks.iter().map(|&i| GoldenCase {
                query: pool[i].0.into(),
                expected_ids: vec![pool[i].1.into()],
            }).collect();

            let report = evaluate(&conn, &NoopEmbedder, &cases, &fts_opts(5)).unwrap();
            let r = report.recall_at_k();
            let m = report.mrr();
            prop_assert!((0.0..=1.0).contains(&r), "recall out of bounds: {r}");
            prop_assert!((0.0..=1.0).contains(&m), "mrr out of bounds: {m}");
            prop_assert!(m <= r + 1e-9, "MRR cannot exceed recall when each case has 1 expected id");
        }
    }
}
