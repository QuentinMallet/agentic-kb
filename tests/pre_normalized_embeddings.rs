//! Contract tests for the persisted pre-normalized embedding format.
//!
//! These tests intentionally name the missing migration seam
//! `db::migrate_embeddings`.  They are RED until the persisted-format work
//! supplies that transactional operation and the `normalized` per-blob marker
//! on both embedding tables.  The marker belongs to the blob row, not only to
//! `kb_meta`: a mixed database must continue to score legacy rows as cosine.

use kb::commands::migrate_embeddings::MigrateEmbeddings;
use kb::components::db::{
    apply_event, migrate_embeddings, open_db_memory, open_unchecked_for_test, search_entries,
    SearchOptions,
};
use kb::components::embedder::Embedder;
use kb::config::Paths;
use kb::models::{decode_emb_blob, f32s_to_blob, normalize_embedding, EMB_BLOB_BYTES, EMB_DIMS};
use proptest::prelude::*;
use rusqlite::params;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;

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
        vec![1.0; EMB_DIMS - 1],
        vec![1.0; EMB_DIMS + 1],
    ] {
        assert!(normalize_embedding(&invalid).is_err());
    }
}

/// A legacy f32 blob must carry the configured embedding dimension before it
/// is eligible for migration.  Otherwise format length alone permits a model
/// upgrade to become a marked dot-product row with incomparable rankings.
#[test]
fn migration_rejects_wrong_dimension_legacy_blobs() {
    let conn = open_db_memory().unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event("wrong-dimension")).unwrap();
    let wrong_dimension = vec![1.0; EMB_DIMS - 1];
    conn.execute(
        "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
        params![
            embedding_rowid(&conn, "wrong-dimension"),
            f32s_to_blob(&wrong_dimension)
        ],
    )
    .unwrap();

    assert!(migrate_embeddings(&conn).is_err());
    let (_, normalized) = stored_entry_embedding(&conn, "wrong-dimension");
    assert_eq!(normalized, 0);
}

/// A 192-element f32 legacy blob is exactly 768 bytes, so generic
/// length-dispatch mistakes it for a canonical 384-element f16 blob. Migration
/// must require the legacy f32 wire length, not merely a decodable blob.
#[test]
fn migration_rejects_ambiguous_half_dimension_entry_blob() {
    let conn = open_db_memory().unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event("ambiguous-entry")).unwrap();
    let ambiguous = f32s_to_blob(&vec![1.0; EMB_DIMS / 2]);
    assert_eq!(ambiguous.len(), EMB_BLOB_BYTES);
    conn.execute(
        "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
        params![embedding_rowid(&conn, "ambiguous-entry"), ambiguous],
    )
    .unwrap();

    assert!(migrate_embeddings(&conn).is_err());
    let (blob, normalized) = stored_entry_embedding(&conn, "ambiguous-entry");
    assert_eq!(blob.len(), EMB_BLOB_BYTES);
    assert_eq!(normalized, 0);
}

/// Cue rows use the same persisted wire contract as entry embeddings. The
/// ambiguous half-dimension f32 shape must fail closed here too.
#[test]
fn migration_rejects_ambiguous_half_dimension_cue_blob() {
    let conn = open_db_memory().unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event_with_cue("ambiguous-cue")).unwrap();
    let ambiguous = f32s_to_blob(&vec![1.0; EMB_DIMS / 2]);
    assert_eq!(ambiguous.len(), EMB_BLOB_BYTES);
    conn.execute(
        "UPDATE cues SET embedding=?1, normalized=0 WHERE entry_id='ambiguous-cue'",
        params![ambiguous],
    )
    .unwrap();

    assert!(migrate_embeddings(&conn).is_err());
    let (blob, normalized): (Vec<u8>, i64) = conn
        .query_row(
            "SELECT embedding, normalized FROM cues WHERE entry_id='ambiguous-cue'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(blob.len(), EMB_BLOB_BYTES);
    assert_eq!(normalized, 0);
}

/// An unmarked 768-byte blob is legacy-state input, not proof that its bytes
/// are canonical f16. Semantic fallback must reject the ambiguous f32 shape
/// rather than manufacture a non-zero cosine score from its f16 interpretation.
#[test]
fn semantic_fallback_rejects_ambiguous_unmarked_blob() {
    let conn = open_db_memory().unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event("ambiguous-semantic")).unwrap();
    let ambiguous = f32s_to_blob(&vec![1.0; EMB_DIMS / 2]);
    conn.execute(
        "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
        params![embedding_rowid(&conn, "ambiguous-semantic"), ambiguous],
    )
    .unwrap();

    let mut query = vec![0.0; EMB_DIMS];
    query[1] = 1.0;
    let results = search_entries(
        &conn,
        &FixedEmbedder(query),
        "ambiguous semantic fallback",
        &SearchOptions {
            limit: 10,
            do_fts: false,
            do_semantic: true,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 0,
            repo_root: None,
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        },
    )
    .unwrap();

    let result = results
        .iter()
        .find(|result| result.id == "ambiguous-semantic")
        .unwrap();
    assert_eq!(result.score, 0.0);
}

/// An embedder with a different model dimension must fail before it can write
/// a normalized marker.  This protects future model upgrades from silently
/// turning incomparable vectors into dot-product candidates.
#[test]
fn wrong_dimension_embedder_output_is_rejected_before_persistence() {
    let conn = open_db_memory().unwrap();
    let error = apply_event(
        &conn,
        &FixedEmbedder(vec![1.0; EMB_DIMS - 1]),
        &entry_event("wrong-dimension-writer"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("dimension"));
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE id='wrong-dimension-writer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "the event transaction must roll back");
}

fn disk_paths() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());
    kb::components::db::open_or_init(&paths).unwrap();
    (dir, paths)
}

fn migration_backup_path(paths: &Paths) -> std::path::PathBuf {
    paths.db.with_extension("db.pre-normalized-embeddings.bak")
}

fn migration_staging_path(paths: &Paths) -> std::path::PathBuf {
    paths
        .db
        .with_extension("db.pre-normalized-embeddings.stage")
}

fn migration_state_path(paths: &Paths) -> std::path::PathBuf {
    paths
        .db
        .with_extension("db.pre-normalized-embeddings.state")
}

fn database_digest(path: &std::path::Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fs::read(path).unwrap());
    format!("{:x}", hasher.finalize())
}

/// A retained backup is recovery state, not a permanent one-shot lockout.
/// Re-running after a completed publish must be idempotent and preserve the
/// backup for operator rollback.
#[test]
fn file_migration_is_idempotent_after_publish_with_retained_backup() {
    let (_dir, paths) = disk_paths();
    let conn = open_unchecked_for_test(&paths.db).unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event("disk-row")).unwrap();
    conn.execute(
        "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
        params![
            embedding_rowid(&conn, "disk-row"),
            f32s_to_blob(&vec![1.0; EMB_DIMS])
        ],
    )
    .unwrap();
    drop(conn);

    let command = MigrateEmbeddings;
    assert_eq!(command.execute_with(&paths).unwrap(), 1);
    let backup = migration_backup_path(&paths);
    assert!(
        backup.exists(),
        "successful migration retains a rollback backup"
    );
    assert_eq!(command.execute_with(&paths).unwrap(), 0);
    assert!(
        backup.exists(),
        "idempotent resume must not discard the backup"
    );
}

/// A pre-publish crash leaves the live DB intact and a resumable staged copy.
/// If a committed write reaches the live WAL before recovery, the manifest
/// must reject that stale staged copy and rebuild from the live database.
#[test]
fn file_migration_resumes_a_pre_publish_staged_copy_without_losing_live_rows() {
    let (_dir, paths) = disk_paths();
    let conn = open_unchecked_for_test(&paths.db).unwrap();
    let noop = kb::components::embedder::NoopEmbedder;
    apply_event(&conn, &noop, &entry_event("survives-crash")).unwrap();
    conn.execute(
        "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
        params![
            embedding_rowid(&conn, "survives-crash"),
            f32s_to_blob(&vec![1.0; EMB_DIMS])
        ],
    )
    .unwrap();
    let staging = migration_staging_path(&paths);
    let backup = migration_backup_path(&paths);
    let state = migration_state_path(&paths);
    let escaped_staging = staging.to_string_lossy().replace('\'', "''");
    let escaped_backup = backup.to_string_lossy().replace('\'', "''");
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    conn.execute_batch(&format!("VACUUM INTO '{escaped_backup}'"))
        .unwrap();
    conn.execute_batch(&format!("VACUUM INTO '{escaped_staging}'"))
        .unwrap();
    drop(conn);

    let staged_conn = open_unchecked_for_test(&staging).unwrap();
    assert_eq!(migrate_embeddings(&staged_conn).unwrap(), 1);
    staged_conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(staged_conn);
    fs::write(&state, format!("{}\\t1\\n", database_digest(&paths.db))).unwrap();
    assert!(staging.exists());

    // This is the crash window the migration must protect: the stage is ready
    // but the live DB has gained a committed row. Recovery may not publish the
    // old stage over it, even after checkpointing the live WAL.
    let late_conn = open_unchecked_for_test(&paths.db).unwrap();
    apply_event(&late_conn, &noop, &entry_event("committed-after-stage")).unwrap();
    late_conn
        .execute(
            "INSERT INTO entries_emb(rowid, embedding, normalized) VALUES(?1, ?2, 0)",
            params![
                embedding_rowid(&late_conn, "committed-after-stage"),
                f32s_to_blob(&vec![1.0; EMB_DIMS])
            ],
        )
        .unwrap();
    drop(late_conn);

    let command = MigrateEmbeddings;
    assert_eq!(command.execute_with(&paths).unwrap(), 2);

    let conn = open_unchecked_for_test(&paths.db).unwrap();
    let (_, normalized) = stored_entry_embedding(&conn, "survives-crash");
    assert_eq!(normalized, 1);
    let (_, normalized) = stored_entry_embedding(&conn, "committed-after-stage");
    assert_eq!(normalized, 1);
    assert!(!staging.exists());
    assert!(!state.exists());
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
