//! `run` subcommand

use crate::commands::add::{acquire_lock, read_omc_session};
use crate::components::embedder::Embedder;
use crate::components::{db, events};
use crate::config;
use abscissa_core::{Command, Runnable};
use anyhow::bail;
use clap::Parser;

/// Record a test run result
#[derive(Command, Debug, Parser)]
pub struct Run {
    /// Test case ID
    pub test_id: String,
    /// Result: pass | fail
    #[arg(long)]
    pub result: String,
    /// Adapter used
    #[arg(long)]
    pub adapter: Option<String>,
    /// Detail message
    #[arg(long)]
    pub detail: Option<String>,
}

impl Runnable for Run {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Run {
    /// Execute the run command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let embedder = crate::components::embedder::NoopEmbedder;
        self.execute_with_paths(&paths, &embedder)
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with_paths(
        &self,
        paths: &config::Paths,
        embedder: &dyn Embedder,
    ) -> anyhow::Result<()> {
        if self.result != "pass" && self.result != "fail" {
            bail!("--result must be 'pass' or 'fail', got: {}", self.result);
        }
        let _lock = acquire_lock(&paths.lock)?;
        let ts = chrono::Utc::now().to_rfc3339();
        let (session, omc_session_id) = read_omc_session();
        let run_id = uuid::Uuid::new_v4().to_string();

        let event = serde_json::json!({
            "action": "insert",
            "table": "run_history",
            "test_id": self.test_id,
            "result": self.result,
            "adapter": self.adapter,
            "detail": self.detail,
            "ts": ts,
            "run_id": run_id,
            "session": session,
            "session_id": omc_session_id,
        });

        events::append_event(&paths.events, &event)?;
        let conn = db::open_db(&paths.db)?;
        db::apply_event(&conn, embedder, &event)?;

        println!(
            "recorded run {}  {} -> {}",
            run_id.get(..8).unwrap_or(&run_id),
            self.test_id,
            self.result
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use std::fs;

    /// T3 (bd-21ef.1.8): the `run` CLI emitter must always carry a `run_id`
    /// on the appended event — the mechanism the keyed-insertion apply arm
    /// (`db.rs`'s `("insert", "run_history")`) relies on for idempotent
    /// replay (CompactMaterialize.tla D5.1).
    #[test]
    fn test_run_execute_emits_run_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let test_case = serde_json::json!({
            "action": "upsert", "table": "test_cases",
            "id": "t1", "app": "kb", "name": "n", "protocol": "rust_tool",
            "config": "{}", "ts": "2024-01-01T00:00:00Z"
        });
        events::append_event(&paths.events, &test_case).unwrap();
        let conn = db::open_db(&paths.db).unwrap();
        db::apply_event(&conn, &NoopEmbedder, &test_case).unwrap();
        drop(conn);

        let cmd = Run {
            test_id: "t1".to_string(),
            result: "pass".to_string(),
            adapter: None,
            detail: None,
        };
        cmd.execute_with_paths(&paths, &NoopEmbedder).unwrap();

        let logged = events::read_events(&paths.events).unwrap().events;
        let run_event = logged
            .iter()
            .find(|e| e["action"] == "insert" && e["table"] == "run_history")
            .expect("run event must be logged");
        let run_id = run_event["run_id"]
            .as_str()
            .expect("run event must carry a non-null run_id");
        assert!(!run_id.is_empty(), "run_id must not be empty");
        assert!(
            uuid::Uuid::parse_str(run_id).is_ok(),
            "run_id must be a uuid, got {run_id}"
        );
    }
}
