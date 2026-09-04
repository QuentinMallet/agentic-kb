//! C2/L1a — first-run UX after the schema-creation policy change (ADR-1).
//!
//! `open_ro` no longer creates the database, so every pure read surface now
//! meets a `DbUninitialized` error on a repository that has never been written
//! to. Each surface must map it to an EMPTY result plus a one-line stderr note,
//! never to an error exit — so `kb search` on a fresh clone behaves exactly as
//! it did when the read path silently created the DB.
//!
//! The two MCP surfaces (`handle_kb_get`, `handle_provenance`) are private
//! functions and are covered by unit tests in `src/commands/mcp.rs`.

use kb::commands::cited_by::CitedBy;
use kb::commands::context::Context;
use kb::commands::search::Search;
use kb::components::embedder::NoopEmbedder;
use kb::config::Paths;

/// A repository whose `.state/agent-kb/` exists but holds no database.
fn fresh_repo(root: &std::path::Path) -> Paths {
    std::fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(root);
    assert!(!paths.db.exists());
    paths
}

#[test]
fn kb_search_on_a_fresh_repo_is_empty_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    let search = Search {
        query: "anything".to_string(),
        limit: 10,
        fts: true,
        semantic: false,
        path_prefix: None,
        tag: None,
        content: false,
        repo: Some(dir.path().to_path_buf()),
        peers: false,
        local_only: true,
        reachable_from: None,
        max_hops: 1,
        slug: None,
    };

    search
        .execute_with(&paths, &NoopEmbedder)
        .expect("a fresh repo must produce an empty result, not an error");
    assert!(
        !paths.db.exists(),
        "the read path must not create the database"
    );
}

#[test]
fn kb_cited_by_on_a_fresh_repo_is_empty_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    let mut out: Vec<u8> = Vec::new();
    CitedBy {
        file: "src/lib.rs".to_string(),
        json: false,
    }
    .execute_with(&paths, None, &mut out)
    .expect("a fresh repo must produce an empty result, not an error");

    assert!(
        out.is_empty(),
        "cited-by must print nothing on a fresh repo, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(!paths.db.exists());
}

#[test]
fn kb_cited_by_json_on_a_fresh_repo_is_an_empty_array() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    let mut out: Vec<u8> = Vec::new();
    CitedBy {
        file: "src/lib.rs".to_string(),
        json: true,
    }
    .execute_with(&paths, None, &mut out)
    .unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(parsed, serde_json::json!([]));
}

#[test]
fn kb_context_on_a_fresh_repo_is_empty_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    let mut out: Vec<u8> = Vec::new();
    Context {
        budget: 1000,
        floor: None,
        json: false,
    }
    .execute_with(&paths, &mut out)
    .expect("a fresh repo must produce an empty result, not an error");

    assert!(
        out.is_empty(),
        "context must print nothing on a fresh repo, got: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(!paths.db.exists());
}
