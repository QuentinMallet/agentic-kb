use std::fs;
use std::path::Path;

/// L1b/L2/L3/L1c: lower this as each migrated call site stops using `open_db`.
///
/// Bumped 82 -> 87 when L1a rebased onto bd-21ef.2 (B2/P1/A1): those commits
/// added 5 unmigrated `db::open_db(&paths.db)` call sites in
/// `src/commands/mcp.rs` test helpers, ahead of L1a's own migration work.
/// Not new legacy debt introduced by L1a itself.
///
/// Lowered 87 -> 72 by L1b: peers.rs and mcp.rs's kb_peers_* handlers moved
/// their remaining production `open_db` call sites to `open_ro`/`open_rw`
/// (peer TTL read-time filter + locked sweep).
///
/// Bumped 72 -> 74 by T5a's D4 swap-sequence crash tests (see that commit):
/// 3 new `db::open_db(&paths.db)` test-fixture call sites in
/// `src/commands/rebuild.rs`, pushing the real count to 73.
///
/// Stays at 74 through C1's D1/T3/D3 work: the real count moves between 70
/// and 74 across that range (D1's crash-recovery test in
/// `src/components/kb_core.rs` adds one; C1's D3 write helper
/// (`cursor::append_and_apply`) migrates several production and test call
/// sites off `open_db` as it lands) without ever exceeding 74, so this
/// ratchet needs no touch of its own here. See the commit that lowers it to
/// 69 once that migration is complete.
const OPEN_DB_CALLSITE_RATCHET: usize = 74;

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
