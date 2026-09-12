//! Option B MCP request-contract regressions.
//!
//! Repository/filesystem access is the sole authority boundary.  These tests
//! prove the Rust port accepts anonymous operations, rejects a client-supplied
//! identity field, and continues to process historical caller-attributed audit
//! data as inert legacy data.

use kb::commands::mcp::tests_api::dispatch_value_for_test;
use kb::components::{db, embedder::NoopEmbedder, events};
use serde_json::{json, Value};

fn add_auditable(paths: &kb::config::Paths, path: &str) -> String {
    let response = dispatch_value_for_test(
        paths,
        &NoopEmbedder,
        &json!({
            "id": format!("add-{path}"),
            "method": "add",
            "path": path,
            "summary": "Option B fixture",
            "content": "Option B anonymous mutation fixture",
            "tags": ["mcp-option-b"],
            "kind": "observation",
            "evidence": [{"kind": "code", "citation_path": "fixture.txt:1-7"}]
        }),
    );
    assert_eq!(response["type"], "ok", "add failed: {response}");
    response["entry_id"].as_str().unwrap().to_owned()
}

fn dispatch(paths: &kb::config::Paths, request: Value) -> Value {
    dispatch_value_for_test(paths, &NoopEmbedder, &request)
}

#[test]
fn anonymous_expire_and_audit_operations_preserve_integrity_and_idempotency() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, _initial_conn) = db::test_db(repo.path());

    let direct_entry = add_auditable(&paths, "option-b/direct-expire");
    let expired = dispatch(
        &paths,
        json!({"id":"expire","method":"expire","entry_id":direct_entry,"reason":"obsolete"}),
    );
    assert_eq!(expired["type"], "ok", "anonymous expire must succeed: {expired}");

    let audit_entry = add_auditable(&paths, "option-b/audit");
    let run = dispatch(
        &paths,
        json!({"id":"run","method":"audit_run","sample_size":1,"mode":"uniform"}),
    );
    assert_eq!(run["type"], "ok", "anonymous audit run must succeed: {run}");
    let run_id = run["run_id"].as_str().unwrap();

    let verdict = json!({"entry_id":audit_entry,"verdict":true});
    let first = dispatch(
        &paths,
        json!({"id":"record-1","method":"audit_record","run_id":run_id,"verdicts":[verdict]}),
    );
    assert_eq!(first["type"], "ok", "anonymous audit record must succeed: {first}");
    assert_eq!(first["recorded"], 1);

    let replay = dispatch(
        &paths,
        json!({"id":"record-2","method":"audit_record","run_id":run_id,"verdicts":[{"entry_id":audit_entry,"verdict":true}]}),
    );
    assert_eq!(replay["type"], "ok", "identical anonymous replay must succeed: {replay}");
    assert_eq!(replay["recorded"], 0, "replay must not duplicate a verdict");

    let events = events::read_events(&paths.events).unwrap().events;
    assert!(events.iter().all(|event| event.get("caller_id").is_none()));
    assert!(events
        .iter()
        .filter(|event| event["action"] == "expire")
        .all(|event| event.get("session").is_none()));
}

#[test]
fn caller_id_is_rejected_on_the_rust_port_boundary() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, _initial_conn) = db::test_db(repo.path());
    let entry_id = add_auditable(&paths, "option-b/reject-client-identity");
    let events_before = events::read_events(&paths.events).unwrap().events.len();

    for request in [
        json!({"id":"expire","method":"expire","entry_id":entry_id,"caller_id":"client"}),
        json!({"id":"run","method":"audit_run","sample_size":1,"caller_id":"client"}),
        json!({"id":"record","method":"audit_record","run_id":"run","verdicts":[],"caller_id":"client"}),
    ] {
        let response = dispatch(&paths, request);
        assert_eq!(response["type"], "error", "caller_id must be an unknown request field: {response}");
        assert_eq!(response["code"], "parse_error");
        assert!(response["message"].as_str().unwrap_or_default().contains("caller_id"));
    }

    assert_eq!(events::read_events(&paths.events).unwrap().events.len(), events_before);
}

#[test]
fn historical_caller_attributed_candidate_replays_and_allows_anonymous_recording() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, conn) = db::test_db(repo.path());
    let entry_id = add_auditable(&paths, "option-b/legacy-replay");

    let legacy = json!({
        "action":"audit_run_candidates_batch",
        "table":"audit_run_candidates",
        "run_id":"legacy-run",
        "caller_id":"legacy-host",
        "created_at":"2026-01-01T00:00:00Z",
        "ts":"2026-01-01T00:00:00Z",
        "candidates":[{"entry_id":entry_id,"arm":"uniform"}]
    });
    events::append_events_batch(&paths.events, std::slice::from_ref(&legacy)).unwrap();
    db::apply_event(&conn, &NoopEmbedder, &legacy).unwrap();

    let response = dispatch(
        &paths,
        json!({
            "id":"legacy-record",
            "method":"audit_record",
            "run_id":"legacy-run",
            "verdicts":[{"entry_id":entry_id,"verdict":true}]
        }),
    );
    assert_eq!(response["type"], "ok", "legacy caller data must not block anonymous use: {response}");
    assert_eq!(response["recorded"], 1);
}
