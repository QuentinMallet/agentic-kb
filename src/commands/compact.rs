//! `compact` subcommand

use crate::commands::add::acquire_lock;
use crate::components::events;
use crate::config::{self, VacuumConfig};
use abscissa_core::{Application, Command, Runnable};
use anyhow::Context;
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// Persistent state for the compact command (JSON-serialized alongside the event log).
///
/// Tracks `compacts_since_vacuum` across invocations so the AND-gated VACUUM trigger
/// fires only after the configured number of compact runs.
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct CompactState {
    compacts_since_vacuum: u64,
}

impl CompactState {
    /// Load from the state file, returning a default if the file is absent or unreadable.
    fn load(path: &std::path::Path) -> Self {
        if let Ok(data) = fs::read(path) {
            serde_json::from_slice(&data).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    /// Persist to the state file. Errors are non-fatal (VACUUM is optional).
    fn save(&self, path: &std::path::Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = fs::write(path, json);
        }
    }
}

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
        let paths = config::Paths::discover()?;
        let vacuum_cfg = crate::application::APP
            .config()
            .vacuum
            .clone()
            .unwrap_or_default();
        let (before, after) = self.execute_with_paths_and_vacuum(&paths, &vacuum_cfg)?;
        println!("compacted: {} events -> {}", before, after);
        Ok(())
    }

    /// Execute with explicit paths (for testing). Uses default VacuumConfig (threshold=8,
    /// floor=1024). Existing callers are unaffected.
    pub fn execute_with_paths(&self, paths: &config::Paths) -> anyhow::Result<(usize, usize)> {
        self.execute_with_paths_and_vacuum(paths, &VacuumConfig::default())
    }

    /// Execute with explicit paths and vacuum config (primary testable entry point).
    ///
    /// Constraint: VACUUM is invoked AFTER the atomic rename so a crash during VACUUM
    /// still leaves the compacted DB (already in place) intact and readable.
    /// Constraint: existing jsonl-purge invariants (br-8sw) must hold.
    pub fn execute_with_paths_and_vacuum(
        &self,
        paths: &config::Paths,
        vacuum_cfg: &VacuumConfig,
    ) -> anyhow::Result<(usize, usize)> {
        let _lock = acquire_lock(&paths.lock)?;
        let read = events::read_events(&paths.events)?;
        let original_count = read.events.len();
        if let Some(torn_tail) = &read.torn_tail {
            let sidecar = events::preserve_torn_tail(&paths.events, &torn_tail.bytes)?;
            eprintln!(
                "compact: WARNING preserved torn final line {} ({} bytes) to {} before rewriting {}",
                torn_tail.line,
                torn_tail.bytes.len(),
                sidecar.display(),
                paths.events.display()
            );
        }
        let evts = read.events;

        let mut entry_last: HashMap<String, usize> = HashMap::new();
        let mut test_last: HashMap<String, usize> = HashMap::new();
        // Track index of the LAST expire per id (not just membership) so that
        // upsert→expire→upsert sequences honour last-write-wins (br-joj).
        let mut expire_last: HashMap<String, usize> = HashMap::new();
        let mut live_entry_ids: HashSet<String> = HashSet::new();
        let mut run_indices: Vec<usize> = Vec::new();
        let mut evidence_indices: Vec<usize> = Vec::new();
        let mut evidence_live_at_index: HashSet<usize> = HashSet::new();
        let mut effective_evidence_add_indices: HashSet<usize> = HashSet::new();
        let mut evidence_owner_by_id: HashMap<String, String> = HashMap::new();
        let mut live_at_cursor: HashSet<String> = HashSet::new();

        for (i, ev) in evts.iter().enumerate() {
            let action = ev["action"].as_str().unwrap_or("");
            let table = ev["table"].as_str().unwrap_or("");
            let id = ev["id"].as_str().unwrap_or("").to_string();
            // Warn on structurally malformed events (missing or non-string action/table).
            // These differ from well-formed events compact may still choose to drop.
            if !ev["action"].is_string() || !ev["table"].is_string() {
                eprintln!("compact: warn: event at index {i} missing valid action/table, dropped");
            }
            match (action, table) {
                ("upsert", "entries") => {
                    entry_last.insert(id.clone(), i);
                    if ev["is_stale"].as_bool().unwrap_or(false) {
                        live_at_cursor.remove(&id);
                        evidence_owner_by_id.retain(|_, owner| owner != &id);
                    } else {
                        live_at_cursor.insert(id);
                    }
                }
                ("expire", "entries") => {
                    expire_last.insert(id.clone(), i);
                    live_at_cursor.remove(&id);
                    evidence_owner_by_id.retain(|_, owner| owner != &id);
                }
                ("upsert", "test_cases") => {
                    test_last.insert(id, i);
                }
                ("insert", "run_history") => {
                    run_indices.push(i);
                }
                ("evidence_add", "evidence")
                | ("citation_healed", "evidence")
                | ("evidence_expire", "evidence") => {
                    evidence_indices.push(i);
                    let live_parent = ev["entry_id"]
                        .as_str()
                        .filter(|entry_id| live_at_cursor.contains(*entry_id));
                    if live_parent.is_some() {
                        evidence_live_at_index.insert(i);
                    }
                    match action {
                        "evidence_add" => {
                            if let (Some(entry_id), Some(evidence_id)) =
                                (live_parent, ev["evidence"]["id"].as_str())
                            {
                                if !evidence_owner_by_id.contains_key(evidence_id) {
                                    evidence_owner_by_id
                                        .insert(evidence_id.to_string(), entry_id.to_string());
                                    effective_evidence_add_indices.insert(i);
                                }
                            }
                        }
                        "evidence_expire" => {
                            if let (Some(entry_id), Some(evidence_id)) =
                                (live_parent, ev["evidence_id"].as_str())
                            {
                                if evidence_owner_by_id
                                    .get(evidence_id)
                                    .is_some_and(|owner| owner == entry_id)
                                {
                                    evidence_owner_by_id.remove(evidence_id);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        let mut retained_indices: Vec<usize> = Vec::new();

        // Entry upserts ordered by original position. Drop entries whose last
        // expire comes after their last upsert, or whose last upsert is itself stale
        // (absent == stale for all query paths).
        // Orphan expire events (expire with no matching upsert in this log) are
        // implicitly dropped — they never appear in entry_pairs, so they are never
        // emitted. This is safe: absent == stale, so rebuild from the compacted log
        // produces identical search-visible state.
        let mut entry_pairs: Vec<(usize, &str)> =
            entry_last.iter().map(|(id, &i)| (i, id.as_str())).collect();
        entry_pairs.sort_by_key(|&(i, _)| i);
        for (i, id) in entry_pairs {
            if evts[i]["is_stale"].as_bool().unwrap_or(false)
                || expire_last.get(id).is_some_and(|&e| e > i)
            {
                continue;
            }
            live_entry_ids.insert(id.to_string());
            retained_indices.push(i);
        }

        // Test case upserts.
        let mut test_pairs: Vec<(usize, String)> =
            test_last.into_iter().map(|(id, i)| (i, id)).collect();
        test_pairs.sort_by_key(|&(i, _)| i);
        for (i, _) in test_pairs {
            retained_indices.push(i);
        }

        // Run history: every run event is retained (D5.2 — the positional cap
        // is removed outright, not shrunk). A cap made compaction NOT a
        // materialization-preserving rewrite: DB rows for capped-away run
        // events survived (apply_event's INSERT already ran) while the log
        // could no longer reproduce them, so a rebuild silently diverged from
        // a live DB (CompactMaterialize.tla CE5 / Critical 4). Retention here
        // no longer bounds `run_history` growth; T3's keyed insertion (D5.1)
        // bounds duplication instead.
        for i in run_indices {
            retained_indices.push(i);
        }

        // Evidence events are retained verbatim for live parent entries, but an
        // entry expire is an evidence-GC boundary: evidence from before the last
        // expire must not be attached to a later re-upsert of the same entry.
        // Require the parent to be live immediately before the evidence event,
        // exactly LiveAtIdx in AgentKbEvidence.tla. This drops orphan events both
        // before the first upsert and between expire and revival. The old explicit
        // first-upsert bound is redundant because LiveAtIdx implies one precedes i.
        let mut evidence_by_entry: HashMap<String, Vec<usize>> = HashMap::new();
        for i in evidence_indices {
            let ev = &evts[i];
            let entry_id = ev["entry_id"].as_str().unwrap_or("");
            // Evidence ids have current-state ownership during replay. Only adds
            // that claimed an unowned id while their parent was live are effective;
            // evidence/entry expiry can free an id for a later effective add.
            let is_effective_add =
                ev["action"] != "evidence_add" || effective_evidence_add_indices.contains(&i);
            if live_entry_ids.contains(entry_id)
                && evidence_live_at_index.contains(&i)
                && is_effective_add
                && expire_last
                    .get(entry_id)
                    .map_or(true, |&expire_i| i > expire_i)
            {
                evidence_by_entry
                    .entry(entry_id.to_string())
                    .or_default()
                    .push(i);
            }
        }

        retained_indices.sort_unstable();
        let mut compacted: Vec<serde_json::Value> = Vec::new();
        for i in retained_indices {
            let ev = &evts[i];
            compacted.push(ev.clone());
            if ev["action"] == "upsert" && ev["table"] == "entries" {
                if let Some(entry_id) = ev["id"].as_str() {
                    if let Some(indices) = evidence_by_entry.remove(entry_id) {
                        compacted.extend(
                            indices
                                .into_iter()
                                .map(|evidence_i| evts[evidence_i].clone()),
                        );
                    }
                }
            }
        }

        let tmp = paths.events.with_extension("jsonl.tmp");
        {
            let mut f = File::create(&tmp)?;
            for ev in &compacted {
                writeln!(f, "{}", serde_json::to_string(ev)?)?;
            }
            f.sync_data()?; // flush pages before rename to prevent truncation on crash
        }
        fs::rename(&tmp, &paths.events)?;

        // Optional VACUUM: fires AFTER the atomic rename so a crash during VACUUM
        // still leaves the compacted DB (already renamed into place) intact and readable.
        maybe_vacuum_after_compact(&paths.db, &paths.compact_state, vacuum_cfg)?;

        Ok((original_count, compacted.len()))
    }
}

/// Run VACUUM if both conditions hold:
/// 1. `compacts_since_vacuum` has reached `vacuum_cfg.vacuum_after_compacts`.
/// 2. SQLite `freelist_count` is at least `vacuum_cfg.vacuum_min_free_pages`.
///
/// The counter is incremented on every compact run. It resets to 0 only after a
/// successful VACUUM. When the floor is not met (condition 2 false but condition 1
/// true), the counter is saved at its current (>=threshold) value so the next compact
/// will re-evaluate the floor.
///
/// Non-fatal: the compact result (renamed JSONL) is already committed before this runs.
fn maybe_vacuum_after_compact(
    db_path: &std::path::Path,
    state_path: &std::path::Path,
    vacuum_cfg: &VacuumConfig,
) -> anyhow::Result<()> {
    // Load persisted counter (default 0 if absent).
    let mut state = CompactState::load(state_path);

    // Increment: this compact run counts toward the next VACUUM.
    state.compacts_since_vacuum = state.compacts_since_vacuum.saturating_add(1);

    // Gate 1: counter must reach the threshold.
    if state.compacts_since_vacuum < vacuum_cfg.vacuum_after_compacts {
        state.save(state_path);
        return Ok(());
    }

    // Gate 2: DB must have enough free pages to reclaim.
    // Only open the DB if the counter gate passed (avoids the open cost on most runs).
    if !db_path.exists() {
        state.save(state_path);
        return Ok(());
    }
    let conn = crate::components::db::open_db(db_path)?;
    let freelist_count: i64 =
        conn.query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0))?;

    if freelist_count < vacuum_cfg.vacuum_min_free_pages as i64 {
        // Floor not met: save counter as-is (>= threshold) so the next compact
        // re-evaluates the floor without losing the count.
        state.save(state_path);
        return Ok(());
    }

    // Both gates passed: run VACUUM and reset counter.
    conn.execute("VACUUM", [])?;
    state.compacts_since_vacuum = 0;
    state.save(state_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::events::append_event;
    use crate::config::{Paths, VacuumConfig};
    use proptest::strategy::Strategy;
    use std::env;
    use std::fs;
    use tempfile::tempdir;

    const FAST_PROPTEST_CASES: u32 = 16;

    fn proptest_cases(default_full: u32) -> u32 {
        env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(FAST_PROPTEST_CASES.min(default_full))
    }

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
        assert_eq!(before.events.len(), 3);

        let cmd = Compact;
        cmd.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(after.events.len(), 1);
        assert_eq!(after.events[0]["summary"], "v2");
    }

    #[test]
    fn test_cmd_compact_force_expired_entry_dropped() {
        // Force-expired entries must be dropped entirely from the compacted log.
        // A rebuild that never sees the upsert produces the same search-visible
        // state as one that sees it with is_stale=true (entry absent in both cases).
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "perm1", "path": "a.rs", "summary": "perm",
            "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z",
            "permanent": true
        });
        append_event(&paths.events, &upsert).unwrap();

        let expire = serde_json::json!({
            "action": "expire", "table": "entries",
            "id": "perm1", "ts": "2024-01-01T01:00:00Z"
        });
        append_event(&paths.events, &expire).unwrap();

        Compact.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(
            after.events.len(),
            0,
            "force-expired entry must be purged from the log"
        );
    }

    #[test]
    fn test_cmd_compact_re_upsert_after_expire_stays_alive() {
        // Regression test for br-joj: re-upsert after expire must NOT be
        // marked stale by compact. Discovered by br-h7c proptest. The flat
        // expire_ids HashSet lost ordering, so the re-upsert inherited
        // is_stale=true even though it superseded the expire.
        //
        // Sequence: upsert("b") → expire("b") → upsert("b") → compact
        // Pre-compact materialized state: b is live (is_stale=0).
        // Post-compact rebuild must yield the same: is_stale=0.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let upsert1 = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "b", "path": "src/b.rs", "summary": "v1",
            "content": "c1", "tags": [], "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &upsert1).unwrap();

        let expire = serde_json::json!({
            "action": "expire", "table": "entries",
            "id": "b", "ts": "2024-01-01T01:00:00Z"
        });
        append_event(&paths.events, &expire).unwrap();

        let upsert2 = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "b", "path": "src/b.rs", "summary": "v2",
            "content": "c2", "tags": [], "ts": "2024-01-01T02:00:00Z"
        });
        append_event(&paths.events, &upsert2).unwrap();

        let cmd = Compact;
        cmd.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(
            after.events.len(),
            1,
            "compact must produce exactly one entry event"
        );
        // The compact must emit the LATEST upsert (v2) and must NOT mark stale.
        assert_eq!(
            after.events[0]["summary"], "v2",
            "compact must keep the last upsert content"
        );
        assert!(
            after.events[0]["is_stale"].is_null() || after.events[0]["is_stale"] == false,
            "re-upsert after expire must NOT be marked stale (br-joj)"
        );
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
        assert_eq!(after.events.len(), 1);
        assert!(after.events[0]["is_stale"].is_null() || after.events[0]["is_stale"] == false);
    }

    #[test]
    fn test_cmd_compact_stale_entries_purged_from_log() {
        // Expired entries must not appear in the compacted log at all.
        // A rebuild from the compacted log must produce no entry for them
        // (absent == stale for all search paths).
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        // One live entry, one expired entry.
        for id in ["live", "dead"] {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": id, "path": format!("src/{id}.rs"), "summary": id,
                "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
            });
            append_event(&paths.events, &ev).unwrap();
        }
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "expire", "table": "entries",
                "id": "dead", "ts": "2024-01-01T01:00:00Z"
            }),
        )
        .unwrap();

        Compact.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(after.events.len(), 1, "only live entry must remain in log");
        assert_eq!(after.events[0]["id"], "live");
    }

    #[test]
    fn test_cmd_compact_run_history_uncapped_all_retained() {
        // D5.2: the positional retention cap is removed outright, not
        // shrunk. A cap made compaction NOT a materialization-preserving
        // rewrite: DB rows for capped-away run events survived (apply_event
        // already ran) while the log could no longer reproduce them
        // (CompactMaterialize.tla CE5 / Critical 4). Well past the old
        // RUN_HISTORY_CAP (500), every run event must still survive.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        const OVER_OLD_CAP: usize = 550;
        for i in 0..OVER_OLD_CAP {
            let ev = serde_json::json!({
                "action": "insert", "table": "run_history",
                "test_id": format!("{i}"), "result": "pass",
                "ts": "2024-01-01T00:00:00Z", "run_id": format!("run-{i}")
            });
            append_event(&paths.events, &ev).unwrap();
        }

        Compact.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(
            after.events.len(),
            OVER_OLD_CAP,
            "compaction must retain every run_history event, not a positional tail"
        );
        assert_eq!(
            after.events[0]["test_id"], "0",
            "the oldest event, which the old cap would have trimmed, must survive"
        );
        assert_eq!(
            after.events[OVER_OLD_CAP - 1]["test_id"],
            format!("{}", OVER_OLD_CAP - 1),
            "the newest event must survive"
        );
    }

    #[test]
    fn test_cmd_compact_run_history_materialization_preserving() {
        // CompactMaterialize.tla run 10 (Safety_Current), the direct
        // statement of CE5: compaction must not change what the log
        // materializes to. A test that only counts post-compaction rows
        // passes under the old cap and must not be accepted as coverage —
        // this rebuilds from both logs and compares run_history row-for-row.
        // Both rebuilds replay through `open_db_memory` (foreign_keys=ON),
        // so this also exercises the run_history -> test_cases FK: it must
        // still hold after cap removal retains older run events.
        use crate::components::db::{apply_event, open_db_memory};
        use crate::components::embedder::NoopEmbedder;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let test_case = serde_json::json!({
            "action": "upsert", "table": "test_cases",
            "id": "t1", "app": "kb", "name": "n", "protocol": "rust_tool",
            "config": "{}", "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &test_case).unwrap();

        const RUNS: usize = 501; // old RUN_HISTORY_CAP (500) + 1
        for i in 0..RUNS {
            let ev = serde_json::json!({
                "action": "insert", "table": "run_history",
                "test_id": "t1", "result": "pass",
                "ts": "2024-01-01T00:00:00Z", "run_id": format!("run-{i}")
            });
            append_event(&paths.events, &ev).unwrap();
        }

        let embedder = NoopEmbedder;
        let pre_compaction_log = events::read_events(&paths.events).unwrap().events;
        let pre_compaction_db = open_db_memory().unwrap();
        for ev in &pre_compaction_log {
            apply_event(&pre_compaction_db, &embedder, ev).unwrap();
        }

        Compact.execute_with_paths(&paths).unwrap();

        let compacted_log = events::read_events(&paths.events).unwrap().events;
        let compacted_db = open_db_memory().unwrap();
        for ev in &compacted_log {
            apply_event(&compacted_db, &embedder, ev).unwrap();
        }

        fn dump(conn: &rusqlite::Connection) -> Vec<(String, String, Option<String>)> {
            conn.prepare("SELECT test_id, result, run_id FROM run_history ORDER BY run_id")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        }

        let pre = dump(&pre_compaction_db);
        let post = dump(&compacted_db);
        assert_eq!(
            pre.len(),
            RUNS,
            "rebuild from the pre-compaction log must materialize every run"
        );
        assert_eq!(
            pre, post,
            "compaction must be materialization-preserving: rebuild from the \
             compacted log must equal rebuild from the pre-compaction log"
        );
    }

    #[test]
    fn test_cmd_compact_preserves_healed_citation_paths_through_rebuild() {
        use crate::components::db::{apply_event, open_db_memory};
        use crate::components::embedder::NoopEmbedder;
        use crate::components::events::{citation_healed_event, evidence_add_event};
        use crate::models::Evidence;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "e1", "path": "src/a.rs", "summary": "entry",
            "content": "c", "tags": [], "kind": "observation",
            "evidence_status": "present", "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &upsert).unwrap();

        let evidence = Evidence {
            id: "ev-1".to_string(),
            entry_id: "e1".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/old.rs:0-66".to_string()),
            citation_sha: None,
            citation_hash: "sha256:abc".to_string(),
            citation_excerpt: Some("strong excerpt".to_string()),
            derived_from: None,
            recorded_at: Some("2026-01-01T00:00:00Z".to_string()),
        };
        append_event(
            &paths.events,
            &evidence_add_event("e1", &evidence, Some("deadbeef")),
        )
        .unwrap();
        append_event(
            &paths.events,
            &citation_healed_event(
                "e1",
                "ev-1",
                "src/old.rs:0-66",
                "src/new.rs:11-77",
                "sha256:abc",
                Some("cafebabe"),
            ),
        )
        .unwrap();

        Compact.execute_with_paths(&paths).unwrap();

        let replay = open_db_memory().unwrap();
        let embedder = NoopEmbedder;
        for ev in events::read_events(&paths.events).unwrap().events {
            apply_event(&replay, &embedder, &ev).unwrap();
        }

        let citation_path: String = replay
            .query_row(
                "SELECT citation_path FROM evidence WHERE id='ev-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(citation_path, "src/new.rs:11-77");
    }

    #[test]
    fn test_cmd_compact_preserves_evidence_after_trailing_reupsert() {
        use crate::components::db::{apply_event, open_db_memory};
        use crate::components::embedder::NoopEmbedder;
        use crate::components::events::evidence_add_event;
        use crate::models::Evidence;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries", "id": "e1",
            "path": "src/a.rs", "summary": "entry", "content": "c",
            "tags": [], "kind": "observation", "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &upsert).unwrap();
        let evidence = Evidence {
            id: "ev-1".to_string(),
            entry_id: "e1".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/a.rs:0-1".to_string()),
            citation_sha: None,
            citation_hash: "sha256:abc".to_string(),
            citation_excerpt: None,
            derived_from: None,
            recorded_at: Some("2024-01-01T00:00:01Z".to_string()),
        };
        append_event(&paths.events, &evidence_add_event("e1", &evidence, None)).unwrap();
        append_event(&paths.events, &upsert).unwrap();

        Compact.execute_with_paths(&paths).unwrap();

        let replay = open_db_memory().unwrap();
        let embedder = NoopEmbedder;
        for ev in events::read_events(&paths.events).unwrap().events {
            apply_event(&replay, &embedder, &ev).unwrap();
        }
        let evidence_count: i64 = replay
            .query_row(
                "SELECT COUNT(*) FROM evidence WHERE entry_id='e1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let evidence_status: String = replay
            .query_row(
                "SELECT evidence_status FROM entries WHERE id='e1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(evidence_count, 1);
        assert_eq!(evidence_status, "present");
    }

    #[test]
    fn test_cmd_compact_drops_evidence_before_last_entry_expire() {
        use crate::components::events::evidence_add_event;
        use crate::models::Evidence;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries", "id": "e1", "path": "src/a.rs",
            "summary": "entry", "content": "c", "tags": [], "kind": "observation",
            "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &upsert).unwrap();
        let evidence = Evidence {
            id: "ev-1".to_string(),
            entry_id: "e1".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/a.rs:0-1".to_string()),
            citation_sha: None,
            citation_hash: "sha256:abc".to_string(),
            citation_excerpt: None,
            derived_from: None,
            recorded_at: None,
        };
        append_event(&paths.events, &evidence_add_event("e1", &evidence, None)).unwrap();
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "expire", "table": "entries", "id": "e1", "ts": "2024-01-01T00:00:02Z"
            }),
        )
        .unwrap();
        append_event(&paths.events, &upsert).unwrap();

        Compact.execute_with_paths(&paths).unwrap();
        let compacted = events::read_events(&paths.events).unwrap().events;
        assert!(compacted
            .iter()
            .all(|ev| ev["table"] != "evidence" || ev["entry_id"] != "e1"));
    }

    #[test]
    fn test_cmd_compact_drops_evidence_expire_before_first_upsert() {
        use crate::components::events::evidence_expire_event;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        append_event(
            &paths.events,
            &evidence_expire_event("d", "ev-d", "orphan regression"),
        )
        .unwrap();
        for id in ["d", "a"] {
            append_event(
                &paths.events,
                &serde_json::json!({
                    "action": "upsert", "table": "entries", "id": id,
                    "path": format!("src/{id}.rs"), "summary": "legacy",
                    "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
                }),
            )
            .unwrap();
        }

        let original = materialize(&paths);
        assert_eq!(
            original
                .iter()
                .find(|(id, _, _)| id == "d")
                .unwrap()
                .1
                .as_str(),
            "n/a"
        );

        Compact.execute_with_paths(&paths).unwrap();

        let compacted = events::read_events(&paths.events).unwrap().events;
        assert!(compacted
            .iter()
            .all(|ev| ev["table"] != "evidence" || ev["entry_id"] != "d"));
        let replayed = materialize(&paths);
        assert_eq!(replayed, original);
    }

    #[test]
    fn test_cmd_compact_drops_evidence_add_before_first_upsert() {
        use crate::components::events::evidence_add_event;
        use crate::models::Evidence;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let evidence = Evidence {
            id: "ev-d".to_string(),
            entry_id: "d".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/d.rs:0-1".to_string()),
            citation_sha: None,
            citation_hash: "sha256:abc".to_string(),
            citation_excerpt: None,
            derived_from: None,
            recorded_at: None,
        };
        append_event(&paths.events, &evidence_add_event("d", &evidence, None)).unwrap();
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "upsert", "table": "entries", "id": "d",
                "path": "src/d.rs", "summary": "legacy", "content": "c",
                "tags": [], "ts": "2024-01-01T00:00:00Z"
            }),
        )
        .unwrap();

        Compact.execute_with_paths(&paths).unwrap();

        let compacted = events::read_events(&paths.events).unwrap().events;
        assert!(compacted
            .iter()
            .all(|ev| ev["table"] != "evidence" || ev["entry_id"] != "d"));
    }

    #[test]
    fn test_cmd_compact_ce5_stale_window_evidence_converges() {
        use crate::components::events::evidence_add_event;
        use crate::models::Evidence;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries", "id": "d",
            "path": "src/d.rs", "summary": "entry", "content": "c",
            "tags": [], "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &upsert).unwrap();
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "expire", "table": "entries", "id": "d",
                "ts": "2024-01-01T00:00:01Z"
            }),
        )
        .unwrap();
        let evidence = Evidence {
            id: "ev-d".to_string(),
            entry_id: "d".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/d.rs:0-1".to_string()),
            citation_sha: None,
            citation_hash: "sha256:abc".to_string(),
            citation_excerpt: None,
            derived_from: None,
            recorded_at: None,
        };
        append_event(&paths.events, &evidence_add_event("d", &evidence, None)).unwrap();
        append_event(&paths.events, &upsert).unwrap();

        let original = materialize(&paths);
        Compact.execute_with_paths(&paths).unwrap();

        let compacted = events::read_events(&paths.events).unwrap().events;
        assert!(compacted.iter().all(|ev| {
            ev["action"] != "evidence_add" || ev["table"] != "evidence" || ev["entry_id"] != "d"
        }));
        assert_eq!(materialize(&paths), original);
    }

    #[test]
    fn test_cmd_compact_evidence_fixpoint() {
        use crate::components::events::{citation_healed_event, evidence_add_event};
        use crate::models::Evidence;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "upsert", "table": "entries", "id": "e1", "path": "src/a.rs",
                "summary": "entry", "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
            }),
        )
        .unwrap();
        let evidence = Evidence {
            id: "ev-1".to_string(),
            entry_id: "e1".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/a.rs:0-1".to_string()),
            citation_sha: None,
            citation_hash: "sha256:abc".to_string(),
            citation_excerpt: None,
            derived_from: None,
            recorded_at: None,
        };
        append_event(&paths.events, &evidence_add_event("e1", &evidence, None)).unwrap();
        append_event(
            &paths.events,
            &citation_healed_event(
                "e1",
                "ev-1",
                "src/a.rs:0-1",
                "src/a.rs:2-3",
                "sha256:abc",
                None,
            ),
        )
        .unwrap();

        Compact.execute_with_paths(&paths).unwrap();
        let once = fs::read(&paths.events).unwrap();
        Compact.execute_with_paths(&paths).unwrap();
        assert_eq!(fs::read(&paths.events).unwrap(), once);
    }

    #[test]
    fn test_cmd_compact_does_not_transfer_cross_parent_evidence_id_ownership() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        for id in ["owner-a", "owner-b"] {
            append_event(
                &paths.events,
                &serde_json::json!({
                    "action": "upsert", "table": "entries", "id": id,
                    "path": format!("src/{id}.rs"), "summary": id, "content": "c",
                    "tags": [], "kind": "belief", "evidence_status": "missing",
                    "ts": "2024-01-01T00:00:00Z"
                }),
            )
            .unwrap();
            append_event(
                &paths.events,
                &serde_json::json!({
                    "action": "evidence_add", "table": "evidence", "entry_id": id,
                    "evidence": {
                        "id": "shared-evidence-id", "entry_id": id, "kind": "code",
                        "citation_path": format!("src/{id}.rs:1-1"), "citation_sha": null,
                        "citation_hash": "sha256:abc", "citation_excerpt": null,
                        "derived_from": null, "recorded_at": null
                    },
                    "ts": "2024-01-01T01:00:00Z"
                }),
            )
            .unwrap();
        }
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "expire", "table": "entries", "id": "owner-a",
                "ts": "2024-01-02T00:00:00Z"
            }),
        )
        .unwrap();

        let before = materialize(&paths);
        Compact.execute_with_paths(&paths).unwrap();
        let after = materialize(&paths);
        assert_eq!(before, after);
        let owner_b = after.iter().find(|(id, _, _)| id == "owner-b").unwrap();
        assert!(owner_b.2.is_empty());
        assert!(events::read_events(&paths.events)
            .unwrap()
            .events
            .iter()
            .all(|event| {
                event["action"] != "evidence_add" || event["evidence"]["id"] != "shared-evidence-id"
            }));
    }

    #[test]
    fn test_cmd_compact_retains_readded_evidence_after_evidence_expire() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        for id in ["a", "d"] {
            append_event(
                &paths.events,
                &serde_json::json!({
                    "action": "upsert", "table": "entries", "id": id,
                    "path": format!("src/{id}.rs"), "summary": id, "content": "c",
                    "tags": [], "kind": "belief", "evidence_status": "missing"
                }),
            )
            .unwrap();
        }
        for id in ["a", "d"] {
            append_event(
                &paths.events,
                &serde_json::json!({
                    "action": "evidence_add", "table": "evidence", "entry_id": id,
                    "evidence": {"id": "ev-shared-0", "entry_id": id, "kind": "code",
                        "citation_path": format!("src/{id}.rs:0-1"),
                        "citation_hash": "sha256:shared"}
                }),
            )
            .unwrap();
        }
        let evidence_add = serde_json::json!({
            "action": "evidence_add", "table": "evidence", "entry_id": "d",
            "evidence": {"id": "ev-d-1", "entry_id": "d", "kind": "code",
                "citation_path": "src/d.rs:0-1", "citation_hash": "sha256:d"}
        });
        append_event(&paths.events, &evidence_add).unwrap();
        append_event(
            &paths.events,
            &crate::components::events::evidence_expire_event("d", "ev-d-1", "replace"),
        )
        .unwrap();
        append_event(&paths.events, &evidence_add).unwrap();
        append_event(
            &paths.events,
            &crate::components::events::citation_healed_event(
                "a",
                "ev-shared-0",
                "src/a.rs:0-1",
                "src/a.rs:2-3",
                "sha256:shared",
                None,
            ),
        )
        .unwrap();

        let before = materialize(&paths);
        Compact.execute_with_paths(&paths).unwrap();
        let after = materialize(&paths);

        assert_eq!(after, before);
        let d = after.iter().find(|(id, _, _)| id == "d").unwrap();
        assert!(d.2.iter().any(|(id, _)| id == "ev-d-1"));
    }

    #[test]
    fn test_cmd_compact_allows_cross_parent_reclaim_after_owner_entry_expire() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        for id in ["owner-a", "owner-b"] {
            append_event(
                &paths.events,
                &serde_json::json!({
                    "action": "upsert", "table": "entries", "id": id,
                    "path": format!("src/{id}.rs"), "summary": id, "content": "c",
                    "tags": [], "kind": "belief", "evidence_status": "missing"
                }),
            )
            .unwrap();
        }
        for (action, entry_id) in [
            ("evidence_add", "owner-a"),
            ("expire", "owner-a"),
            ("evidence_add", "owner-b"),
        ] {
            let event = if action == "expire" {
                serde_json::json!({"action": "expire", "table": "entries", "id": entry_id})
            } else {
                serde_json::json!({
                    "action": "evidence_add", "table": "evidence", "entry_id": entry_id,
                    "evidence": {"id": "shared-evidence-id", "entry_id": entry_id,
                        "kind": "code", "citation_path": format!("src/{entry_id}.rs:0-1"),
                        "citation_hash": "sha256:shared"}
                })
            };
            append_event(&paths.events, &event).unwrap();
        }

        let before = materialize(&paths);
        Compact.execute_with_paths(&paths).unwrap();
        let after = materialize(&paths);

        assert_eq!(after, before);
        let owner_b = after.iter().find(|(id, _, _)| id == "owner-b").unwrap();
        assert!(owner_b.2.iter().any(|(id, _)| id == "shared-evidence-id"));
    }

    #[test]
    fn test_cmd_compact_orphan_expire_dropped() {
        // An expire event with no matching upsert in the log is an orphan. Compact
        // must drop it silently — absent == stale for all query paths. Two cases:
        //   (a) Pure orphan: expire arrives before any upsert for that id.
        //   (b) Post-compact orphan: a second expire arrives after a prior compact
        //       already purged the entry; the second compact must also produce an
        //       empty log.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        // Case (a): pure orphan expire.
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "expire", "table": "entries",
                "id": "ghost", "ts": "2024-01-01T00:00:00Z"
            }),
        )
        .unwrap();
        Compact.execute_with_paths(&paths).unwrap();
        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(after.events.len(), 0, "pure orphan expire must be dropped");

        // Case (b): post-compact orphan — another expire after log is empty.
        append_event(
            &paths.events,
            &serde_json::json!({
                "action": "expire", "table": "entries",
                "id": "ghost", "ts": "2024-01-01T01:00:00Z"
            }),
        )
        .unwrap();
        Compact.execute_with_paths(&paths).unwrap();
        let after2 = events::read_events(&paths.events).unwrap();
        assert_eq!(
            after2.events.len(),
            0,
            "post-compact orphan expire must also be dropped"
        );
    }

    #[test]
    fn test_cmd_compact_preserves_torn_tail_in_sidecar() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        fs::write(
            &paths.events,
            b"{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"live\",\"path\":\"src/live.rs\",\"summary\":\"live\",\"content\":\"c\",\"tags\":[],\"ts\":\"2024-01-01T00:00:00Z\"}\n{\"action\":",
        )
        .unwrap();

        Compact.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(after.events.len(), 1);
        assert!(after.torn_tail.is_none());
        assert_eq!(after.events[0]["id"], "live");

        let sidecars: Vec<_> = fs::read_dir(paths.events.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("agent-kb-events.jsonl.torn-"))
            })
            .collect();
        assert_eq!(sidecars.len(), 1, "compact must preserve torn bytes");
        assert_eq!(fs::read(&sidecars[0]).unwrap(), b"{\"action\":");
        let compacted_bytes = fs::read(&paths.events).unwrap();
        assert!(!compacted_bytes.ends_with(b"{\"action\":"));
    }

    // br-h7c: proptest target #4 — expire/compact state machine.
    //
    // Invariant: compact preserves the set of LIVE entries.
    //   ∀ event sequence S ∈ (Upsert | Expire | Compact)*:
    //     live_entries(rebuild(events_after_compact(S)))
    //       == live_entries(rebuild(events_pre_compact(S)))
    //
    // Stale (expired) entries are purged from the log by compact rather than
    // retained with is_stale=true, so the invariant is scoped to live state only.
    // Compact must not resurrect expired entries nor erase live ones.
    //
    // br-joj fixed: the generator is now UNRESTRICTED - re-upsert after
    // expire is a valid sequence and compact must honour last-write-wins.
    //
    // Fast tier defaults to 16 cases; export PROPTEST_CASES=256 for the
    // pre-merge full tier. Each case opens a tempdir and replays a small DB.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: proptest_cases(256),
            .. proptest::prelude::ProptestConfig::default()
        })]
        #[test]
        fn proptest_compact_preserves_live_state(
            ops in arb_compact_ops(),
        ) {
            let dir = tempdir().unwrap();
            let root = dir.path();
            fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
            let paths = Paths::from_root(root);

            for op in &ops {
                match op {
                    CompactOp::Upsert {
                        id,
                        kind,
                        is_stale,
                        evidence_status,
                    } => {
                        let mut ev = serde_json::json!({
                            "action": "upsert", "table": "entries",
                            "id": id, "path": format!("src/{id}.rs"),
                            "summary": format!("s {id}"), "content": format!("c {id}"),
                            "tags": [], "is_stale": is_stale,
                            "evidence_status": evidence_status,
                            "ts": "2024-01-01T00:00:00Z"
                        });
                        if let Some(kind) = kind {
                            ev["kind"] = serde_json::json!(kind);
                        }
                        append_event(&paths.events, &ev).unwrap();
                    }
                    CompactOp::Expire(id) => {
                        let ev = serde_json::json!({
                            "action": "expire", "table": "entries",
                            "id": id, "ts": "2024-01-01T01:00:00Z"
                        });
                        append_event(&paths.events, &ev).unwrap();
                    }
                    CompactOp::EvidenceAdd { id, evidence_slot } => {
                        append_event(&paths.events, &serde_json::json!({
                            "action": "evidence_add", "table": "evidence", "entry_id": id,
                            "evidence": {"id": format!("ev-{id}-{evidence_slot}"), "entry_id": id, "kind": "code",
                                "citation_path": format!("src/{id}.rs:0-1"), "citation_sha": null,
                                "citation_hash": "sha256:abc", "citation_excerpt": null,
                                "derived_from": null, "recorded_at": null},
                            "ts": "2024-01-01T02:00:00Z"
                        })).unwrap();
                    }
                    CompactOp::CrossParentEvidenceCollision {
                        first_id,
                        second_id,
                        evidence_slot,
                    } => {
                        for id in [first_id, second_id] {
                            append_event(&paths.events, &serde_json::json!({
                                "action": "upsert", "table": "entries", "id": id,
                                "path": format!("src/{id}.rs"), "summary": format!("s {id}"),
                                "content": format!("c {id}"), "tags": [], "kind": "belief",
                                "evidence_status": "missing", "ts": "2024-01-01T01:59:59Z"
                            })).unwrap();
                        }
                        for id in [first_id, second_id] {
                            append_event(&paths.events, &serde_json::json!({
                                "action": "evidence_add", "table": "evidence", "entry_id": id,
                                "evidence": {"id": format!("ev-shared-{evidence_slot}"), "entry_id": id, "kind": "code",
                                    "citation_path": format!("src/{id}.rs:0-1"), "citation_sha": null,
                                    "citation_hash": "sha256:abc", "citation_excerpt": null,
                                    "derived_from": null, "recorded_at": null},
                                "ts": "2024-01-01T02:00:00Z"
                            })).unwrap();
                        }
                    }
                    CompactOp::EvidenceExpire { id, evidence_slot } => {
                        append_event(&paths.events, &crate::components::events::evidence_expire_event(
                            id, &format!("ev-{id}-{evidence_slot}"), "property test"
                        )).unwrap();
                    }
                    CompactOp::CitationHealed { id, evidence_slot } => {
                        append_event(&paths.events, &crate::components::events::citation_healed_event(
                            id, &format!("ev-{id}-{evidence_slot}"), &format!("src/{id}.rs:0-1"),
                            &format!("src/{id}.rs:2-3"), "sha256:abc", None
                        )).unwrap();
                    }
                    CompactOp::Compact => {
                        let before = materialize(&paths);
                        Compact.execute_with_paths(&paths).unwrap();
                        let after = materialize(&paths);
                        proptest::prop_assert_eq!(
                            before, after,
                            "compact must preserve live entry state"
                        );
                    }
                }
            }

            // A trailing compact must also be a no-op on the live state.
            let before = materialize(&paths);
            Compact.execute_with_paths(&paths).unwrap();
            let after = materialize(&paths);
            proptest::prop_assert_eq!(before, after, "trailing compact must also no-op");
        }
    }

    #[derive(Debug, Clone)]
    enum CompactOp {
        Upsert {
            id: String,
            kind: Option<String>,
            is_stale: bool,
            evidence_status: String,
        },
        Expire(String),
        EvidenceAdd {
            id: String,
            evidence_slot: u8,
        },
        CrossParentEvidenceCollision {
            first_id: String,
            second_id: String,
            evidence_slot: u8,
        },
        EvidenceExpire {
            id: String,
            evidence_slot: u8,
        },
        CitationHealed {
            id: String,
            evidence_slot: u8,
        },
        Compact,
    }

    fn arb_compact_ops() -> impl proptest::strategy::Strategy<Value = Vec<CompactOp>> {
        proptest::collection::vec(arb_raw_op(), 0..32).prop_map(repair_writer_producible_ops)
    }

    /// Raw op generator — unrestricted alphabet across 4 ids.
    fn arb_raw_op() -> impl proptest::strategy::Strategy<Value = CompactOp> {
        use proptest::prelude::*;
        let arb_id = proptest::sample::select(vec!["a", "b", "c", "d"]).prop_map(|s| s.to_string());
        let arb_evidence = (arb_id.clone(), 0_u8..2);
        let arb_cross_parent_collision = (arb_id.clone(), arb_id.clone(), 0_u8..2)
            .prop_filter("collision parents must differ", |(first, second, _)| {
                first != second
            });
        let arb_kind = prop_oneof![
            Just(None),
            proptest::sample::select(vec!["belief", "convention"])
                .prop_map(|kind| Some(kind.to_string())),
        ];
        let arb_status = proptest::sample::select(vec!["present", "missing", "n/a"])
            .prop_map(|status| status.to_string());
        prop_oneof![
            (arb_id.clone(), arb_kind, Just(false), arb_status).prop_map(
                |(id, kind, is_stale, evidence_status)| CompactOp::Upsert {
                    id,
                    kind,
                    is_stale,
                    evidence_status,
                }
            ),
            arb_id.clone().prop_map(CompactOp::Expire),
            arb_evidence
                .clone()
                .prop_map(|(id, evidence_slot)| CompactOp::EvidenceAdd { id, evidence_slot }),
            arb_cross_parent_collision.prop_map(|(first_id, second_id, evidence_slot)| {
                CompactOp::CrossParentEvidenceCollision {
                    first_id,
                    second_id,
                    evidence_slot,
                }
            }),
            arb_evidence
                .clone()
                .prop_map(|(id, evidence_slot)| CompactOp::EvidenceExpire { id, evidence_slot }),
            arb_evidence
                .prop_map(|(id, evidence_slot)| CompactOp::CitationHealed { id, evidence_slot }),
            Just(CompactOp::Compact),
        ]
    }

    fn repair_writer_producible_ops(mut ops: Vec<CompactOp>) -> Vec<CompactOp> {
        // CE4 in .state/agent-kb/tla/T0-counterexample.md is an explicit-kind upsert
        // followed by a kindless legacy upsert of the same id. The spec closes that
        // counterexample with an alphabet guard on DoLegacyAdd: kb_core::add is the
        // only production writer and always emits kind, and kindless events only exist
        // in pre-Phase-0 history. This repair constrains the generator to those
        // writer-producible logs; it is not masking a bug on reachable executions.
        //
        // Likewise, is_stale=true upserts are writer-unproducible and corpus-absent:
        // their staleness is payload-borne, while compact models liveness only from
        // expire events. Compact therefore does not preserve materialization
        // equivalence for synthetic stale-upsert logs, and the replay branch remains
        // covered by dedicated unit tests in db.rs.
        let mut last_explicit_kind_by_id: HashMap<String, String> = HashMap::new();
        let mut collision_slot = 0_u8;
        for op in &mut ops {
            if let CompactOp::CrossParentEvidenceCollision {
                first_id,
                second_id,
                evidence_slot,
            } = op
            {
                // Collision events are deliberately writer-unproducible, but each
                // generated pair gets its own global id so the only duplication is
                // the intended two-parent collision within that operation.
                *evidence_slot = collision_slot;
                collision_slot = collision_slot.saturating_add(1);
                // This op carries implicit explicit-kind upserts for both parents
                // (the emitter always writes "kind": "belief" for first_id and
                // second_id) — register them so a later kindless Upsert of either
                // id gets repaired instead of recreating the CE4 shape.
                last_explicit_kind_by_id.insert(first_id.clone(), "belief".to_string());
                last_explicit_kind_by_id.insert(second_id.clone(), "belief".to_string());
            }
            if let CompactOp::Upsert { id, kind, .. } = op {
                match kind {
                    Some(explicit_kind) => {
                        last_explicit_kind_by_id.insert(id.clone(), explicit_kind.clone());
                    }
                    None => {
                        if let Some(explicit_kind) = last_explicit_kind_by_id.get(id).cloned() {
                            *kind = Some(explicit_kind);
                        }
                    }
                }
            }
        }
        ops
    }

    /// Replay the current event log into a fresh in-memory DB and return each
    /// live entry's ID, derived evidence status, and sorted evidence rows.
    fn materialize(paths: &Paths) -> Vec<(String, String, Vec<(String, String)>)> {
        use crate::components::{db, embedder::NoopEmbedder};

        let conn = db::open_db_memory().unwrap();
        let embedder = NoopEmbedder;
        let evts = events::read_events(&paths.events).unwrap();
        for ev in &evts.events {
            db::apply_event(&conn, &embedder, ev).unwrap();
        }

        let mut stmt = conn
            .prepare("SELECT id, evidence_status FROM entries WHERE COALESCE(is_stale, 0) = 0 ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        rows.map(|row| {
            let (id, status) = row.unwrap();
            let evidence = {
                let mut evidence_stmt = conn.prepare(
                    "SELECT id, COALESCE(citation_path, '') FROM evidence WHERE entry_id=?1 ORDER BY id"
                ).unwrap();
                evidence_stmt
                    .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            };
            (id, status, evidence)
        })
        .collect()
    }

    // ── VACUUM gate helpers ───────────────────────────────────────────────────

    /// Read `compacts_since_vacuum` from the compact state file (0 if absent).
    fn read_counter(paths: &Paths) -> u64 {
        CompactState::load(&paths.compact_state).compacts_since_vacuum
    }

    /// Read `freelist_count` from the DB at `paths.db` (0 if DB absent).
    fn freelist_count(paths: &Paths) -> i64 {
        if !paths.db.exists() {
            return 0;
        }
        let conn = crate::components::db::open_db(&paths.db).unwrap();
        conn.query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
    }

    // Full tier matches the documented AC fixture exactly: 12 000 entries at
    // ~400 bytes each. Fast tier trades entry count for padding size so the
    // same freelist-floor assertion holds with far fewer apply_event/
    // append_event calls (the dominant per-entry cost), not fewer bytes freed.
    const FULL_VACUUM_ENTRIES: usize = 12_000;
    const FULL_VACUUM_PADDING_BYTES: usize = 400;
    const FAST_VACUUM_ENTRIES: usize = 500;
    const FAST_VACUUM_PADDING_BYTES: usize = 16_000;

    fn vacuum_fixture_size() -> (usize, usize) {
        if env::var("PROPTEST_CASES").is_ok() {
            (FULL_VACUUM_ENTRIES, FULL_VACUUM_PADDING_BYTES)
        } else {
            (FAST_VACUUM_ENTRIES, FAST_VACUUM_PADDING_BYTES)
        }
    }

    /// Build a test root with a large DB so `freelist_count >= 1024` after expiry.
    ///
    /// Strategy: insert N entries (each with padded content) into the DB via
    /// apply_event, then expire them all. SQLite keeps the freed pages in its
    /// freelist until VACUUM reclaims them.
    fn setup_with_db(root: &std::path::Path) -> Paths {
        use crate::components::{db, embedder::NoopEmbedder};
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let conn = db::open_db(&paths.db).unwrap();
        let embedder = NoopEmbedder;

        // Insert N entries with padded content each so that expiring them all
        // leaves at least 1024 SQLite free pages (see vacuum_fixture_size).
        let (n_entries, padding_bytes) = vacuum_fixture_size();
        let padding: String = "x".repeat(padding_bytes);
        for i in 0..n_entries {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": format!("bulk{i}"),
                "path": format!("src/bulk{i}.rs"),
                "summary": format!("bulk entry {i}"),
                "content": format!("content {i} {padding}"),
                "tags": [],
                "ts": "2024-01-01T00:00:00Z"
            });
            db::apply_event(&conn, &embedder, &ev).unwrap();
            append_event(&paths.events, &ev).unwrap();
        }
        // Expire all entries to populate the SQLite freelist.
        for i in 0..n_entries {
            let ev = serde_json::json!({
                "action": "expire", "table": "entries",
                "id": format!("bulk{i}"),
                "ts": "2024-01-02T00:00:00Z"
            });
            db::apply_event(&conn, &embedder, &ev).unwrap();
            append_event(&paths.events, &ev).unwrap();
        }
        drop(conn); // close before compact opens it
        paths
    }

    // ── VACUUM gate tests (AC1–AC5) ──────────────────────────────────────────

    /// AC1 + AC4: after exactly 8 compacts with >=1024 free pages, VACUUM fires
    /// and the counter resets to 0.
    #[test]
    fn test_vacuum_fires_after_n_compacts_and_counter_resets() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let paths = setup_with_db(root);

        let vcfg = VacuumConfig {
            vacuum_after_compacts: 8,
            vacuum_min_free_pages: 1024,
        };

        // Runs 1-7: counter increments, no VACUUM yet.
        for n in 1..=7_u64 {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": format!("s{n}"), "path": "src/s.rs",
                "summary": "s", "content": "c", "tags": [],
                "ts": "2024-01-01T00:00:00Z"
            });
            append_event(&paths.events, &ev).unwrap();
            Compact
                .execute_with_paths_and_vacuum(&paths, &vcfg)
                .unwrap();
            assert_eq!(
                read_counter(&paths),
                n,
                "AC1: after {n} compacts, counter should be {n}"
            );
        }

        // Run 8: tips counter to 8 (>= threshold), VACUUM fires, counter resets to 0.
        let ev = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "s8", "path": "src/s.rs",
            "summary": "s", "content": "c", "tags": [],
            "ts": "2024-01-01T00:00:00Z"
        });
        append_event(&paths.events, &ev).unwrap();
        Compact
            .execute_with_paths_and_vacuum(&paths, &vcfg)
            .unwrap();

        assert_eq!(
            read_counter(&paths),
            0,
            "AC1+AC4: counter must reset to 0 after VACUUM fires on the 8th compact"
        );

        let conn = crate::components::db::open_db(&paths.db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert!(count >= 0, "AC1: DB must be readable after VACUUM");
    }

    /// AC2: after 7 compacts with >=1024 free pages, no VACUUM fires (counter stays at 7).
    #[test]
    fn test_vacuum_not_fired_before_threshold() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let paths = setup_with_db(root);

        let vcfg = VacuumConfig {
            vacuum_after_compacts: 8,
            vacuum_min_free_pages: 1024,
        };

        assert!(
            freelist_count(&paths) >= 1024,
            "AC2 precondition: >=1024 free pages"
        );

        for n in 1..=7_u64 {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": format!("s{n}"), "path": "src/s.rs",
                "summary": "s", "content": "c", "tags": [],
                "ts": "2024-01-01T00:00:00Z"
            });
            append_event(&paths.events, &ev).unwrap();
            Compact
                .execute_with_paths_and_vacuum(&paths, &vcfg)
                .unwrap();
        }

        assert_eq!(
            read_counter(&paths),
            7,
            "AC2: counter must be 7 after 7 compacts with threshold=8"
        );
    }

    /// AC3: after 8 compacts with freelist_count < 1024, VACUUM does not fire.
    #[test]
    fn test_vacuum_not_fired_when_freelist_below_floor() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        // Create a minimal DB: schema only, very few free pages (< 1024).
        {
            use crate::components::db;
            let _conn = db::open_db(&paths.db).unwrap();
        }
        let free = freelist_count(&paths);
        assert!(
            free < 1024,
            "AC3 precondition: minimal DB must have < 1024 free pages, got {free}"
        );

        let vcfg = VacuumConfig {
            vacuum_after_compacts: 8,
            vacuum_min_free_pages: 1024,
        };

        for n in 0..8_u64 {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": format!("s{n}"), "path": "src/s.rs",
                "summary": "s", "content": "c", "tags": [],
                "ts": "2024-01-01T00:00:00Z"
            });
            append_event(&paths.events, &ev).unwrap();
            Compact
                .execute_with_paths_and_vacuum(&paths, &vcfg)
                .unwrap();
        }

        assert_eq!(
            read_counter(&paths),
            8,
            "AC3: counter must be 8 (not reset) when freelist < floor"
        );
    }

    /// AC5: SIGKILL crash-safety.
    ///
    /// Spawn a subprocess that runs VACUUM on the DB, SIGKILL it mid-flight,
    /// then assert the DB is still readable. The atomic rename happened before VACUUM
    /// so the compacted DB is intact regardless of what VACUUM did before the kill.
    #[allow(unsafe_code)]
    #[test]
    #[cfg(unix)]
    fn test_sigkill_during_vacuum_leaves_db_readable() {
        use libc;
        use std::process::{Command, Stdio};

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let paths = setup_with_db(&root);

        assert!(
            freelist_count(&paths) >= 1024,
            "AC5 precondition: >=1024 free pages needed"
        );

        let db_path_str = paths.db.to_str().unwrap().to_string();

        // Spawn child running many VACUUM calls. Use sqlite3 if available, else python3.
        let child_result = Command::new("sqlite3")
            .arg(&db_path_str)
            .arg("VACUUM; VACUUM; VACUUM; VACUUM; VACUUM; VACUUM; VACUUM; VACUUM; VACUUM; VACUUM;")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child_result {
            Ok(c) => c,
            Err(_) => Command::new("python3")
                .args([
                    "-c",
                    &format!(
                        "import sqlite3; c=sqlite3.connect({db_path_str:?}); \
                         [c.execute('VACUUM') or c.commit() for _ in range(30)]"
                    ),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("AC5: neither sqlite3 nor python3 available for SIGKILL test"),
        };

        // Brief pause to let the child start VACUUM, then SIGKILL it.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let pid = child.id();
        // SAFETY: POSIX syscall; pid is a live child process PID obtained from spawn().
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = child.wait();

        // DB must still be openable and queryable after SIGKILL.
        let conn = crate::components::db::open_db(&paths.db)
            .expect("AC5: DB must be openable after SIGKILL during VACUUM");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .expect("AC5: SELECT COUNT(*) must succeed after SIGKILL");
        assert!(count >= 0, "AC5: DB readable after SIGKILL, count={count}");
    }
}
