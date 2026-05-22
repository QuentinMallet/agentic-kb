//! `compact` subcommand

use crate::commands::add::acquire_lock;
use crate::components::events;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;

/// Compact the event log (squash superseded events)
#[derive(Command, Debug, Parser)]
pub struct Compact;

impl Runnable for Compact {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Compact {
    /// Execute the compact command.
    pub fn execute(&self) -> anyhow::Result<()> {
        self.execute_with_paths(&config::Paths::discover()?)
    }

    /// Execute with explicit paths (for testing).
    pub fn execute_with_paths(&self, paths: &config::Paths) -> anyhow::Result<()> {
        let _lock = acquire_lock(&paths.lock)?;
        let evts = events::read_events(&paths.events)?;
        let original_count = evts.len();

        let mut entry_last: HashMap<String, usize> = HashMap::new();
        let mut test_last: HashMap<String, usize> = HashMap::new();
        let mut expire_ids: HashSet<String> = HashSet::new();
        let mut run_indices: Vec<usize> = Vec::new();

        for (i, ev) in evts.iter().enumerate() {
            let action = ev["action"].as_str().unwrap_or("");
            let table = ev["table"].as_str().unwrap_or("");
            let id = ev["id"].as_str().unwrap_or("").to_string();
            match (action, table) {
                ("upsert", "entries") => {
                    entry_last.insert(id, i);
                }
                ("expire", "entries") => {
                    expire_ids.insert(id);
                }
                ("upsert", "test_cases") => {
                    test_last.insert(id, i);
                }
                ("insert", "run_history") => {
                    run_indices.push(i);
                }
                _ => {}
            }
        }

        let mut compacted: Vec<serde_json::Value> = Vec::new();

        // Entry upserts (ordered by original position), with stale flag folded in.
        let mut entry_pairs: Vec<(usize, &str)> = entry_last
            .iter()
            .map(|(id, &i)| (i, id.as_str()))
            .collect();
        entry_pairs.sort_by_key(|(i, _)| *i);
        for (i, id) in entry_pairs {
            let mut ev = evts[i].clone();
            // Always fold is_stale when an expire event exists — even for permanent
            // entries. The permanent flag protects against `expire` without --force
            // at write time, but once --force is used and the expire event is in the
            // log, compact must honour it. Without this, force-expired permanent
            // entries would resurrect after compact+rebuild.
            if expire_ids.contains(id) {
                ev["is_stale"] = serde_json::json!(true);
            }
            compacted.push(ev);
        }

        // Test case upserts.
        let mut test_pairs: Vec<(usize, String)> =
            test_last.into_iter().map(|(id, i)| (i, id)).collect();
        test_pairs.sort_by_key(|(i, _)| *i);
        for (i, _) in test_pairs {
            compacted.push(evts[i].clone());
        }

        // Run history (all, original order).
        for i in run_indices {
            compacted.push(evts[i].clone());
        }

        let tmp = paths.events.with_extension("jsonl.tmp");
        {
            let mut f = File::create(&tmp)?;
            for ev in &compacted {
                writeln!(f, "{}", serde_json::to_string(ev)?)?;
            }
        }
        fs::rename(&tmp, &paths.events)?;

        println!(
            "compacted: {} events -> {}",
            original_count,
            compacted.len()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::events::append_event;
    use crate::config::Paths;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_cmd_compact_squashes_events() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        // 3 upserts for the same id — compact should keep only the last
        for i in 0..3 {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": "e1", "path": "a.rs", "summary": format!("v{i}"),
                "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
            });
            append_event(&paths.events, &ev).unwrap();
        }

        let before = events::read_events(&paths.events).unwrap();
        assert_eq!(before.len(), 3);

        let cmd = Compact;
        cmd.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["summary"], "v2");
    }

    #[test]
    fn test_cmd_compact_permanent_entry_with_force_expire_is_marked_stale() {
        // Regression test: force-expired permanent entries must have is_stale folded
        // in by compact so they stay gone after a subsequent rebuild.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        // Permanent entry upsert
        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "perm1", "path": "a.rs", "summary": "perm",
            "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z",
            "permanent": true
        });
        append_event(&paths.events, &upsert).unwrap();

        // Expire event — represents a `kb expire --force` call
        let expire = serde_json::json!({
            "action": "expire", "table": "entries",
            "id": "perm1", "ts": "2024-01-01T01:00:00Z"
        });
        append_event(&paths.events, &expire).unwrap();

        let cmd = Compact;
        cmd.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(after.len(), 1);
        // is_stale must be folded in — entry was force-expired and must not resurrect
        assert_eq!(after[0]["is_stale"], serde_json::json!(true));
    }

    #[test]
    fn test_cmd_compact_permanent_entry_without_expire_stays_alive() {
        // Permanent entries without any expire event must not be marked stale.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "perm2", "path": "b.rs", "summary": "perm",
            "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z",
            "permanent": true
        });
        append_event(&paths.events, &upsert).unwrap();

        let cmd = Compact;
        cmd.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0]["is_stale"].is_null() || after[0]["is_stale"] == false);
    }
}
