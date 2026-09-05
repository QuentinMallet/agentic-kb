//! L1c: permanent opener mapping and lock behavior for the migrated surfaces.

use kb::commands::add::{acquire_lock, Add};
use kb::commands::compress::{run as compress_run, Compress};
use kb::commands::eval::Eval;
use kb::commands::stale_check::{RelocateArg, StaleCheck};
use kb::commands::tests::Tests;
use kb::components::db;
use kb::components::embedder::NoopEmbedder;
use kb::config::{KbConfig, Paths};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// The portion of `source` before its own `#[cfg(test)] mod ... { ... }`
/// unit-test module (every file below has exactly one, the file's own unit
/// tests), so a grep against it cannot be satisfied by a mention inside the
/// file's own tests rather than its production code.
fn production_source(source: &str) -> &str {
    match source.find("\n#[cfg(test)]") {
        Some(idx) => &source[..idx],
        None => source,
    }
}

#[test]
fn migrated_read_surfaces_are_pinned_to_open_ro() {
    let readers = [
        ("search", include_str!("../src/commands/search.rs")),
        ("eval", include_str!("../src/commands/eval.rs")),
        ("digest", include_str!("../src/commands/digest.rs")),
        ("older-than", include_str!("../src/commands/older_than.rs")),
        (
            "stale-check",
            include_str!("../src/commands/stale_check.rs"),
        ),
        ("tests list", include_str!("../src/commands/tests.rs")),
        ("compress", include_str!("../src/commands/compress.rs")),
        (
            "mcp (handle_audit_report)",
            include_str!("../src/commands/mcp.rs"),
        ),
        (
            "migrate-citations",
            include_str!("../src/commands/migrate_citations.rs"),
        ),
    ];
    for (name, source) in readers {
        let production = production_source(source);
        assert!(
            production.contains("open_ro("),
            "{name} lost its read-only opener"
        );
        assert!(
            !production.contains("open_db("),
            "{name} revived the legacy opener"
        );
    }
}

/// Seed one real entry through the production `Add` CLI path, matching the
/// fixture `src/commands/eval.rs`'s own tests already use, so every reader
/// exercised below has real data to serve rather than an empty database.
fn seed_repo(paths: &Paths) {
    Add {
        path: "src/auth.rs".to_string(),
        summary: "authentication jwt tokens".to_string(),
        content: "verifies bearer jwt".to_string(),
        tags: "auth".to_string(),
        version_ref: Some("abc123".to_string()),
        id: Some("l1c-seed-1".to_string()),
        permanent: false,
        replace_path: false,
        kind: "convention".to_string(),
        evidence: vec![],
        evidence_file: None,
        cues: vec![],
    }
    .execute_with(paths, &NoopEmbedder)
    .unwrap();
}

/// Run `f` on its own thread and require it to finish within [`TIMEOUT`]
/// with `Ok`, so a migrated reader that actually blocks on `paths.lock` (or
/// errors while it is held) fails this assertion instead of hanging the
/// test suite.
fn require_unblocked<F>(label: &str, f: F)
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(f()).unwrap());
    rx.recv_timeout(TIMEOUT)
        .unwrap_or_else(|_| panic!("{label} blocked on paths.lock"))
        .unwrap_or_else(|e| panic!("{label} failed while paths.lock was held: {e}"));
}

/// A migrated read surface must genuinely serve the caller's data while a
/// concurrent writer holds `paths.lock` -- the property ADR-1's split is
/// for, not merely a property of the shared opener functions (which
/// `tests/open_split.rs` already covers directly). This drives the actual
/// command entry points named by the L1c review, not `db::open_ro` itself.
#[test]
fn migrated_readers_serve_data_while_the_write_lock_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    seed_repo(&paths);
    let entries_before: i64 = db::open_ro(&paths.db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(entries_before, 1, "the seed must have landed");

    let lock = acquire_lock(&paths.lock).unwrap();

    // Tests::execute_with: a pure read, must not block or error.
    {
        let paths = paths.clone();
        require_unblocked("Tests::execute_with", move || {
            Tests { app: None }.execute_with(&paths)
        });
    }

    // compress::run: a pure read (the seeded entry is well below any
    // threshold, so this never reaches the locked write path) -- must not
    // block or error.
    {
        let paths = paths.clone();
        require_unblocked("compress::run", move || {
            let cmd = Compress {
                path: "src/auth.rs".to_string(),
                threshold_chars: None,
                dry_run: false,
            };
            compress_run(&cmd, &KbConfig::from_paths(&paths), &paths, &NoopEmbedder)
        });
    }

    // StaleCheck::execute_with: returns structured data rather than `()`,
    // so assert on it directly -- `checked` must equal the number of files
    // named, proving the migrated reader actually ran against the caller's
    // arguments rather than short-circuiting on the held lock.
    {
        let (tx, rx) = mpsc::channel();
        let paths = paths.clone();
        thread::spawn(move || {
            let report = StaleCheck {
                files: vec!["src/auth.rs".to_string()],
                commits: vec![],
                blame: false,
                relocate: RelocateArg::Never,
            }
            .execute_with(&paths);
            tx.send(report).unwrap();
        });
        let report = rx
            .recv_timeout(TIMEOUT)
            .expect("StaleCheck::execute_with blocked on paths.lock")
            .expect("StaleCheck::execute_with failed while paths.lock was held");
        assert_eq!(report.checked, 1, "must have scanned the named file");
    }

    // Eval::execute_with: also returns `()`, but a `min_recall` gate turns
    // "found the seeded entry" into the difference between `Ok` and `Err` --
    // an empty read (the failure mode this test exists to catch) would score
    // 0.0 and fail the gate, while a genuine read of the seeded entry scores
    // 1.0 on an exact FTS match and passes it.
    {
        let golden_dir = tempfile::tempdir().unwrap();
        let golden = golden_dir.path().join("golden.jsonl");
        std::fs::write(
            &golden,
            "{\"query\": \"authentication jwt\", \"expected_ids\": [\"l1c-seed-1\"], \"split\": \"dev\"}\n",
        )
        .unwrap();
        let paths = paths.clone();
        require_unblocked("Eval::execute_with", move || {
            let eval = Eval {
                golden: Some(golden),
                sealed: false,
                compare: None,
                fts: true,
                semantic: false,
                k: 10,
                json: false,
                min_recall: Some(0.99),
                min_mrr: None,
            };
            eval.execute_with(&paths, &NoopEmbedder)
        });
        drop(golden_dir);
    }

    drop(lock);

    // No reader above ever took the write lock or mutated the repository:
    // the same entry is still there, by count rather than raw file bytes
    // (SQLite may checkpoint the WAL into the main file on connection
    // close, which a byte-for-byte comparison would misreport as a write).
    let entries_after: i64 = db::open_ro(&paths.db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(entries_after, entries_before);
}

#[test]
fn migrated_writer_entry_waits_for_the_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    db::open_or_init(&paths).unwrap();
    let lock = acquire_lock(&paths.lock).unwrap();

    let (tx, rx) = mpsc::channel();
    let writer_paths = paths.clone();
    let writer = thread::spawn(move || tx.send(db::open_or_init(&writer_paths)).unwrap());
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(500)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    drop(lock);
    rx.recv_timeout(TIMEOUT)
        .expect("writer did not resume after paths.lock was released")
        .expect("writer failed after paths.lock was released");
    writer.join().unwrap();
}
