//! `run` subcommand

use crate::commands::add::acquire_lock;
use crate::components::{db, events};
use crate::config;
use anyhow::bail;
use abscissa_core::{Command, Runnable};
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
        if self.result != "pass" && self.result != "fail" {
            bail!("--result must be 'pass' or 'fail', got: {}", self.result);
        }
        let paths = config::Paths::discover()?;
        let _lock = acquire_lock(&paths.lock)?;
        let ts = chrono::Utc::now().to_rfc3339();
        let omc_session_id = std::env::var("OMC_SESSION_ID")
            .ok()
            .filter(|v| !v.is_empty());
        let session = omc_session_id
            .clone()
            .unwrap_or_else(|| "cli".to_string());
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
        let embedder = crate::components::embedder::NoopEmbedder;
        db::apply_event(&conn, &embedder, &event)?;

        println!("recorded run {}  {} -> {}", run_id.get(..8).unwrap_or(&run_id), self.test_id, self.result);
        Ok(())
    }
}
