//! Regression/property tests for bd-r05y.2 (B3).
//!
//! Pins the invariant that epic bd-r05y depends on: memory-class findings must
//! stay retrievable, and retrieval/FTS indexing must not grow a kind-based
//! filter that silently excludes observation, belief, procedure, convention, or
//! memory entries.
//!
//! Coverage note: with `NoopEmbedder`, the semantic lane is intentionally
//! unobservable because `search_entries` skips it when `embedder.is_noop()`.
//! The lane under guard here is FTS: both FTS tables are asserted to index live
//! entries on upsert, and the observable search modes (`fts` and hybrid with a
//! noop embedder) must return every kind.

use kb::components::db::{
    apply_event, fts_query_content_entries, fts_query_contentless, open_db_memory,
    search_entries, SearchOptions,
};
use kb::components::embedder::NoopEmbedder;
use proptest::prelude::*;
use serde_json::json;

const VALID_KINDS: [&str; 5] = [
    "observation",
    "belief",
    "procedure",
    "convention",
    "memory",
];

fn evidence_status_for_kind(kind: &str) -> &'static str {
    match kind {
        "observation" | "belief" | "procedure" => "present",
        "convention" | "memory" => "n/a",
        _ => panic!("unexpected kind {kind}"),
    }
}

fn entry_event(id: &str, kind: &str, token: &str, is_stale: bool) -> serde_json::Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": format!("notes/{kind}/{id}.md"),
        "summary": format!("{kind} retrieval regression {token}"),
        "content": format!("distinctive token {token} proves {kind} stays searchable"),
        "tags": ["kind-regression", kind],
        "kind": kind,
        "evidence_status": evidence_status_for_kind(kind),
        "permanent": false,
        "is_stale": is_stale,
        "ts": "2024-01-01T00:00:00Z",
        "session_id": null,
    })
}

fn fts_opts(limit: usize) -> SearchOptions {
    SearchOptions {
        limit,
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

fn hybrid_opts(limit: usize) -> SearchOptions {
    SearchOptions {
        do_semantic: true,
        ..fts_opts(limit)
    }
}

fn quoted_query(token: &str) -> String {
    format!("\"{}\"", token.replace('"', "\"\""))
}

fn assert_fts_reads_include(conn: &rusqlite::Connection, token: &str, id: &str) {
    let safe_query = quoted_query(token);
    let opts = fts_opts(10);

    let contentless_ids: Vec<String> = fts_query_contentless(conn, &safe_query, &opts)
        .unwrap()
        .into_iter()
        .map(|(row_id, ..)| row_id)
        .collect();
    assert!(
        contentless_ids.iter().any(|row_id| row_id == id),
        "entries_fts must return {id} for token {token}, got {contentless_ids:?}"
    );

    let content_entries_ids: Vec<String> = fts_query_content_entries(conn, &safe_query, &opts)
        .unwrap()
        .into_iter()
        .map(|(row_id, ..)| row_id)
        .collect();
    assert!(
        content_entries_ids.iter().any(|row_id| row_id == id),
        "entries_fts_v2 must return {id} for token {token}, got {content_entries_ids:?}"
    );
}

fn assert_search_hits_include(conn: &rusqlite::Connection, token: &str, id: &str) {
    let fts_ids: Vec<String> = search_entries(conn, &NoopEmbedder, token, &fts_opts(10))
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert!(
        fts_ids.iter().any(|entry_id| entry_id == id),
        "FTS search must return {id} for token {token}, got {fts_ids:?}"
    );

    let hybrid_ids: Vec<String> = search_entries(conn, &NoopEmbedder, token, &hybrid_opts(10))
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert!(
        hybrid_ids.iter().any(|entry_id| entry_id == id),
        "hybrid search with NoopEmbedder must still return {id} for token {token}, got {hybrid_ids:?}"
    );
}

fn assert_not_returned_anywhere(conn: &rusqlite::Connection, token: &str, id: &str) {
    let safe_query = quoted_query(token);
    let opts = fts_opts(10);

    let contentless_ids: Vec<String> = fts_query_contentless(conn, &safe_query, &opts)
        .unwrap()
        .into_iter()
        .map(|(row_id, ..)| row_id)
        .collect();
    assert!(
        !contentless_ids.iter().any(|row_id| row_id == id),
        "stale entry {id} unexpectedly present in entries_fts: {contentless_ids:?}"
    );

    let content_entries_ids: Vec<String> = fts_query_content_entries(conn, &safe_query, &opts)
        .unwrap()
        .into_iter()
        .map(|(row_id, ..)| row_id)
        .collect();
    assert!(
        !content_entries_ids.iter().any(|row_id| row_id == id),
        "stale entry {id} unexpectedly present in entries_fts_v2: {content_entries_ids:?}"
    );

    let fts_ids: Vec<String> = search_entries(conn, &NoopEmbedder, token, &fts_opts(10))
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert!(
        !fts_ids.iter().any(|entry_id| entry_id == id),
        "stale entry {id} unexpectedly returned by FTS search: {fts_ids:?}"
    );

    let hybrid_ids: Vec<String> = search_entries(conn, &NoopEmbedder, token, &hybrid_opts(10))
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert!(
        !hybrid_ids.iter().any(|entry_id| entry_id == id),
        "stale entry {id} unexpectedly returned by hybrid search: {hybrid_ids:?}"
    );
}

fn token_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9]{1,12}").unwrap()
}

#[test]
fn test_all_kinds_are_indexed_and_retrievable() {
    let conn = open_db_memory().unwrap();
    let embedder = NoopEmbedder;

    for kind in VALID_KINDS {
        let id = format!("kind-{kind}");
        let token = format!("kindtok{kind}");
        let event = entry_event(&id, kind, &token, false);
        apply_event(&conn, &embedder, &event).unwrap();

        assert_fts_reads_include(&conn, &token, &id);
        assert_search_hits_include(&conn, &token, &id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_kind_does_not_change_fts_retrievability(
        kind in prop::sample::select(VALID_KINDS.to_vec()),
        token in token_strategy(),
    ) {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;
        let id = format!("prop-{kind}-{token}");
        let event = entry_event(&id, kind, &format!("prop{token}"), false);

        apply_event(&conn, &embedder, &event).unwrap();

        let distinctive = format!("prop{token}");
        assert_fts_reads_include(&conn, &distinctive, &id);
        assert_search_hits_include(&conn, &distinctive, &id);
    }
}

#[test]
fn test_stale_entry_is_filtered_from_index_and_search() {
    let conn = open_db_memory().unwrap();
    let embedder = NoopEmbedder;
    let id = "stale-memory";
    let token = "staletokmemory";
    let event = entry_event(id, "memory", token, true);

    apply_event(&conn, &embedder, &event).unwrap();

    assert_not_returned_anywhere(&conn, token, id);
}
