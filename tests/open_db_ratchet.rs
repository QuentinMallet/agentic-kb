use std::fs;
use std::path::Path;

/// L1b/L2/L3/L1c: lower this as each migrated call site stops using `open_db`.
const OPEN_DB_CALLSITE_RATCHET: usize = 82;

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
