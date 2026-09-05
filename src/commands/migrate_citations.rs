//! `migrate-citations` subcommand
//!
//! Heals the legacy whole-file workaround citation form `path:0-N` into the
//! new bare whole-file form `path`, but only when the current file still hashes
//! to the stored `citation_hash` and still has size `N`.

#![allow(deprecated)] // db::open_db (ADR-1) — remaining call sites migrate in C2/L1b, L2, L3, L1c
use crate::commands::add::{acquire_lock, make_embedder};
use crate::components::cursor;
use crate::components::db;
use crate::components::embedder::NoopEmbedder;
use crate::components::events;
use crate::components::verification::{compute_citation_hash_and_size, parse_citation_path};
use crate::config;
use abscissa_core::{Command, Runnable};
use anyhow::Result;
use clap::Parser;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Component, Path, PathBuf};

const SELF_REFERENTIAL_LOG_REASON: &str =
    "self-referential log citation — re-cite or expire manually";
const SKIP_ALREADY_BARE_REASON: &str = "already bare whole-file citation";
const SKIP_NON_LEGACY_REASON: &str = "not a legacy whole-file workaround citation";
const SKIP_PARENT_STALE_REASON: &str = "parent went stale";

#[derive(Command, Debug, Parser)]
pub struct MigrateCitations {
    /// Show the migration plan without writing citation_healed events
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

impl Runnable for MigrateCitations {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

#[derive(Debug, Default)]
pub struct MigrationReport {
    pub would_heal: Vec<MigrationRow>,
    pub skipped: Vec<MigrationRow>,
    pub failed: Vec<MigrationRow>,
    pub emitted_events: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRow {
    pub entry_id: String,
    pub evidence_id: String,
    pub old_path: String,
    pub new_path: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
struct EvidenceCitationRow {
    entry_id: String,
    evidence_id: String,
    citation_path: String,
    citation_hash: String,
}

enum PlannedAction {
    WouldHeal { new_path: String },
    Skip { reason: &'static str },
    Fail { reason: String },
}

impl MigrateCitations {
    pub fn execute(&self) -> Result<()> {
        let paths = config::Paths::discover()?;
        let embedder = make_embedder(&paths);
        crate::commands::rebuild::recover_if_needed(&paths, embedder.as_ref())?;
        self.execute_with_paths(&paths)?;
        Ok(())
    }

    pub fn execute_with_paths(&self, paths: &config::Paths) -> Result<MigrationReport> {
        let conn = db::open_db(&paths.db)?;
        let repo_root = &paths.root;
        let mut report = plan_migration(&conn, repo_root.as_path(), &paths.events)?;
        if !self.dry_run {
            apply_heals(paths, repo_root.as_path(), &mut report)?;
        }
        render_cli(&report, self.dry_run);
        Ok(report)
    }
}

fn plan_migration(
    conn: &Connection,
    repo_root: &Path,
    events_path: &Path,
) -> Result<MigrationReport> {
    let mut stmt = conn.prepare(
        "SELECT ev.entry_id, ev.id, ev.citation_path, ev.citation_hash
         FROM evidence ev
         JOIN entries e ON e.id = ev.entry_id
         WHERE e.is_stale = 0
           AND ev.citation_path IS NOT NULL
         ORDER BY ev.entry_id, ev.id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(EvidenceCitationRow {
                entry_id: row.get(0)?,
                evidence_id: row.get(1)?,
                citation_path: row.get(2)?,
                citation_hash: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut report = MigrationReport::default();
    for row in rows {
        let view = MigrationRow {
            entry_id: row.entry_id.clone(),
            evidence_id: row.evidence_id.clone(),
            old_path: row.citation_path.clone(),
            new_path: None,
            reason: None,
        };
        match classify_row(&row, repo_root, events_path) {
            PlannedAction::WouldHeal { new_path } => report.would_heal.push(MigrationRow {
                new_path: Some(new_path),
                ..view
            }),
            PlannedAction::Skip { reason } => report.skipped.push(MigrationRow {
                reason: Some(reason.to_string()),
                ..view
            }),
            PlannedAction::Fail { reason } => report.failed.push(MigrationRow {
                reason: Some(reason),
                ..view
            }),
        }
    }

    Ok(report)
}

fn classify_row(row: &EvidenceCitationRow, repo_root: &Path, events_path: &Path) -> PlannedAction {
    let (file_rel, range) = match parse_citation_path(&row.citation_path) {
        Ok(parsed) => parsed,
        Err(_) => {
            return PlannedAction::Skip {
                reason: SKIP_NON_LEGACY_REASON,
            };
        }
    };

    let Some((start, end)) = range else {
        return PlannedAction::Skip {
            reason: SKIP_ALREADY_BARE_REASON,
        };
    };
    if start != 0 {
        return PlannedAction::Skip {
            reason: SKIP_NON_LEGACY_REASON,
        };
    }
    if citation_targets_events_log(file_rel, repo_root, events_path) {
        return PlannedAction::Fail {
            reason: SELF_REFERENTIAL_LOG_REASON.to_string(),
        };
    }

    match compute_citation_hash_and_size(repo_root, file_rel, None) {
        Ok(current) => {
            if current.file_size as usize != end {
                return PlannedAction::Fail {
                    reason: format!(
                        "size mismatch: stored range end {end} != current file size {}",
                        current.file_size
                    ),
                };
            }
            let expected = row
                .citation_hash
                .strip_prefix("sha256:")
                .unwrap_or(&row.citation_hash);
            if !current.sha256_hex.eq_ignore_ascii_case(expected) {
                return PlannedAction::Fail {
                    reason: "whole-file hash mismatch".to_string(),
                };
            }
            PlannedAction::WouldHeal {
                new_path: file_rel.to_string(),
            }
        }
        Err(reason) => PlannedAction::Fail {
            reason: reason.to_string(),
        },
    }
}

fn normalize_repo_relative(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn citation_targets_events_log(file_rel: &str, repo_root: &Path, events_path: &Path) -> bool {
    let Some(citation_rel) = normalize_repo_relative(Path::new(file_rel)) else {
        return false;
    };
    let Ok(configured_rel) = events_path.strip_prefix(repo_root) else {
        return false;
    };
    normalize_repo_relative(configured_rel).as_ref() == Some(&citation_rel)
}

/// Apply the planned heals under the write lock.
///
/// Opens its own mutating connection rather than reusing the planning read
/// connection: a mutation must be performed on a handle obtained with the lock
/// in hand (ADR-1, principle 2).
fn apply_heals(
    paths: &config::Paths,
    repo_root: &Path,
    report: &mut MigrationReport,
) -> Result<()> {
    let version_ref = config::git_head_sha_at(repo_root);
    let lock = acquire_lock(&paths.lock)?;
    let conn = &db::open_rw(paths, &lock)?;

    let planned = std::mem::take(&mut report.would_heal);
    for row in planned {
        let Some((current_path, citation_hash)) = conn
            .query_row(
                "SELECT ev.citation_path, ev.citation_hash
                 FROM evidence ev
                 JOIN entries e ON e.id = ev.entry_id
                 WHERE ev.id=?1 AND ev.entry_id=?2 AND e.is_stale=0",
                params![&row.evidence_id, &row.entry_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            let evidence_still_exists = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM evidence WHERE id=?1 AND entry_id=?2)",
                params![&row.evidence_id, &row.entry_id],
                |r| r.get::<_, bool>(0),
            )?;
            report.skipped.push(MigrationRow {
                reason: Some(if evidence_still_exists {
                    SKIP_PARENT_STALE_REASON.to_string()
                } else {
                    "evidence row disappeared before apply".to_string()
                }),
                ..row
            });
            continue;
        };

        let verify_row = EvidenceCitationRow {
            entry_id: row.entry_id.clone(),
            evidence_id: row.evidence_id.clone(),
            citation_path: current_path,
            citation_hash,
        };
        let current_view = MigrationRow {
            entry_id: row.entry_id.clone(),
            evidence_id: row.evidence_id.clone(),
            old_path: verify_row.citation_path.clone(),
            new_path: None,
            reason: None,
        };

        match classify_row(&verify_row, repo_root, &paths.events) {
            PlannedAction::WouldHeal { new_path } => {
                let event = events::citation_healed_event(
                    &row.entry_id,
                    &row.evidence_id,
                    &verify_row.citation_path,
                    &new_path,
                    &verify_row.citation_hash,
                    version_ref.as_deref(),
                );
                // Writer 6 of 10. If append succeeds but apply fails, a rerun
                // may append a second citation_healed event. Applying the same
                // target is a state-idempotent no-op; deterministic op IDs are
                // deferred.
                cursor::append_and_apply(&lock, conn, paths, &NoopEmbedder, &[event])?;
                report.emitted_events += 1;
                report.would_heal.push(MigrationRow {
                    new_path: Some(new_path),
                    ..current_view
                });
            }
            PlannedAction::Skip { reason } => report.skipped.push(MigrationRow {
                reason: Some(reason.to_string()),
                ..current_view
            }),
            PlannedAction::Fail { reason } => report.failed.push(MigrationRow {
                reason: Some(reason),
                ..current_view
            }),
        }
    }

    Ok(())
}

fn render_cli(report: &MigrationReport, dry_run: bool) {
    for row in &report.would_heal {
        let status = if dry_run { "WOULD-HEAL" } else { "HEALED" };
        println!(
            "{status}\t{}\t{}\t{}\t{}",
            row.entry_id,
            row.evidence_id,
            row.old_path,
            row.new_path.as_deref().unwrap_or("")
        );
    }
    for row in &report.skipped {
        let status = if dry_run { "WOULD-SKIP" } else { "SKIP" };
        println!(
            "{status}\t{}\t{}\t{}\t{}",
            row.entry_id,
            row.evidence_id,
            row.old_path,
            row.reason.as_deref().unwrap_or("")
        );
    }
    for row in &report.failed {
        let status = if dry_run { "WOULD-REPORT" } else { "REPORT" };
        println!(
            "{status}\t{}\t{}\t{}\t{}",
            row.entry_id,
            row.evidence_id,
            row.old_path,
            row.reason.as_deref().unwrap_or("")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::db::apply_event;
    use crate::components::embedder::NoopEmbedder;
    use crate::components::events::{evidence_add_event, read_events};
    use crate::config::Paths;
    use crate::models::Evidence;
    use std::fs;
    use std::process::Command as Cmd;
    use tempfile::tempdir;

    fn setup_repo() -> (tempfile::TempDir, Paths, Connection) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        Cmd::new("git")
            .args(["init", "-b", "main"])
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
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        fs::write(root.join("README.md"), "init\n").unwrap();
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

        let paths = Paths::from_root(root);
        let conn = db::open_db(&paths.db).unwrap();
        (dir, paths, conn)
    }

    fn seed_live_entry(paths: &Paths, conn: &Connection, entry_id: &str) {
        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": entry_id,
            "path": "src/lib.rs",
            "summary": "entry",
            "content": "content",
            "tags": [],
            "kind": "observation",
            "evidence_status": "present",
            "is_stale": false,
            "ts": "2024-01-01T00:00:00Z"
        });
        let lock = acquire_lock(&paths.lock).unwrap();
        cursor::append_and_apply(&lock, conn, paths, &NoopEmbedder, &[upsert]).unwrap();
    }

    fn seed_evidence(
        paths: &Paths,
        conn: &Connection,
        entry_id: &str,
        evidence_id: &str,
        citation_path: &str,
        citation_hash: &str,
    ) {
        let evidence = Evidence {
            id: evidence_id.to_string(),
            entry_id: entry_id.to_string(),
            kind: "code".to_string(),
            citation_path: Some(citation_path.to_string()),
            citation_sha: None,
            citation_hash: citation_hash.to_string(),
            citation_excerpt: Some("excerpt long enough\nwith two lines".to_string()),
            derived_from: None,
            recorded_at: Some("2026-09-02T00:00:00Z".to_string()),
        };
        let event = evidence_add_event(entry_id, &evidence, Some("deadbeef"));
        let lock = acquire_lock(&paths.lock).unwrap();
        cursor::append_and_apply(&lock, conn, paths, &NoopEmbedder, &[event]).unwrap();
    }

    #[test]
    fn test_migrate_citations_heals_matching_legacy_whole_file_range() {
        let (_dir, paths, conn) = setup_repo();
        let root = paths.root.clone();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let bytes = fs::read(root.join("src/lib.rs")).unwrap();
        let end = bytes.len();
        let hash = crate::components::verification::compute_citation_hash(
            root.as_path(),
            "src/lib.rs",
            None,
        )
        .unwrap();

        seed_live_entry(&paths, &conn, "entry-1");
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!("src/lib.rs:0-{end}"),
            &hash,
        );

        let report = MigrateCitations { dry_run: false }
            .execute_with_paths(&paths)
            .unwrap();
        assert_eq!(report.emitted_events, 1);
        assert_eq!(report.would_heal.len(), 1);

        let citation_path: String = conn
            .query_row(
                "SELECT citation_path FROM evidence WHERE id='ev-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let citation_hash: String = conn
            .query_row(
                "SELECT citation_hash FROM evidence WHERE id='ev-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(citation_path, "src/lib.rs");
        assert_eq!(citation_hash, hash);

        let events = read_events(&paths.events).unwrap().events;
        assert_eq!(events.len(), 3);
        assert_eq!(events[2]["action"], "citation_healed");
        assert_eq!(events[2]["old_path"], format!("src/lib.rs:0-{end}"));
        assert_eq!(events[2]["new_path"], "src/lib.rs");
        assert_eq!(events[2]["citation_hash"], hash);

        // C1/T4: the heal writer must leave the applied cursor caught up, or
        // every later open replays its events.
        let ro = db::open_ro(&paths.db).unwrap();
        assert_eq!(
            cursor::inspect(&ro, &paths),
            cursor::Decision::NoOp,
            "the citation_healed writer left the applied cursor behind the log"
        );
    }

    #[test]
    fn test_migrate_citations_skips_bare_rows_on_rerun() {
        let (_dir, paths, conn) = setup_repo();
        let root = paths.root.clone();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let end = fs::read(root.join("src/lib.rs")).unwrap().len();
        let hash = crate::components::verification::compute_citation_hash(
            root.as_path(),
            "src/lib.rs",
            None,
        )
        .unwrap();

        seed_live_entry(&paths, &conn, "entry-1");
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!("src/lib.rs:0-{end}"),
            &hash,
        );

        MigrateCitations { dry_run: false }
            .execute_with_paths(&paths)
            .unwrap();
        let before = read_events(&paths.events).unwrap().events.len();

        let report = MigrateCitations { dry_run: false }
            .execute_with_paths(&paths)
            .unwrap();
        let after = read_events(&paths.events).unwrap().events.len();

        assert_eq!(report.emitted_events, 0);
        assert!(report.would_heal.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            report.skipped[0].reason.as_deref(),
            Some(SKIP_ALREADY_BARE_REASON)
        );
        assert_eq!(before, after);
    }

    #[test]
    fn test_migrate_citations_reports_changed_file_rows() {
        let (_dir, paths, conn) = setup_repo();
        let root = paths.root.clone();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "abc\n").unwrap();
        let end = fs::read(root.join("src/lib.rs")).unwrap().len();
        let old_hash = crate::components::verification::compute_citation_hash(
            root.as_path(),
            "src/lib.rs",
            None,
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "xyz\n").unwrap();

        seed_live_entry(&paths, &conn, "entry-1");
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!("src/lib.rs:0-{end}"),
            &old_hash,
        );

        let report = MigrateCitations { dry_run: false }
            .execute_with_paths(&paths)
            .unwrap();

        assert_eq!(report.emitted_events, 0);
        assert!(report.would_heal.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            report.failed[0].reason.as_deref(),
            Some("whole-file hash mismatch")
        );
    }

    #[test]
    fn test_migrate_citations_reports_size_mismatch_rows() {
        let (_dir, paths, conn) = setup_repo();
        let root = paths.root.clone();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "abc").unwrap();
        let old_end = fs::read(root.join("src/lib.rs")).unwrap().len();
        let old_hash = crate::components::verification::compute_citation_hash(
            root.as_path(),
            "src/lib.rs",
            None,
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "abcd").unwrap();

        seed_live_entry(&paths, &conn, "entry-1");
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!("src/lib.rs:0-{old_end}"),
            &old_hash,
        );

        let report = MigrateCitations { dry_run: false }
            .execute_with_paths(&paths)
            .unwrap();

        assert_eq!(report.emitted_events, 0);
        assert!(report.would_heal.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(
            report.failed[0]
                .reason
                .as_deref()
                .is_some_and(|r| r.starts_with("size mismatch:")),
            "{report:?}"
        );
    }

    #[test]
    fn test_migrate_citations_dry_run_emits_no_events() {
        let (_dir, paths, conn) = setup_repo();
        let root = paths.root.clone();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let end = fs::read(root.join("src/lib.rs")).unwrap().len();
        let hash = crate::components::verification::compute_citation_hash(
            root.as_path(),
            "src/lib.rs",
            None,
        )
        .unwrap();

        seed_live_entry(&paths, &conn, "entry-1");
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!("src/lib.rs:0-{end}"),
            &hash,
        );

        let report = MigrateCitations { dry_run: true }
            .execute_with_paths(&paths)
            .unwrap();

        assert_eq!(report.emitted_events, 0);
        assert_eq!(report.would_heal.len(), 1);
        assert_eq!(read_events(&paths.events).unwrap().events.len(), 2);
        let citation_path: String = conn
            .query_row(
                "SELECT citation_path FROM evidence WHERE id='ev-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(citation_path, format!("src/lib.rs:0-{end}"));
    }

    #[test]
    fn test_migrate_citations_reports_self_referential_events_log_citation() {
        let (_dir, paths, conn) = setup_repo();

        seed_live_entry(&paths, &conn, "entry-1");
        let events_len = fs::metadata(&paths.events).unwrap().len();
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!(".state/agent-kb/agent-kb-events.jsonl:0-{events_len}"),
            "sha256:deadbeef",
        );

        let report = MigrateCitations { dry_run: false }
            .execute_with_paths(&paths)
            .unwrap();

        assert_eq!(report.emitted_events, 0);
        assert!(report.would_heal.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            report.failed[0].reason.as_deref(),
            Some(SELF_REFERENTIAL_LOG_REASON)
        );
    }

    #[test]
    fn test_migrate_citations_heals_subdir_file_named_like_events_log() {
        let (_dir, paths, conn) = setup_repo();
        let root = paths.root.clone();
        fs::create_dir_all(root.join("fixtures")).unwrap();
        fs::write(root.join("fixtures/agent-kb-events.jsonl"), "fixture\n").unwrap();
        let end = fs::metadata(root.join("fixtures/agent-kb-events.jsonl"))
            .unwrap()
            .len() as usize;
        let hash = crate::components::verification::compute_citation_hash(
            &root,
            "fixtures/agent-kb-events.jsonl",
            None,
        )
        .unwrap();
        seed_live_entry(&paths, &conn, "entry-1");
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!("fixtures/agent-kb-events.jsonl:0-{end}"),
            &hash,
        );

        let report = MigrateCitations { dry_run: false }
            .execute_with_paths(&paths)
            .unwrap();

        assert_eq!(report.emitted_events, 1);
        assert_eq!(
            report.would_heal[0].new_path.as_deref(),
            Some("fixtures/agent-kb-events.jsonl")
        );
    }

    #[test]
    fn test_migrate_citations_skips_when_parent_goes_stale_between_plan_and_apply() {
        let (_dir, paths, conn) = setup_repo();
        let root = paths.root.clone();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let end = fs::metadata(root.join("src/lib.rs")).unwrap().len() as usize;
        let hash =
            crate::components::verification::compute_citation_hash(&root, "src/lib.rs", None)
                .unwrap();
        seed_live_entry(&paths, &conn, "entry-1");
        seed_evidence(
            &paths,
            &conn,
            "entry-1",
            "ev-1",
            &format!("src/lib.rs:0-{end}"),
            &hash,
        );
        let mut report = plan_migration(&conn, &root, &paths.events).unwrap();

        let expire = serde_json::json!({
            "action": "expire",
            "table": "entries",
            "id": "entry-1",
            "reason": "test race",
            "ts": "2026-09-02T00:00:01Z"
        });
        events::append_event(&paths.events, &expire).unwrap();
        apply_event(&conn, &NoopEmbedder, &expire).unwrap();
        let before = read_events(&paths.events).unwrap().events.len();

        apply_heals(&paths, &root, &mut report).unwrap();

        assert_eq!(report.emitted_events, 0);
        assert!(report.would_heal.is_empty());
        // Under ADR-2, expiring the parent entry GCs its evidence rows, so the
        // requery finds the row gone entirely — the SKIP_PARENT_STALE_REASON
        // branch (live parent, stale evidence) is unreachable for
        // expire-driven staleness. The skip itself (the actual requirement)
        // still happens.
        assert_eq!(
            report.skipped[0].reason.as_deref(),
            Some("evidence row disappeared before apply")
        );
        assert_eq!(read_events(&paths.events).unwrap().events.len(), before);
    }
}
