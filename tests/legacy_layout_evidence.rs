use kb::components::db;
use kb::components::embedder::NoopEmbedder;
use kb::components::kb_core::{add, AddArgs};
use kb::config::Paths;
use rusqlite::params;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;

#[test]
fn legacy_layout_add_hashes_evidence_inside_repository_root() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("repo");
    let intended = b"evidence from the selected repository\n";
    let wrong = b"evidence from its parent\n";
    fs::create_dir_all(root.join("agent-kb")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(container.path().join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("src/x.txt"), intended).unwrap();
    fs::write(container.path().join("src/x.txt"), wrong).unwrap();

    let paths = Paths::from_db(&root.join("agent-kb/agent-kb.db"));
    add(
        &paths,
        &NoopEmbedder,
        AddArgs {
            id: "legacy-evidence".into(),
            path: "tests/legacy-evidence".into(),
            summary: "legacy evidence root".into(),
            content: "body".into(),
            tags: json!([]),
            version_ref: None,
            permanent: false,
            replace_path: false,
            kind: "belief".into(),
            evidence_status: "present".into(),
            evidence_rows: vec![json!({"kind": "code", "citation_path": "src/x.txt"})],
            ts: "2026-09-05T00:00:00Z".into(),
            session: "test".into(),
            session_id: None,
            expire_reason: String::new(),
            dedup_cutoff: None,
            cues: vec![],
        },
    )
    .unwrap();

    let expected = format!("sha256:{:x}", Sha256::digest(intended));
    let conn = db::open_ro(&paths.db).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT citation_hash FROM evidence WHERE entry_id = ?1",
            params!["legacy-evidence"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, expected);
}
