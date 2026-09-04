//! Embedding-text construction tests (Memora pickup .3 acceptance gate).
//!
//! Memora principle: index abstractions, not content. `Abstraction` mode
//! embeds path + summary + tags only; raw content stays out of the vector
//! (it remains FTS-indexed). `Full` mode is the pre-existing behavior and
//! stays the default until the eval harness validates the flip.
//!
//! Invariants:
//!   1. Full mode reproduces the legacy "path summary content" text.
//!   2. Abstraction mode contains path, summary, and flattened tags — not content.
//!   3. JSON-array tags are flattened to plain words (no brackets/quotes).
//!   4. Mode parsing: "abstraction" → Abstraction, anything else/unset → Full.
//!   5. apply_event feeds the embedder the mode-selected text.

use kb::components::db::{apply_event, entry_embed_text, open_db_memory, EmbedTextMode};
use kb::components::embedder::Embedder;
use serde_json::json;
use std::sync::Mutex;

#[test]
fn test_full_mode_matches_legacy_text() {
    let text = entry_embed_text(
        EmbedTextMode::Full,
        "src/auth.rs",
        "authentication module",
        "handles JWT tokens",
        r#"["auth","security"]"#,
    );
    assert_eq!(text, "src/auth.rs authentication module handles JWT tokens");
}

#[test]
fn test_abstraction_mode_excludes_content_includes_tags() {
    let text = entry_embed_text(
        EmbedTextMode::Abstraction,
        "src/auth.rs",
        "authentication module",
        "handles JWT tokens",
        r#"["auth","security"]"#,
    );
    assert!(
        !text.contains("JWT"),
        "content must not leak into abstraction embedding: {text}"
    );
    assert!(text.contains("src/auth.rs"));
    assert!(text.contains("authentication module"));
    assert!(text.contains("auth") && text.contains("security"));
    assert!(
        !text.contains('[') && !text.contains('"'),
        "tags must be flattened, got: {text}"
    );
}

#[test]
fn test_non_json_tags_pass_through() {
    let text = entry_embed_text(EmbedTextMode::Abstraction, "p", "s", "c", "plain,csv");
    assert!(text.contains("plain,csv"));
}

#[test]
fn test_mode_parsing() {
    assert_eq!(
        EmbedTextMode::parse(Some("abstraction")),
        EmbedTextMode::Abstraction
    );
    assert_eq!(EmbedTextMode::parse(Some("full")), EmbedTextMode::Full);
    assert_eq!(EmbedTextMode::parse(Some("garbage")), EmbedTextMode::Full);
    assert_eq!(EmbedTextMode::parse(None), EmbedTextMode::Full);
}

/// Embedder that records every text it is asked to embed.
struct CapturingEmbedder {
    seen: Mutex<Vec<String>>,
}

impl Embedder for CapturingEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        self.seen.lock().unwrap().push(text.to_string());
        Ok(vec![0.5; 384])
    }
    fn is_noop(&self) -> bool {
        false
    }
}

/// Vintage stamp: the first embed write records the active mode in kb_meta;
/// a later write under a different mode leaves the stamp unchanged (warns on
/// stderr — mixed vintages are visible, never silently re-stamped).
#[test]
fn test_embed_mode_vintage_stamp() {
    use kb::components::db::{check_embed_mode_vintage, EmbedTextMode};
    let conn = open_db_memory().unwrap();

    check_embed_mode_vintage(&conn, EmbedTextMode::Full);
    let stored: String = conn
        .query_row(
            "SELECT value FROM kb_meta WHERE key='embed_text_mode'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "full", "first write must stamp the active mode");

    // Different mode: stamp must NOT be overwritten (warning path).
    check_embed_mode_vintage(&conn, EmbedTextMode::Abstraction);
    let stored: String = conn
        .query_row(
            "SELECT value FROM kb_meta WHERE key='embed_text_mode'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "full", "mismatched mode must not re-stamp");
}

/// apply_event embeds the legacy full text by default (KB_EMBED_TEXT unset in
/// the test environment).
#[test]
fn test_apply_event_uses_full_text_by_default() {
    let conn = open_db_memory().unwrap();
    let emb = CapturingEmbedder {
        seen: Mutex::new(vec![]),
    };
    apply_event(
        &conn,
        &emb,
        &json!({
            "action": "upsert", "table": "entries",
            "id": "et-1", "path": "src/x.rs", "summary": "sum words",
            "content": "content words", "tags": ["t1"],
            "kind": "observation", "evidence_status": "missing",
            "permanent": false, "is_stale": false,
            "ts": "2024-01-01T00:00:00Z", "session_id": null,
        }),
    )
    .unwrap();
    let seen = emb.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one embed call per upsert");
    assert!(
        seen[0].contains("content words"),
        "default (Full) mode must embed content; got: {}",
        seen[0]
    );
}
