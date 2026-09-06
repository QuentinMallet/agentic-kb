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

/// Strip a `//` line comment from `line`, so a mention of the raw
/// constructor in prose (a doc comment explaining what a call site used to
/// do, say) does not itself trip a gate. A `//` immediately preceded by
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

/// For each line of `source`, whether it falls inside (at any nesting depth)
/// a `#[cfg(test)] mod <name> { ... }` block — the crate's unit-test modules
/// are not all named `tests` (`components/cursor.rs` also has a
/// `crash_tests`, `commands/stale_check.rs` a `heal_writer_tests`), so this
/// matches the `#[cfg(test)]` attribute on the immediately preceding line
/// rather than a fixed module name. Brace-counting on the comment-stripped
/// line, not a parser: the same line-based limitation `strip_line_comment`
/// above already accepts.
fn lines_inside_cfg_test_module(source: &str) -> Vec<bool> {
    let mut inside = Vec::with_capacity(source.lines().count());
    let mut depth: i32 = 0;
    let mut test_mod_start_depth: Option<i32> = None;
    let mut prev_line_was_cfg_test = false;

    for line in source.lines() {
        let code = strip_line_comment(line);
        let trimmed = code.trim();
        let after_vis = trimmed
            .strip_prefix("pub(crate) ")
            .or_else(|| trimmed.strip_prefix("pub "))
            .unwrap_or(trimmed);
        let is_mod_open = after_vis.starts_with("mod ") && after_vis.ends_with('{');

        if test_mod_start_depth.is_none() && is_mod_open && prev_line_was_cfg_test {
            test_mod_start_depth = Some(depth);
        }

        inside.push(test_mod_start_depth.is_some());

        let opens = code.matches('{').count() as i32;
        let closes = code.matches('}').count() as i32;
        depth += opens - closes;

        if let Some(start) = test_mod_start_depth {
            if depth <= start {
                test_mod_start_depth = None;
            }
        }

        prev_line_was_cfg_test = trimmed == "#[cfg(test)]";
    }

    inside
}

#[test]
fn open_db_callsites_do_not_increase() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut count = 0usize;

    for path in src_rs_files(&src_root) {
        for line in fs::read_to_string(&path).unwrap().lines() {
            if !strip_line_comment(line).contains("open_db(") {
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

/// `db::open_unchecked_for_test` is a bare `Connection::open` with none of
/// ADR-1's policy (no lock, no `query_only`, no `DbUninitialized` mapping) —
/// it is `#[doc(hidden)] pub` only so integration-test crates can reach it,
/// never for production use. A production call site would bypass every
/// opener contract in the crate while leaving both this file's other gates
/// green, so it gets its own confinement: forbidden everywhere in `src/`
/// except inside a `mod tests { ... }` block or `components/db.rs` itself
/// (the opener's own definition).
#[test]
fn open_unchecked_for_test_is_confined_to_test_modules() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let db_component = src_root.join("components/db.rs");
    let mut offenders = Vec::new();

    for path in src_rs_files(&src_root) {
        if path == db_component {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        let test_flags = lines_inside_cfg_test_module(&source);
        for (index, line) in source.lines().enumerate() {
            if test_flags[index] {
                continue;
            }
            let code = strip_line_comment(line);
            if code.contains("open_unchecked_for_test(") {
                offenders.push(format!("{}:{}", path.display(), index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "open_unchecked_for_test called outside a test module: {}",
        offenders.join(", ")
    );
}

/// `commands::mcp::tests_api::dispatch_for_test` / `dispatch_value_for_test`
/// serialize a request and call the real `handle_request` dispatcher, but
/// hardcode the retrieval tuning a production port session reads from live
/// config (`inline_verify_k`, `recency_lambda`, `mmr_lambda` —
/// `src/commands/mcp.rs:72`) instead. They are `#[doc(hidden)] pub`, like
/// `open_unchecked_for_test` above, only so integration-test crates can
/// reach them: a production call site would compile silently and run every
/// search with `recency_lambda = 0.0` and `mmr_lambda = 0.0`. Forbidden
/// everywhere in `src/` except inside a `mod tests { ... }` block or
/// `commands/mcp.rs` itself (the seam's own definition).
#[test]
fn dispatch_for_test_is_confined_to_test_modules() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mcp_component = src_root.join("commands/mcp.rs");
    let mut offenders = Vec::new();
    const SEAM_CALLS: &[&str] = &["dispatch_for_test(", "dispatch_value_for_test("];

    for path in src_rs_files(&src_root) {
        if path == mcp_component {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        let test_flags = lines_inside_cfg_test_module(&source);
        for (index, line) in source.lines().enumerate() {
            if test_flags[index] {
                continue;
            }
            let code = strip_line_comment(line);
            if SEAM_CALLS.iter().any(|call| code.contains(call)) {
                offenders.push(format!("{}:{}", path.display(), index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "dispatch_for_test/dispatch_value_for_test called outside a test module: {}",
        offenders.join(", ")
    );
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
