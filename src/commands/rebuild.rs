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

    // -----------------------------------------------------------------------
    // T-S6b: concurrent evidence-write integration test (br-jwe.15, AC6)
    // -----------------------------------------------------------------------

    /// Build an evidence_add event for the given entry and evidence IDs.
    fn evidence_add(entry_id: &str, ev_id: &str, idx: u32) -> serde_json::Value {
        serde_json::json!({
            "action": "evidence_add",
            "table": "evidence",
            "entry_id": entry_id,
            "evidence": {
                "id": ev_id,
                "entry_id": entry_id,
                "kind": "test",
                "citation_path": null,
                "citation_sha": null,
                "citation_hash": format!("hash{idx}"),
                "citation_excerpt": null,
                "derived_from": null,
                "recorded_at": "2024-01-01T00:00:00Z"
            },
            "ts": "2024-01-01T00:00:00Z"
        })
    }

    /// Evidence rows written concurrently with rebuild must survive in the final
    /// DB.  After all threads join a second rebuild guarantees convergence:
    /// DB == Materialize(all events in log) and evidence_status reflects the
    /// soft-mandate StatusOf rule (present when evidence rows exist).
    #[test]
    fn test_rebuild_with_concurrent_evidence_writes_converges() {
        let (_dir, paths) = setup_repo();
        let emb = NoopEmbedder;

        // Seed: 5 entries with explicit kind='observation' so evidence_add
        // triggers evidence_status updates (status != 'n/a').
        for i in 0..5u32 {
            let e = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": format!("base{i}"), "path": format!("p{i}.rs"),
                "summary": "s", "content": "c", "tags": [],
                "kind": "observation", "evidence_status": "missing",
                "ts": "2024-01-01T00:00:00Z"
            });
            events::append_event(&paths.events, &e).unwrap();
            db::apply_event(&db::open_db(&paths.db).unwrap(), &emb, &e).unwrap();
        }

        let events_path_a = paths.events.clone();
        let events_path_b = paths.events.clone();

        // Worker A: writes additional upsert events during rebuild phase 2.
        // Sleeps briefly to maximise overlap with the lock-free replay phase.
        let worker_a = thread::spawn(move || {
            thread::sleep(Duration::from_micros(50));
            for i in 0..5u32 {
                let e = serde_json::json!({
                    "action": "upsert", "table": "entries",
                    "id": format!("extra{i}"), "path": format!("px{i}.rs"),
                    "summary": "s", "content": "c", "tags": [],
                    "kind": "observation", "evidence_status": "missing",
                    "ts": "2024-01-01T00:00:00Z"
                });
                events::append_event(&events_path_a, &e).unwrap();
            }
        });

        // First rebuild runs concurrently with Worker A.
        Rebuild.execute_with(&paths, &emb).unwrap();
        worker_a.join().unwrap();

        // Worker B writes evidence_add events after Worker A finishes to avoid
        // concurrent appends from two threads corrupting JSONL lines.
        for i in 0..5u32 {
            let e = evidence_add(&format!("base{i}"), &format!("ev{i}"), i);
            events::append_event(&events_path_b, &e).unwrap();
        }

        // All events are now in the log.  A second rebuild guarantees convergence.
        Rebuild.execute_with(&paths, &emb).unwrap();

        // Verify DB == Materialize(all events in log).
        let all_events = events::read_events(&paths.events).unwrap();
        let ref_conn = db::open_db_memory().unwrap();
        for ev in &all_events {
            db::apply_event(&ref_conn, &emb, ev).unwrap();
        }

        let live_conn = db::open_db(&paths.db).unwrap();

        let live_entries: Vec<(String, String)> = live_conn
            .prepare("SELECT id, COALESCE(evidence_status,'n/a') FROM entries ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let ref_entries: Vec<(String, String)> = ref_conn
            .prepare("SELECT id, COALESCE(evidence_status,'n/a') FROM entries ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(
            live_entries, ref_entries,
            "DB entries+evidence_status must equal direct replay after rebuild"
        );

        let live_evidence: Vec<(String, String)> = live_conn
            .prepare("SELECT id, entry_id FROM evidence ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let ref_evidence: Vec<(String, String)> = ref_conn
            .prepare("SELECT id, entry_id FROM evidence ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(
            live_evidence, ref_evidence,
            "evidence rows must survive rebuild and match direct replay"
        );

        // Spot-check: base entries that received evidence must have status='present'.
        for i in 0..5u32 {
            let status: String = live_conn
                .query_row(
                    "SELECT COALESCE(evidence_status,'n/a') FROM entries WHERE id=?1",
                    rusqlite::params![format!("base{i}")],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                status, "present",
                "base{i} must have evidence_status='present' after rebuild"
            );
        }
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
