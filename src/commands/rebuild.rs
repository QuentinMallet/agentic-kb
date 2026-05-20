//! `rebuild` subcommand

use crate::commands::add::{acquire_lock, make_embedder};
use crate::components::embedder::Embedder;
use crate::components::{db, events};
use crate::config;
use anyhow::Context;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use std::fs;

/// Replay all events and rebuild agent-kb.db from scratch
#[derive(Command, Debug, Parser)]
pub struct Rebuild;

impl Runnable for Rebuild {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Rebuild {
    /// Execute the rebuild command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let embedder = make_embedder(&paths);
        self.execute_with(&paths, embedder.as_ref())
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn Embedder,
    ) -> anyhow::Result<()> {
        let _lock = acquire_lock(&paths.lock)?;

        // Drop the DB so we start from a clean slate.
        if paths.db.exists() {
            fs::remove_file(&paths.db)?;
            let db_str = paths.db.to_string_lossy();
            let _ = fs::remove_file(format!("{}-wal", db_str));
            let _ = fs::remove_file(format!("{}-shm", db_str));
        }

        let conn = db::open_db(&paths.db)?;
        let evts = events::read_events(&paths.events)?;

        eprintln!("replaying {} events...", evts.len());
        for event in &evts {
            db::apply_event(&conn, embedder, event)
                .with_context(|| format!("apply event: {}", event))?;
        }

        eprintln!("rebuild complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::embedder::NoopEmbedder;
    use crate::components::events::append_event;
    use crate::config::Paths;
    use rusqlite::Connection;
    use std::fs;
    use std::process::Command as Cmd;
    use tempfile::tempdir;

    #[test]
    fn test_cmd_rebuild_from_events() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        Cmd::new("git")
            .args(["init", "-b", "master"])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::write(root.join("R"), "i").unwrap();
        Cmd::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["commit", "-m", "i"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Write events directly to JSONL
        let e1 = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "rb1", "path": "a.rs", "summary": "first",
            "content": "content1", "tags": [], "ts": "2024-01-01T00:00:00Z"
        });
        let e2 = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "rb2", "path": "b.rs", "summary": "second",
            "content": "content2", "tags": [], "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &e1).unwrap();
        append_event(&paths.events, &e2).unwrap();

        // Rebuild
        let cmd = Rebuild;
        cmd.execute_with(&paths, &embedder).unwrap();

        // Verify DB has both entries
        let conn = Connection::open(&paths.db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
