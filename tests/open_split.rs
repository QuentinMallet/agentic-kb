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

fn insert_expired_peer(conn: &Connection, source_repo: &str, target_repo: &str) {
    conn.execute(
        "INSERT INTO graphs(id, graph_type, source_repo, created_at, expires_at)
         VALUES('graph-expired', 'epic', ?1, '2024-01-01T00:00:00Z', '2000-01-01 00:00:00')",
        [source_repo],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO peers(
            id, graph_id, source_repo, target_repo, edge_type, created_at, expires_at
         ) VALUES(
            'peer-expired', 'graph-expired', ?1, ?2, 'member', '2024-01-01T00:00:00Z',
            '2000-01-01 00:00:00'
         )",
        [source_repo, target_repo],
    )
    .unwrap();
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

/// Companion to the test above, but for `open_ro_peer`: unlike `open_ro`, a
/// peer opener must never write a single byte of the peer's `db`, `-wal`,
/// or `-shm` files, not even to serve a read. It always opens with the
/// `immutable=1` URI hint (see its doc comment for why a plain
/// `SQLITE_OPEN_READ_ONLY` handle is not sufficient — it can still write
/// read-mark bytes into an *existing* `-shm`), which ignores the WAL/shm
/// machinery entirely, so a row sitting only in a hot, unmerged WAL is
/// invisible through it even when the `-shm` is missing outright. That is
/// the documented trade this opener makes to guarantee a peer's on-disk
/// bytes are never written.
#[test]
fn open_ro_peer_never_writes_and_ignores_a_hot_wal_when_shm_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    {
        let conn = Connection::open(&paths.db).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        conn.execute(
            "INSERT INTO entries(id, path, summary, content, tags)
             VALUES('hot','p/hot','hot summary','hot content','[]')",
            [],
        )
        .unwrap();
        // Never closed: no checkpoint, no clean shutdown — a hot WAL.
        std::mem::forget(conn);
    }

    let wal = wal_path(&paths.db);
    assert!(
        std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 0,
        "precondition: the committed row must still live only in the hot WAL"
    );
    // Discard the wal-index so a strictly read-only opener cannot recover it.
    let _ = std::fs::remove_file(shm_path(&paths.db));

    let db_bytes_before = std::fs::read(&paths.db).unwrap();
    let wal_bytes_before = std::fs::read(&wal).unwrap();

    let conn = db::open_ro_peer(&paths.db)
        .expect("open_ro_peer must open successfully even with no -shm present");
    let found: i64 = conn
        .query_row("SELECT COUNT(*) FROM entries WHERE id='hot'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        found, 0,
        "open_ro_peer's immutable open ignores the WAL, so the hot-only row must not be visible"
    );
    drop(conn);

    assert_eq!(
        std::fs::read(&paths.db).unwrap(),
        db_bytes_before,
        "open_ro_peer must never rewrite the peer's main db file"
    );
    assert_eq!(
        std::fs::read(&wal).unwrap(),
        wal_bytes_before,
        "open_ro_peer must never touch the peer's -wal file"
    );
    assert!(
        !shm_path(&paths.db).exists(),
        "open_ro_peer's immutable open must never recreate the peer's -shm file"
    );
}

/// `open_ro_peer` builds a `file:...?immutable=1` URI by string
/// interpolation, so a peer path containing a character meaningful to the
/// URI-filename grammar has to be percent-encoded first, or SQLite misparses
/// the path and the peer is silently skipped as uninitialized. A `#` is the
/// sharpest case: unencoded, it introduces a URI fragment, truncating the
/// path SQLite actually opens.
#[test]
fn open_ro_peer_percent_encodes_a_hash_in_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().join("repo#with-hash");
    let paths = repo(&repo_root);
    db::open_or_init(&paths).unwrap();

    let conn = db::open_ro_peer(&paths.db)
        .expect("open_ro_peer must percent-encode a '#' rather than misparse the URI");
    let entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entries'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        entries, 1,
        "open_ro_peer must open the real db at the '#'-containing path, not a misparsed one"
    );
}

#[test]
fn open_or_init_does_not_sweep_expired_peers() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    {
        let conn = Connection::open(&paths.db).unwrap();
        insert_expired_peer(&conn, "repo-a", "repo-b");
    }

    db::open_or_init(&paths).unwrap();

    let conn = Connection::open(&paths.db).unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM peers WHERE id='peer-expired'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining, 1,
        "open_or_init must keep expired peers physically present; L1b moves deletion to locked writers"
    );
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

    for (opener, err) in [
        ("open_rw", db::open_rw(&paths, &other_lock).unwrap_err()),
        (
            "open_rw_existing",
            db::open_rw_existing(&paths, &other_lock).unwrap_err(),
        ),
    ] {
        let msg = err.to_string();
        assert!(
            msg.contains("lock"),
            "{opener} must reject a token whose path is not paths.lock, got: {msg}"
        );
    }
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
fn open_scratch_refuses_the_live_db_through_an_equivalent_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    db::open_or_init(&paths).unwrap();

    let spelled_differently = dir
        .path()
        .join(".")
        .join(".state")
        .join("agent-kb")
        .join("agent-kb.db");
    let err = db::open_scratch(&spelled_differently).unwrap_err();
    assert!(
        err.to_string().contains("live"),
        "open_scratch must refuse the live DB even through an equivalent path spelling, got: {err}"
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

#[test]
fn open_scratch_allows_an_unrelated_agent_kb_db_name() {
    let dir = tempfile::tempdir().unwrap();
    let unrelated = dir.path().join("nested").join("agent-kb.db");

    let conn = db::open_scratch(&unrelated).unwrap();
    conn.execute(
        "INSERT INTO entries(id, path, summary, content, tags) VALUES('u','p','s','c','[]')",
        [],
    )
    .expect("an unrelated agent-kb.db filename must not be treated as the live DB");
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

/// The self-deadlock ADR-1 targets: one call chain acquires a lock it already
/// holds. Both acquires run on the worker thread; the main thread's bounded
/// wait turns a regression into a failing test rather than a wedged process.
#[test]
fn second_acquire_on_the_same_thread_errors_instead_of_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());

    let (tx, rx) = mpsc::channel();
    let lock_path = paths.lock.clone();
    thread::spawn(move || {
        let first_site = line!() + 1;
        let _first = acquire_lock(&lock_path).expect("first acquire");
        let second = acquire_lock(&lock_path)
            .map(|_| ())
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send((first_site, second));
    });

    let (first_site, outcome) = rx.recv_timeout(DEADLOCK_TIMEOUT).expect(
        "a second acquire on the same thread must return an error, not block on the flock forever",
    );
    let err = outcome.expect_err("the second acquire must fail");
    // The FIRST acquisition's file:line, not the second's — that is what makes
    // the error actionable when the two live in different modules.
    let expected_site = format!("tests/open_split.rs:{first_site}");
    assert!(
        err.contains(&expected_site),
        "the error must name the first acquisition site ({expected_site}), got: {err}"
    );
}

/// The registry must NOT reject a different thread: two threads contending for
/// one flock is ordinary mutual exclusion, and `rebuild`'s schema-upgrade
/// single-flight and its Phase 2 concurrent-writer guarantee both depend on it.
/// The second thread blocks while the first holds the lock, then succeeds.
#[test]
fn a_second_thread_blocks_on_the_flock_rather_than_being_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());
    let first = acquire_lock(&paths.lock).unwrap();

    let (tx, rx) = mpsc::channel();
    let lock_path = paths.lock.clone();
    let waiter = thread::spawn(move || {
        let guard = acquire_lock(&lock_path);
        let _ = tx.send(guard.map(|_| ()).map_err(|e| format!("{e:#}")));
    });

    // While the first guard lives the waiter must be blocked, not rejected.
    match rx.recv_timeout(Duration::from_millis(500)) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(other) => panic!("waiter thread died: {other:?}"),
        Ok(early) => panic!("a second thread must block on the flock, got: {early:?}"),
    }

    drop(first);
    let outcome = rx
        .recv_timeout(DEADLOCK_TIMEOUT)
        .expect("the waiter must acquire once the first guard is released");
    assert!(outcome.is_ok(), "waiter acquire: {outcome:?}");
    waiter.join().unwrap();
}

/// Waiver row "process-local re-entrancy": the registry keys on the
/// CANONICALIZED path, so a re-acquire spelled differently is still recognized
/// as re-entrant rather than silently deadlocking.
#[test]
fn registry_canonicalizes_two_spellings_of_one_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = repo(dir.path());

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

    let (tx, rx) = mpsc::channel();
    let lock_path = paths.lock.clone();
    thread::spawn(move || {
        let _first = acquire_lock(&lock_path).expect("first acquire");
        assert_eq!(
            std::fs::canonicalize(&aliased).unwrap(),
            std::fs::canonicalize(&lock_path).unwrap(),
            "precondition: both spellings name one file"
        );
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
