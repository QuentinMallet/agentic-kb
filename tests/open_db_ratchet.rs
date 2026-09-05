use std::fs;
use std::path::Path;

/// L1b/L2/L3/L1c: lower this as each migrated call site stops using `open_db`.
///
/// Bumped 82 -> 87 when L1a rebased onto bd-21ef.2 (B2/P1/A1): those commits
/// added 5 unmigrated `db::open_db(&paths.db)` call sites in
/// `src/commands/mcp.rs` test helpers, ahead of L1a's own migration work.
/// Not new legacy debt introduced by L1a itself.
const OPEN_DB_CALLSITE_RATCHET: usize = 87;

fn src_rs_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            files.extend(src_rs_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn open_db_callsites_do_not_increase() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut count = 0usize;

    for path in src_rs_files(&src_root) {
        for line in fs::read_to_string(&path).unwrap().lines() {
            if !line.contains("open_db(") {
                continue;
            }
            let trimmed = line.trim();
            if trimmed.starts_with("fn legacy_open_db(")
                || trimmed.starts_with("pub fn open_db(")
                || trimmed.contains("legacy_open_db(db_path)")
            {
                continue;
            }
            count += 1;
        }
    }

    assert!(
        count <= OPEN_DB_CALLSITE_RATCHET,
        "open_db call sites increased: found {count}, ratchet is {OPEN_DB_CALLSITE_RATCHET}"
    );
}

#[test]
fn connection_open_is_confined_to_the_db_component() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let db_component = src_root.join("components/db.rs");
    let mut offenders = Vec::new();

    for path in src_rs_files(&src_root) {
        if path == db_component {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        for (index, line) in source.lines().enumerate() {
            if line.contains("Connection::open(") {
                offenders.push(format!("{}:{}", path.display(), index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "Connection::open sites outside components/db.rs: {}",
        offenders.join(", ")
    );
}
