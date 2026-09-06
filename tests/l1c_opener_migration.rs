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

/// Path to the built `kb` binary, provided by Cargo for integration tests.
/// Driving these checks through the real compiled CLI -- argv parsing,
/// `EntryPoint::run`'s dispatch gate, `main()` -- rather than calling
/// `execute_with` in-process is what actually exercises dispatch, which is
/// where the C2/L1c and pre-existing stale-check/compress/reembed
/// classification bugs lived; `execute_with` alone would have passed before
/// any of those fixes too. A subprocess also sidesteps the process-global
/// state (cwd, stdio fds) an in-process dispatch call would otherwise force
/// onto this test file, at the cost of one process spawn per case (well
/// within `TIMEOUT`).
const KB_BIN: &str = env!("CARGO_BIN_EXE_kb");

/// Run `kb <args>` in `root` and require it to finish within [`TIMEOUT`]
/// with a clean exit, so a subcommand that took the write lock at dispatch
/// (or errored while it was held) fails this assertion instead of hanging
/// or panicking the test suite. Returns captured stdout on success.
fn require_unblocked_dispatch(label: &str, root: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let (tx, rx) = mpsc::channel();
    let root = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    thread::spawn(move || {
        let result = std::process::Command::new(KB_BIN)
            .args(&args)
            .current_dir(&root)
            .env("KB_NO_EMBED", "1")
            .output();
        tx.send(result).unwrap();
    });
    let output = rx
        .recv_timeout(TIMEOUT)
        .unwrap_or_else(|_| panic!("{label} blocked on paths.lock"))
        .unwrap_or_else(|e| panic!("{label} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "{label} failed while paths.lock was held: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// The portion of `source` before its `#[cfg(test)] mod tests { ... }`
/// unit-test module, so a grep against it cannot be satisfied by a mention
/// inside the file's own tests rather than its production code.
///
/// Rule: cut at the first `#[cfg(test)]\nmod tests` marker; if none is
/// found, cut at the last `#[cfg(test)]` occurrence instead; if there is no
/// `#[cfg(test)]` at all, return the whole file unchanged. The first branch
/// exists because a production item can carry its own leading
/// `#[cfg(test)]` attribute (a test-only `use`, a `#[cfg(test)] fn`, ...)
/// ahead of the file's actual unit-test module, and cutting at that first
/// occurrence would discard the rest of the real production code along
/// with it -- `src/commands/tests.rs` has no `#[cfg(test)]` at all (third
/// branch) and `src/commands/stale_check.rs` has a second, later
/// `#[cfg(test)] mod heal_writer_tests` after its `mod tests` (still caught
/// by the first branch, since it matches on the first occurrence of that
/// exact marker).
///
/// The returned slice is a textual cut, not a purified production view: a
/// standalone `#[cfg(test)]`-gated item declared *before* the `mod tests`
/// marker (mcp.rs's `use crate::crash_sim::KillPoint` and its `fn tr` test
/// fixture helper, for instance) is still inside it. The `open_ro`/`open_db`
/// pins below are therefore a textual grep over "everything before the unit
/// tests", not "only genuine production code" -- a future test-only item
/// declared in that region that happened to contain the literal text
/// `open_db(` would trip the negative pin even though it is not a real
/// production call site.
fn production_source(source: &str) -> &str {
    if let Some(idx) = source.find("\n#[cfg(test)]\nmod tests") {
        return &source[..idx];
    }
    match source.rfind("\n#[cfg(test)]") {
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
            "peers list/show/edge-list",
            include_str!("../src/commands/peers.rs"),
        ),
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

    // mcp.rs carries its own leading `#[cfg(test)]` item (a test-only
    // `use`) ahead of `handle_audit_report`, so a truncation regression in
    // `production_source` that stopped at the *first* `#[cfg(test)]`
    // instead of the unit-test module boundary would silently discard
    // `handle_audit_report` itself -- the `open_ro(` assertion above would
    // then fail with the misleading "lost its read-only opener" message
    // even though the opener call is untouched. Assert the function
    // actually survived into the production slice so a future regression
    // of this kind fails with the real reason instead.
    let mcp_production = production_source(include_str!("../src/commands/mcp.rs"));
    assert!(
        mcp_production.contains("fn handle_audit_report"),
        "mcp.rs production slice lost fn handle_audit_report -- production_source truncated too early"
    );

    // peers.rs is a mixed file holding three readers (list, show, edge list)
    // alongside six writers, so a bare "contains open_ro(" pin above would
    // still pass if a reader regressed to open_rw as long as one other
    // reader kept its open_ro call. Require all three occurrences by name.
    let peers_production = production_source(include_str!("../src/commands/peers.rs"));
    assert_eq!(
        peers_production.matches("open_ro(").count(),
        3,
        "peers.rs must keep exactly three open_ro readers (list, show, edge list)"
    );
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

    // Seed one live peer row so each peer reader must return actual data.
    {
        let source_repo = paths.root.to_string_lossy().into_owned();
        let seed_lock = acquire_lock(&paths.lock).unwrap();
        let conn = db::open_rw(&paths, &seed_lock).unwrap();
        conn.execute(
            "INSERT INTO graphs (id, graph_type, epic_slug, source_repo, created_at) VALUES ('g1', 'dep', NULL, ?1, '2026-09-05T00:00:00Z')",
            [&source_repo],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO peers (id, graph_id, source_repo, target_repo, edge_type, created_at) VALUES ('p1', 'g1', ?1, 'target-repo', 'member', '2026-09-05T00:00:00Z')",
            [&source_repo],
        )
        .unwrap();
    }

    let lock = acquire_lock(&paths.lock).unwrap();

    // Driven through the real CLI dispatch path (`EntryPoint::run`), not
    // `execute_with` -- dispatch's own `if self.cmd.mutates() { open_or_init
    // }` gate is where the bug this branch fixes lived, so a reader whose
    // classification regressed to `true` would deadlock here against the
    // held `paths.lock` even though its `execute_with` body never changed.
    let root = paths.root.clone();
    for (label, args) in [
        ("kb peers list", vec!["peers", "list"]),
        (
            "kb peers show",
            vec!["peers", "show", root.to_str().unwrap()],
        ),
        ("kb peers edge list", vec!["peers", "edge", "list"]),
    ] {
        let stdout = require_unblocked_dispatch(label, &root, &args);
        let rows: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert!(
            rows.as_array().is_some_and(|rows| rows.len() == 1),
            "{label} must return the seeded row, got: {}",
            String::from_utf8_lossy(&stdout)
        );
    }

    // `stale-check` (default flags), `compress --dry-run`, `reembed
    // --dry-run`, `ingest --dry-run`, and `import --dry-run` are the other
    // five subcommands whose dispatch classification this branch corrects
    // (pre-existing bugs, same shape as the peers one above): each must
    // skip `open_or_init` at dispatch and therefore must not block on the
    // held write lock either.
    require_unblocked_dispatch(
        "kb stale-check (default)",
        &root,
        &["stale-check", "src/auth.rs"],
    );
    require_unblocked_dispatch(
        "kb compress --dry-run",
        &root,
        &["compress", "src/auth.rs", "--dry-run"],
    );
    require_unblocked_dispatch("kb reembed --dry-run", &root, &["reembed", "--dry-run"]);

    let doc_file = root.join("l1c-ingest-doc.md");
    std::fs::write(&doc_file, "some document text").unwrap();
    require_unblocked_dispatch(
        "kb ingest --dry-run",
        &root,
        &[
            "ingest",
            "--path",
            "docs/l1c",
            "--summary",
            "s",
            "--tags",
            "t",
            "--file",
            doc_file.to_str().unwrap(),
            "--dry-run",
        ],
    );

    let seeds_file = root.join("l1c-import-seeds.json");
    std::fs::write(
        &seeds_file,
        r#"[{"path":"p","summary":"s","content":"c","tags":["t"]}]"#,
    )
    .unwrap();
    require_unblocked_dispatch(
        "kb import --dry-run",
        &root,
        &["import", seeds_file.to_str().unwrap(), "--dry-run"],
    );

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

#[cfg(test)]
mod tests {
    use super::production_source;

    /// Regression test for the bug this file's own truncation idiom once
    /// had: a source file that carries a leading `#[cfg(test)]` item (a
    /// test-only `use`, matching mcp.rs's `use crate::crash_sim::KillPoint;`)
    /// ahead of real production code, followed by the file's actual
    /// `#[cfg(test)] mod tests { ... }` unit-test module. The old
    /// implementation truncated at the *first* `#[cfg(test)]` occurrence,
    /// which discarded all of the production code after the early item --
    /// including, in mcp.rs's case, `handle_audit_report` itself.
    #[test]
    fn production_source_keeps_code_after_an_early_cfg_test_item() {
        let source = concat!(
            "use std::fs;\n",
            "\n",
            "#[cfg(test)]\n",
            "use some::test_only::Helper;\n",
            "\n",
            "fn production_fn() -> bool {\n",
            "    true\n",
            "}\n",
            "\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    use super::*;\n",
            "\n",
            "    #[test]\n",
            "    fn it_works() {\n",
            "        assert!(production_fn());\n",
            "    }\n",
            "}\n",
        );

        let production = production_source(source);

        assert!(
            production.contains("fn production_fn"),
            "production code after an early #[cfg(test)] item must survive: {production:?}"
        );
        assert!(
            !production.contains("mod tests"),
            "the trailing unit-test module must still be stripped: {production:?}"
        );
        assert!(
            !production.contains("it_works"),
            "unit-test-only code must not leak into the production slice: {production:?}"
        );
    }

    /// When there is no trailing `#[cfg(test)] mod tests` marker, falling
    /// back to the *last* `#[cfg(test)]` occurrence still strips the
    /// rightmost test-shaped block rather than the first one.
    #[test]
    fn production_source_falls_back_to_the_last_cfg_test_when_no_mod_tests_marker() {
        let source = concat!(
            "#[cfg(test)]\n",
            "use some::test_only::Helper;\n",
            "\n",
            "fn production_fn() -> bool {\n",
            "    true\n",
            "}\n",
            "\n",
            "#[cfg(test)]\n",
            "fn test_only_helper() -> bool {\n",
            "    false\n",
            "}\n",
        );

        let production = production_source(source);

        assert!(
            production.contains("fn production_fn"),
            "production code before the trailing #[cfg(test)] item must survive: {production:?}"
        );
        assert!(
            !production.contains("test_only_helper"),
            "the trailing #[cfg(test)] item must still be stripped: {production:?}"
        );
    }
}
