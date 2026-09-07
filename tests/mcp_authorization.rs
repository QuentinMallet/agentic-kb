//! Cross-language authorization contracts for bd-1orr.
//!
//! The Elixir MCP boundary attaches `caller_id` from immutable host-launch
//! metadata.  Rust treats this private-port field as required for every audit
//! mutation, binds runs to it, and writes it as durable event attribution.

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
            "summary": format!("authorization fixture {path}"),
            "content": "authorization test content",
            "tags": ["mcp-authorization"],
            "kind": "observation",
            "evidence": [{"kind": "code", "citation_path": "fixture.txt:1-7"}]
        }),
    );
    assert_eq!(response["type"], "ok", "add failed: {response}");
    response["entry_id"].as_str().unwrap().to_owned()
}

fn audit_run(paths: &kb::config::Paths, caller: &str, sample_size: usize) -> Value {
    dispatch_value_for_test(
        paths,
        &NoopEmbedder,
        &json!({
            "id": format!("run-{caller}"),
            "method": "audit_run",
            // This is supplied only by the trusted Elixir port bridge, never
            // by a tool's arguments or initialize.clientInfo metadata.
            "caller_id": caller,
            "sample_size": sample_size,
            "mode": "uniform"
        }),
    )
}

fn audit_record(
    paths: &kb::config::Paths,
    caller: &str,
    run_id: &str,
    verdicts: Vec<Value>,
) -> Value {
    dispatch_value_for_test(
        paths,
        &NoopEmbedder,
        &json!({
            "id": format!("record-{caller}"),
            "method": "audit_record",
            "caller_id": caller,
            "run_id": run_id,
            "verdicts": verdicts,
        }),
    )
}

fn direct_expire(paths: &kb::config::Paths, caller: &str, entry_id: &str) -> Value {
    dispatch_value_for_test(
        paths,
        &NoopEmbedder,
        &json!({
            "id": format!("expire-{caller}"),
            "method": "expire",
            "caller_id": caller,
            "entry_id": entry_id,
            "reason": "authorization attribution fixture",
            "force": false,
        }),
    )
}

#[test]
fn audit_runs_are_owned_by_the_launch_caller_and_store_that_attribution() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, _initial_conn) = db::test_db(repo.path());

    let entry_id = add_auditable(&paths, "authorization/owner");
    let run = audit_run(&paths, "host-agent-a", 1);
    assert_eq!(run["type"], "ok", "run must accept trusted caller: {run}");
    let run_id = run["run_id"].as_str().unwrap();

    let stolen = audit_record(
        &paths,
        "host-agent-b",
        run_id,
        vec![json!({"entry_id": entry_id, "verdict": true})],
    );
    assert_eq!(stolen["type"], "error");
    assert_eq!(stolen["code"], "run_owner_mismatch");

    let conn = db::open_ro(&paths.db).unwrap();
    let stolen_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_runs WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stolen_rows, 0,
        "cross-caller record must not leave a prefix row"
    );

    let accepted = audit_record(
        &paths,
        "host-agent-a",
        run_id,
        vec![json!({"entry_id": entry_id, "verdict": true})],
    );
    assert_eq!(
        accepted["type"], "ok",
        "owner record must succeed: {accepted}"
    );

    let caller: String = conn
        .query_row(
            "SELECT caller_id FROM audit_runs WHERE run_id=?1 AND entry_id=?2",
            [run_id, entry_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(caller, "host-agent-a");
}

#[test]
fn false_verdict_events_use_the_launch_caller_not_the_legacy_mcp_label() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, _initial_conn) = db::test_db(repo.path());

    let entry_id = add_auditable(&paths, "authorization/attribution");
    let run = audit_run(&paths, "host-agent-a", 1);
    let run_id = run["run_id"].as_str().unwrap();

    let recorded = audit_record(
        &paths,
        "host-agent-a",
        run_id,
        vec![json!({
            "entry_id": entry_id,
            "verdict": false,
            "note": "evidence no longer supports this fixture"
        })],
    );
    assert_eq!(recorded["type"], "ok", "record must succeed: {recorded}");

    let event = events::read_events(&paths.events)
        .unwrap()
        .events
        .into_iter()
        .rev()
        .find(|event| event["action"] == "audit_record_batch")
        .expect("false verdict must append a durable audit batch event");
    assert_eq!(event["caller_id"], "host-agent-a");
    assert_ne!(event["caller_id"], "mcp");
    assert_eq!(event["run_id"], run_id);
    assert_eq!(event["verdicts"][0]["entry_id"], entry_id);
    assert_eq!(event["verdicts"][0]["verdict"], false);
}

#[test]
fn direct_expire_events_also_use_the_launch_caller() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, _initial_conn) = db::test_db(repo.path());

    let entry_id = add_auditable(&paths, "authorization/direct-expire");
    let expired = direct_expire(&paths, "host-agent-a", &entry_id);
    assert_eq!(
        expired["type"], "ok",
        "trusted direct expire must succeed: {expired}"
    );

    let event = events::read_events(&paths.events)
        .unwrap()
        .events
        .into_iter()
        .rev()
        .find(|event| event["action"] == "expire")
        .expect("direct expire must append an event");
    assert_eq!(event["session"], "host-agent-a");
    assert_ne!(event["session"], "mcp");
}

#[test]
fn failed_verdict_three_of_five_leaves_no_partial_audit_or_expire_state() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, trigger_conn) = db::test_db(repo.path());

    for suffix in 0..5 {
        add_auditable(&paths, &format!("authorization/atomic-{suffix}"));
    }

    let run = audit_run(&paths, "host-agent-a", 5);
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let sampled: Vec<String> = run["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|sample| sample["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sampled.len(), 5);

    trigger_conn
        .execute_batch(&format!(
            "CREATE TRIGGER fail_third_audit BEFORE INSERT ON audit_runs
         WHEN NEW.run_id = '{}' AND NEW.entry_id = '{}'
         BEGIN SELECT RAISE(ABORT, 'fault verdict 3'); END;",
            run_id, sampled[2]
        ))
        .unwrap();
    drop(trigger_conn);

    let before_events = events::read_events(&paths.events).unwrap().events.len();
    let response = audit_record(
        &paths,
        "host-agent-a",
        &run_id,
        sampled
            .iter()
            .map(|entry_id| {
                json!({
                    "entry_id": entry_id,
                    "verdict": false,
                    "note": "atomicity fault-injection fixture"
                })
            })
            .collect(),
    );
    assert_eq!(response["type"], "error");
    assert_eq!(response["code"], "db_error");

    let conn = db::open_ro(&paths.db).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_runs WHERE run_id=?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 0,
        "a verdict-3 failure must roll back the entire request"
    );

    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE id IN (?1, ?2, ?3, ?4, ?5) AND is_stale=1",
            rusqlite::params![sampled[0], sampled[1], sampled[2], sampled[3], sampled[4]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale, 0, "a failed request must not expire a proper prefix");

    assert_eq!(
        events::read_events(&paths.events).unwrap().events.len(),
        before_events,
        "a failed request must not append durable expire events"
    );
}

#[test]
fn exact_duplicate_audit_record_is_a_noop() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, _initial_conn) = db::test_db(repo.path());

    let entry_id = add_auditable(&paths, "authorization/idempotent");
    let run = audit_run(&paths, "host-agent-a", 1);
    let run_id = run["run_id"].as_str().unwrap();
    let verdict = vec![json!({"entry_id": entry_id, "verdict": true})];

    let first = audit_record(&paths, "host-agent-a", run_id, verdict.clone());
    assert_eq!(first["type"], "ok", "first record must succeed: {first}");
    assert_eq!(first["recorded"], 1);

    let before_events = events::read_events(&paths.events).unwrap().events.len();
    let second = audit_record(&paths, "host-agent-a", run_id, verdict);
    assert_eq!(second["type"], "ok", "exact replay must succeed: {second}");
    assert_eq!(second["recorded"], 0);
    assert_eq!(second["expired"], 0);
    assert_eq!(
        events::read_events(&paths.events).unwrap().events.len(),
        before_events
    );

    let conn = db::open_ro(&paths.db).unwrap();
    let audit_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_runs WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_rows, 1, "exact replay must not duplicate audit rows");

    let successes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(successes),0) FROM source_weights",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(successes, 1, "exact replay must not double-count weights");
}

#[test]
fn changed_duplicate_audit_record_rejects_before_effects() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, _initial_conn) = db::test_db(repo.path());

    let entry_id = add_auditable(&paths, "authorization/conflict");
    let run = audit_run(&paths, "host-agent-a", 1);
    let run_id = run["run_id"].as_str().unwrap();

    let first = audit_record(
        &paths,
        "host-agent-a",
        run_id,
        vec![json!({"entry_id": entry_id, "verdict": true})],
    );
    assert_eq!(first["type"], "ok", "first record must succeed: {first}");

    let before_events = events::read_events(&paths.events).unwrap().events.len();
    let conflict = audit_record(
        &paths,
        "host-agent-a",
        run_id,
        vec![json!({
            "entry_id": entry_id,
            "verdict": false,
            "note": "changed replay"
        })],
    );
    assert_eq!(conflict["type"], "error");
    assert_eq!(conflict["code"], "audit_record_conflict");
    assert_eq!(
        events::read_events(&paths.events).unwrap().events.len(),
        before_events,
        "changed replay must not append an expire event"
    );

    let conn = db::open_ro(&paths.db).unwrap();
    let (audit_rows, stale, failures): (i64, i64, i64) = conn
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM audit_runs WHERE run_id=?1),
                (SELECT is_stale FROM entries WHERE id=?2),
                (SELECT COALESCE(SUM(failures),0) FROM source_weights)",
            rusqlite::params![run_id, entry_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(audit_rows, 1, "changed replay must not add an audit row");
    assert_eq!(stale, 0, "changed replay must not expire the entry");
    assert_eq!(
        failures, 0,
        "changed replay must not count a failure weight"
    );
}
