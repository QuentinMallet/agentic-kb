use kb::commands::mcp::tests_api::{
    dispatch_for_test, dispatch_value_for_test, AuditRecordRequest, AuditReportRequest,
    AuditRunRequest, AuditVerdict,
};
use kb::components::{db, embedder::NoopEmbedder, events};
use serde_json::json;
use std::collections::BTreeSet;

fn add_auditable(paths: &kb::config::Paths, path: &str, permanent: bool) -> String {
    let response = dispatch_value_for_test(
        paths,
        &NoopEmbedder,
        &json!({
            "id": format!("add-{path}"),
            "method": "add",
            "path": path,
            "summary": format!("audit fixture {path}"),
            "content": "composition-test content",
            "tags": ["audit-composition"],
            "kind": "observation",
            "permanent": permanent,
            "evidence": [{"kind": "code", "citation_path": "fixture.txt:1-7"}]
        }),
    );
    assert_eq!(response["type"], "ok", "add failed: {response}");
    response["entry_id"].as_str().unwrap().to_owned()
}

fn verdict(entry_id: &str, value: bool, note: Option<&str>) -> AuditVerdict {
    AuditVerdict {
        entry_id: entry_id.to_owned(),
        verdict: value,
        note: note.map(str::to_owned),
    }
}

fn live_ids(conn: &rusqlite::Connection, ids: &[String]) -> BTreeSet<String> {
    ids.iter()
        .filter(|id| {
            conn.query_row(
                "SELECT COUNT(*) FROM entries WHERE id=?1 AND is_stale=0",
                [id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
                == 1
        })
        .cloned()
        .collect()
}

#[test]
fn audit_run_record_report_composes_through_the_mcp_dispatcher_and_replays() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("fixture.txt"), "fixture\n").unwrap();
    let (paths, initial_conn) = db::test_db(repo.path());
    drop(initial_conn);

    let accepted_id = add_auditable(&paths, "audit/accepted", false);
    let expired_id = add_auditable(&paths, "audit/expired", false);
    let permanent_id = add_auditable(&paths, "audit/permanent", true);
    let seeded = vec![
        accepted_id.clone(),
        expired_id.clone(),
        permanent_id.clone(),
    ];

    let run = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRunRequest::new(json!("run"), Some(3), Some("uniform")),
    );
    assert_eq!(run["type"], "ok", "audit_run failed: {run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    assert!(!run_id.is_empty());
    let sampled: BTreeSet<&str> = run["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|sample| sample["id"].as_str().unwrap())
        .collect();
    assert_eq!(sampled, seeded.iter().map(String::as_str).collect());

    let accepted = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRecordRequest::new(
            json!("record-true"),
            &run_id,
            vec![verdict(&accepted_id, true, None)],
        ),
    );
    assert_eq!(accepted["recorded"], 1);

    let expired = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRecordRequest::new(
            json!("record-false"),
            &run_id,
            vec![verdict(
                &expired_id,
                false,
                Some("evidence does not support it"),
            )],
        ),
    );
    assert_eq!(expired["recorded"], 1);
    assert_eq!(expired["expired"], 1);

    let note_less = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRecordRequest::new(
            json!("record-note-less"),
            &run_id,
            vec![verdict(&accepted_id, false, None)],
        ),
    );
    assert_eq!(note_less["code"], "parse_error");
    assert!(note_less["message"]
        .as_str()
        .unwrap()
        .contains(&accepted_id));

    let permanent = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRecordRequest::new(
            json!("record-permanent"),
            &run_id,
            vec![verdict(&permanent_id, false, Some("attempted expiry"))],
        ),
    );
    assert_eq!(permanent["code"], "permanent_guard");
    assert!(permanent["message"]
        .as_str()
        .unwrap()
        .contains(&permanent_id));
    assert!(permanent["message"].as_str().unwrap().contains("permanent"));

    let report = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditReportRequest::new(json!("report")),
    );
    assert_eq!(report["type"], "result");
    assert_eq!(report["total_runs"], 2);
    assert_eq!(report["per_kind_session_precision"][0]["n"], 2);
    assert_eq!(report["per_kind_session_precision"][0]["precision"], 0.5);
    assert_eq!(report["per_arm_precision"][0]["n"], 2);
    assert_eq!(report["per_arm_precision"][0]["precision"], 0.5);

    let search = dispatch_value_for_test(
        &paths,
        &NoopEmbedder,
        &json!({"id":"search", "method":"search", "query":"audit fixture", "mode":"fts", "limit":10}),
    );
    let search_ids: BTreeSet<&str> = search["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert!(!search_ids.contains(expired_id.as_str()));
    assert!(search_ids.contains(permanent_id.as_str()));

    let too_many = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRecordRequest::new(
            json!("record-cap"),
            &run_id,
            (0..51).map(|_| verdict(&accepted_id, true, None)).collect(),
        ),
    );
    assert_eq!(too_many["code"], "parse_error");
    assert!(too_many["message"].as_str().unwrap().contains("50"));

    let original = db::open_ro(&paths.db).unwrap();
    assert_eq!(
        live_ids(&original, &seeded),
        BTreeSet::from([accepted_id.clone(), permanent_id.clone()])
    );

    let replay_dir = tempfile::tempdir().unwrap();
    let (_replay_paths, replay) = db::test_db(replay_dir.path());
    for event in events::read_events(&paths.events).unwrap().events {
        db::apply_event(&replay, &NoopEmbedder, &event).unwrap();
    }
    assert_eq!(live_ids(&replay, &seeded), live_ids(&original, &seeded));
}
