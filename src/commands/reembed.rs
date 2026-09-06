//! `reembed` subcommand — exclusion-correct backfill of missing embeddings.

use crate::commands::add::{acquire_lock, make_embedder};
use crate::components::{db, embedder};
use crate::config;
use crate::models::normalized_f32s_to_f16_blob;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::params;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;

/// Writes per lock acquisition, in one transaction per batch (a single
/// commit, not one implicit commit per row — see `write_batches`). Budget:
/// <= 50 ms lock-hold per batch on an idle host, timed from lock
/// acquisition to the batch connection being dropped after commit.
/// `test_reembed_batch_lock_hold_budget` (ignored by default — see its doc
/// comment) times this exact window on a real batch and prints the
/// observed duration; it is a measurement to be taken on a quiet host, not
/// a CI gate.
///
/// Measured 2026-09-05 on this dev machine under heavy concurrent-build
/// load (uptime load average ~24-28 on 12 cores; ~17 concurrent
/// cargo/rustc processes across sibling worktrees sharing the target dir):
/// 94 ms and 72 ms, timing the real batch (this replaces an earlier,
/// incorrect measurement of a synthetic kb_meta insert loop that never
/// exercised write_batches at all, and predates the per-batch transaction
/// above — both were review findings). Still well above the 50 ms
/// idle-host budget, but the transaction cut it roughly 4-6x versus the
/// pre-transaction code path measured the same way (319-634 ms).
///
/// Re-measured 2026-09-06 in the release profile on an otherwise idle
/// 12-core host (load1 2.50 at start; no concurrent build/test jobs), using
/// `CARGO_TARGET_DIR=/tmp/agentic-kb-target-bd-21ef.2.18 cargo test --release
/// -p kb test_reembed_batch_lock_hold_budget -- --ignored --nocapture`.
/// Three warm samples were 96.898973 ms, 109.389693 ms, and 64.847207 ms.
/// The release-profile idle-host result still exceeds the 50 ms budget, so
/// this obligation remains open pending a smaller batch or moving embedding
/// work fully outside the lock.
///
/// Re-measured 2026-09-06 after one-time schema/stamp preflight plus a fresh
/// locked live-path reopen for each 32-row batch. Three warm release samples
/// were 81.982865 ms, 77.634364 ms, and 71.449899 ms. Correctness and swap
/// regressions pass, but all samples still miss the <= 50 ms gate; retain the
/// universal lock and durable transaction semantics while investigating a
/// structurally different reduction in per-batch SQLite work.
pub(crate) const REEMBED_WRITE_BATCH_SIZE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReembedFailure {
    pub id: String,
    pub cause: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ReembedReport {
    pub embedded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub missing: usize,
    /// Rows whose write did not apply because the entry raced away between
    /// selection and the batch: it already had an embedding (a concurrent
    /// writer got there first) or it no longer resolves live (deleted or
    /// marked stale). Neither is a failure of this run; without this
    /// counter these rows silently vanished — embedded + failed + skipped
    /// could be less than missing with no explanation (review finding).
    pub raced: usize,
    pub failures: Vec<ReembedFailure>,
}

struct Candidate {
    id: String,
    path: String,
    summary: String,
    content: String,
    tags: String,
    updated_at: String,
}
struct PendingWrite {
    id: String,
    updated_at: String,
    blob: Vec<u8>,
}

/// Ordered boundary markers inside one batch's lock window, from the moment
/// the universal write lock is held to the moment it is about to be
/// released. The measurement tests subtract consecutive marks to attribute
/// lock-hold time to a phase; production passes a closure that ignores them,
/// so no clock is read on the real write path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchPhase {
    /// Before `acquire_lock`. The only mark outside the lock window; it
    /// exists so the flock acquisition itself is attributable.
    BatchStart,
    /// The universal write lock is held and nothing else has run yet.
    LockAcquired,
    /// `open_rw_existing` returned a connection on the live pathname.
    Opened,
    /// The embed-text-mode vintage check finished.
    VintageChecked,
    /// `BEGIN` returned.
    Begun,
    /// Every row in the batch has been executed, but not yet committed.
    Inserted,
    /// The durable commit returned.
    Committed,
    /// The connection has been dropped; the lock is released next.
    ConnDropped,
}

impl BatchPhase {
    /// The phases in the order `write_batches` emits them. The first is a
    /// start marker, so there are `ORDER.len() - 1` measurable spans.
    pub(crate) const ORDER: [BatchPhase; 8] = [
        BatchPhase::BatchStart,
        BatchPhase::LockAcquired,
        BatchPhase::Opened,
        BatchPhase::VintageChecked,
        BatchPhase::Begun,
        BatchPhase::Inserted,
        BatchPhase::Committed,
        BatchPhase::ConnDropped,
    ];

    /// Label for the span that ENDS at this phase.
    pub(crate) fn span_label(self) -> &'static str {
        match self {
            BatchPhase::BatchStart => "(start marker)",
            BatchPhase::LockAcquired => "flock acquire",
            BatchPhase::Opened => "open_rw_existing",
            BatchPhase::VintageChecked => "vintage check",
            BatchPhase::Begun => "BEGIN",
            BatchPhase::Inserted => "inserts",
            BatchPhase::Committed => "COMMIT (durable)",
            BatchPhase::ConnDropped => "connection drop",
        }
    }
}

#[derive(Command, Debug, Parser)]
pub struct Reembed {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long, default_value_t = 1800)]
    pub max_chars: usize,
}

impl Runnable for Reembed {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Reembed {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let emb = make_embedder(&paths);
        self.execute_with(&paths, emb.as_ref())
    }

    pub fn execute_with(
        &self,
        paths: &config::Paths,
        emb: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let report = run_reembed(paths, emb, self.dry_run, self.max_chars)?;
        if self.dry_run {
            println!(
                "dry-run: {} entries to re-embed, {} skipped (exceeds {} chars)",
                report.missing, report.skipped, self.max_chars
            );
        } else if emb.is_noop() {
            eprintln!("reembed: KB_NO_EMBED is set — skipping (no embedder available)");
        } else {
            for failure in &report.failures {
                eprintln!("  skip {}: {}", failure.id, failure.cause);
            }
            println!(
                "reembed: {} embedded, {} failed, {} skipped (too large), {} raced",
                report.embedded, report.failed, report.skipped, report.raced
            );
        }
        if report.failed > 0 {
            anyhow::bail!("{} embedding(s) failed", report.failed);
        }
        Ok(())
    }
}

pub(crate) fn run_reembed(
    paths: &config::Paths,
    emb: &dyn embedder::Embedder,
    dry_run: bool,
    max_chars: usize,
) -> anyhow::Result<ReembedReport> {
    run_reembed_with_hooks(paths, emb, dry_run, max_chars, |_, _| {}, |_, _| {})
}

#[cfg(test)]
fn run_reembed_with_hook<B>(
    paths: &config::Paths,
    emb: &dyn embedder::Embedder,
    dry_run: bool,
    max_chars: usize,
    before_batch: B,
) -> anyhow::Result<ReembedReport>
where
    B: FnMut(usize, &[PendingWrite]),
{
    run_reembed_with_hooks(paths, emb, dry_run, max_chars, before_batch, |_, _| {})
}

/// `before_batch(batch_index, batch)` fires right before a batch's write
/// lock is acquired, with the rows that batch will write.
/// `phase(batch_index, phase)` fires at every [`BatchPhase`] boundary of that
/// batch, in [`BatchPhase::ORDER`]. Tests use `before_batch` to observe
/// batching and inject races, and `phase` both to act at a precise point
/// (notably [`BatchPhase::ConnDropped`], which is still inside the lock) and
/// to time the real acquire-to-connection-drop window phase by phase.
fn run_reembed_with_hooks<B, P>(
    paths: &config::Paths,
    emb: &dyn embedder::Embedder,
    dry_run: bool,
    max_chars: usize,
    mut before_batch: B,
    mut phase: P,
) -> anyhow::Result<ReembedReport>
where
    B: FnMut(usize, &[PendingWrite]),
    P: FnMut(usize, BatchPhase),
{
    // Selection is unlocked and read-only.
    let conn = match db::open_ro(&paths.db) {
        Ok(conn) => conn,
        Err(e) if db::is_db_uninitialized(&e) => {
            if dry_run {
                // A fresh repository (no database yet) has nothing to
                // re-embed -- match the other first-run-safe readers and
                // report zero rather than erroring, since `--dry-run`
                // dispatch no longer initializes the database up front
                // (C2/L1c).
                db::note_uninitialized(&paths.db);
                return Ok(ReembedReport::default());
            }
            // Non-dry-run `reembed` is a writer: dispatch already
            // initializes it (best-effort, warning on failure rather than
            // erroring). If that startup recovery failed or was skipped,
            // self-heal here instead of silently reporting empty success --
            // the same pattern `stale_check.rs`'s `heal_relocations` uses
            // for its own write path, which calls `open_or_init` itself
            // rather than trusting the caller to have done so.
            db::open_or_init(paths)?;
            db::open_ro(&paths.db)?
        }
        Err(e) => return Err(e),
    };
    let mut stmt = conn.prepare(
        "SELECT e.id, e.path, e.summary, e.content, e.tags, e.updated_at FROM entries e
         WHERE e.is_stale = 0 AND e.rowid NOT IN (SELECT rowid FROM entries_emb)",
    )?;
    let candidates = stmt
        .query_map([], |r| {
            Ok(Candidate {
                id: r.get(0)?,
                path: r.get(1)?,
                summary: r.get(2)?,
                content: r.get(3)?,
                tags: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    drop(conn);

    let mode = db::EmbedTextMode::from_env();
    let mut report = ReembedReport::default();
    let selected: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| {
            let fits = db::entry_embed_text(
                mode,
                &candidate.path,
                &candidate.summary,
                &candidate.content,
                &candidate.tags,
            )
            .len()
                <= max_chars;
            if !fits {
                report.skipped += 1;
            }
            fits
        })
        .collect();
    report.missing = selected.len();
    if dry_run || emb.is_noop() {
        return Ok(report);
    }

    // All embedding computation happens outside the write lock.
    let mut writes = Vec::with_capacity(selected.len());
    for candidate in selected {
        let text = db::entry_embed_text(
            mode,
            &candidate.path,
            &candidate.summary,
            &candidate.content,
            &candidate.tags,
        );
        match emb.embed(&text) {
            Ok(vector) => match normalized_f32s_to_f16_blob(&vector) {
                Ok(blob) => writes.push(PendingWrite {
                    id: candidate.id,
                    updated_at: candidate.updated_at,
                    blob,
                }),
                Err(error) => record_failure(&mut report, candidate.id, error.to_string()),
            },
            Err(error) => record_failure(&mut report, candidate.id, error.to_string()),
        }
    }

    // Establish schema and metadata once outside the timed per-batch
    // critical section. Every batch below still opens the current live
    // pathname under the universal lock, but can safely skip this repeated
    // DDL/stamp work.
    preflight_reembed_schema(paths)?;

    let mut embedded_ids = HashSet::new();
    let initial_db_identity = db_identity(&paths.db);
    write_batches(
        paths,
        mode,
        &writes,
        &mut report,
        &mut embedded_ids,
        &mut before_batch,
        &mut phase,
    );
    // Reconcile once against the live pathname. If rebuild atomically replaced
    // the database between batches, rows committed to the old inode are
    // restored here without overwriting embeddings already in the new DB.
    // Pass one's failed/failures/raced all reflect that stale database, not
    // the live one, so they are discarded rather than kept alongside pass
    // two's results — otherwise a real failure (e.g. a rejecting trigger
    // still in place) would be recorded twice for the same id, and a row
    // that raced against the old inode but writes cleanly against the new
    // one would still be reported as raced even though nothing about it is
    // actually still contested (review finding).
    // embedded_ids is NOT reset here: an id can already be live on the
    // CURRENT path before the swap is even detected (a later batch that
    // landed on the replacement file before this check runs), and resetting
    // would forget that real write. The final live-db verification below,
    // not this reset, is what decides report.embedded.
    if db_identity(&paths.db) != initial_db_identity {
        report.failed = 0;
        report.failures.clear();
        report.raced = 0;
        let mut no_before = |_: usize, _: &[PendingWrite]| {};
        let mut no_phase = |_: usize, _: BatchPhase| {};
        write_batches(
            paths,
            mode,
            &writes,
            &mut report,
            &mut embedded_ids,
            &mut no_before,
            &mut no_phase,
        );
    }

    // Ground truth: embedded_ids accumulates every id that got a successful
    // INSERT at some point, but a swap mid-run can invalidate that — an id
    // written to an inode that is no longer live, and which no longer
    // resolves at all in the replacement (deleted, or now stale), is not
    // actually embedded anywhere (review finding). Confirm every candidate
    // against the CURRENT live db and only count what is really there; a
    // dropped id is not double-reported here because the reconcile pass
    // above already re-attempted its write and recorded the miss as
    // `raced` at that point (see write_batches).
    report.embedded = confirm_embedded_ids_are_live(paths, &embedded_ids)?;
    Ok(report)
}

fn db_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// Counts how many of `ids` currently have a live embedding row (joined
/// through the entries table by rowid, on whatever file `paths.db` names
/// right now). Chunked into IN-clause batches to bound one query's
/// parameter count for a large candidate set.
fn confirm_embedded_ids_are_live(
    paths: &config::Paths,
    ids: &HashSet<String>,
) -> anyhow::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let conn = db::open_ro(&paths.db)?;
    let ordered: Vec<&String> = ids.iter().collect();
    let mut confirmed = 0usize;
    for chunk in ordered.chunks(500) {
        let placeholders = (1..=chunk.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT COUNT(*) FROM entries e JOIN entries_emb emb ON emb.rowid = e.rowid
             WHERE e.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let count: i64 = stmt.query_row(
            rusqlite::params_from_iter(chunk.iter().map(|id| id.as_str())),
            |r| r.get(0),
        )?;
        confirmed += count as usize;
    }
    Ok(confirmed)
}

fn write_batches<B, P>(
    paths: &config::Paths,
    mode: db::EmbedTextMode,
    writes: &[PendingWrite],
    report: &mut ReembedReport,
    embedded_ids: &mut HashSet<String>,
    before_batch: &mut B,
    phase: &mut P,
) where
    B: FnMut(usize, &[PendingWrite]),
    P: FnMut(usize, BatchPhase),
{
    for (batch_index, batch) in writes.chunks(REEMBED_WRITE_BATCH_SIZE).enumerate() {
        before_batch(batch_index, batch);
        phase(batch_index, BatchPhase::BatchStart);
        let lock = match acquire_lock(&paths.lock) {
            Ok(lock) => lock,
            Err(error) => {
                record_batch_failure(report, batch, format!("acquire write lock: {error}"));
                continue;
            }
        };
        phase(batch_index, BatchPhase::LockAcquired);
        let conn = match db::open_rw_existing(paths, &lock) {
            Ok(conn) => conn,
            Err(error) => {
                record_batch_failure(report, batch, format!("open live database: {error}"));
                continue;
            }
        };
        phase(batch_index, BatchPhase::Opened);
        db::check_embed_mode_vintage(&conn, mode);
        phase(batch_index, BatchPhase::VintageChecked);

        // One transaction per batch: without it, each INSERT is its own
        // implicit commit — REEMBED_WRITE_BATCH_SIZE fsync-durable WAL
        // commits under the exclusive lock instead of one, which measured
        // as the dominant cost of the lock-hold time (review finding).
        // Per-row error handling still works inside the transaction:
        // SQLite's RAISE(ABORT) in a trigger rolls back only the failing
        // statement, not the whole transaction, so earlier rows in the
        // batch survive a later row's failure.
        let txn = match conn.unchecked_transaction() {
            Ok(txn) => txn,
            Err(error) => {
                record_batch_failure(report, batch, format!("begin transaction: {error}"));
                continue;
            }
        };
        phase(batch_index, BatchPhase::Begun);
        let mut successes = Vec::new();
        let mut stmt_failures = Vec::new();
        let mut raced = 0usize;
        for write in batch {
            // updated_at is re-checked alongside id/is_stale/absence
            // (review finding): a concurrent content edit that has not yet
            // written its own embedding still leaves the rowid absent from
            // entries_emb, so the id/absence check alone would let this
            // batch's vector — computed from the pre-edit content — land
            // on top of the new content.
            match txn.execute(
                "INSERT OR IGNORE INTO entries_emb(rowid, embedding, normalized)
                 SELECT e.rowid, ?3, 1 FROM entries e WHERE e.id = ?1 AND e.is_stale = 0
                 AND e.updated_at = ?2
                 AND e.rowid NOT IN (SELECT rowid FROM entries_emb)",
                params![write.id, write.updated_at, write.blob],
            ) {
                Ok(1) => successes.push(write.id.clone()),
                Ok(_) => raced += 1,
                Err(error) => stmt_failures.push((write.id.clone(), error.to_string())),
            }
        }
        phase(batch_index, BatchPhase::Inserted);
        if let Err(error) = txn.commit() {
            record_batch_failure(report, batch, format!("commit batch: {error}"));
            continue;
        }
        phase(batch_index, BatchPhase::Committed);
        // Dropped before the in-memory bookkeeping below, not after: closing
        // the connection is the last thing this batch needs the database for,
        // and everything that follows is process-local. Keeping it here also
        // makes ConnDropped a clean measurement of the close itself.
        drop(conn);
        phase(batch_index, BatchPhase::ConnDropped);
        for id in successes {
            if embedded_ids.insert(id) {
                report.embedded += 1;
            }
        }
        report.raced += raced;
        for (id, cause) in stmt_failures {
            record_failure(report, id, cause);
        }
    }
}

fn preflight_reembed_schema(paths: &config::Paths) -> anyhow::Result<()> {
    let lock = acquire_lock(&paths.lock)?;
    let conn = db::open_rw(paths, &lock)?;
    drop(conn);
    Ok(())
}

fn record_failure(report: &mut ReembedReport, id: String, cause: String) {
    report.failed += 1;
    report.failures.push(ReembedFailure { id, cause });
}
fn record_batch_failure(report: &mut ReembedReport, batch: &[PendingWrite], cause: String) {
    for write in batch {
        record_failure(report, write.id.clone(), cause.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::add::Add;
    use anyhow::anyhow;

    struct FixedEmbedder(f32);
    impl embedder::Embedder for FixedEmbedder {
        fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![self.0; 384])
        }
    }

    struct FailingEmbedder;
    impl embedder::Embedder for FailingEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            if text.contains("fail") {
                Err(anyhow!("fixture embedding failure"))
            } else {
                Ok(vec![0.5; 384])
            }
        }
    }

    const FAST_PROPTEST_CASES: u32 = 16;

    fn proptest_cases(default_full: u32) -> u32 {
        std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(FAST_PROPTEST_CASES.min(default_full))
    }

    /// Warm batches the lock-hold budget measurement averages over. More than
    /// one so a single cold-cache batch cannot decide the verdict.
    const BUDGET_SAMPLE_BATCHES: usize = 5;

    /// Runs a real `reembed` over exactly `batches` full batches and returns,
    /// per batch, the duration of every span in [`BatchPhase::ORDER`] (so
    /// `ORDER.len() - 1` durations, in that order).
    fn measure_batch_phases(prefix: &str, batches: usize) -> Vec<Vec<std::time::Duration>> {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        for index in 0..(batches * REEMBED_WRITE_BATCH_SIZE) {
            seed(&paths, &format!("{prefix}-{index}"), "seed");
        }
        let marks = std::cell::RefCell::new(Vec::new());
        run_reembed_with_hooks(
            &paths,
            &FixedEmbedder(0.5),
            false,
            1800,
            |_batch_index, _batch| {},
            |batch_index, phase| {
                marks
                    .borrow_mut()
                    .push((batch_index, phase, std::time::Instant::now()));
            },
        )
        .unwrap();
        let marks = marks.into_inner();
        (0..batches)
            .map(|batch| {
                let stamps: Vec<std::time::Instant> = marks
                    .iter()
                    .filter(|(index, _, _)| *index == batch)
                    .map(|(_, _, at)| *at)
                    .collect();
                assert_eq!(
                    stamps.len(),
                    BatchPhase::ORDER.len(),
                    "batch {batch} must emit every phase exactly once"
                );
                stamps
                    .windows(2)
                    .map(|pair| pair[1].duration_since(pair[0]))
                    .collect()
            })
            .collect()
    }

    /// The measured budget window per batch: lock acquisition through the
    /// connection drop, i.e. every span after `flock acquire`.
    fn lock_windows(samples: &[Vec<std::time::Duration>]) -> Vec<std::time::Duration> {
        samples
            .iter()
            .map(|spans| spans[1..].iter().sum())
            .collect()
    }

    fn median(mut values: Vec<std::time::Duration>) -> std::time::Duration {
        values.sort_unstable();
        values[values.len() / 2]
    }

    fn millis(value: std::time::Duration) -> String {
        format!("{:.3}", value.as_secs_f64() * 1000.0)
    }

    /// Renders per-phase medians plus every raw sample, in batch order.
    fn phase_table(samples: &[Vec<std::time::Duration>]) -> String {
        let mut out = format!("{:<20} {:>10}  samples (ms, in batch order)\n", "phase", "median");
        for (span, phase) in BatchPhase::ORDER.iter().enumerate().skip(1) {
            let column: Vec<std::time::Duration> =
                samples.iter().map(|spans| spans[span - 1]).collect();
            let raw: Vec<String> = column.iter().map(|d| millis(*d)).collect();
            out.push_str(&format!(
                "{:<20} {:>10}  {}\n",
                phase.span_label(),
                millis(median(column)),
                raw.join(" ")
            ));
        }
        let windows = lock_windows(samples);
        let raw: Vec<String> = windows.iter().map(|d| millis(*d)).collect();
        out.push_str(&format!(
            "{:<20} {:>10}  {}\n",
            "LOCK WINDOW TOTAL",
            millis(median(windows)),
            raw.join(" ")
        ));
        out
    }

    fn seed(paths: &config::Paths, id: &str, summary: &str) {
        Add {
            path: format!("docs/{id}"),
            summary: summary.to_string(),
            content: "body".to_string(),
            tags: "test".to_string(),
            version_ref: None,
            id: Some(id.to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        }
        .execute_with(paths, &embedder::NoopEmbedder)
        .unwrap();
    }

    #[test]
    fn test_reembed_does_not_clobber_fresh_embedding_added_after_selection() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        seed(&paths, "race", "old");
        let fresh = FixedEmbedder(0.75);
        let report =
            run_reembed_with_hook(&paths, &FixedEmbedder(0.25), false, 1800, |batch, _| {
                if batch == 0 {
                    Add {
                        path: "docs/race".to_string(),
                        summary: "fresh".to_string(),
                        content: "body".to_string(),
                        tags: "test".to_string(),
                        version_ref: None,
                        id: Some("race".to_string()),
                        permanent: false,
                        replace_path: false,
                        kind: "convention".to_string(),
                        evidence: vec![],
                        evidence_file: None,
                        cues: vec![],
                    }
                    .execute_with(&paths, &fresh)
                    .unwrap();
                }
            })
            .unwrap();
        assert_eq!(report.embedded, 0);
        assert_eq!(
            report.raced, 1,
            "the raced-away write must be accounted for, not silently dropped"
        );
        let conn = db::open_ro(&paths.db).unwrap();
        let blob: Vec<u8> = conn.query_row(
            "SELECT emb.embedding FROM entries e JOIN entries_emb emb ON emb.rowid=e.rowid WHERE e.id='race'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(blob, normalized_f32s_to_f16_blob(&vec![0.75; 384]).unwrap());
    }

    #[test]
    fn test_reembed_skips_when_content_changed_after_selection() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        seed(&paths, "content-race", "old-summary");
        // Concurrent edit that does NOT write an embedding (e.g. KB_NO_EMBED,
        // or a writer racing ahead of its own reembed pass): the rowid is
        // still absent from entries_emb, so an id-only re-check would let
        // this batch's stale-content vector through.
        let report =
            run_reembed_with_hook(&paths, &FixedEmbedder(0.25), false, 1800, |batch, _| {
                if batch == 0 {
                    Add {
                        path: "docs/content-race".to_string(),
                        summary: "new-summary-after-selection".to_string(),
                        content: "body".to_string(),
                        tags: "test".to_string(),
                        version_ref: None,
                        id: Some("content-race".to_string()),
                        permanent: false,
                        replace_path: false,
                        kind: "convention".to_string(),
                        evidence: vec![],
                        evidence_file: None,
                        cues: vec![],
                    }
                    .execute_with(&paths, &embedder::NoopEmbedder)
                    .unwrap();
                }
            })
            .unwrap();
        assert_eq!(report.embedded, 0);
        assert_eq!(report.raced, 1);
        let conn = db::open_ro(&paths.db).unwrap();
        let has_embedding: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries e JOIN entries_emb emb ON emb.rowid=e.rowid WHERE e.id='content-race')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !has_embedding,
            "a vector computed from stale content must not be written after a concurrent content edit"
        );
    }

    #[test]
    fn test_reembed_database_swap_between_batches_reopens_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        for index in 0..(REEMBED_WRITE_BATCH_SIZE + 3) {
            seed(&paths, &format!("swap-{index}"), "seed");
        }
        {
            let lock = acquire_lock(&paths.lock).unwrap();
            let conn = db::open_rw(&paths, &lock).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
        let replacement = paths.db.with_extension("replacement");
        std::fs::copy(&paths.db, &replacement).unwrap();
        let report = run_reembed_with_hooks(
            &paths,
            &FixedEmbedder(0.5),
            false,
            1800,
            |batch, _| {
                if batch == 1 {
                    std::fs::rename(&replacement, &paths.db).unwrap();
                }
            },
            |_, _| {},
        )
        .unwrap();
        assert_eq!(report.embedded, REEMBED_WRITE_BATCH_SIZE + 3);
        assert_eq!(
            report.failed, 0,
            "pass one's failures must not survive into the reconcile pass"
        );
        let conn = db::open_ro(&paths.db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries_emb", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, report.embedded as i64,
            "report.embedded must equal the rows actually present in the live db"
        );
        assert_eq!(count, (REEMBED_WRITE_BATCH_SIZE + 3) as i64);
    }

    #[test]
    fn test_reembed_swap_drops_ids_that_no_longer_resolve_in_the_replacement_db() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        let total = REEMBED_WRITE_BATCH_SIZE + 2;
        for index in 0..total {
            seed(&paths, &format!("gone-{index}"), "seed");
        }
        {
            let lock = acquire_lock(&paths.lock).unwrap();
            let conn = db::open_rw(&paths, &lock).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
        let replacement = paths.db.with_extension("replacement");
        std::fs::copy(&paths.db, &replacement).unwrap();
        // In the replacement db only (not the one pass one writes against
        // before the swap), mark one entry stale — simulating a rebuild
        // that dropped it. Pass one embeds it fine against the still-live
        // old db; after the swap, it must no longer be counted.
        {
            let conn = db::open_unchecked_for_test(&replacement).unwrap();
            conn.execute("UPDATE entries SET is_stale = 1 WHERE id = 'gone-0'", [])
                .unwrap();
        }
        let report = run_reembed_with_hook(&paths, &FixedEmbedder(0.5), false, 1800, |batch, _| {
            if batch == 1 {
                std::fs::rename(&replacement, &paths.db).unwrap();
            }
        })
        .unwrap();
        assert_eq!(
            report.embedded,
            total - 1,
            "gone-0 no longer resolves live and must not stay counted"
        );
        assert_eq!(report.failed, 0);
        let conn = db::open_ro(&paths.db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries_emb", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, report.embedded as i64,
            "report.embedded must equal the rows actually present in the live db, not overcount a vanished id"
        );
    }

    #[test]
    fn test_reembed_raced_count_from_pass_one_is_discarded_after_a_swap_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        let total = REEMBED_WRITE_BATCH_SIZE; // exactly one batch
        for index in 0..total {
            seed(&paths, &format!("raced-swap-{index}"), "seed");
        }
        {
            let lock = acquire_lock(&paths.lock).unwrap();
            let conn = db::open_rw(&paths, &lock).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
        // Snapshot taken before the concurrent write below and before pass
        // one's own batch commits, so the swapped-in replacement carries
        // none of pass one's writes at all — including no embedding for
        // raced-swap-0, whose only race in this test is against the db
        // pass one actually wrote to.
        let replacement = paths.db.with_extension("replacement");
        std::fs::copy(&paths.db, &replacement).unwrap();
        let fresh = FixedEmbedder(0.75);
        let report = run_reembed_with_hooks(
            &paths,
            &FixedEmbedder(0.25),
            false,
            1800,
            |batch, _| {
                if batch == 0 {
                    // A concurrent writer embeds raced-swap-0 against the
                    // still-live original db before pass one's only batch
                    // writes it, so pass one legitimately races on this id
                    // (mirrors
                    // test_reembed_does_not_clobber_fresh_embedding_added_after_selection).
                    Add {
                        path: "docs/raced-swap-0".to_string(),
                        summary: "fresh".to_string(),
                        content: "body".to_string(),
                        tags: "test".to_string(),
                        version_ref: None,
                        id: Some("raced-swap-0".to_string()),
                        permanent: false,
                        replace_path: false,
                        kind: "convention".to_string(),
                        evidence: vec![],
                        evidence_file: None,
                        cues: vec![],
                    }
                    .execute_with(&paths, &fresh)
                    .unwrap();
                }
            },
            |batch, phase| {
                if batch == 0 && phase == BatchPhase::ConnDropped {
                    // Swap in the pre-write snapshot only after pass one's
                    // batch has fully committed against the original db, so
                    // the reconcile pass starts from a live db with none
                    // of pass one's writes already present in it.
                    std::fs::rename(&replacement, &paths.db).unwrap();
                }
            },
        )
        .unwrap();

        assert_eq!(
            report.embedded, total,
            "every id, including the one pass one raced on, ends up embedded in the live db"
        );
        assert_eq!(report.failed, 0);
        assert_eq!(
            report.raced, 0,
            "pass one's raced count reflects the stale pre-swap db; pass two starts from an \
             empty live db and re-embeds everything cleanly, so pass one's count must not carry \
             forward (same reasoning already applied to failed/failures)"
        );
    }

    #[test]
    fn test_reembed_counts_embedding_failures_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        seed(&paths, "good", "good");
        seed(&paths, "bad", "fail");
        let report = run_reembed(&paths, &FailingEmbedder, false, 1800).unwrap();
        assert_eq!((report.embedded, report.failed), (1, 1));
        assert_eq!(report.failures[0].id, "bad");
        assert!(report.failures[0]
            .cause
            .contains("fixture embedding failure"));
    }

    #[test]
    fn test_reembed_counts_sqlite_write_errors_with_causes_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        seed(&paths, "write-fails", "good");
        {
            let lock = acquire_lock(&paths.lock).unwrap();
            let conn = db::open_rw(&paths, &lock).unwrap();
            conn.execute_batch(
                "CREATE TRIGGER reject_reembed BEFORE INSERT ON entries_emb
                 BEGIN SELECT RAISE(ABORT, 'fixture write rejected'); END",
            )
            .unwrap();
        }
        let report = run_reembed(&paths, &FixedEmbedder(0.5), false, 1800).unwrap();
        assert_eq!((report.embedded, report.failed), (0, 1));
        assert_eq!(report.failures[0].id, "write-fails");
        assert!(report.failures[0].cause.contains("fixture write rejected"));
    }

    #[test]
    fn test_reembed_batch_runs_in_one_transaction_surviving_a_mid_batch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        seed(&paths, "before", "good");
        seed(&paths, "middle", "good");
        seed(&paths, "after", "good");
        {
            let lock = acquire_lock(&paths.lock).unwrap();
            let conn = db::open_rw(&paths, &lock).unwrap();
            conn.execute_batch(
                "CREATE TRIGGER reject_middle BEFORE INSERT ON entries_emb
                 WHEN (SELECT id FROM entries WHERE rowid = NEW.rowid) = 'middle'
                 BEGIN SELECT RAISE(ABORT, 'fixture reject middle'); END",
            )
            .unwrap();
        }
        let report = run_reembed(&paths, &FixedEmbedder(0.5), false, 1800).unwrap();
        assert_eq!(
            report.embedded, 2,
            "before and after must both survive middle's failure in the same batch/transaction"
        );
        assert_eq!(report.failed, 1);
        assert_eq!(report.failures[0].id, "middle");
        let conn = db::open_ro(&paths.db).unwrap();
        for ok_id in ["before", "after"] {
            let has: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM entries e JOIN entries_emb emb ON emb.rowid=e.rowid WHERE e.id=?1)",
                    [ok_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                has,
                "{ok_id} must survive a mid-batch failure in the same transaction"
            );
        }
    }

    /// Times the real batch — lock acquisition through transaction commit and
    /// drop, via the before/after hooks — against the
    /// <= 50 ms budget documented on `REEMBED_WRITE_BATCH_SIZE`. This is a
    /// measurement to be taken on a quiet host, not a CI gate: on a machine
    /// with concurrent builds or tests competing for disk/CPU, the same
    /// batch measures hundreds of ms slower for reasons unrelated to this
    /// code (see the doc comment on the constant for recorded samples).
    ///
    /// An earlier version of this test measured a synthetic loop of
    /// `kb_meta` inserts on a hand-built connection: it never called
    /// write_batches, never touched entries_emb, and started the timer
    /// after acquire_lock/open_rw had already returned — a review finding.
    /// This version exercises the real code path.
    /// Run explicitly with `cargo test -p kb test_reembed_batch_lock_hold_budget -- --ignored --nocapture`
    /// on an idle host and record the printed duration.
    #[test]
    #[ignore = "lock-hold budget measurement; run explicitly on a quiet host"]
    fn test_reembed_batch_lock_hold_budget() {
        let samples = measure_batch_phases("budget", BUDGET_SAMPLE_BATCHES);
        let windows = lock_windows(&samples);
        let table = phase_table(&samples);
        eprintln!("reembed batch lock hold measurement (acquire -> connection drop)\n{table}");
        let worst = windows.iter().copied().max().expect("at least one batch");
        assert!(
            worst <= std::time::Duration::from_millis(50),
            "lock-hold budget exceeded: worst sample {worst:?} > 50ms\n{table}"
        );
    }

    /// Attributes the same acquire-to-drop window to its phases, so a budget
    /// miss can be blamed on a specific operation rather than guessed at.
    /// Run explicitly with
    /// `cargo test --release -p kb test_reembed_batch_lock_hold_phase_breakdown -- --ignored --nocapture`
    /// on a quiet host.
    #[test]
    #[ignore = "lock-hold phase breakdown measurement; run explicitly on a quiet host"]
    fn test_reembed_batch_lock_hold_phase_breakdown() {
        let samples = measure_batch_phases("phases", 8);
        eprintln!(
            "reembed batch lock-hold phase breakdown ({} batches x {} rows)\n{}",
            samples.len(),
            REEMBED_WRITE_BATCH_SIZE,
            phase_table(&samples)
        );
    }

    /// Non-ignored structural companion to the budget measurement above:
    /// asserts that `write_batches` actually chunks writes by
    /// `REEMBED_WRITE_BATCH_SIZE` (via the batch-index hook), rather than
    /// some other size, without depending on wall-clock timing.
    #[test]
    fn test_reembed_batches_writes_by_the_configured_batch_size() {
        let dir = tempfile::tempdir().unwrap();
        let paths = config::Paths::from_root(dir.path());
        db::open_or_init(&paths).unwrap();
        let total = REEMBED_WRITE_BATCH_SIZE + 1;
        for index in 0..total {
            seed(&paths, &format!("batch-{index}"), "seed");
        }
        let batches_seen = std::cell::Cell::new(0usize);
        let report = run_reembed_with_hook(
            &paths,
            &FixedEmbedder(0.5),
            false,
            1800,
            |batch_index, _| {
                batches_seen.set(batches_seen.get().max(batch_index + 1));
            },
        )
        .unwrap();
        assert_eq!(report.embedded, total);
        let expected_batches = total.div_ceil(REEMBED_WRITE_BATCH_SIZE);
        assert_eq!(
            batches_seen.get(),
            expected_batches,
            "write_batches must chunk writes using REEMBED_WRITE_BATCH_SIZE"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: proptest_cases(64),
            .. proptest::prelude::ProptestConfig::default()
        })]
        #[test]
        fn proptest_reembed_batches_cover_every_id_exactly_once(count in 0usize..48) {
            // The previous version of this property asserted
            // chunks().flatten() == input — a std-library fact about
            // slice::chunks that never called any reembed code (review
            // finding). This version seeds real entries and collects the
            // ids write_batches actually puts in each batch via the hook,
            // proving the real partition covers every id exactly once.
            let dir = tempfile::tempdir().unwrap();
            let paths = config::Paths::from_root(dir.path());
            db::open_or_init(&paths).unwrap();
            let ids: Vec<String> = (0..count).map(|i| format!("prop-{i}")).collect();
            for id in &ids {
                seed(&paths, id, "seed");
            }
            let seen: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
            run_reembed_with_hook(&paths, &FixedEmbedder(0.5), false, 1800, |_batch_index, batch| {
                seen.borrow_mut().extend(batch.iter().map(|w| w.id.clone()));
            })
            .unwrap();
            let mut union = seen.into_inner();
            let mut expected = ids;
            union.sort();
            expected.sort();
            proptest::prop_assert_eq!(union, expected, "every id must appear in exactly one batch, with no duplicates or omissions");
        }
    }
}
