//! `expire` subcommand

use crate::commands::add::acquire_lock;
use crate::components::embedder::{Embedder, NoopEmbedder};
use crate::components::{db, events};
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::OptionalExtension;

/// Mark an entry stale
#[derive(Command, Debug, Parser)]
pub struct Expire {
    /// Entry ID to expire
    pub id: String,
    /// Reason for expiration
    #[arg(long)]
    pub reason: Option<String>,
    /// Force expiration of a permanent entry
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

impl Runnable for Expire {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Expire {
    /// Execute the expire command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        self.execute_with(&paths, &NoopEmbedder)
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn Embedder,
    ) -> anyhow::Result<()> {
        let _lock = acquire_lock(&paths.lock)?;
        let conn = db::open_db(&paths.db)?;

        // Guard: refuse to expire permanent entries unless --force
        if !self.force {
            let permanent: Option<i64> = conn
                .query_row(
                    "SELECT permanent FROM entries WHERE id=?1",
                    rusqlite::params![self.id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            if permanent == Some(1) {
                anyhow::bail!(
                    "entry '{}' is permanent; use --force to expire it",
                    self.id
                );
            }
        }

        let ts = chrono::Utc::now().to_rfc3339();
        let omc_session_id = std::env::var("OMC_SESSION_ID")
            .ok()
            .filter(|v| !v.is_empty());
        let session = omc_session_id
            .clone()
            .unwrap_or_else(|| "cli".to_string());

        let event = serde_json::json!({
            "action": "expire",
            "table": "entries",
            "id": self.id,
            "reason": self.reason,
            "ts": ts,
            "session": session,
            "session_id": omc_session_id,
        });

        events::append_event(&paths.events, &event)?;
        db::apply_event(&conn, embedder, &event)?;

        println!("expired {}", self.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::add::Add;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use rusqlite::Connection;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_cmd_expire_marks_stale() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Add entry with explicit version_ref (no cwd change needed)
        let add_cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test".to_string(),
            content: "content".to_string(),
            tags: "t".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("expire-test-1".to_string()),
            permanent: false,
            replace_path: false,
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // Expire it
        let expire_cmd = Expire {
            id: "expire-test-1".to_string(),
            reason: Some("outdated".to_string()),
            force: false,
        };
        expire_cmd.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
        let is_stale: i64 = conn
            .query_row(
                "SELECT is_stale FROM entries WHERE id='expire-test-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_stale, 1);
    }

    #[test]
    fn test_cmd_expire_permanent_without_force_fails() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let add_cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "perm entry".to_string(),
            content: "content".to_string(),
            tags: "t".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("perm-expire-1".to_string()),
            permanent: true,
            replace_path: false,
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        let expire_cmd = Expire {
            id: "perm-expire-1".to_string(),
            reason: None,
            force: false,
        };
        let result = expire_cmd.execute_with(&paths, &embedder);
        assert!(result.is_err(), "expire without --force must fail for permanent entry");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("permanent"), "error must mention 'permanent'");
    }

    #[test]
    fn test_cmd_expire_permanent_with_force_succeeds() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let add_cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "perm entry".to_string(),
            content: "content".to_string(),
            tags: "t".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("perm-expire-2".to_string()),
            permanent: true,
            replace_path: false,
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        let expire_cmd = Expire {
            id: "perm-expire-2".to_string(),
            reason: None,
            force: true,
        };
        expire_cmd.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
        let is_stale: i64 = conn
            .query_row(
                "SELECT is_stale FROM entries WHERE id='perm-expire-2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_stale, 1);
    }
}
