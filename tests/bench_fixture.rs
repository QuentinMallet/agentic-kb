use kb::bench_fixture::{logical_checksum, seed_db, seed_fixture, BenchEmbedder, DEFAULT_SEED};
use kb::commands::add::acquire_lock;
use kb::components::cursor::{self, Decision};
use kb::components::db;
use kb::components::events;
use kb::components::kb_core::{self, AddArgs};
use serde_json::{json, Value};
use std::fs;

/// Bytes scanned backwards by `events::ends_on_intact_span`'s fast path. Kept
/// as a literal because the production constant is module-private; there is
/// no read-budget seam exposed yet to assert the actual bytes a `kb add`
/// reads against a fresh fixture, so this test only checks that the final
/// span fits inside the window the fast path scans.
const READER_TAIL_WINDOW_BYTES: usize = 64 * 1024;

/// Byte length of every framed `batch_begin`/body/`batch_commit` span in a raw
/// event log, in file order. Also asserts each span is well-formed: paired
/// begin/commit with matching `batch_id` and `n`, and exactly `n` body lines
/// between them — the same shape a dropped `batch.clear()` or an off-by-one
/// at a batch boundary would corrupt.
fn span_byte_lengths(log: &str) -> Vec<usize> {
    let mut lengths = Vec::new();
    let mut span_start: Option<usize> = None;
    let mut expected: Option<(String, u64)> = None;
    let mut body_count = 0u64;
    let mut byte_pos = 0usize;
    for line in log.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        if !trimmed.is_empty() {
            let value: Value = serde_json::from_str(trimmed)
                .unwrap_or_else(|e| panic!("invalid log line {trimmed:?}: {e}"));
            match value["action"].as_str() {
                Some("batch_begin") => {
                    assert!(
                        span_start.is_none(),
                        "batch_begin without a preceding batch_commit"
                    );
                    span_start = Some(byte_pos);
                    let batch_id = value["batch_id"].as_str().unwrap().to_string();
                    let n = value["n"].as_u64().unwrap();
                    expected = Some((batch_id, n));
                    body_count = 0;
                }
                Some("batch_commit") => {
                    let start = span_start.take().expect("batch_commit without batch_begin");
                    let (batch_id, n) = expected.take().unwrap();
                    assert_eq!(value["batch_id"].as_str(), Some(batch_id.as_str()));
                    assert_eq!(value["n"].as_u64(), Some(n));
                    assert_eq!(body_count, n, "body event count does not match declared n");
                    lengths.push(byte_pos + line.len() - start);
                }
                _ => {
                    if span_start.is_some() {
                        body_count += 1;
                    }
                }
            }
        }
        byte_pos += line.len();
    }
    assert!(span_start.is_none(), "log ends with an unclosed span");
    lengths
}

#[test]
fn seeded_benchmark_fixture_is_converged_and_accepts_add() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let (paths, unlocked_conn) = db::test_db(dir.path());
    drop(unlocked_conn);
    let lock = acquire_lock(&paths.lock).unwrap();
    let conn = db::open_rw(&paths, &lock).unwrap();
    let embedder = BenchEmbedder::new(DEFAULT_SEED);

    // 200 entries -> 400 events. With SEED_BATCH_EVENTS=128 that is three
    // in-loop flushes (128, 128, 128) plus a 16-event trailing flush: the
    // in-loop `batch.clear()` branch runs multiple times, which a 50-entry
    // (100-event, below the threshold) seed could never exercise.
    seed_fixture(&lock, &conn, &paths, &embedder, 200, DEFAULT_SEED).unwrap();

    assert_eq!(cursor::inspect(&conn, &paths), Decision::NoOp);
    assert!(paths.events.is_file());
    let log = fs::read_to_string(&paths.events).unwrap();
    assert!(log.contains("\"action\":\"batch_begin\""));
    assert!(log.contains("\"action\":\"batch_commit\""));
    let committed_len = events::read_events(&paths.events).unwrap().committed_len;
    assert_eq!(cursor::read(&conn).unwrap().unwrap().offset, committed_len);

    let spans = span_byte_lengths(&log);
    assert!(
        spans.len() >= 2,
        "expected multiple framed spans (in-loop flush branch exercised), got {}",
        spans.len()
    );
    let last = *spans.last().unwrap();
    assert!(
        last < READER_TAIL_WINDOW_BYTES,
        "final span is {last} bytes, at or above the reader's {READER_TAIL_WINDOW_BYTES}-byte \
         tail window; a kb add against a fresh copy of this fixture would miss the fast path \
         and fall back to a full log scan"
    );

    drop(conn);
    drop(lock);
    kb_core::add(
        &paths,
        &embedder,
        AddArgs {
            id: "after-fixture".to_string(),
            path: "bench/after-fixture".to_string(),
            summary: "write after fixture seed".to_string(),
            content: "the converged fixture accepts a sanctioned write".to_string(),
            tags: json!(["bench"]),
            version_ref: None,
            permanent: false,
            replace_path: false,
            kind: "observation".to_string(),
            evidence_status: "missing".to_string(),
            evidence_rows: vec![],
            ts: "2024-01-02T00:00:00Z".to_string(),
            session: "test".to_string(),
            session_id: None,
            expire_reason: "replaced by test".to_string(),
            dedup_cutoff: None,
            cues: vec![],
        },
    )
    .unwrap();
}

#[test]
fn seed_db_and_seed_fixture_agree_including_evidence_status() {
    let embedder = BenchEmbedder::new(DEFAULT_SEED);

    let mem_conn = db::open_db_memory().unwrap();
    seed_db(&mem_conn, &embedder, 50, DEFAULT_SEED).unwrap();
    let mem_checksum = logical_checksum(&mem_conn).unwrap();

    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let (paths, unlocked_conn) = db::test_db(dir.path());
    drop(unlocked_conn);
    let lock = acquire_lock(&paths.lock).unwrap();
    let file_conn = db::open_rw(&paths, &lock).unwrap();
    seed_fixture(&lock, &file_conn, &paths, &embedder, 50, DEFAULT_SEED).unwrap();
    let file_checksum = logical_checksum(&file_conn).unwrap();

    assert_eq!(
        mem_checksum, file_checksum,
        "seed_db (criterion benches) and seed_fixture (CLI fixture) must produce identical \
         entries, including evidence_status, for the same n/seed"
    );
}
