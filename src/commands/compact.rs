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
use std::path::{Path, PathBuf};

/// Maximum number of `run_history` events to retain after compaction.
/// Older records beyond this tail are purged.
const RUN_HISTORY_CAP: usize = 500;

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
            let sidecar = preserve_torn_tail(&paths.events, torn_tail)?;
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
                    live_at_cursor.insert(id);
                }
                ("expire", "entries") => {
                    expire_last.insert(id.clone(), i);
                    live_at_cursor.remove(&id);
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
                    if ev["entry_id"]
                        .as_str()
                        .is_some_and(|entry_id| live_at_cursor.contains(entry_id))
                    {
                        evidence_live_at_index.insert(i);
                    }
                }
                _ => {}
            }
        }

        let mut retained_indices: Vec<usize> = Vec::new();

        // Entry upserts ordered by original position. Drop entries whose last
        // expire comes after their last upsert (absent == stale for all query paths).
        // Orphan expire events (expire with no matching upsert in this log) are
        // implicitly dropped — they never appear in entry_pairs, so they are never
        // emitted. This is safe: absent == stale, so rebuild from the compacted log
        // produces identical search-visible state.
        let mut entry_pairs: Vec<(usize, &str)> =
            entry_last.iter().map(|(id, &i)| (i, id.as_str())).collect();
        entry_pairs.sort_by_key(|&(i, _)| i);
        for (i, id) in entry_pairs {
            if expire_last.get(id).is_some_and(|&e| e > i) {
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

        // Run history: keep only the last RUN_HISTORY_CAP records (original order).
        let run_start = run_indices.len().saturating_sub(RUN_HISTORY_CAP);
        for i in run_indices[run_start..].iter().copied() {
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
            if live_entry_ids.contains(entry_id)
                && evidence_live_at_index.contains(&i)
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

fn preserve_torn_tail(events_path: &Path, torn_tail: &events::TornTail) -> anyhow::Result<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.fZ");
    let file_name = format!(
        "{}.torn-{}",
        events_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("events.jsonl"),
        stamp
    );
    let sidecar = events_path.with_file_name(file_name);
    fs::write(&sidecar, &torn_tail.bytes).with_context(|| {
        format!(
            "preserve torn tail from {} into {}",
            events_path.display(),
            sidecar.display()
        )
    })?;
    Ok(sidecar)
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
    fn test_cmd_compact_run_history_capped() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let over = RUN_HISTORY_CAP + 50;
        for i in 0..over {
            let ev = serde_json::json!({
                "action": "insert", "table": "run_history",
                // Encode insertion order in test_id so we can verify which
                // records survive: the LAST RUN_HISTORY_CAP (oldest 50 trimmed).
                "test_id": format!("{i}"), "result": "pass",
                "ts": "2024-01-01T00:00:00Z"
            });
            append_event(&paths.events, &ev).unwrap();
        }

        Compact.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(
            after.events.len(),
            RUN_HISTORY_CAP,
            "compact must retain at most RUN_HISTORY_CAP run_history events"
        );
        // Verify the LAST RUN_HISTORY_CAP records are kept (oldest 50 trimmed).
        assert_eq!(
            after.events[0]["test_id"], "50",
            "first retained must be event 50"
        );
        assert_eq!(
            after.events[RUN_HISTORY_CAP - 1]["test_id"],
            format!("{}", over - 1),
            "last retained must be the most recent event"
        );
    }

    #[test]
    fn test_cmd_compact_run_history_at_boundary_not_trimmed() {
        // At exactly RUN_HISTORY_CAP events, no records should be trimmed.
        // Guards against an off-by-one in the saturating_sub slice direction.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        for i in 0..RUN_HISTORY_CAP {
            let ev = serde_json::json!({
                "action": "insert", "table": "run_history",
                "test_id": format!("{i}"), "result": "pass",
                "ts": "2024-01-01T00:00:00Z"
            });
            append_event(&paths.events, &ev).unwrap();
        }

        Compact.execute_with_paths(&paths).unwrap();

        let after = events::read_events(&paths.events).unwrap();
        assert_eq!(
            after.events.len(),
            RUN_HISTORY_CAP,
            "exactly RUN_HISTORY_CAP events must not be trimmed"
        );
        assert_eq!(after.events[0]["test_id"], "0");
        assert_eq!(
            after.events[RUN_HISTORY_CAP - 1]["test_id"],
            format!("{}", RUN_HISTORY_CAP - 1)
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
            id: "ev-1".to_string(), entry_id: "e1".to_string(), kind: "code".to_string(),
            citation_path: Some("src/a.rs:0-1".to_string()), citation_sha: None,
            citation_hash: "sha256:abc".to_string(), citation_excerpt: None,
            derived_from: None, recorded_at: Some("2024-01-01T00:00:01Z".to_string()),
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
            .query_row("SELECT COUNT(*) FROM evidence WHERE entry_id='e1'", [], |row| row.get(0))
            .unwrap();
        let evidence_status: String = replay
            .query_row("SELECT evidence_status FROM entries WHERE id='e1'", [], |row| row.get(0))
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
            id: "ev-1".to_string(), entry_id: "e1".to_string(), kind: "code".to_string(),
            citation_path: Some("src/a.rs:0-1".to_string()), citation_sha: None,
            citation_hash: "sha256:abc".to_string(), citation_excerpt: None,
            derived_from: None, recorded_at: None,
        };
        append_event(&paths.events, &evidence_add_event("e1", &evidence, None)).unwrap();
        append_event(&paths.events, &serde_json::json!({
            "action": "expire", "table": "entries", "id": "e1", "ts": "2024-01-01T00:00:02Z"
        })).unwrap();
        append_event(&paths.events, &upsert).unwrap();

        Compact.execute_with_paths(&paths).unwrap();
        let compacted = events::read_events(&paths.events).unwrap().events;
        assert!(compacted.iter().all(|ev| ev["table"] != "evidence" || ev["entry_id"] != "e1"));
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
            original.iter().find(|(id, _, _)| id == "d").unwrap().1.as_str(),
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
            id: "ev-d".to_string(), entry_id: "d".to_string(), kind: "code".to_string(),
            citation_path: Some("src/d.rs:0-1".to_string()), citation_sha: None,
            citation_hash: "sha256:abc".to_string(), citation_excerpt: None,
            derived_from: None, recorded_at: None,
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
            id: "ev-d".to_string(), entry_id: "d".to_string(), kind: "code".to_string(),
            citation_path: Some("src/d.rs:0-1".to_string()), citation_sha: None,
            citation_hash: "sha256:abc".to_string(), citation_excerpt: None,
            derived_from: None, recorded_at: None,
        };
        append_event(&paths.events, &evidence_add_event("d", &evidence, None)).unwrap();
        append_event(&paths.events, &upsert).unwrap();

        let original = materialize(&paths);
        Compact.execute_with_paths(&paths).unwrap();

        let compacted = events::read_events(&paths.events).unwrap().events;
        assert!(compacted.iter().all(|ev| {
            ev["action"] != "evidence_add"
                || ev["table"] != "evidence"
                || ev["entry_id"] != "d"
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
        append_event(&paths.events, &serde_json::json!({
            "action": "upsert", "table": "entries", "id": "e1", "path": "src/a.rs",
            "summary": "entry", "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
        })).unwrap();
        let evidence = Evidence {
            id: "ev-1".to_string(), entry_id: "e1".to_string(), kind: "code".to_string(),
            citation_path: Some("src/a.rs:0-1".to_string()), citation_sha: None,
            citation_hash: "sha256:abc".to_string(), citation_excerpt: None,
            derived_from: None, recorded_at: None,
        };
        append_event(&paths.events, &evidence_add_event("e1", &evidence, None)).unwrap();
        append_event(&paths.events, &citation_healed_event("e1", "ev-1", "src/a.rs:0-1", "src/a.rs:2-3", "sha256:abc", None)).unwrap();

        Compact.execute_with_paths(&paths).unwrap();
        let once = fs::read(&paths.events).unwrap();
        Compact.execute_with_paths(&paths).unwrap();
        assert_eq!(fs::read(&paths.events).unwrap(), once);
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
    // br-joj fixed: the generator is now UNRESTRICTED — re-upsert after
    // expire is a valid sequence and compact must honour last-write-wins.
    //
    // PROPTEST_CASES default tuned to 256 — each case opens a tempdir +
    // replays a small DB. Override via PROPTEST_CASES env var.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 256,
            .. proptest::prelude::ProptestConfig::default()
        })]
        #[test]
        fn proptest_compact_preserves_live_state(
            ops in proptest::collection::vec(arb_raw_op(), 0..32),
        ) {
            let dir = tempdir().unwrap();
            let root = dir.path();
            fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
            let paths = Paths::from_root(root);

            for op in &ops {
                match op {
                    CompactOp::Upsert(id) => {
                        let ev = serde_json::json!({
                            "action": "upsert", "table": "entries",
                            "id": id, "path": format!("src/{id}.rs"),
                            "summary": format!("s {id}"), "content": format!("c {id}"),
                            "tags": [], "ts": "2024-01-01T00:00:00Z"
                        });
                        append_event(&paths.events, &ev).unwrap();
                    }
                    CompactOp::Expire(id) => {
                        let ev = serde_json::json!({
                            "action": "expire", "table": "entries",
                            "id": id, "ts": "2024-01-01T01:00:00Z"
                        });
                        append_event(&paths.events, &ev).unwrap();
                    }
                    CompactOp::EvidenceAdd(id) => {
                        append_event(&paths.events, &serde_json::json!({
                            "action": "evidence_add", "table": "evidence", "entry_id": id,
                            "evidence": {"id": format!("ev-{id}"), "entry_id": id, "kind": "code",
                                "citation_path": format!("src/{id}.rs:0-1"), "citation_sha": null,
                                "citation_hash": "sha256:abc", "citation_excerpt": null,
                                "derived_from": null, "recorded_at": null},
                            "ts": "2024-01-01T02:00:00Z"
                        })).unwrap();
                    }
                    CompactOp::EvidenceExpire(id) => {
                        append_event(&paths.events, &crate::components::events::evidence_expire_event(
                            id, &format!("ev-{id}"), "property test"
                        )).unwrap();
                    }
                    CompactOp::CitationHealed(id) => {
                        append_event(&paths.events, &crate::components::events::citation_healed_event(
                            id, &format!("ev-{id}"), &format!("src/{id}.rs:0-1"),
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
        Upsert(String),
        Expire(String),
        EvidenceAdd(String),
        EvidenceExpire(String),
        CitationHealed(String),
        Compact,
    }

    /// Raw op generator — unrestricted alphabet across 4 ids.
    fn arb_raw_op() -> impl proptest::strategy::Strategy<Value = CompactOp> {
        use proptest::prelude::*;
        let arb_id = proptest::sample::select(vec!["a", "b", "c", "d"]).prop_map(|s| s.to_string());
        prop_oneof![
            arb_id.clone().prop_map(CompactOp::Upsert),
            arb_id.clone().prop_map(CompactOp::Expire),
            arb_id.clone().prop_map(CompactOp::EvidenceAdd),
            arb_id.clone().prop_map(CompactOp::EvidenceExpire),
            arb_id.prop_map(CompactOp::CitationHealed),
            Just(CompactOp::Compact),
        ]
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
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
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

    /// Build a test root with a large DB so `freelist_count >= 1024` after expiry.
    ///
    /// Strategy: insert N entries (each with ~400-byte content) into the DB via
    /// apply_event, then expire them all. SQLite keeps the freed pages in its
    /// freelist until VACUUM reclaims them.
    fn setup_with_db(root: &std::path::Path) -> Paths {
        use crate::components::{db, embedder::NoopEmbedder};
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        let conn = db::open_db(&paths.db).unwrap();
        let embedder = NoopEmbedder;

        // Insert 12 000 entries with ~400 bytes of content each so that expiring
        // them all leaves at least 1024 SQLite free pages.
        let n_entries = 12_000_usize;
        let padding: String = "x".repeat(400);
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
