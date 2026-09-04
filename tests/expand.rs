//! Frontier expand primitive tests (Memora pickup .7 acceptance gate).
//!
//! Memora's iterative retrieval walks a frontier of neighbors of the working
//! set (EXPAND / RE_QUERY / STOP). In agentic-kb the calling agent IS the
//! policy loop; the KB supplies only the expand primitive:
//! `expand_entries(conn, ids, limit)` returns live entries adjacent to the
//! given ids, where adjacency = shared path directory, shared tag, shared cue
//! text, or shared evidence citation file. Ranked by facet overlap count.
//!
//! Invariants:
//!   1. Path-sibling neighbor found (same dirname).
//!   2. Shared-tag neighbor found.
//!   3. Shared-evidence-file neighbor found.
//!   4. Seed ids themselves are never returned.
//!   5. Stale entries are never returned.
//!   6. A neighbor sharing multiple facets outranks a single-facet neighbor.
//!   7. Unknown ids produce empty result, not an error.

use kb::components::db::{apply_event, expand_entries, open_db_memory};
use kb::components::embedder::NoopEmbedder;
use serde_json::json;

fn entry(id: &str, path: &str, tags: serde_json::Value) -> serde_json::Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": path,
        "summary": format!("summary {id}"),
        "content": format!("content {id}"),
        "tags": tags,
        "kind": "observation",
        "evidence_status": "missing",
        "permanent": false,
        "is_stale": false,
        "ts": "2024-01-01T00:00:00Z",
        "session_id": null,
    })
}

fn evidence(entry_id: &str, ev_id: &str, file: &str) -> serde_json::Value {
    json!({
        "action": "evidence_add",
        "table": "evidence",
        "entry_id": entry_id,
        "evidence": {
            "id": ev_id,
            "entry_id": entry_id,
            "kind": "code",
            "citation_path": format!("{file}:0-10"),
            "citation_sha": null,
            "citation_hash": "sha256:abc",
            "citation_excerpt": null,
            "derived_from": null,
            "recorded_at": "2024-01-01T00:00:00Z",
        },
    })
}

/// Corpus:
///   seed      at "arch/search/rrf",  tags [fusion],  evidence src/db.rs
///   sib       at "arch/search/fts"                       (path sibling)
///   tagmate   at "ops/deploy",       tags [fusion]       (shared tag)
///   evmate    at "notes/db",         evidence src/db.rs  (shared evidence file)
///   multi     at "arch/search/mmr",  tags [fusion]       (sibling + tag = 2 facets)
///   far       at "misc/other"                            (no relation)
///   stale_sib at "arch/search/old"  — expired
fn setup() -> rusqlite::Connection {
    let conn = open_db_memory().unwrap();
    let emb = NoopEmbedder;
    apply_event(
        &conn,
        &emb,
        &entry("seed", "arch/search/rrf", json!(["fusion"])),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &entry("sib", "arch/search/fts", json!(["lane"])),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &entry("tagmate", "ops/deploy", json!(["fusion"])),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &entry("evmate", "notes/db", json!(["storage"])),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &entry("multi", "arch/search/mmr", json!(["fusion"])),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &entry("far", "misc/other", json!(["unrelated"])),
    )
    .unwrap();
    apply_event(
        &conn,
        &emb,
        &entry("stale_sib", "arch/search/old", json!(["fusion"])),
    )
    .unwrap();
    apply_event(&conn, &emb, &evidence("seed", "ev-1", "src/db.rs")).unwrap();
    apply_event(&conn, &emb, &evidence("evmate", "ev-2", "src/db.rs")).unwrap();
    apply_event(
        &conn,
        &emb,
        &json!({"action": "expire", "table": "entries", "id": "stale_sib",
                "reason": "test", "ts": "2024-01-02T00:00:00Z"}),
    )
    .unwrap();
    conn
}

#[test]
fn test_expand_finds_all_facet_neighbors() {
    let conn = setup();
    let results = expand_entries(&conn, &["seed".to_string()], 10).unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();

    assert!(ids.contains(&"sib"), "path sibling must be found: {ids:?}");
    assert!(
        ids.contains(&"tagmate"),
        "shared-tag neighbor must be found: {ids:?}"
    );
    assert!(
        ids.contains(&"evmate"),
        "shared-evidence neighbor must be found: {ids:?}"
    );
    assert!(
        ids.contains(&"multi"),
        "multi-facet neighbor must be found: {ids:?}"
    );
    assert!(
        !ids.contains(&"far"),
        "unrelated entry must not appear: {ids:?}"
    );
}

#[test]
fn test_expand_excludes_seed_and_stale() {
    let conn = setup();
    let results = expand_entries(&conn, &["seed".to_string()], 10).unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(!ids.contains(&"seed"), "seed must not be returned: {ids:?}");
    assert!(
        !ids.contains(&"stale_sib"),
        "stale entries must not be returned: {ids:?}"
    );
}

#[test]
fn test_expand_multi_facet_ranks_first() {
    let conn = setup();
    let results = expand_entries(&conn, &["seed".to_string()], 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(
        results[0].id,
        "multi",
        "two-facet neighbor (sibling + shared tag) must outrank single-facet ones: {:?}",
        results.iter().map(|r| (&r.id, r.score)).collect::<Vec<_>>()
    );
    assert!(results[0].score > results.last().unwrap().score);
    for r in &results {
        assert_eq!(r.score_kind, "expand");
    }
}

#[test]
fn test_expand_unknown_id_empty() {
    let conn = setup();
    let results = expand_entries(&conn, &["nope".to_string()], 10).unwrap();
    assert!(results.is_empty());
}

/// Request-amplification cap: seeds beyond MAX_EXPAND_SEEDS (32) are dropped
/// silently — the call succeeds using the first 32.
#[test]
fn test_expand_seed_count_capped() {
    let conn = setup();
    let mut ids: Vec<String> = (0..40).map(|i| format!("junk-{i}")).collect();
    ids.push("seed".to_string()); // position 41 — beyond the cap, must be ignored
    let results = expand_entries(&conn, &ids, 10).unwrap();
    assert!(
        results.is_empty(),
        "seed beyond the 32-seed cap must not contribute facets: {:?}",
        results.iter().map(|r| &r.id).collect::<Vec<_>>()
    );

    let mut ids2: Vec<String> = vec!["seed".to_string()]; // inside the cap
    ids2.extend((0..40).map(|i| format!("junk-{i}")));
    let results2 = expand_entries(&conn, &ids2, 10).unwrap();
    assert!(
        !results2.is_empty(),
        "seed inside the cap must still expand"
    );
}

#[test]
fn test_expand_respects_limit() {
    let conn = setup();
    let results = expand_entries(&conn, &["seed".to_string()], 2).unwrap();
    assert!(results.len() <= 2);
}
