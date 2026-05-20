//! `add` subcommand

use crate::components::db;
use crate::components::embedder;
use crate::components::events;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;

/// Add or update a knowledge entry
#[derive(Command, Debug, Parser)]
pub struct Add {
    /// Category/file path
    #[arg(long)]
    pub path: String,
    /// Short summary
    #[arg(long)]
    pub summary: String,
    /// Full content
    #[arg(long)]
    pub content: String,
    /// Comma-separated tags
    #[arg(long)]
    pub tags: String,
    /// Git commit SHA (auto-populated from HEAD if omitted)
    #[arg(long)]
    pub version_ref: Option<String>,
    /// Entry ID (auto-generated UUID if omitted)
    #[arg(long)]
    pub id: Option<String>,
}

impl Runnable for Add {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Add {
    /// Execute the add command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let embedder = make_embedder(&paths);
        self.execute_with(&paths, embedder.as_ref())
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let _lock = acquire_lock(&paths.lock)?;
        let id = self
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let version_ref = self.version_ref.clone().or_else(config::git_head_sha);
        let tags_json: serde_json::Value = serde_json::json!(
            self.tags.split(',').map(|t| t.trim()).collect::<Vec<_>>()
        );
        let ts = chrono::Utc::now().to_rfc3339();
        let session =
            std::env::var("OMC_SESSION_ID").unwrap_or_else(|_| "cli".to_string());

        let event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": id,
            "path": self.path,
            "summary": self.summary,
            "content": self.content,
            "tags": tags_json,
            "version_ref": version_ref,
            "ts": ts,
            "session": session,
        });

        events::append_event(&paths.events, &event)?;

        let conn = db::open_db(&paths.db)?;
        db::apply_event(&conn, embedder, &event)?;

        println!("added  {} ({})", self.path, id);
        Ok(())
    }
}

/// Build the appropriate embedder based on KB_NO_EMBED env var.
pub fn make_embedder(paths: &config::Paths) -> Box<dyn embedder::Embedder> {
    if std::env::var("KB_NO_EMBED").is_ok() {
        Box::new(embedder::NoopEmbedder)
    } else {
        Box::new(embedder::CandleEmbedder::new(&paths.fastembed_cache))
    }
}

/// Acquire the agentic lock.
pub fn acquire_lock(lock_path: &std::path::Path) -> anyhow::Result<Lock> {
    use anyhow::Context;
    use fs2::FileExt;
    use std::fs::{self, OpenOptions};

    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    f.lock_exclusive()
        .with_context(|| format!("acquire lock {}", lock_path.display()))?;
    Ok(Lock(f))
}

/// RAII lock guard — holds the file lock until dropped.
pub struct Lock(#[allow(dead_code)] std::fs::File);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use rusqlite::Connection;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command as Cmd;
    use tempfile::tempdir;

    /// RAII guard that restores cwd on drop.
    struct CwdGuard(PathBuf);
    impl CwdGuard {
        fn set(dir: &std::path::Path) -> Self {
            let orig = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            CwdGuard(orig)
        }
    }
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    fn setup_test_repo() -> (tempfile::TempDir, Paths) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        Cmd::new("git")
            .args(["init", "-b", "master"])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.email", "test@test"])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::write(root.join("README"), "init").unwrap();
        Cmd::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();

        let paths = Paths::from_root(root);
        (dir, paths)
    }

    #[test]
    fn test_cmd_add_writes_event_and_db_row() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test entry".to_string(),
            content: "content".to_string(),
            tags: "rust,test".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("test-id-1".to_string()),
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        // Verify JSONL event was written
        let events_content = fs::read_to_string(&paths.events).unwrap();
        assert!(events_content.contains("test-id-1"));

        // Verify DB row
        let conn = Connection::open(&paths.db).unwrap();
        let (path, summary): (String, String) = conn
            .query_row(
                "SELECT path, summary FROM entries WHERE id='test-id-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path, "src/lib.rs");
        assert_eq!(summary, "test entry");
    }

    #[test]
    fn test_cmd_add_auto_populates_version_ref() {
        let (dir, paths) = setup_test_repo();
        let embedder = NoopEmbedder;

        let expected_sha = String::from_utf8(
            Cmd::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let _guard = CwdGuard::set(dir.path());

        let cmd = Add {
            path: "src/lib.rs".to_string(),
            summary: "test entry".to_string(),
            content: "content".to_string(),
            tags: "test".to_string(),
            version_ref: None,
            id: None,
        };
        cmd.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
        let version_ref: Option<String> = conn
            .query_row(
                "SELECT version_ref FROM entries WHERE path = 'src/lib.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version_ref, Some(expected_sha));
    }
}
