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

/// Full-table replay comparison, the same shape `tests/log_framing.rs`'s
/// `dump_materialized` uses: `created_at`/`updated_at` are skipped because
/// they are `datetime('now')` defaults that can straddle a second boundary
/// across two replays of the same log, and `cues.id` is skipped because cue
/// rows are replaced wholesale (DELETE + re-INSERT) on every upsert, so its
/// AUTOINCREMENT surrogate keeps climbing across replays even when the
/// resulting `(entry_id, cue)` content is unchanged. `audit_runs` and
/// `source_weights` are not event-sourced, so a log replay cannot reproduce
/// them and both are absent here too, same as in the existing helper.
fn dump_materialized(conn: &rusqlite::Connection) -> Vec<String> {
    let mut rows = Vec::new();
    for table in ["entries", "evidence", "cues", "test_cases", "run_history"] {
        let columns: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .filter(|c| c != "created_at" && c != "updated_at")
            .filter(|c| !(table == "cues" && c == "id"))
            .collect();
        let mut stmt = conn
            .prepare(&format!("SELECT {} FROM {table}", columns.join(",")))
            .unwrap();
        let count = stmt.column_count();
        let mut dumped: Vec<String> = stmt
            .query_map([], |row| {
                let mut cells = Vec::new();
                for i in 0..count {
                    let v: rusqlite::types::Value = row.get(i)?;
                    cells.push(format!("{v:?}"));
                }
                Ok(cells.join("|"))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        dumped.sort();
        rows.push(format!("{table}: {}", dumped.join(";")));
    }
    rows
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
    // Sampled but never given a solo verdict: only used inside the mixed
    // batches below, so a wrongly-applied leading verdict is visible as
    // this id staying live when it should have been rejected along with
    // the rest of its batch.
    let mixed_true_id = add_auditable(&paths, "audit/mixed-true", false);
    let seeded = vec![
        accepted_id.clone(),
        expired_id.clone(),
        permanent_id.clone(),
        mixed_true_id.clone(),
    ];

    let run = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRunRequest::new(json!("run"), Some(4), Some("uniform")),
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
    assert_eq!(accepted["expired"], 0);

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

    // The single-item cases above satisfy "rejects the whole batch before
    // any item applies" only vacuously: a batch of one cannot partially
    // apply. Pin the real property with a mixed batch that leads with a
    // valid verdict, for both rejection paths that run their checks over
    // the full batch before any write (src/commands/mcp.rs's note check and
    // permanent-guard check both loop over every verdict up front).
    let before_mixed = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditReportRequest::new(json!("report-before-mixed")),
    );

    let mixed_note_less = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRecordRequest::new(
            json!("record-mixed-note-less"),
            &run_id,
            vec![
                verdict(&mixed_true_id, true, None),
                verdict(&accepted_id, false, None),
            ],
        ),
    );
    assert_eq!(mixed_note_less["code"], "parse_error");
    assert!(mixed_note_less["message"]
        .as_str()
        .unwrap()
        .contains(&accepted_id));

    let after_mixed_note_less = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditReportRequest::new(json!("report-after-mixed-note-less")),
    );
    assert_eq!(
        after_mixed_note_less["total_runs"], before_mixed["total_runs"],
        "a note-less false verdict must void the whole batch, so the leading \
         true verdict for {mixed_true_id} must not have been recorded either"
    );

    let mixed_permanent = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditRecordRequest::new(
            json!("record-mixed-permanent"),
            &run_id,
            vec![
                verdict(&mixed_true_id, true, None),
                verdict(&permanent_id, false, Some("attempted expiry")),
            ],
        ),
    );
    assert_eq!(mixed_permanent["code"], "permanent_guard");
    assert!(mixed_permanent["message"]
        .as_str()
        .unwrap()
        .contains(&permanent_id));

    let after_mixed_permanent = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditReportRequest::new(json!("report-after-mixed-permanent")),
    );
    assert_eq!(
        after_mixed_permanent["total_runs"], before_mixed["total_runs"],
        "a permanent-guard refusal must void the whole batch, so the leading \
         true verdict for {mixed_true_id} must not have been recorded either"
    );

    let report = dispatch_for_test(
        &paths,
        &NoopEmbedder,
        &AuditReportRequest::new(json!("report")),
    );
    assert_eq!(report["type"], "result");
    assert_eq!(report["total_runs"], 2);

    // Pin the group itself, not just index 0: the handler groups by
    // (kind, session_id) and by arm (src/commands/mcp.rs's handle_audit_report),
    // so a second group introduced elsewhere would otherwise shift index 0
    // silently and this assertion would still pass against the wrong row.
    let per_kind = report["per_kind_session_precision"].as_array().unwrap();
    assert_eq!(per_kind.len(), 1);
    assert_eq!(per_kind[0]["kind"], "observation");
    assert_eq!(per_kind[0]["session_id"], "__GLOBAL__");
    assert_eq!(per_kind[0]["n"], 2);
    assert_eq!(per_kind[0]["precision"], 0.5);

    let per_arm = report["per_arm_precision"].as_array().unwrap();
    assert_eq!(per_arm.len(), 1);
    assert_eq!(per_arm[0]["arm"], "uniform");
    assert_eq!(per_arm[0]["n"], 2);
    assert_eq!(per_arm[0]["precision"], 0.5);

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
        BTreeSet::from([
            accepted_id.clone(),
            permanent_id.clone(),
            mixed_true_id.clone(),
        ])
    );

    let replay_dir = tempfile::tempdir().unwrap();
    let (_replay_paths, replay) = db::test_db(replay_dir.path());
    for event in events::read_events(&paths.events).unwrap().events {
        db::apply_event(&replay, &NoopEmbedder, &event).unwrap();
    }
    // Full-table comparison, not just the four ids' liveness: a replay
    // that reproduced liveness while diverging on tags, evidence rows,
    // cues, or content would otherwise pass. `entries_fts`/`entries_emb`
    // are outside dump_materialized's table list (same as its
    // tests/log_framing.rs precedent), so an FTS- or embedding-only
    // divergence would not be caught here.
    assert_eq!(dump_materialized(&original), dump_materialized(&replay));
}
