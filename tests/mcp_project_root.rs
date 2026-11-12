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

// ---------------------------------------------------------------------------
// Round 2 — store containment (P1) and pre-authorization discovery (P2)
// ---------------------------------------------------------------------------

/// Every path under `dir` with its kind, length, mtime and link target, so a
/// test can assert that the directory is byte-for-byte untouched.
fn snapshot(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let md = fs::symlink_metadata(&p).unwrap();
            let mtime = md.modified().unwrap();
            out.push(format!(
                "{} dir={} len={} mtime={:?} link={:?}",
                p.display(),
                md.is_dir(),
                md.len(),
                mtime,
                fs::read_link(&p).ok()
            ));
            if md.is_dir() {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StoreSlot {
    StateDir,
    AgentKbDir,
    Db,
    Lock,
    Events,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LinkTarget {
    Dir,
    ExistingFile,
    Dangling,
}

/// Builds `<tmp>/allowed` (a real checkout) and `<tmp>/outside`, replaces
/// `slot` inside `allowed` with a symlink into `outside`, and returns both.
fn symlinked_store(tmp: &Path, slot: StoreSlot, target: LinkTarget) -> (PathBuf, PathBuf) {
    let allowed = tmp.join("allowed");
    let outside = tmp.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let is_dir_slot = matches!(slot, StoreSlot::StateDir | StoreSlot::AgentKbDir);
    let link_to = match (is_dir_slot, target) {
        (true, _) => {
            fs::create_dir_all(outside.join("victim")).unwrap();
            outside.join("victim")
        }
        (false, LinkTarget::ExistingFile) => {
            fs::write(outside.join("victim"), b"").unwrap();
            outside.join("victim")
        }
        (false, _) => outside.join("victim-not-yet"),
    };
    match slot {
        StoreSlot::StateDir => {
            fs::create_dir_all(&allowed).unwrap();
            std::os::unix::fs::symlink(&link_to, allowed.join(".state")).unwrap();
        }
        StoreSlot::AgentKbDir => {
            fs::create_dir_all(allowed.join(".state")).unwrap();
            std::os::unix::fs::symlink(&link_to, allowed.join(".state/agent-kb")).unwrap();
        }
        StoreSlot::Db | StoreSlot::Events | StoreSlot::Lock => {
            fs::create_dir_all(allowed.join(".state/agent-kb")).unwrap();
            let name = match slot {
                StoreSlot::Db => ".state/agent-kb/agent-kb.db",
                StoreSlot::Events => ".state/agent-kb/agent-kb-events.jsonl",
                _ => ".state/.lock",
            };
            std::os::unix::fs::symlink(&link_to, allowed.join(name)).unwrap();
        }
    }
    (allowed, outside)
}

fn assert_store_symlink_rejected(slot: StoreSlot, target: LinkTarget) {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let (server, _conn) = db::test_db(&server_root);
    let (allowed, outside) = symlinked_store(tmp.path(), slot, target);
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));
    let before = snapshot(&outside);

    // Direct resolution refuses.
    let direct = roots.resolve(allowed.to_str().unwrap());
    assert!(
        matches!(direct, Err(ProjectRootError::StoreEscapes { .. })),
        "{slot:?}/{target:?}: expected StoreEscapes, got {direct:?}"
    );
    // A write through the port refuses too, and nothing is created outside.
    for method in ["add", "cite"] {
        let req = match method {
            "add" => json!({"id":"p1","method":"add","repo_root":allowed.to_string_lossy(),
                "path":"p/q","summary":"s","content":"c","kind":"observation"}),
            _ => json!({"id":"p1","method":"cite","repo_root":allowed.to_string_lossy(),
                "path":"src/lib.rs"}),
        };
        let resp = dispatch(&server, &roots, req);
        assert_eq!(
            resp["type"], "error",
            "{slot:?}/{target:?}/{method}: {resp}"
        );
        assert_eq!(resp["code"], "project_root_rejected", "{resp}");
    }
    assert_eq!(
        before,
        snapshot(&outside),
        "{slot:?}/{target:?}: outside dir changed"
    );
}

#[test]
fn a_store_component_symlinked_outside_the_allowlist_is_rejected_and_nothing_is_created_there() {
    for slot in [
        StoreSlot::StateDir,
        StoreSlot::AgentKbDir,
        StoreSlot::Db,
        StoreSlot::Lock,
        StoreSlot::Events,
    ] {
        for target in [
            LinkTarget::Dir,
            LinkTarget::ExistingFile,
            LinkTarget::Dangling,
        ] {
            let is_dir_slot = matches!(slot, StoreSlot::StateDir | StoreSlot::AgentKbDir);
            if is_dir_slot && target != LinkTarget::Dir {
                continue;
            }
            assert_store_symlink_rejected(slot, target);
        }
    }
}

#[test]
fn a_store_symlink_that_stays_inside_the_selected_root_is_still_accepted() {
    // Legacy checkouts alias `agent-kb` to `.state/agent-kb`; containment, not
    // a blanket symlink ban, is the rule.
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let proj = checkout(tmp.path(), "proj");
    fs::create_dir_all(proj.join("real-store")).unwrap();
    fs::remove_dir_all(proj.join(".state/agent-kb")).unwrap();
    std::os::unix::fs::symlink(proj.join("real-store"), proj.join(".state/agent-kb")).unwrap();
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&proj));
    assert!(roots.resolve(proj.to_str().unwrap()).is_ok());
}

/// Minimal inotify watcher: reports opens/reads of the watched files.
struct OpenWatch {
    fd: i32,
}

impl OpenWatch {
    fn new(files: &[&Path]) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "inotify_init1 failed");
        for f in files {
            let c = std::ffi::CString::new(f.as_os_str().as_bytes()).unwrap();
            let wd =
                unsafe { libc::inotify_add_watch(fd, c.as_ptr(), libc::IN_OPEN | libc::IN_ACCESS) };
            assert!(wd >= 0, "inotify_add_watch {} failed", f.display());
        }
        Self { fd }
    }

    /// Number of open/access events observed since the last call.
    fn events(&self) -> usize {
        let mut buf = [0u8; 4096];
        let mut seen = 0usize;
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut off = 0usize;
            while off < n as usize {
                let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                seen += 1;
                off += 16 + len;
            }
        }
        seen
    }
}

impl Drop for OpenWatch {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// `<tmp>/discovery/.state/{.git,.lock,agent-kb/agent-kb.db}` are sentinels
/// *outside* the allowlist; the allowlisted directory is `.state/allowed`.
fn discovery_fixture(tmp: &Path) -> (PathBuf, Vec<PathBuf>) {
    let outer = tmp.join("discovery");
    let state = outer.join(".state");
    let allowed = state.join("allowed");
    fs::create_dir_all(allowed.join("src")).unwrap();
    fs::create_dir_all(state.join("agent-kb")).unwrap();
    let sentinels = vec![
        state.join(".git"),
        state.join(".lock"),
        state.join("agent-kb/agent-kb.db"),
    ];
    fs::write(&sentinels[0], b"gitdir: nonexistent\n").unwrap();
    fs::write(&sentinels[1], b"").unwrap();
    fs::write(&sentinels[2], b"not a database").unwrap();
    (allowed, sentinels)
}

#[test]
fn discovery_never_opens_files_in_ancestors_outside_the_allowlist() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let (allowed, sentinels) = discovery_fixture(tmp.path());
    let paths: Vec<&Path> = sentinels.iter().map(|p| p.as_path()).collect();
    let watch = OpenWatch::new(&paths);
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));

    // Sanity: the watcher does see a read (guards against a deaf test).
    let _ = fs::read(&sentinels[0]).unwrap();
    assert!(watch.events() > 0, "inotify watcher is not observing reads");

    let result = roots.resolve(allowed.to_str().unwrap());
    assert!(result.is_err(), "no store inside the allowlist: {result:?}");
    assert_eq!(watch.events(), 0, "an outside ancestor file was opened");
    let sub = roots.resolve(allowed.join("src").to_str().unwrap());
    assert!(sub.is_err());
    assert_eq!(watch.events(), 0, "an outside ancestor file was opened");
}

#[test]
fn a_managed_state_worktree_selects_its_project_without_leaving_it() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let proj = checkout(tmp.path(), "proj");
    fs::write(
        proj.join(".state/.git"),
        b"gitdir: ../.git/worktrees/state\n",
    )
    .unwrap();
    let wt = proj.join(".state/worktrees/wt");
    fs::create_dir_all(wt.join(".state/agent-kb")).unwrap();
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&proj));
    let sel = roots.resolve(wt.to_str().unwrap()).unwrap();
    assert_eq!(sel.root, fs::canonicalize(&proj).unwrap());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Wherever under the allowlisted directory the request points, discovery
    /// never opens a sentinel in an outside ancestor.
    #[test]
    fn discovery_from_any_subdirectory_never_touches_outside_ancestors(
        components in proptest::collection::vec(
            prop_oneof![Just("..".to_string()), Just(".".to_string()), Just("src".to_string()), "[a-z]{1,3}"],
            0..6,
        )
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let server_root = checkout(tmp.path(), "server");
        let (allowed, sentinels) = discovery_fixture(tmp.path());
        let paths: Vec<&Path> = sentinels.iter().map(|p| p.as_path()).collect();
        let watch = OpenWatch::new(&paths);
        let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));
        let mut candidate = allowed.clone();
        for c in &components {
            candidate.push(c);
        }
        let _ = roots.resolve(candidate.to_str().unwrap());
        prop_assert_eq!(watch.events(), 0);
    }

    /// Any single symlinked store component pointing outside is refused and
    /// leaves the outside directory untouched.
    #[test]
    fn any_symlinked_store_component_is_refused(
        slot in prop_oneof![
            Just(StoreSlot::StateDir), Just(StoreSlot::AgentKbDir), Just(StoreSlot::Db),
            Just(StoreSlot::Lock), Just(StoreSlot::Events)],
        target in prop_oneof![
            Just(LinkTarget::Dir), Just(LinkTarget::ExistingFile),
            Just(LinkTarget::Dangling)],
    ) {
        let is_dir_slot = matches!(slot, StoreSlot::StateDir | StoreSlot::AgentKbDir);
        prop_assume!(!is_dir_slot || target == LinkTarget::Dir);
        assert_store_symlink_rejected(slot, target);
    }
}

// ---------------------------------------------------------------------------
// Round 3 — directory-level store containment and no-follow `.state/.git`
// ---------------------------------------------------------------------------

/// Sidecar names the store code really writes, plus one nobody has invented
/// yet: the rule is per directory entry, not per known name.
const SIDECAR_NAMES: &[&str] = &[
    "agent-kb-events.jsonl.tmp",
    "agent-kb-events.jsonl.generation",
    "agent-kb-events.jsonl.generation.tmp",
    "agent-kb-events.jsonl.deadletter",
    "agent-kb-events.jsonl.deadletter.tmp",
    "schema-upgrade.lock",
    "rebuild-lifetime.lock",
    "agent-kb.db.tmp.4242",
    "agent-kb.db.pre-v7.bak",
    "a-sidecar-name-from-the-future.tmp",
];

#[test]
fn any_store_directory_entry_symlinked_outside_is_refused_and_the_target_untouched() {
    for name in SIDECAR_NAMES {
        for method in ["add", "compact"] {
            let tmp = tempfile::tempdir().unwrap();
            let server_root = checkout(tmp.path(), "server");
            let (server, _conn) = db::test_db(&server_root);
            let allowed = checkout(tmp.path(), "allowed");
            let outside = tmp.path().join("outside");
            fs::create_dir_all(&outside).unwrap();
            let victim = outside.join("victim");
            fs::write(&victim, b"precious\n").unwrap();
            let link = allowed.join(".state/agent-kb").join(name);
            std::os::unix::fs::symlink(&victim, &link).unwrap();
            let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));

            let req = match method {
                "add" => add_request("r3", Some(&allowed), "src/lib.rs"),
                _ => json!({"id":"r3","method":"compact","repo_root":allowed.to_string_lossy()}),
            };
            let resp = dispatch(&server, &roots, req);
            assert_eq!(resp["type"], "error", "{name}/{method}: {resp}");
            assert_eq!(
                resp["code"], "project_root_rejected",
                "{name}/{method}: {resp}"
            );
            assert_eq!(fs::read(&victim).unwrap(), b"precious\n", "{name}/{method}");
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{name}/{method}: planted link was replaced"
            );
            assert!(
                !allowed
                    .join(".state/agent-kb/agent-kb-events.jsonl")
                    .exists(),
                "{name}/{method}: event log created despite refusal"
            );
        }
    }
}

#[test]
fn a_dangling_store_directory_entry_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let allowed = checkout(tmp.path(), "allowed");
    std::os::unix::fs::symlink(
        tmp.path().join("nowhere"),
        allowed.join(".state/agent-kb/agent-kb-events.jsonl.tmp"),
    )
    .unwrap();
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));
    let r = roots.resolve(allowed.to_str().unwrap());
    assert!(
        matches!(r, Err(ProjectRootError::StoreEscapes { .. })),
        "{r:?}"
    );
}

#[test]
fn an_in_root_store_directory_entry_symlink_is_still_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let allowed = checkout(tmp.path(), "allowed");
    std::os::unix::fs::symlink(
        allowed.join("src/lib.rs"),
        allowed.join(".state/agent-kb/an-alias"),
    )
    .unwrap();
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));
    assert!(roots.resolve(allowed.to_str().unwrap()).is_ok());
}

#[test]
fn compact_never_follows_a_stale_in_root_tmp_link() {
    // Defense in depth for the check-then-use window: even when a link
    // survives the directory scan (it points inside the root), creating the
    // tmp file replaces the link instead of writing through it.
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let (server, _conn) = db::test_db(&server_root);
    let allowed = checkout(tmp.path(), "allowed");
    let target = allowed.join("src/lib.rs");
    let before = fs::read(&target).unwrap();
    std::os::unix::fs::symlink(
        &target,
        allowed.join(".state/agent-kb/agent-kb-events.jsonl.tmp"),
    )
    .unwrap();
    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));
    let add = dispatch(
        &server,
        &roots,
        add_request("r3a", Some(&allowed), "src/lib.rs"),
    );
    assert_eq!(add["type"], "ok", "{add}");
    // `mcp` compact needs the abscissa app cell; drive the command directly.
    let selected = roots.resolve(allowed.to_str().unwrap()).unwrap();
    kb::commands::compact::Compact
        .execute_with_paths(&selected.paths)
        .unwrap();
    assert_eq!(fs::read(&target).unwrap(), before, "tmp link was followed");
    let events = allowed.join(".state/agent-kb/agent-kb-events.jsonl");
    assert!(
        !fs::symlink_metadata(&events)
            .unwrap()
            .file_type()
            .is_symlink(),
        "event log became a symlink"
    );
}

#[test]
fn a_symlinked_state_git_is_not_opened_during_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let server_root = checkout(tmp.path(), "server");
    let allowed = checkout(tmp.path(), "allowed");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let gitlink = outside.join("gitlink");
    fs::write(&gitlink, b"gitdir: nonexistent\n").unwrap();
    std::os::unix::fs::symlink(&gitlink, allowed.join(".state/.git")).unwrap();
    let wt = allowed.join(".state/worktrees/wt");
    fs::create_dir_all(&wt).unwrap();
    let watch = OpenWatch::new(&[gitlink.as_path()]);
    let _ = fs::read(&gitlink).unwrap();
    assert!(watch.events() > 0, "inotify watcher is not observing reads");

    let roots = ProjectRoots::new(&server_root, std::slice::from_ref(&allowed));
    let _ = roots.resolve(wt.to_str().unwrap());
    let _ = roots.resolve(allowed.to_str().unwrap());
    assert_eq!(watch.events(), 0, "outside gitlink target was opened");
}
