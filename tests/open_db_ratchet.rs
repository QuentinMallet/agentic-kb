use std::fs;
use std::path::Path;

/// L1c removed the legacy opener; keep this gate at zero permanently.
const OPEN_DB_CALLSITE_RATCHET: usize = 0;

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
            count += 1;
        }
    }

    assert!(
        count <= OPEN_DB_CALLSITE_RATCHET,
        "open_db call sites increased: found {count}, ratchet is {OPEN_DB_CALLSITE_RATCHET}"
    );
}

/// Strip a `//` line comment from `line`, so a mention of the raw
/// constructor in prose (a doc comment explaining what a call site used to
/// do, say) does not itself trip the gate. A `//` immediately preceded by
/// `:` is treated as part of a `scheme://` URL rather than a comment
/// introducer, so doc comments that link to something don't get truncated.
///
/// Known limitation: this is a line-based heuristic, not a lexer, so a `//`
/// inside a string literal (e.g. a raw-string test fixture) is still
/// treated as a comment introducer and truncates the rest of the line. That
/// can only hide an offender inside a string, never manufacture a false
/// one, and no such case exists in this codebase today.
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
            return &line[..i];
        }
    }
    line
}

#[test]
fn connection_open_is_confined_to_the_db_component() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let db_component = src_root.join("components/db.rs");
    let mut offenders = Vec::new();
    const RAW_CONSTRUCTORS: &[&str] = &[
        "Connection::open(",
        "Connection::open_with_flags(",
        "Connection::open_in_memory(",
    ];

    for path in src_rs_files(&src_root) {
        if path == db_component {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        for (index, line) in source.lines().enumerate() {
            let code = strip_line_comment(line);
            if RAW_CONSTRUCTORS.iter().any(|ctor| code.contains(ctor)) {
                offenders.push(format!("{}:{}", path.display(), index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "raw rusqlite Connection constructor sites outside components/db.rs: {}",
        offenders.join(", ")
    );
}
