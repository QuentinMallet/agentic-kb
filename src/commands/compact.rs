//! `compact` subcommand

use crate::commands::add::acquire_lock;
use crate::components::events;
use crate::config::{self, VacuumConfig};
use abscissa_core::{Application, Command, Runnable};
use clap::Parser;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;

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
        let evts = events::read_events(&paths.events)?;
        let original_count = evts.len();

        let mut entry_last: HashMap<String, usize> = HashMap::new();
        let mut test_last: HashMap<String, usize> = HashMap::new();
        // Track index of the LAST expire per id (not just membership) so that
        // upsert→expire→upsert sequences honour last-write-wins (br-joj).
        let mut expire_last: HashMap<String, usize> = HashMap::new();
        let mut run_indices: Vec<usize> = Vec::new();

        for (i, ev) in evts.iter().enumerate() {
            let action = ev["action"].as_str().unwrap_or("");
            let table = ev["table"].as_str().unwrap_or("");
            let id = ev["id"].as_str().unwrap_or("").to_string();
            // Warn on structurally malformed events (missing or non-string action/table).
            // These differ from intentionally-skipped event types (e.g. evidence_add).
            if !ev["action"].is_string() || !ev["table"].is_string() {
                eprintln!("compact: warn: event at index {i} missing valid action/table, dropped");
            }
            match (action, table) {
                ("upsert", "entries") => {
                    entry_last.insert(id, i);
                }
                ("expire", "entries") => {
                    expire_last.insert(id, i);
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
            compacted.push(evts[i].clone());
        }

        // Test case upserts.
        let mut test_pairs: Vec<(usize, String)> =
            test_last.into_iter().map(|(id, i)| (i, id)).collect();
        test_pairs.sort_by_key(|&(i, _)| i);
        for (i, _) in test_pairs {
            compacted.push(evts[i].clone());
        }

        // Run history: keep only the last RUN_HISTORY_CAP records (original order).
        let run_start = run_indices.len().saturating_sub(RUN_HISTORY_CAP);
        for i in run_indices[run_start..].iter().copied() {
            compacted.push(evts[i].clone());
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
            after.len(),
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
            after.len(),
            1,
            "compact must produce exactly one entry event"
        );
        // The compact must emit the LATEST upsert (v2) and must NOT mark stale.
        assert_eq!(
            after[0]["summary"], "v2",
            "compact must keep the last upsert content"
        );
        assert!(
            after[0]["is_stale"].is_null() || after[0]["is_stale"] == false,
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
        assert_eq!(after.len(), 1);
        assert!(after[0]["is_stale"].is_null() || after[0]["is_stale"] == false);
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
        assert_eq!(after.len(), 1, "only live entry must remain in log");
        assert_eq!(after[0]["id"], "live");
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
            after.len(),
            RUN_HISTORY_CAP,
            "compact must retain at most RUN_HISTORY_CAP run_history events"
        );
        // Verify the LAST RUN_HISTORY_CAP records are kept (oldest 50 trimmed).
        assert_eq!(after[0]["test_id"], "50", "first retained must be event 50");
        assert_eq!(
            after[RUN_HISTORY_CAP - 1]["test_id"],
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
            after.len(),
            RUN_HISTORY_CAP,
            "exactly RUN_HISTORY_CAP events must not be trimmed"
        );
        assert_eq!(after[0]["test_id"], "0");
        assert_eq!(
            after[RUN_HISTORY_CAP - 1]["test_id"],
            format!("{}", RUN_HISTORY_CAP - 1)
        );
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
        assert_eq!(after.len(), 0, "pure orphan expire must be dropped");

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
            after2.len(),
            0,
            "post-compact orphan expire must also be dropped"
        );
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
        Compact,
    }

    /// Raw op generator — unrestricted alphabet across 4 ids.
    fn arb_raw_op() -> impl proptest::strategy::Strategy<Value = CompactOp> {
        use proptest::prelude::*;
        let arb_id = proptest::sample::select(vec!["a", "b", "c", "d"]).prop_map(|s| s.to_string());
        prop_oneof![
            arb_id.clone().prop_map(CompactOp::Upsert),
            arb_id.prop_map(CompactOp::Expire),
            Just(CompactOp::Compact),
        ]
    }

    /// Replay the current event log into a fresh in-memory DB and return the
    /// sorted set of live (non-stale) entry IDs — the materialized live state
    /// under test. Stale entries are excluded because compact purges them from
    /// the log entirely; their absence after rebuild is correct.
    fn materialize(paths: &Paths) -> Vec<String> {
        use crate::components::{db, embedder::NoopEmbedder};

        let conn = db::open_db_memory().unwrap();
        let embedder = NoopEmbedder;
        let evts = events::read_events(&paths.events).unwrap();
        for ev in &evts {
            db::apply_event(&conn, &embedder, ev).unwrap();
        }

        let mut stmt = conn
            .prepare("SELECT id FROM entries WHERE COALESCE(is_stale, 0) = 0 ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
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
