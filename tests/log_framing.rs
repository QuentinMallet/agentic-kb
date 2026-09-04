//! C1/T2a — framing envelope, span-aware readers, `committed_len`, repair.
//!
//! Spec: `.state/.omc/plans/c1-log-durability.md` D1 (framing envelope, the
//! reader rule table, `committed_len`, repair) and D7 (version-skew posture).
//! Verified model: `.state/agent-kb/tla/DurableBatch.tla` plus
//! `DurableBatch-counterexamples.md` §5 "test obligations for T2a".
//!
//! Every append is wrapped in-band:
//!
//! ```jsonl
//! {"action":"batch_begin","batch_id":"<uuid>","n":3}
//! … the 3 event lines, unchanged …
//! {"action":"batch_commit","batch_id":"<uuid>","n":3}
//! ```
//!
//! A span counts as committed only when its `batch_commit` line is present and
//! newline-terminated. Marker lines are consumed by the reader and never
//! returned as events.

use kb::commands::add::acquire_lock;
use kb::components::db::{apply_event, open_db_memory};
use kb::components::embedder::NoopEmbedder;
use kb::components::events::{
    append_event, append_events_batch, read_events, read_events_from_offset,
};
use proptest::prelude::*;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn log_dir() -> (TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    (dir, path)
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn actions(path: &Path) -> Vec<String> {
    lines(path)
        .iter()
        .map(|v| v["action"].as_str().unwrap_or("").to_string())
        .collect()
}

fn write_raw(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap()
}

fn append_raw(path: &Path, contents: &[u8]) {
    let mut f = OpenOptions::new().append(true).create(true).open(path).unwrap();
    f.write_all(contents).unwrap();
}

fn begin(batch_id: &str, n: usize) -> String {
    json!({"action": "batch_begin", "batch_id": batch_id, "n": n}).to_string()
}

fn commit(batch_id: &str, n: usize) -> String {
    json!({"action": "batch_commit", "batch_id": batch_id, "n": n}).to_string()
}

fn upsert(id: &str) -> Value {
    json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": format!("t/{id}"),
        "summary": "s",
        "content": "c",
        "tags": [],
        "kind": "memory",
        "ts": "2024-01-01T00:00:00Z",
    })
}

/// Sidecars written by the repair path, in directory order.
fn torn_sidecars(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("events.jsonl.torn-"))
        })
        .collect();
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// D1 — writers emit an envelope around EVERY append
// ---------------------------------------------------------------------------

#[test]
fn test_single_append_is_wrapped_in_a_commit_envelope() {
    let (_dir, path) = log_dir();
    let event = upsert("solo");

    append_event(&path, &event).unwrap();

    assert_eq!(actions(&path), vec!["batch_begin", "upsert", "batch_commit"]);
    let raw = lines(&path);
    assert_eq!(raw[0]["n"], 1);
    assert_eq!(raw[2]["n"], 1);
    assert_eq!(raw[0]["batch_id"], raw[2]["batch_id"]);
    assert!(raw[0]["batch_id"].as_str().is_some_and(|s| !s.is_empty()));

    // Markers are consumed by the reader and never surface as events.
    let read = read_events(&path).unwrap();
    assert_eq!(read.events, vec![event]);
    assert_eq!(read.committed_len, fs::metadata(&path).unwrap().len());
}

#[test]
fn test_batch_append_of_three_events_is_one_span() {
    // DurableBatch-counterexamples.md §5 obligation 5: no spec config exercises
    // a three-event batch, so batch arity is a code-side obligation.
    let (_dir, path) = log_dir();
    let batch = vec![upsert("a"), upsert("b"), upsert("c")];

    append_events_batch(&path, &batch).unwrap();

    assert_eq!(
        actions(&path),
        vec!["batch_begin", "upsert", "upsert", "upsert", "batch_commit"]
    );
    let raw = lines(&path);
    assert_eq!(raw[0]["n"], 3);
    assert_eq!(raw[4]["n"], 3);
    assert_eq!(read_events(&path).unwrap().events, batch);
}

#[test]
fn test_consecutive_spans_each_get_a_distinct_batch_id() {
    let (_dir, path) = log_dir();
    append_event(&path, &upsert("one")).unwrap();
    append_event(&path, &upsert("two")).unwrap();

    let raw = lines(&path);
    assert_ne!(raw[0]["batch_id"], raw[3]["batch_id"]);
    assert_eq!(
        read_events(&path).unwrap().events,
        vec![upsert("one"), upsert("two")]
    );
}

// ---------------------------------------------------------------------------
// D1 — the reader rule table
// ---------------------------------------------------------------------------

#[test]
fn test_legacy_marker_less_log_reads_as_standalone_committed_events() {
    let (_dir, path) = log_dir();
    let legacy = format!("{}\n{}\n", upsert("l1"), upsert("l2"));
    write_raw(&path, legacy.as_bytes());

    let read = read_events(&path).unwrap();
    assert_eq!(read.events, vec![upsert("l1"), upsert("l2")]);
    assert!(read.torn_tail.is_none());
    assert_eq!(read.committed_len, legacy.len() as u64);
}

#[test]
fn test_dangling_begin_at_eof_is_dropped_by_the_reader() {
    let (_dir, path) = log_dir();
    let committed = format!("{}\n", upsert("kept"));
    let mut raw = committed.clone();
    raw.push_str(&format!("{}\n{}\n{}\n", begin("b1", 3), upsert("x"), upsert("y")));
    write_raw(&path, raw.as_bytes());

    let read = read_events(&path).unwrap();
    assert_eq!(read.events, vec![upsert("kept")]);
    assert_eq!(read.committed_len, committed.len() as u64);
}

#[test]
fn test_span_without_a_newline_terminated_commit_is_uncommitted() {
    let (_dir, path) = log_dir();
    let committed = format!("{}\n", upsert("kept"));
    let mut raw = committed.clone();
    raw.push_str(&format!("{}\n{}\n{}", begin("b1", 1), upsert("x"), commit("b1", 1)));
    write_raw(&path, raw.as_bytes());

    let read = read_events(&path).unwrap();
    assert_eq!(read.events, vec![upsert("kept")]);
    assert_eq!(read.committed_len, committed.len() as u64);
}

#[test]
fn test_mid_log_dangling_begin_is_a_hard_error() {
    // D7: an old binary that appends after a dangling `batch_begin` cements a
    // mid-log dangling begin. The new reader must stop loudly, never drop.
    let (_dir, path) = log_dir();
    let raw = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        begin("b1", 2),
        upsert("x"),
        begin("b2", 1),
        upsert("y"),
        commit("b2", 1)
    );
    write_raw(&path, raw.as_bytes());

    let err = read_events(&path).unwrap_err().to_string();
    assert!(
        err.contains("batch_begin") && err.contains("line 3"),
        "expected a mid-log dangling begin error, got: {err}"
    );
}

#[test]
fn test_commit_marker_without_a_begin_is_a_hard_error() {
    let (_dir, path) = log_dir();
    let raw = format!("{}\n{}\n", upsert("x"), commit("b1", 1));
    write_raw(&path, raw.as_bytes());

    let err = read_events(&path).unwrap_err().to_string();
    assert!(
        err.contains("batch_commit") && err.contains("line 2"),
        "expected an unmatched commit error, got: {err}"
    );
}

#[test]
fn test_commit_n_disagreeing_with_the_span_is_a_hard_error() {
    let (_dir, path) = log_dir();
    let raw = format!(
        "{}\n{}\n{}\n",
        begin("b1", 2),
        upsert("x"),
        commit("b1", 2)
    );
    write_raw(&path, raw.as_bytes());

    let err = read_events(&path).unwrap_err().to_string();
    assert!(err.contains("n"), "expected an n-mismatch error, got: {err}");
}

#[test]
fn test_more_event_lines_than_declared_n_is_a_hard_error() {
    // The D7 skew shape: an old binary appends a standalone event into an open
    // span, overrunning the declared arity.
    let (_dir, path) = log_dir();
    let raw = format!(
        "{}\n{}\n{}\n{}\n",
        begin("b1", 1),
        upsert("x"),
        upsert("y"),
        commit("b1", 1)
    );
    write_raw(&path, raw.as_bytes());

    let err = read_events(&path).unwrap_err().to_string();
    assert!(err.contains("n"), "expected an n-mismatch error, got: {err}");
}

#[test]
fn test_commit_with_a_mismatched_batch_id_is_a_hard_error() {
    let (_dir, path) = log_dir();
    let raw = format!(
        "{}\n{}\n{}\n",
        begin("b1", 1),
        upsert("x"),
        commit("b2", 1)
    );
    write_raw(&path, raw.as_bytes());

    let err = read_events(&path).unwrap_err().to_string();
    assert!(
        err.contains("batch_id"),
        "expected a batch_id mismatch error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// D1 — committed_len is a span boundary
// ---------------------------------------------------------------------------

#[test]
fn test_committed_len_is_a_span_boundary_not_a_line_boundary() {
    let (_dir, path) = log_dir();
    append_events_batch(&path, &[upsert("a"), upsert("b")]).unwrap();
    let after_first = fs::metadata(&path).unwrap().len();
    assert_eq!(read_events(&path).unwrap().committed_len, after_first);

    // A second span, left open: committed_len must not move.
    append_raw(&path, format!("{}\n{}\n", begin("open", 2), upsert("c")).as_bytes());
    let read = read_events(&path).unwrap();
    assert_eq!(read.committed_len, after_first);
    assert_eq!(read.events, vec![upsert("a"), upsert("b")]);
}

#[test]
fn test_read_events_from_offset_rejects_a_mid_span_offset() {
    let (_dir, path) = log_dir();
    append_events_batch(&path, &[upsert("a"), upsert("b")]).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    let after_begin = (raw.lines().next().unwrap().len() + 1) as u64;

    // An offset inside the span hides its `batch_begin`; the remaining lines
    // must never be applied as standalone committed events.
    assert!(
        read_events_from_offset(&path, after_begin).is_err(),
        "reading from inside a span must not silently apply its remaining lines"
    );
}

#[test]
fn test_read_events_from_offset_at_a_span_boundary_reads_the_tail() {
    let (_dir, path) = log_dir();
    append_events_batch(&path, &[upsert("a")]).unwrap();
    let boundary = read_events(&path).unwrap().committed_len;
    append_events_batch(&path, &[upsert("b"), upsert("c")]).unwrap();

    let tail = read_events_from_offset(&path, boundary).unwrap();
    assert_eq!(tail.events, vec![upsert("b"), upsert("c")]);
    assert_eq!(tail.committed_len, fs::metadata(&path).unwrap().len());
}

// ---------------------------------------------------------------------------
// D1 — repair_uncommitted_tail_before_append
// ---------------------------------------------------------------------------

#[test]
fn test_body_written_newline_failed_single_append_is_not_promoted() {
    // The write-error defect D1 names: the body write succeeds and the newline
    // write fails, `append_event` returns Err, the caller applies nothing — and
    // the next append must NOT promote that record to committed.
    let (dir, path) = log_dir();
    let committed = format!("{}\n", upsert("kept"));
    write_raw(&path, committed.as_bytes());
    // A failed single append leaves its begin marker and a newline-less body.
    append_raw(
        &path,
        format!("{}\n{}", begin("failed", 1), upsert("never-committed")).as_bytes(),
    );

    let _lock = acquire_lock(&dir.path().join(".lock")).unwrap();
    append_event(&path, &upsert("next")).unwrap();

    let read = read_events(&path).unwrap();
    assert_eq!(read.events, vec![upsert("kept"), upsert("next")]);
    assert!(
        !read
            .events
            .iter()
            .any(|e| e["id"] == "never-committed"),
        "a newline-failed append must never be promoted by the next append"
    );
}

#[test]
fn test_append_truncates_a_complete_dangling_span() {
    let (dir, path) = log_dir();
    let committed = format!("{}\n", upsert("kept"));
    write_raw(&path, committed.as_bytes());
    append_raw(
        &path,
        format!("{}\n{}\n{}\n", begin("dangling", 2), upsert("x"), upsert("y")).as_bytes(),
    );

    let _lock = acquire_lock(&dir.path().join(".lock")).unwrap();
    append_event(&path, &upsert("next")).unwrap();

    let read = read_events(&path).unwrap();
    assert_eq!(read.events, vec![upsert("kept"), upsert("next")]);
    // The discarded span is preserved best-effort, but never re-read as events.
    assert_eq!(torn_sidecars(dir.path()).len(), 1);
}

#[test]
fn test_span_truncation_proceeds_when_the_sidecar_write_fails() {
    // ENOSPC is a motivating fault for this epic: an uncommitted span was never
    // reader-accepted, so the sidecar must never block its truncation.
    let dir = tempdir().unwrap();
    let logs = dir.path().join("logs");
    fs::create_dir(&logs).unwrap();
    let path = logs.join("events.jsonl");
    let committed = format!("{}\n", upsert("kept"));
    write_raw(&path, committed.as_bytes());
    append_raw(
        &path,
        format!("{}\n{}\n", begin("dangling", 2), upsert("x")).as_bytes(),
    );

    // A read-only directory makes the sidecar creation fail with EACCES while
    // `set_len` on the already-open log handle still succeeds — the same shape
    // as ENOSPC on the sidecar write.
    let mut perms = fs::metadata(&logs).unwrap().permissions();
    let original = perms.clone();
    perms.set_readonly(true);
    fs::set_permissions(&logs, perms).unwrap();

    let _lock = acquire_lock(&dir.path().join(".lock")).unwrap();
    let result = append_event(&path, &upsert("next"));

    fs::set_permissions(&logs, original).unwrap();

    // The append itself cannot create files either, so it may fail — what must
    // hold is that the uncommitted span is gone and no event of it is readable.
    drop(result);
    let read = read_events(&path).unwrap();
    assert!(
        !read.events.iter().any(|e| e["id"] == "x"),
        "an uncommitted span must be truncated even when the sidecar cannot be written"
    );
    assert!(torn_sidecars(&logs).is_empty());
}

#[test]
fn test_retry_after_a_failed_span_does_not_duplicate_its_events() {
    // DurableBatch-counterexamples.md §5 obligation 6: batches are never
    // retried in the model, so append-fail-then-retry is a code-side obligation.
    let (dir, path) = log_dir();
    let batch = vec![upsert("a"), upsert("b")];
    append_raw(
        &path,
        format!("{}\n{}\n", begin("attempt-1", 2), upsert("a")).as_bytes(),
    );

    let _lock = acquire_lock(&dir.path().join(".lock")).unwrap();
    append_events_batch(&path, &batch).unwrap();

    assert_eq!(read_events(&path).unwrap().events, batch);
}

#[test]
fn test_legacy_reader_accepted_newlineless_tail_is_still_preserved() {
    // The `events.rs` regression fence: a legacy standalone record that the
    // reader already accepts must never be removed by an append.
    let (_dir, path) = log_dir();
    write_raw(&path, upsert("accepted").to_string().as_bytes());
    let before = read_events(&path).unwrap();
    assert_eq!(before.events, vec![upsert("accepted")]);

    append_event(&path, &upsert("next")).unwrap();

    assert_eq!(
        read_events(&path).unwrap().events,
        vec![upsert("accepted"), upsert("next")]
    );
}

// ---------------------------------------------------------------------------
// DurableIsPrefix — the code-side obligation (§5 obligation 1)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(32)
    ))]

    /// `DurableIsPrefix`, the invariant `DurableBatch.tla` documents but cannot
    /// discriminate (counterexamples §5 obligation 1): the repair path must
    /// never leave the durable log a non-prefix of what was written. Both
    /// halves are checked — the committed bytes survive untouched, and the
    /// committed events survive as a prefix of what the next read returns.
    #[test]
    fn test_repair_never_rewrites_the_committed_prefix(
        committed in 0usize..4,
        tail in prop::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 0..24),
    ) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut raw = String::new();
        for i in 0..committed {
            raw.push_str(&format!("{}\n", upsert(&format!("c{i}"))));
        }
        let mut raw = raw.into_bytes();
        raw.extend_from_slice(&tail);
        fs::write(&path, &raw).unwrap();

        let before = read_events(&path).unwrap();
        let durable = before.committed_len as usize;

        let _lock = acquire_lock(&dir.path().join(".lock")).unwrap();
        if append_event(&path, &upsert("probe")).is_err() {
            return Ok(());
        }

        let after_bytes = fs::read(&path).unwrap();
        prop_assert!(
            after_bytes.len() >= durable && after_bytes[..durable] == raw[..durable],
            "repair rewrote already-durable bytes"
        );
        let after = read_events(&path).unwrap();
        prop_assert!(
            after.events.starts_with(&before.events),
            "repair dropped or reordered a committed event"
        );
        prop_assert_eq!(after.events.last(), Some(&upsert("probe")));
    }
}

// ---------------------------------------------------------------------------
// Corpus replay — legacy and every event kind
// ---------------------------------------------------------------------------

/// One event of every kind the log carries, in the order a real session emits
/// them: entry upserts, an expire, evidence add/expire, a citation heal, a test
/// case upsert and a run-history insert.
fn synthetic_corpus() -> Vec<Value> {
    let mut corpus = Vec::new();
    for i in 0..3 {
        corpus.push(upsert(&format!("e{i}")));
    }
    corpus.push(json!({
        "action": "expire", "table": "entries", "id": "e1",
        "reason": "superseded", "ts": "2024-01-02T00:00:00Z",
    }));
    corpus.push(json!({
        "action": "evidence_add", "table": "evidence", "entry_id": "e0",
        "evidence": {
            "id": "ev-1", "entry_id": "e0", "kind": "code",
            "citation_path": "src/lib.rs", "citation_sha": "deadbeef",
            "citation_hash": "cafe", "citation_excerpt": "fn main() {}",
            "derived_from": null, "recorded_at": "2024-01-02T00:00:00Z",
        },
        "version_ref": "deadbeef", "ts": "2024-01-02T00:00:00Z",
    }));
    corpus.push(json!({
        "action": "citation_healed", "table": "evidence", "entry_id": "e0",
        "evidence_id": "ev-1", "old_path": "src/lib.rs", "new_path": "src/main.rs",
        "citation_hash": "cafe", "version_ref": null, "ts": "2024-01-03T00:00:00Z",
    }));
    corpus.push(json!({
        "action": "evidence_expire", "table": "evidence", "entry_id": "e0",
        "evidence_id": "ev-1", "reason": "stale", "ts": "2024-01-04T00:00:00Z",
    }));
    corpus.push(json!({
        "action": "upsert", "table": "test_cases", "id": "tc-1",
        "app": "kb", "name": "smoke", "protocol": "rust_tool",
        "config": "{}", "ts": "2024-01-05T00:00:00Z",
    }));
    corpus.push(json!({
        "action": "insert", "table": "run_history", "test_id": "tc-1",
        "run_id": "run-1", "result": "pass", "detail": "ok",
        "adapter": "rust_tool", "ts": "2024-01-05T00:01:00Z",
    }));
    corpus
}

/// Materialize a log into an in-memory DB and dump every materialized table.
///
/// `created_at` and `updated_at` are skipped: they are populated by
/// `datetime('now')` defaults, so two replays of the same log straddling a
/// second boundary disagree on them. That replay non-determinism is C1's T6
/// rider, not something framing changes. Every other column, including the
/// event-supplied `ts` and `recorded_at`, is compared.
fn materialize(events: &[Value]) -> Vec<String> {
    let conn = open_db_memory().unwrap();
    for ev in events {
        apply_event(&conn, &NoopEmbedder, ev).unwrap();
    }
    let mut rows = Vec::new();
    for table in ["entries", "evidence", "cues", "test_cases", "run_history"] {
        let columns: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .filter(|c| c != "created_at" && c != "updated_at")
            .collect();
        let mut stmt = conn
            .prepare(&format!("SELECT {} FROM {table}", columns.join(",")))
            .unwrap();
        let count = stmt.column_count();
        let mut dumped: Vec<String> = stmt
            .query_map([], |row| {
                let mut cells = Vec::new();
                for i in 0..count {
                    let v: rusqlite::types::Value = row.get(i)?;
                    cells.push(format!("{v:?}"));
                }
                Ok(cells.join("|"))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        dumped.sort();
        rows.push(format!("{table}: {}", dumped.join(";")));
    }
    rows
}

#[test]
fn test_synthetic_corpus_of_every_kind_replays_identically_framed_and_unframed() {
    let (_dir, legacy_path) = log_dir();
    let (_dir2, framed_path) = log_dir();
    let corpus = synthetic_corpus();

    // Legacy shape: one marker-less line per event.
    let mut legacy = String::new();
    for ev in &corpus {
        legacy.push_str(&format!("{ev}\n"));
    }
    write_raw(&legacy_path, legacy.as_bytes());

    // Framed shape: written through the new writers, one span per append.
    for ev in &corpus {
        append_event(&framed_path, ev).unwrap();
    }

    let legacy_read = read_events(&legacy_path).unwrap();
    let framed_read = read_events(&framed_path).unwrap();
    assert_eq!(legacy_read.events, corpus);
    assert_eq!(framed_read.events, corpus);
    assert_eq!(materialize(&legacy_read.events), materialize(&corpus));
    assert_eq!(materialize(&framed_read.events), materialize(&corpus));
}

/// The real event log of this repository. Not committed: it is the user's live
/// knowledge base. Point `KB_REAL_CORPUS_EVENTS` at a log to run this lane, or
/// let it discover `.state/agent-kb/agent-kb-events.jsonl` above the manifest.
fn real_corpus_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("KB_REAL_CORPUS_EVENTS") {
        let p = PathBuf::from(explicit);
        return p.exists().then_some(p);
    }
    let mut cursor = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = cursor
            .join(".state")
            .join("agent-kb")
            .join("agent-kb-events.jsonl");
        if candidate.exists() {
            return Some(candidate);
        }
        if !cursor.pop() {
            return None;
        }
    }
}

#[test]
fn test_real_corpus_replays_identically_through_the_framed_writers() {
    let Some(corpus_path) = real_corpus_path() else {
        eprintln!(
            "log_framing: no real corpus found — set KB_REAL_CORPUS_EVENTS to run this lane"
        );
        return;
    };
    let original = read_events(&corpus_path).unwrap();
    assert!(
        original.events.len() > 100,
        "the real corpus lane needs the production log, found {} events",
        original.events.len()
    );

    let (_dir, framed) = log_dir();
    append_events_batch(&framed, &original.events).unwrap();
    let reread = read_events(&framed).unwrap();

    assert_eq!(reread.events, original.events);
    assert_eq!(reread.committed_len, fs::metadata(&framed).unwrap().len());
    assert_eq!(materialize(&reread.events), materialize(&original.events));
}

// ---------------------------------------------------------------------------
// D7 — `kb compact` is the documented downgrade path: it strips markers
// ---------------------------------------------------------------------------

#[test]
fn test_compact_emits_a_marker_free_legacy_readable_log() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
    let paths = kb::config::Paths::from_root(root);

    append_events_batch(&paths.events, &[upsert("k1"), upsert("k2")]).unwrap();
    append_event(&paths.events, &upsert("k3")).unwrap();
    assert!(actions(&paths.events).iter().any(|a| a == "batch_begin"));

    let (before, after) = kb::commands::compact::Compact
        .execute_with_paths(&paths)
        .unwrap();
    assert_eq!(before, 3, "marker lines must not count toward original_count");
    assert_eq!(after, 3);

    let compacted = actions(&paths.events);
    assert!(
        compacted.iter().all(|a| a == "upsert"),
        "compact must emit a marker-free log, got {compacted:?}"
    );
    assert_eq!(
        read_events(&paths.events).unwrap().events.len(),
        3,
        "the compacted log stays readable"
    );
}
