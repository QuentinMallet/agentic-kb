//! L1c: permanent opener mapping and lock behavior for the migrated surfaces.

use kb::commands::add::acquire_lock;
use kb::components::db;
use kb::config::Paths;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

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
    ];
    for (name, source) in readers {
        assert!(
            source.contains("open_ro("),
            "{name} lost its read-only opener"
        );
        assert!(
            !source.contains("open_db("),
            "{name} revived the legacy opener"
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    db::open_or_init(&paths).unwrap();
    let bytes_before = std::fs::read(&paths.db).unwrap();
    let lock = acquire_lock(&paths.lock).unwrap();

    let (tx, rx) = mpsc::channel();
    let read_path = paths.db.clone();
    let reader = thread::spawn(move || tx.send(db::open_ro(&read_path).map(|_| ())).unwrap());
    rx.recv_timeout(TIMEOUT)
        .expect("a migrated read blocked on paths.lock")
        .expect("a migrated read failed while paths.lock was held");
    reader.join().unwrap();
    assert_eq!(std::fs::read(&paths.db).unwrap(), bytes_before);
    drop(lock);
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
