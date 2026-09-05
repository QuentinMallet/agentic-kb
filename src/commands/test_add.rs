//! `test-add` subcommand

use crate::commands::add::{acquire_lock, read_omc_session};
use crate::components::{cursor, db};
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;

/// Add or update a test case
#[derive(Command, Debug, Parser)]
pub struct TestAdd {
    /// Application name
    #[arg(long)]
    pub app: String,
    /// Test name
    #[arg(long)]
    pub name: String,
    /// Protocol: browser | rust_tool
    #[arg(long)]
    pub protocol: String,
    /// JSON blob of test config
    #[arg(long)]
    pub config: String,
    /// Test case ID (auto-generated if omitted)
    #[arg(long)]
    pub id: Option<String>,
    /// Git commit SHA
    #[arg(long)]
    pub version_ref: Option<String>,
}

impl Runnable for TestAdd {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl TestAdd {
    /// Execute the test-add command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let lock = acquire_lock(&paths.lock)?;
        let id = self
            .id
            .clone()
            .unwrap_or_else(|| format!("{}-{}", self.app, self.name.replace(' ', "-")));
        let version_ref = self.version_ref.clone().or_else(config::git_head_sha);
        let ts = chrono::Utc::now().to_rfc3339();
        let (session, omc_session_id) = read_omc_session();

        let event = serde_json::json!({
            "action": "upsert",
            "table": "test_cases",
            "id": id,
            "app": self.app,
            "name": self.name,
            "protocol": self.protocol,
            "config": self.config,
            "version_ref": version_ref,
            "ts": ts,
            "session": session,
            "session_id": omc_session_id,
        });

        // Writer 4 of 10.
        let conn = db::open_rw(&paths, &lock)?;
        let embedder = crate::components::embedder::NoopEmbedder;
        cursor::append_and_apply(&lock, &conn, &paths, &embedder, &[event])?;

        println!("added test case  {}/{} ({})", self.app, self.name, id);
        Ok(())
    }
}
