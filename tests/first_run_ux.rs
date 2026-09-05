//! C2/L1a — first-run UX after the schema-creation policy change (ADR-1).
//!
//! `open_ro` no longer creates the database, so every pure read surface now
//! meets a `DbUninitialized` error on a repository that has never been written
//! to. Each surface must map it to an EMPTY result plus a one-line stderr note,
//! never to an error exit — so `kb search` on a fresh clone behaves exactly as
//! it did when the read path silently created the DB.
//!
//! The MCP surfaces (`handle_kb_get`, `handle_provenance`, `handle_search`,
//! `handle_audit_report`) are private functions and are covered by unit
//! tests in `src/commands/mcp.rs`.

use kb::commands::cited_by::CitedBy;
use kb::commands::compact::Compact;
use kb::commands::context::Context;
use kb::commands::eval::Eval;
use kb::commands::search::Search;
use kb::commands::stale_check::{RelocateArg, StaleCheck};
use kb::commands::tests::Tests;
use kb::components::embedder::NoopEmbedder;
use kb::components::events;
use kb::config::Paths;
use serde_json::{json, Value};

/// A repository whose `.state/agent-kb/` exists but holds no database.
fn fresh_repo(root: &std::path::Path) -> Paths {
    std::fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(root);
    assert!(!paths.db.exists());
    paths
}

fn upsert(id: &str, summary: &str) -> Value {
    json!({
        "action": "upsert", "table": "entries",
        "id": id, "path": format!("p/{id}"), "summary": summary,
        "content": format!("content of {id}"), "tags": ["t"],
        "kind": "belief", "evidence_status": "n/a",
        "ts": "2026-09-05T00:00:00Z",
    })
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

/// C1's convergence gate (compact.rs) must not fall through to `open_rw` when
/// no database exists — `open_rw` creates the schema on an absent DB, and the
/// locked peer sweep it exists for is a side effect compact must not have on
/// a fresh clone's log. Compacting before the first local build must stay a
/// pure log rewrite: the log is rewritten as it always is, but no database is
/// spontaneously materialized.
#[test]
fn kb_compact_on_a_fresh_repo_stays_a_pure_log_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    // A log with one superseded event, as if cloned before ever being built
    // locally (no database, nothing materialized from it yet).
    events::append_event(&paths.events, &upsert("e1", "first")).unwrap();
    events::append_event(&paths.events, &upsert("e1", "second")).unwrap();

    let (before, after) = Compact
        .execute_with_paths(&paths)
        .expect("compact must succeed on a fresh clone's log with no local database");

    assert_eq!(before, 2, "both events were present before compaction");
    assert_eq!(after, 1, "compact must still squash the superseded event");
    assert!(
        !paths.db.exists(),
        "compact must not materialize a database as a side effect of the \
         locked peer sweep when nothing has been built locally yet"
    );
}

#[test]
fn kb_tests_on_a_fresh_repo_is_empty_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    Tests { app: None }
        .execute_with(&paths)
        .expect("a fresh repo must produce an empty result, not an error");

    assert!(
        !paths.db.exists(),
        "the read path must not create the database"
    );
}

#[test]
fn kb_eval_on_a_fresh_repo_is_empty_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    let golden = dir.path().join("golden.jsonl");
    std::fs::write(
        &golden,
        "{\"query\": \"anything\", \"expected_ids\": [\"missing\"], \"split\": \"dev\"}\n",
    )
    .unwrap();

    let eval = Eval {
        golden: Some(golden),
        sealed: false,
        compare: None,
        fts: false,
        semantic: false,
        k: 10,
        json: false,
        min_recall: None,
        min_mrr: None,
    };

    eval.execute_with(&paths, &NoopEmbedder)
        .expect("a fresh repo must produce an empty result, not an error");

    assert!(
        !paths.db.exists(),
        "the read path must not create the database"
    );
}

#[test]
fn kb_stale_check_on_a_fresh_repo_is_empty_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let paths = fresh_repo(dir.path());

    let report = StaleCheck {
        files: vec!["src/lib.rs".to_string()],
        commits: vec![],
        blame: false,
        relocate: RelocateArg::Never,
    }
    .execute_with(&paths)
    .expect("a fresh repo must produce an empty result, not an error");

    assert!(report.stale.is_empty());
    assert!(report.review.is_empty());
    assert!(report.unreachable.is_empty());
    assert!(
        !paths.db.exists(),
        "the read path must not create the database"
    );
}
