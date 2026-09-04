//! C2/L1a — the `open_db` split, `kb_core::add_locked`, and the `acquire_lock`
//! re-entrancy registry.
//!
//! Plan: `.state/.omc/plans/c2-exclusion-boundary.md` ADR-1 and ADR-7.
//! Waiver: `.state/agent-kb/tla/decisions/lock-contract-no-spec.md` — the
//! re-entrancy row requires that the registry test cover two path spellings
//! that canonicalize to one file.
//!
//! Every test that could hang on a lock runs the candidate operation on a
//! worker thread and waits with a bounded `recv_timeout`, so a re-entrancy or
//! self-deadlock regression fails CI instead of wedging production.

use kb::commands::add::acquire_lock;
use kb::components::{db, embedder::NoopEmbedder, kb_core};
use kb::config::Paths;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Bounded wait for any operation whose regression mode is "blocks forever".
const DEADLOCK_TIMEOUT: Duration = Duration::from_secs(10);

fn repo(root: &Path) -> Paths {
    std::fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
    Paths::from_root(root)
}

/// `<db>-wal`, not `<db>.wal`: SQLite appends the suffix to the full filename.
fn wal_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db_path.display()))
}

fn shm_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-shm", db_path.display()))
}

// ---------------------------------------------------------------------------
// open_ro
// ---------------------------------------------------------------------------

#[test]
fn open_ro_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    let conn = db::open_ro(&paths.db).unwrap();
    let err = conn
        .execute(
            "INSERT INTO entries(id, path, summary, content, tags) VALUES('x','p','s','c','[]')",
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("readonly"),
        "PRAGMA query_only must reject the INSERT, got: {err}"
    );
}

#[test]
fn open_ro_on_missing_db_is_db_uninitialized() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());

    let err = db::open_ro(&paths.db).unwrap_err();
    assert!(
        db::is_db_uninitialized(&err),
        "missing DB must map to DbUninitialized, got: {err:#}"
    );
    assert!(
        !paths.db.exists(),
        "open_ro must not create the database file"
    );
}

#[test]
fn open_ro_on_schemaless_db_is_db_uninitialized() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    // A file that is a valid SQLite database but carries no `entries` table.
    Connection::open(&paths.db)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated(x)")
        .unwrap();

    let err = db::open_ro(&paths.db).unwrap_err();
    assert!(
        db::is_db_uninitialized(&err),
        "a DB without the entries table must map to DbUninitialized, got: {err:#}"
    );
}

#[test]
fn uninitialized_note_names_kb_rebuild() {
    let note = db::uninitialized_note(Path::new("/tmp/nowhere/agent-kb.db"));
    assert_eq!(note.lines().count(), 1, "the note must be a single line");
    assert!(
        note.contains("kb rebuild"),
        "ADR-7: the staleness/first-run note names `kb rebuild`, got: {note}"
    );
}

/// ADR-1 Option D rejection: a read-only *file handle* cannot rebuild the
/// WAL index, so a reader arriving after a writer crash would fail instead of
/// recovering. `open_ro` therefore uses a read-write file handle plus
/// `PRAGMA query_only`. This test pins that premise.
///
/// Crash simulation, in-process and deterministic: the writer connection
/// commits with `wal_autocheckpoint=0` and is then `std::mem::forget`-ed, so
/// it is never closed and never checkpoints — the committed frames stay in the
/// `-wal` file. Removing the `-shm` wal-index file forces the next opener down
/// the recovery path, which is exactly the write that `SQLITE_OPEN_READ_ONLY`
/// would forbid.
#[test]
fn open_ro_recovers_a_hot_wal_left_by_a_crashed_writer() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    {
        let conn = Connection::open(&paths.db).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        conn.execute(
            "INSERT INTO entries(id, path, summary, content, tags)
             VALUES('crashed','p/crash','summary','content','[]')",
            [],
        )
        .unwrap();
        // Never closed: no checkpoint, no clean shutdown — the crash.
        std::mem::forget(conn);
    }

    let wal = wal_path(&paths.db);
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert!(
        wal_len > 0,
        "precondition: the committed row must still live in the hot WAL at {}",
        wal.display()
    );
    // Discard the wal-index so the next opener must rebuild it.
    let _ = std::fs::remove_file(shm_path(&paths.db));

    let conn = db::open_ro(&paths.db).expect("open_ro must recover a hot WAL");
    let summary: String = conn
        .query_row("SELECT summary FROM entries WHERE id='crashed'", [], |r| {
            r.get(0)
        })
        .expect("open_ro must read data committed before the crash");
    assert_eq!(summary, "summary");
}

// ---------------------------------------------------------------------------
// open_rw
// ---------------------------------------------------------------------------

#[test]
fn open_rw_rejects_a_lock_on_the_wrong_path() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    let other_lock_path = paths.lock.with_extension("other.lock");
    let other_lock = acquire_lock(&other_lock_path).unwrap();

    let err = db::open_rw(&paths, &other_lock).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("lock"),
        "open_rw must reject a token whose path is not paths.lock, got: {msg}"
    );
}

#[test]
fn open_rw_accepts_the_matching_lock_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    let lock = acquire_lock(&paths.lock).unwrap();
    let conn = db::open_rw(&paths, &lock).unwrap();
    conn.execute(
        "INSERT INTO entries(id, path, summary, content, tags) VALUES('w','p','s','c','[]')",
        [],
    )
    .expect("open_rw connection must accept writes");
}

/// `open_rw` must be usable on a repository that has never been initialized:
/// the write path holds the lock, so it may create the schema itself.
#[test]
fn open_rw_initializes_a_fresh_repository() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());

    let lock = acquire_lock(&paths.lock).unwrap();
    let conn = db::open_rw(&paths, &lock).unwrap();
    conn.execute(
        "INSERT INTO entries(id, path, summary, content, tags) VALUES('w','p','s','c','[]')",
        [],
    )
    .unwrap();
    assert!(db::schema_is_current(&conn));
}

// ---------------------------------------------------------------------------
// open_scratch
// ---------------------------------------------------------------------------

#[test]
fn open_scratch_refuses_the_live_db() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    let err = db::open_scratch(&paths.db).unwrap_err();
    assert!(
        err.to_string().contains("live"),
        "open_scratch must refuse paths.db, got: {err}"
    );
}

#[test]
fn open_scratch_opens_a_tmp_db_with_schema() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    let tmp = paths.db.with_extension("db.tmp.4242");

    let conn = db::open_scratch(&tmp).unwrap();
    conn.execute(
        "INSERT INTO entries(id, path, summary, content, tags) VALUES('t','p','s','c','[]')",
        [],
    )
    .expect("scratch DB must carry the schema and accept writes");
}

// ---------------------------------------------------------------------------
// open_or_init
// ---------------------------------------------------------------------------

#[test]
fn open_or_init_creates_dirs_schema_and_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path()); // note: no mkdir — open_or_init does it

    db::open_or_init(&paths).unwrap();

    assert!(paths.db.exists(), "open_or_init must create the DB file");
    let conn = db::open_ro(&paths.db).unwrap();
    assert!(
        db::schema_is_current(&conn),
        "a fresh DB must be stamped with the current schema version"
    );
}

/// `open_or_init` acquires and RELEASES `paths.lock`. If it leaked the guard,
/// the next acquire in this process would either hang on the flock or trip the
/// re-entrancy registry.
#[test]
fn open_or_init_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    let (tx, rx) = mpsc::channel();
    let lock_path = paths.lock.clone();
    thread::spawn(move || {
        let _ = tx.send(
            acquire_lock(&lock_path)
                .map(|_| ())
                .map_err(|e| e.to_string()),
        );
    });
    let outcome = rx
        .recv_timeout(DEADLOCK_TIMEOUT)
        .expect("open_or_init must release paths.lock before returning");
    assert!(outcome.is_ok(), "acquire after open_or_init: {outcome:?}");
}

// ---------------------------------------------------------------------------
// acquire_lock re-entrancy registry
// ---------------------------------------------------------------------------

#[test]
fn second_in_process_acquire_errors_instead_of_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    let first = acquire_lock(&paths.lock).unwrap();

    let (tx, rx) = mpsc::channel();
    let lock_path = paths.lock.clone();
    thread::spawn(move || {
        let _ = tx.send(
            acquire_lock(&lock_path)
                .map(|_| ())
                .map_err(|e| format!("{e:#}")),
        );
    });

    let outcome = rx
        .recv_timeout(DEADLOCK_TIMEOUT)
        .expect("a second in-process acquire must return an error, not block on the flock forever");
    let err = outcome.expect_err("the second acquire must fail");
    assert!(
        err.contains("open_split.rs"),
        "the error must name the first acquisition site, got: {err}"
    );
    drop(first);
}

/// Waiver row "process-local re-entrancy": the registry keys on the
/// CANONICALIZED path, so a second acquire spelled differently is still
/// recognized as re-entrant rather than silently deadlocking.
#[test]
fn registry_canonicalizes_two_spellings_of_one_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    let first = acquire_lock(&paths.lock).unwrap();

    // Same file, different spelling: `<root>/.state/agent-kb/../.lock`.
    let aliased = dir
        .path()
        .join(".state")
        .join("agent-kb")
        .join("..")
        .join(".lock");
    assert_ne!(
        aliased, paths.lock,
        "the two spellings must differ textually"
    );
    assert_eq!(
        std::fs::canonicalize(&aliased).unwrap(),
        std::fs::canonicalize(&paths.lock).unwrap(),
        "precondition: both spellings name one file"
    );

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(
            acquire_lock(&aliased)
                .map(|_| ())
                .map_err(|e| format!("{e:#}")),
        );
    });
    let outcome = rx
        .recv_timeout(DEADLOCK_TIMEOUT)
        .expect("an aliased re-acquire must error, not block");
    assert!(
        outcome.is_err(),
        "an aliased second acquire must be recognized as re-entrant"
    );
    drop(first);
}

#[test]
fn registry_entry_is_released_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());

    drop(acquire_lock(&paths.lock).unwrap());
    acquire_lock(&paths.lock).expect("dropping the guard must clear the registry entry");
}

#[test]
fn lock_carries_its_canonical_path() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    let lock = acquire_lock(&paths.lock).unwrap();
    assert_eq!(lock.path(), std::fs::canonicalize(&paths.lock).unwrap());
}

// ---------------------------------------------------------------------------
// kb_core::add_locked
// ---------------------------------------------------------------------------

fn add_args(id: &str) -> kb_core::AddArgs {
    kb_core::AddArgs {
        id: id.to_string(),
        path: "tests/open-split".to_string(),
        summary: "add_locked".to_string(),
        content: "body".to_string(),
        tags: serde_json::json!([]),
        version_ref: None,
        permanent: false,
        replace_path: false,
        kind: "convention".to_string(),
        evidence_status: "n/a".to_string(),
        evidence_rows: vec![],
        ts: "2026-09-04T00:00:00Z".to_string(),
        session: "test".to_string(),
        session_id: None,
        expire_reason: String::new(),
        dedup_cutoff: None,
        cues: vec![],
    }
}

/// ADR-1: without `add_locked`, any caller already holding the flock
/// self-deadlocks in `add`. The bounded wait turns that regression into a
/// failing test rather than a wedged process.
#[test]
fn add_locked_does_not_block_a_caller_that_already_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    let (tx, rx) = mpsc::channel();
    let paths_for_worker = paths.clone();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<String> {
            let lock = acquire_lock(&paths_for_worker.lock)?;
            let conn = db::open_rw(&paths_for_worker, &lock)?;
            let outcome = kb_core::add_locked(
                &lock,
                &conn,
                &paths_for_worker,
                &NoopEmbedder,
                add_args("locked-1"),
            )?;
            Ok(outcome.entry_id)
        })();
        let _ = tx.send(result.map_err(|e| format!("{e:#}")));
    });

    let outcome = rx
        .recv_timeout(DEADLOCK_TIMEOUT)
        .expect("add_locked must not re-acquire the flock it was handed");
    assert_eq!(outcome.unwrap(), "locked-1");

    let conn = db::open_ro(&paths.db).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE id='locked-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "add_locked must have written the entry");
}

/// The thin wrapper still works end to end for callers that hold no lock.
#[test]
fn add_acquires_the_lock_for_callers_that_hold_none() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());

    let (tx, rx) = mpsc::channel();
    let paths_for_worker = paths.clone();
    thread::spawn(move || {
        let result = kb_core::add(&paths_for_worker, &NoopEmbedder, add_args("wrapped-1"))
            .map(|o| o.entry_id)
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
    });
    let outcome = rx
        .recv_timeout(DEADLOCK_TIMEOUT)
        .expect("add must not hang");
    assert_eq!(outcome.unwrap(), "wrapped-1");
}

// ---------------------------------------------------------------------------
// test_db fixture helper
// ---------------------------------------------------------------------------

#[test]
fn test_db_fixture_returns_paths_and_a_writable_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (paths, conn) = db::test_db(dir.path());

    assert_eq!(paths.db, Paths::from_root(dir.path()).db);
    conn.execute(
        "INSERT INTO entries(id, path, summary, content, tags) VALUES('f','p','s','c','[]')",
        [],
    )
    .expect("the fixture connection must be writable");
}
