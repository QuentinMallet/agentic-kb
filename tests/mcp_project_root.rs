//! ADR-6 — per-request project root on the MCP port.
//!
//! The port is started for one store, but a long-lived server serves sessions
//! working in other checkouts. A request may name its project with
//! `repo_root`; the server maps it to that project's own store and resolves
//! every citation against that root. The operator allowlist, not the client,
//! bounds which directories the server will ever open.

use kb::commands::mcp::tests_api::dispatch_value_for_test_with_roots;
use kb::components::{db, embedder::NoopEmbedder};
use kb::config::{ProjectRootError, ProjectRoots};
use proptest::prelude::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// A checkout: a `.state/` marker (first-run canonical layout) and a file to cite.
fn checkout(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), b"fn lib() {}\n").unwrap();
    root
}

fn add_request(id: &str, repo_root: Option<&Path>, citation_path: &str) -> Value {
    let mut req = json!({
        "id": id,
        "method": "add",
        "path": "cross/project",
        "summary": "cross-project finding",
        "content": "recorded from a session rooted elsewhere",
        "kind": "observation",
        "evidence": [{"kind": "code", "citation_path": citation_path}]
    });
    if let Some(root) = repo_root {
        req["repo_root"] = json!(root.to_string_lossy());
    }
    req
}

fn dispatch(server: &kb::config::Paths, roots: &ProjectRoots, req: Value) -> Value {
    dispatch_value_for_test_with_roots(server, roots, &NoopEmbedder, &req)
}

#[test]
fn add_without_repo_root_still_resolves_against_the_servers_own_store() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, &[]);

    let resp = dispatch(&server, &roots, add_request("a", None, "src/lib.rs"));
    assert_eq!(resp["type"], "ok", "{resp}");
}

#[test]
fn add_with_repo_root_writes_to_that_projects_store_and_verifies_against_it() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let other_root = checkout(tmp.path(), "other");
    // A file that exists only in `other`: resolving it against `server` would fail.
    fs::write(other_root.join("src/only_here.rs"), b"fn only_here() {}\n").unwrap();
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, &[tmp.path().to_path_buf()]);

    let resp = dispatch(
        &server,
        &roots,
        add_request("b", Some(&other_root), "src/only_here.rs"),
    );
    assert_eq!(resp["type"], "ok", "{resp}");

    // The entry landed in `other`'s store, not the server's.
    let other = kb::config::Paths::from_root(&other_root);
    assert!(other.db.exists(), "other project's db must be initialized");
    let conn = db::open_ro(&other.db).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let server_conn = db::open_ro(&server.db).unwrap();
    let n: i64 = server_conn
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "the server's own store must be untouched");

    // The citation verifies in that project (kb_cite agrees on the hash).
    let cite = dispatch(
        &server,
        &roots,
        json!({"id":"c","method":"cite","repo_root": other_root.to_string_lossy(),
               "path":"src/only_here.rs"}),
    );
    assert_eq!(cite["type"], "result", "{cite}");
    let stored: String = conn
        .query_row("SELECT citation_hash FROM evidence", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cite["citation_hash"], stored);
}

#[test]
fn citation_inside_the_other_project_is_rejected_without_repo_root() {
    // The bug: the server root is the only root, so a path that is valid in
    // the caller's checkout is resolved against the wrong repository.
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let other_root = checkout(tmp.path(), "other");
    fs::write(other_root.join("src/only_here.rs"), b"x").unwrap();
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, &[tmp.path().to_path_buf()]);

    let resp = dispatch(&server, &roots, add_request("d", None, "src/only_here.rs"));
    assert_eq!(resp["type"], "error", "{resp}");
}

#[test]
fn repo_root_outside_the_allowlist_is_refused_before_any_handler_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let allowed = tmp.path().join("allowed");
    let _ = checkout(&allowed, "proj");
    let stranger = checkout(tmp.path(), "stranger");
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));

    let resp = dispatch(
        &server,
        &roots,
        add_request("e", Some(&stranger), "src/lib.rs"),
    );
    assert_eq!(resp["type"], "error", "{resp}");
    assert_eq!(resp["code"], "project_root_rejected", "{resp}");
    assert_eq!(resp["id"], "e");
    let stranger_paths = kb::config::Paths::from_root(&stranger);
    assert!(
        !stranger_paths.db.exists(),
        "a refused project must not be initialized"
    );
    // Even a read refuses: no file under a non-allowlisted root is opened.
    let cite = dispatch(
        &server,
        &roots,
        json!({"id":"f","method":"cite","repo_root": stranger.to_string_lossy(),"path":"src/lib.rs"}),
    );
    assert_eq!(cite["code"], "project_root_rejected", "{cite}");
}

#[test]
fn repo_root_must_be_absolute_and_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, &[tmp.path().to_path_buf()]);

    for bad in ["relative/dir", "", "src"] {
        let resp = dispatch(
            &server,
            &roots,
            add_request("g", Some(Path::new(bad)), "src/lib.rs"),
        );
        assert_eq!(resp["code"], "project_root_rejected", "{bad:?}: {resp}");
    }
    let missing = tmp.path().join("does-not-exist");
    let resp = dispatch(
        &server,
        &roots,
        add_request("h", Some(&missing), "src/lib.rs"),
    );
    assert_eq!(resp["code"], "project_root_rejected", "{resp}");
}

#[test]
fn repo_root_with_dotdot_or_symlink_cannot_escape_the_allowlist() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let allowed = tmp.path().join("allowed");
    let inside = checkout(&allowed, "proj");
    let outside = checkout(tmp.path(), "outside");
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));

    // `..` out of the allowlisted tree.
    let dotdot = allowed.join("proj/../../outside");
    let resp = dispatch(
        &server,
        &roots,
        add_request("i", Some(&dotdot), "src/lib.rs"),
    );
    assert_eq!(resp["code"], "project_root_rejected", "{resp}");

    // A symlink inside the allowlisted tree pointing outside of it.
    let link = allowed.join("escape");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let resp = dispatch(&server, &roots, add_request("j", Some(&link), "src/lib.rs"));
    assert_eq!(resp["code"], "project_root_rejected", "{resp}");
    assert!(!kb::config::Paths::from_root(&outside).db.exists());

    // Control: the genuinely-inside checkout is accepted.
    let resp = dispatch(
        &server,
        &roots,
        add_request("k", Some(&inside), "src/lib.rs"),
    );
    assert_eq!(resp["type"], "ok", "{resp}");
}

#[test]
fn citation_escape_check_still_applies_inside_the_selected_project() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let other = checkout(tmp.path(), "other");
    fs::write(tmp.path().join("secret.txt"), b"outside both").unwrap();
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, &[tmp.path().to_path_buf()]);

    for escaping in ["../secret.txt", "/etc/hostname", "../server/src/lib.rs"] {
        let resp = dispatch(&server, &roots, add_request("l", Some(&other), escaping));
        assert_eq!(
            resp["type"], "error",
            "{escaping:?} must not resolve: {resp}"
        );
    }
    let cite = dispatch(
        &server,
        &roots,
        json!({"id":"m","method":"cite","repo_root": other.to_string_lossy(),"path":"../secret.txt"}),
    );
    assert_eq!(cite["type"], "error", "{cite}");
}

#[test]
fn a_subdirectory_or_state_worktree_of_an_allowlisted_project_selects_that_project() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let other = checkout(tmp.path(), "other");
    let (server, _conn) = db::test_db(&server_root);
    let roots = ProjectRoots::new(&server_root, &[tmp.path().to_path_buf()]);

    // Subdirectory: discovery walks up to `other`.
    let resp = dispatch(
        &server,
        &roots,
        add_request("n", Some(&other.join("src")), "src/lib.rs"),
    );
    assert_eq!(resp["type"], "ok", "{resp}");
    assert!(kb::config::Paths::from_root(&other).db.exists());
    assert!(!other.join("src/.state").exists());
}

#[test]
fn project_roots_resolution_is_exact_about_prefixes() {
    // `/tmp/x/allowed` must not admit `/tmp/x/allowed-evil`.
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let allowed = tmp.path().join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    let evil = checkout(tmp.path(), "allowed-evil");
    let roots = ProjectRoots::new(&server_root, &[allowed]);
    assert!(matches!(
        roots.resolve(evil.to_str().unwrap()),
        Err(ProjectRootError::NotAllowlisted(_))
    ));
}

#[test]
fn project_roots_skips_missing_allowlist_entries_and_always_keeps_the_server_root() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let roots = ProjectRoots::new(&server_root, &[tmp.path().join("nope")]);
    assert_eq!(roots.allowed().len(), 1);
    assert_eq!(roots.allowed()[0], fs::canonicalize(&server_root).unwrap());
    let resolved = roots.resolve(server_root.to_str().unwrap()).unwrap();
    assert_eq!(resolved.root, fs::canonicalize(&server_root).unwrap());
}

proptest! {
    /// Any relative component sequence appended to an allowlisted directory,
    /// including `..` runs, either resolves to a directory canonically inside
    /// the allowlist or is refused — never to a root outside it.
    #[test]
    fn arbitrary_component_sequences_never_select_a_root_outside_the_allowlist(
        components in proptest::collection::vec(
            prop_oneof![Just("..".to_string()), Just(".".to_string()), "[a-z]{1,4}"],
            0..8,
        )
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = checkout(tmp.path(), "server");
        let allowed = tmp.path().join("allowed");
        let _proj = checkout(&allowed, "proj");
        let _outside = checkout(tmp.path(), "outside");
        let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));

        let mut candidate = allowed.join("proj");
        for c in &components {
            candidate.push(c);
        }
        if let Ok(selected) = roots.resolve(candidate.to_str().unwrap()) {
            let inside = roots
                .allowed()
                .iter()
                .any(|a| selected.root.starts_with(a));
            prop_assert!(inside, "{} escaped the allowlist", selected.root.display());
        }
    }
}
