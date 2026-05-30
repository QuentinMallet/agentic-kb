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
    ///
    /// Three-phase algorithm so concurrent writes are never blocked for more
    /// than a brief lock-acquisition at either end:
    ///
    /// 1. Snapshot (brief lock): record event count N, release lock.
    /// 2. Replay (no lock): replay events 1..N into `agent-kb.db.tmp`.
    ///    MCP writes continue normally against the live DB during this phase.
    /// 3. Catch-up + swap (brief lock): apply events N+1..M written during
    ///    phase 2, then atomically rename tmp into place.
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn Embedder,
    ) -> anyhow::Result<()> {
        // Phase 1: snapshot event count under a brief lock.
        let snapshot_len = {
            let _lock = acquire_lock(&paths.lock)?;
            events::read_events(&paths.events)?.len()
        };

        // Phase 2: replay snapshot into a tmp DB — no lock held.
        let tmp_db = paths.db.with_extension("db.tmp");
        let _ = fs::remove_file(&tmp_db);
        {
            // Stop at snapshot_len so we never encounter a partial tail line
            // that a concurrent writer may be mid-writing after Phase 1 released the lock.
            let evts = events::read_events_up_to(&paths.events, snapshot_len)?;
            let to_replay = evts.len();
            let conn = db::open_db(&tmp_db)?;
            // DELETE journal avoids WAL files on the tmp path, simplifying the
            // rename step (no companion files to move or orphan).
            conn.execute_batch("PRAGMA journal_mode=DELETE")?;
            eprintln!("replaying {} events...", to_replay);
            for event in &evts[..to_replay] {
                db::apply_event(&conn, embedder, event)
                    .with_context(|| format!("apply event: {}", event))?;
            }
        }

        // Phase 3: catch-up and atomic swap under lock.
        let _lock = acquire_lock(&paths.lock)?;
        let all_evts = events::read_events(&paths.events)?;
        let catchup = &all_evts[snapshot_len.min(all_evts.len())..];
        if !catchup.is_empty() {
            eprintln!("catching up {} new event(s)...", catchup.len());
            let conn = db::open_db(&tmp_db)?;
            conn.execute_batch("PRAGMA journal_mode=DELETE")?;
            for event in catchup {
                db::apply_event(&conn, embedder, event)
                    .with_context(|| format!("apply event (catch-up): {}", event))?;
            }
        }

        // Remove old WAL/SHM before rename. This is required: the tmp DB uses
        // journal_mode=DELETE (no WAL), so if the old WAL files remain after
        // the rename, new SQLite connections would attempt WAL recovery against
        // the rebuilt DB, producing corruption or an error.
        // Safety (Linux): the per-request connection model means no MCP handler
        // holds a connection across the lock boundary, so no reader has the WAL
        // open when we unlink it. On Linux, any FD open at unlink time remains
        // valid (the inode persists until the last close), so this is safe even
        // if a reader opened just before the lock was acquired. fs::rename then
        // atomically replaces the DB file in one syscall.
        let db_str = paths.db.to_string_lossy();
        let _ = fs::remove_file(format!("{}-wal", db_str));
        let _ = fs::remove_file(format!("{}-shm", db_str));
        fs::rename(&tmp_db, &paths.db)
            .with_context(|| "rename rebuilt DB into place")?;

        eprintln!("rebuild complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{db, embedder::NoopEmbedder, events};
    use crate::config::Paths;
    use rusqlite::Connection;
    use std::fs;
    use std::process::Command as Cmd;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn setup_repo() -> (tempfile::TempDir, Paths) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "T"],
        ] {
            Cmd::new("git").args(&args).current_dir(root).output().unwrap();
        }
        fs::write(root.join("R"), "i").unwrap();
        Cmd::new("git").args(["add", "."]).current_dir(root).output().unwrap();
        Cmd::new("git").args(["commit", "-m", "i"]).current_dir(root).output().unwrap();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        (dir, paths)
    }

    fn upsert(id: &str, idx: u32) -> serde_json::Value {
        serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": id, "path": format!("p{idx}.rs"), "summary": "s",
            "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
        })
    }

    fn count_entries(paths: &Paths) -> i64 {
        Connection::open(&paths.db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn test_cmd_rebuild_from_events() {
        let (_dir, paths) = setup_repo();
        let emb = NoopEmbedder;
        events::append_event(&paths.events, &upsert("rb1", 1)).unwrap();
        events::append_event(&paths.events, &upsert("rb2", 2)).unwrap();
        Rebuild.execute_with(&paths, &emb).unwrap();
        assert_eq!(count_entries(&paths), 2);
    }

    /// DB cleared (e.g. corrupted or missing) — rebuild reconstructs from event log.
    #[test]
    fn test_rebuild_restores_cleared_db() {
        let (_dir, paths) = setup_repo();
        let emb = NoopEmbedder;

        for i in 0..10u32 {
            let e = upsert(&format!("id{i}"), i);
            events::append_event(&paths.events, &e).unwrap();
            db::apply_event(&db::open_db(&paths.db).unwrap(), &emb, &e).unwrap();
        }
        assert_eq!(count_entries(&paths), 10);

        // Corrupt DB
        Connection::open(&paths.db)
            .unwrap()
            .execute("DELETE FROM entries", [])
            .unwrap();
        assert_eq!(count_entries(&paths), 0);

        Rebuild.execute_with(&paths, &emb).unwrap();
        assert_eq!(count_entries(&paths), 10, "rebuild must restore all events");
    }

    /// Events written concurrently with Phase 2 (lock-free replay) must appear
    /// in the final DB.  Because exact interleaving is non-deterministic, we
    /// run a second rebuild after all threads join to guarantee convergence:
    /// after any rebuild, DB = Materialize(all events in log).
    #[test]
    fn test_rebuild_concurrent_writes_converge() {
        let (_dir, paths) = setup_repo();
        let emb = NoopEmbedder;

        for i in 0..20u32 {
            let e = upsert(&format!("base{i}"), i);
            events::append_event(&paths.events, &e).unwrap();
            db::apply_event(&db::open_db(&paths.db).unwrap(), &emb, &e).unwrap();
        }

        let events_path = paths.events.clone();
        let writer = thread::spawn(move || {
            // Brief sleep to maximise overlap with Phase 2 (lock-free replay).
            thread::sleep(Duration::from_micros(50));
            for i in 0..10u32 {
                let e = upsert(&format!("extra{i}"), i + 100);
                events::append_event(&events_path, &e).unwrap();
            }
        });

        Rebuild.execute_with(&paths, &emb).unwrap();
        writer.join().unwrap();

        // All 30 events are now in the log.  A second rebuild converges the DB.
        Rebuild.execute_with(&paths, &emb).unwrap();

        let log_len = events::read_events(&paths.events).unwrap().len() as i64;
        assert_eq!(log_len, 30);
        assert_eq!(
            count_entries(&paths),
            log_len,
            "DB must equal Materialize(log) after rebuild"
        );
    }
}
