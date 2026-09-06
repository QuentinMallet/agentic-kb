//! Contract tests for the persisted pre-normalized embedding format.
//!
//! These tests intentionally name the missing migration seam
//! `db::migrate_embeddings`.  They are RED until the persisted-format work
//! supplies that transactional operation and the `normalized` per-blob marker
//! on both embedding tables.  The marker belongs to the blob row, not only to
//! `kb_meta`: a mixed database must continue to score legacy rows as cosine.

use kb::components::db::{apply_event, migrate_embeddings, open_db_memory};
use kb::components::embedder::Embedder;
use kb::models::{decode_emb_blob, f32s_to_blob, normalize_embedding, EMB_BLOB_BYTES, EMB_DIMS};
use proptest::prelude::*;
use rusqlite::params;
use serde_json::json;

struct FixedEmbedder(Vec<f32>);

impl Embedder for FixedEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(self.0.clone())
    }
}

fn entry_event(id: &str) -> serde_json::Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": format!("test/{id}"),
        "summary": "embedding migration test",
        "content": "embedding migration test",
        "tags": [],
        "kind": "memory",
        "evidence_status": "n/a",
        "permanent": false,
        "is_stale": false,
        "ts": "2026-09-06T00:00:00Z",
        "session_id": null,
    })
}

fn entry_event_with_cue(id: &str) -> serde_json::Value {
    let mut event = entry_event(id);
    event["cues"] = json!(["normalization cue"]);
    event
}

fn l2_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(left, right)| left * right).sum()
}

fn embedding_rowid(conn: &rusqlite::Connection, id: &str) -> i64 {
    conn.query_row(
        "SELECT rowid FROM entries WHERE id=?1",
        params![id],
        |row| row.get(0),
    )
    .unwrap()
}

fn stored_entry_embedding(conn: &rusqlite::Connection, id: &str) -> (Vec<u8>, i64) {
    let rowid = embedding_rowid(conn, id);
    conn.query_row(
        "SELECT embedding, normalized FROM entries_emb WHERE rowid=?1",
        params![rowid],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn finite_vector() -> impl Strategy<Value = Vec<f32>> {
    prop::collection::vec(-1.0f32..=1.0, EMB_DIMS)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Every finite non-zero Embedder output is persisted as a finite,
    /// approximately unit-length f16 vector and is marked per blob.
    #[test]
    fn finite_embedder_outputs_are_normalized_before_persistence(raw in finite_vector()) {
        prop_assume!(l2_norm(&raw) > 1.0e-6);

        let conn = open_db_memory().unwrap();
        apply_event(&conn, &FixedEmbedder(raw), &entry_event_with_cue("write")).unwrap();

        let (blob, normalized) = stored_entry_embedding(&conn, "write");
        let stored = decode_emb_blob(&blob);
        prop_assert_eq!(blob.len(), EMB_BLOB_BYTES);
        prop_assert_eq!(normalized, 1, "write path must mark this blob normalized");
        prop_assert!(stored.iter().all(|value| value.is_finite()));
        prop_assert!(
            (l2_norm(&stored) - 1.0).abs() < 0.01,
            "stored f16 vector must remain unit length: {}",
            l2_norm(&stored),
        );

        let (cue_blob, cue_normalized): (Vec<u8>, i64) = conn.query_row(
            "SELECT embedding, normalized FROM cues WHERE entry_id='write'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        let cue = decode_emb_blob(&cue_blob);
        prop_assert_eq!(cue_normalized, 1, "cue write path must mark this blob normalized");
        prop_assert!(cue.iter().all(|value| value.is_finite()));
        prop_assert!((l2_norm(&cue) - 1.0).abs() < 0.01);
    }

    /// A mixed database is migrated row by row: legacy f32 rows become
    /// normalized f16 rows and dot-product order matches their old cosine
    /// order whenever the source scores are not tied.
    #[test]
    fn migration_preserves_cosine_ranking_for_legacy_rows(
        query in finite_vector(),
        left in finite_vector(),
        right in finite_vector(),
    ) {
        prop_assume!(l2_norm(&query) > 1.0e-6);
        prop_assume!(l2_norm(&left) > 1.0e-6);
        prop_assume!(l2_norm(&right) > 1.0e-6);

        let old_left = dot(&query, &left) / (l2_norm(&query) * l2_norm(&left));
        let old_right = dot(&query, &right) / (l2_norm(&query) * l2_norm(&right));
        prop_assume!((old_left - old_right).abs() > 0.01);

        let conn = open_db_memory().unwrap();
        let noop = kb::components::embedder::NoopEmbedder;
        apply_event(&conn, &noop, &entry_event("left")).unwrap();
        apply_event(&conn, &noop, &entry_event("right")).unwrap();
        conn.execute(
            "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
            params![embedding_rowid(&conn, "left"), f32s_to_blob(&left)],
        ).unwrap();
        conn.execute(
            "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
            params![embedding_rowid(&conn, "right"), f32s_to_blob(&right)],
        ).unwrap();

        migrate_embeddings(&conn).unwrap();

        let normalized_query = normalize_embedding(&query).unwrap();
        let (left_blob, left_marker) = stored_entry_embedding(&conn, "left");
        let (right_blob, right_marker) = stored_entry_embedding(&conn, "right");
        let new_left = dot(&normalized_query, &decode_emb_blob(&left_blob));
        let new_right = dot(&normalized_query, &decode_emb_blob(&right_blob));
        prop_assert_eq!(left_marker, 1);
        prop_assert_eq!(right_marker, 1);
        prop_assert_eq!(old_left > old_right, new_left > new_right);
    }
}

/// Non-finite and zero-norm values must be rejected before they can be
/// serialized.  In particular, normalizing zero must not manufacture NaNs.
#[test]
fn non_finite_and_zero_norm_outputs_are_rejected() {
    for invalid in [
        vec![f32::NAN; EMB_DIMS],
        vec![f32::INFINITY; EMB_DIMS],
        vec![f32::NEG_INFINITY; EMB_DIMS],
        vec![0.0; EMB_DIMS],
    ] {
        assert!(normalize_embedding(&invalid).is_err());
    }
}

/// Migration is fail-closed: one corrupt legacy blob aborts without marking
/// that blob normalized.  This leaves the legacy cosine path available rather
/// than silently scoring it as a dot product.
#[test]
fn migration_rejects_non_finite_legacy_blobs_without_marking_them_normalized() {
    let conn = open_db_memory().unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event("corrupt")).unwrap();
    let corrupt = vec![f32::NAN; EMB_DIMS];
    conn.execute(
        "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
        params![embedding_rowid(&conn, "corrupt"), f32s_to_blob(&corrupt)],
    )
    .unwrap();

    assert!(migrate_embeddings(&conn).is_err());
    let (blob, normalized) = stored_entry_embedding(&conn, "corrupt");
    assert_eq!(normalized, 0);
    assert_eq!(blob, f32s_to_blob(&corrupt));
}

/// Cue embeddings are persisted blobs too.  A migration that rewrites only
/// `entries_emb` would leave the cue lane taking an unsafe dot-product path.
#[test]
fn migration_converts_legacy_cue_rows_and_marks_them_per_blob() {
    let conn = open_db_memory().unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event_with_cue("cue")).unwrap();
    let legacy = (0..EMB_DIMS)
        .map(|index| (index as f32 + 1.0) / EMB_DIMS as f32)
        .collect::<Vec<_>>();
    conn.execute(
        "UPDATE cues SET embedding=?1, normalized=0 WHERE entry_id='cue'",
        params![f32s_to_blob(&legacy)],
    )
    .unwrap();

    migrate_embeddings(&conn).unwrap();

    let (blob, normalized): (Vec<u8>, i64) = conn
        .query_row(
            "SELECT embedding, normalized FROM cues WHERE entry_id='cue'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(normalized, 1);
    assert_eq!(blob.len(), EMB_BLOB_BYTES);
    assert!((l2_norm(&decode_emb_blob(&blob)) - 1.0).abs() < 0.01);
}
