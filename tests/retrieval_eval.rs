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
use kb::components::retrieval_eval::{compare_reports, corpus_hash_from_event_log, evaluate, evaluate_split, parse_golden_jsonl, validate_sealed_manifest, CaseResult, EvalReport, GoldenCase, Split, SplitManifest, Verdict};
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
        GoldenCase { query: "authentication jwt".into(), expected_ids: vec!["e-auth".into()], split: Split::Dev },
        GoldenCase { query: "hybrid search ranking".into(), expected_ids: vec!["e-search".into()], split: Split::Dev },
        GoldenCase { query: "deployment nixos".into(), expected_ids: vec!["e-deploy".into()], split: Split::Dev },
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
        GoldenCase { query: "authentication jwt".into(), expected_ids: vec!["e-auth".into()], split: Split::Dev },
        GoldenCase { query: "zzz nonexistent query".into(), expected_ids: vec!["e-deploy".into()], split: Split::Dev },
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
        split: Split::Dev,
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
{"query": "authentication jwt", "expected_ids": ["e-auth"], "split":"dev"}

{"query": "hybrid search", "expected_ids": ["e-search", "e-auth"], "split":"sealed"}
"#;
    let cases = parse_golden_jsonl(good).unwrap();
    assert_eq!(cases.len(), 2);
    assert_eq!(cases[0].query, "authentication jwt");
    assert_eq!(cases[1].expected_ids.len(), 2);

    let bad = r#"{"query": "no expectations", "expected_ids": []}"#;
    assert!(parse_golden_jsonl(bad).is_err(), "empty expected_ids must be rejected");

    let empty = "\n# only comments\n";
    assert!(parse_golden_jsonl(empty).is_err(), "golden set with zero cases must be rejected");
    let legacy = r#"{"query":"legacy","expected_ids":["e-auth"]}"#;
    assert!(parse_golden_jsonl(legacy).unwrap_err().to_string().contains("invalid JSON"));
}

#[test]
fn sealed_case_is_refused_by_dev_scorer() {
    let case = GoldenCase { query: "secret".into(), expected_ids: vec!["e-auth".into()], split: Split::Sealed };
    let err = evaluate_split(&setup_corpus(), &NoopEmbedder, &[case], &fts_opts(10), Split::Dev).unwrap_err();
    assert!(err.to_string().contains("EVAL_SPLIT_REFUSAL"));
}

fn report(bits: &[bool]) -> EvalReport {
    EvalReport { k: 10, per_case: bits.iter().enumerate().map(|(i, hit)| CaseResult {
        query: format!("q{i}"), expected: 1, hits: usize::from(*hit), first_rank: hit.then_some(1),
    }).collect() }
}

#[test]
fn mcnemar_preregistered_table() {
    for (before, after, verdict, n) in [
        (vec![false; 6], vec![true; 6], Verdict::Significant, 6),
        (vec![false; 5], vec![true; 5], Verdict::Inconclusive, 5),
        (vec![true; 6], vec![false; 6], Verdict::Regression, 6),
        (vec![true; 3], vec![true; 3], Verdict::Inconclusive, 0),
    ] {
        let got = compare_reports(&report(&before), &report(&after)).unwrap();
        assert_eq!((got.verdict, got.discordant_pairs), (verdict, n));
    }
}

#[test]
fn manifest_hash_is_stable_and_absent_id_is_detected() {
    use std::collections::BTreeSet;
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    fs::write(&path, serde_json::to_string(&make_entry("present", "p", "s", "body")).unwrap() + "\n").unwrap();
    let ids = ["present".to_string(), "absent".to_string()].into_iter().collect::<BTreeSet<_>>();
    let first = corpus_hash_from_event_log(&path, &ids).unwrap();
    let second = corpus_hash_from_event_log(&path, &ids).unwrap();
    assert_eq!(first, second);
    assert!(!first.1.contains("absent"), "sealed preflight must hard-refuse this id");
    let cases = vec![GoldenCase { query: "q".into(), expected_ids: vec!["absent".into()], split: Split::Sealed }];
    let manifest = SplitManifest { corpus_hash: "irrelevant".into(), sealed_ids: vec!["absent".into()], dev_ids: vec![], frozen_at: "now".into(), corpus_hash_domain: "ids-only".into() };
    let err = validate_sealed_manifest(&cases, &path, &manifest).unwrap_err();
    assert!(err.to_string().contains("SEALED_CORPUS_STALE_OR_ABSENT"));

    let present_cases = vec![GoldenCase { query: "q".into(), expected_ids: vec!["present".into()], split: Split::Sealed }];
    let wrong_hash = SplitManifest { corpus_hash: "wrong".into(), sealed_ids: vec!["present".into()], dev_ids: vec![], frozen_at: "now".into(), corpus_hash_domain: "ids-only".into() };
    let err = validate_sealed_manifest(&present_cases, &path, &wrong_hash).unwrap_err();
    assert!(err.to_string().contains("SEALED_MANIFEST_HASH_MISMATCH"));
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
                split: if i % 2 == 0 { Split::Dev } else { Split::Sealed },
            }).collect();

            let report = evaluate(&conn, &NoopEmbedder, &cases, &fts_opts(5)).unwrap();
            let r = report.recall_at_k();
            let m = report.mrr();
            prop_assert!((0.0..=1.0).contains(&r), "recall out of bounds: {r}");
            prop_assert!((0.0..=1.0).contains(&m), "mrr out of bounds: {m}");
            prop_assert!(m <= r + 1e-9, "MRR cannot exceed recall when each case has 1 expected id");
        }
    }

    proptest! {
        #[test]
        fn prop_dev_sealed_partition_is_total_and_disjoint(splits in proptest::collection::vec(any::<bool>(), 1..40)) {
            let text = splits.iter().enumerate().map(|(i, dev)| format!(
                "{{\"query\":\"q{i}\",\"expected_ids\":[\"id{i}\"],\"split\":\"{}\"}}", if *dev { "dev" } else { "sealed" }
            )).collect::<Vec<_>>().join("\n");
            let cases = parse_golden_jsonl(&text).unwrap();
            let dev = cases.iter().filter(|c| c.split == Split::Dev).map(|c| &c.query).collect::<std::collections::BTreeSet<_>>();
            let sealed = cases.iter().filter(|c| c.split == Split::Sealed).map(|c| &c.query).collect::<std::collections::BTreeSet<_>>();
            prop_assert_eq!(dev.len() + sealed.len(), cases.len());
            prop_assert!(dev.is_disjoint(&sealed));
        }
    }
}
