//! The applied cursor (C1/D3) and the one write helper that maintains it.
//!
//! # What the cursor is
//!
//! Three `kb_meta` rows — `applied_log_generation`, `applied_log_offset`,
//! `applied_log_tail_sha` — written **inside the same SQLite transaction that
//! applies a batch**, so the cursor and the state it describes commit or roll
//! back together. Without them, an append that survives a crash while its
//! `apply_event` does not leaves the DB permanently behind the log with nothing
//! to detect it (plan §4 D3).
//!
//! * `applied_log_offset` is a [`events::ReadEvents::committed_len`] boundary:
//!   the offset **immediately after** the last committed byte, i.e. after the
//!   `batch_commit` line's newline. Pinning it there matters — the other
//!   choice puts the cursor one byte past EOF after an ordinary torn tail and
//!   forces a full rebuild on every crash.
//! * `applied_log_generation` is a monotonic counter kept in a sidecar next to
//!   the log and bumped by compaction **under the same lock as its rename**.
//!   Compaction only removes lines, so a cursor whose bytes all survive
//!   validates against a compacted log and would replay the compacted tail onto
//!   a DB that already holds the original tail. The generation makes that
//!   detection O(1) and total.
//! * `applied_log_tail_sha` is a bounded fingerprint of the prefix
//!   `[0, offset)`: the prefix length, the first [`TAIL_SHA_WINDOW`] bytes, and
//!   the last [`TAIL_SHA_WINDOW`] bytes. Two anchored windows rather than one,
//!   because a rewrite that removes lines shifts everything after it — anchoring
//!   at byte 0 catches an early rewrite that a tail-only window would miss — and
//!   the length is folded in so a rewrite that changes it cannot match on
//!   coincidentally identical window bytes. It stays *bounded* on purpose: the
//!   generation counter, not a longer window, is what makes detection total.
//!   **If the generation counter is ever dropped, its replacement is a
//!   whole-prefix hash over `[0, offset)` — never a wider window**, which is
//!   strictly weaker than the whole-prefix hash `rebuild` already uses and
//!   misses compaction.
//!
//!   Residual: a log rewritten by an older pinned binary that leaves no
//!   generation sidecar, keeps the length, and reproduces both windows byte for
//!   byte would still validate. Closing that needs the generation inside the
//!   log, which D7 rules out for this epic.
//!
//! # The single write path
//!
//! [`append_and_apply`] owns append + sync + apply + cursor as one unit, and
//! every production writer routes through it. Any writer that appends and
//! applies without touching the cursor leaves it permanently behind, and then
//! *every subsequent open* replays that writer's events — a rare crash gap
//! turned into a guaranteed loop.
//!
//! Embeddings are pre-resolved **before** the transaction opens
//! ([`PrefetchedEmbedder`]), because `apply_event` embeds inside its savepoint
//! and wrapping the batch in one outer transaction would otherwise hold a
//! SQLite write transaction across up to nine model calls. The prefetch is
//! sealed at `BEGIN`, so a miss inside the transaction is a loud error rather
//! than a silent stall.
//!
//! [`inspect`]'s convergence check plus its bounded [`tail_sha`] fingerprint
//! read run on every call to [`append_and_apply`], independent of batch size —
//! that is the gate this module's docs describe below. Measured under load
//! (concurrent builds/tests sharing the machine), the gate costs roughly
//! 25-40 ms per write; a caller doing many small writes should batch events
//! into one `append_and_apply` call rather than pay the gate once per event.
//!
//! # The recovery table, and when a write is refused
//!
//! [`inspect`] classifies a database against its log. The plan's D3 table has
//! eight rows; a ninth, [`Decision::LogMissing`], was split out of row 6 because
//! it is the one "cannot tell" state where continuing is destructive rather than
//! merely uninformed — the append path CREATES a missing log, so a write would
//! resurrect it holding one batch and orphan everything earlier.
//!
//! **Every decision except a converged database and an obsolete schema stamp
//! refuses writes** ([`Decision::blocks_writes`]). In particular row 6 refuses.
//! Letting a write through during a deferral would apply a batch while events
//! before the damage are durable but unapplied, leaving the database holding the
//! committed prefix plus a later, disjoint batch: ahead of the durable log in
//! one place and behind it in another, so no prefix of the log at all. Declining
//! to advance the cursor does not repair that — only refusing does.
//!
//! An obsolete schema stamp is the exception, and it is evaluated **last** for
//! that reason: it is the only non-blocking decision, so any blocking condition
//! ranked behind it would be masked on every database whose stamp is stale,
//! which is every pre-v3 one.
//!
//! Row 6 has four causes, and all four refuse writes:
//!
//! 1. the cursor row itself cannot be read;
//! 2. the log's `committed_len` cannot be determined;
//! 3. the prefix the cursor names cannot be hashed;
//! 4. a line past the cursor cannot be parsed — the message names the first
//!    damaged byte, so an operator is pointed at the line rather than the log.
//!
//! What row 6 still buys is that **reads keep working**. Recovery warns and
//! declines instead of propagating a parse error out of every entry point,
//! `search` and `kb_get` serve what they have with a staleness note, and the
//! repair stays available because the log is never written to in the meantime.
//! Once the damage is repaired the prefix fingerprint or the tail read agrees
//! again, recovery replays from the cursor, and the cursor is stamped at the
//! log's committed end.
//!
//! `kb compact` takes the same gate. It rewrites the log of record and bumps its
//! generation, so running it during a deferral would convert "nobody can tell
//! what the log holds" into "whatever survived the rewrite is the whole truth".
//!
//! # Measured cost of the gate (reference for the T2b write lane)
//!
//! The gate is charged per WRITE, not per event. [`inspect`] verifies the final
//! framing span in O(span bytes), then reads the two [`TAIL_SHA_WINDOW`]
//! fingerprint anchors; the write hashes two more bounded anchors for the
//! cursor it stamps. The fingerprint windows are fixed by design because they
//! are sampling anchors, not containers that must hold a complete span.
//!
//! Before bd-21ef.1.20 the framing check read one fixed 64 KiB suffix but
//! counted event lines. A closing span larger than that suffix therefore
//! missed its `batch_begin` and forced a whole-log scan on every write. The
//! growing suffix now bounds that work by the bytes in the closing span.
//!
//! Measured on a loaded shared machine, 1000 events of ~450 bytes each:
//!
//! | Shape | Per write | Per event |
//! |---|---|---|
//! | 1000 single-event writes | ~44 ms | ~44 ms |
//! | 1000 events in one batch | ~2.6 s | ~2.6 ms |
//!
//! Of that ~44 ms, ~43 ms is the gate itself — `inspect` ~36 ms and the cursor's
//! own fingerprint ~7 ms — so at this log size the append and its fsync are
//! nearly free beside it. Treat the absolute numbers as an upper bound, since
//! the box was running several suites at once, and the ratio as the finding:
//! **batching is worth roughly 17x per event, and a caller that writes one event
//! at a time pays the full fixed cost every time.** That is why the vacuum
//! fixtures seed in batches, and it is the number the T2b write-lane benchmark
//! should be re-measured against on a quiet machine.
//!
//! The lead to pull if that re-run finds the cost too high: the initial framing
//! probe still reads 64 KiB when the closing span is usually a few hundred
//! bytes.

use crate::commands::add::Lock;
use crate::components::db;
use crate::components::embedder::{Embedder, PrefetchedEmbedder};
use crate::components::events;
use crate::components::fsync::sync_parent_dir;
use crate::config;
use crate::crash_sim::{kill_point, KillPoint};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// `kb_meta` key holding the log generation the cursor was taken against.
pub const APPLIED_LOG_GENERATION: &str = "applied_log_generation";
/// `kb_meta` key holding the committed-byte boundary applied so far.
pub const APPLIED_LOG_OFFSET: &str = "applied_log_offset";
/// `kb_meta` key holding the bounded tail hash at that boundary.
pub const APPLIED_LOG_TAIL_SHA: &str = "applied_log_tail_sha";

/// Size of each of the two anchored windows in `applied_log_tail_sha`.
///
/// Bounded on purpose — see the module docs on why widening this is never the
/// right answer to a dropped generation counter.
pub const TAIL_SHA_WINDOW: u64 = 64 * 1024;

/// `K` from `.state/agent-kb/tla/DurableBatch.tla`: apply attempts on one
/// record before the poison policy dead-letters it and advances the cursor.
pub const POISON_MAX_ATTEMPTS: u32 = 2;

/// The applied cursor as it lives in `kb_meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub generation: u64,
    pub offset: u64,
    pub tail_sha: String,
}

/// Why [`inspect`] asked for a full rebuild. One variant per full-rebuild row
/// of the D3 recovery table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildReason {
    /// Row 1 — no cursor rows present (every database written before D3).
    CursorMissing,
    /// Row 2 — the schema stamp is obsolete.
    SchemaObsolete,
    /// Row 3 — the log was compacted or otherwise rewritten.
    GenerationMismatch,
    /// Row 4 — the bytes at the cursor offset are not the bytes it recorded.
    TailShaMismatch,
    /// Row 5 — the cursor points past the log's committed end.
    OffsetBeyondLog,
}

impl RebuildReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CursorMissing => "no applied-cursor rows in the database",
            Self::SchemaObsolete => "the database schema stamp is obsolete",
            Self::GenerationMismatch => "the event log generation changed (compaction or rewrite)",
            Self::TailShaMismatch => "the event log bytes at the cursor offset changed",
            Self::OffsetBeyondLog => "the cursor points past the log's committed end",
        }
    }
}

/// A write was refused because the database is not converged with the log.
///
/// The applied cursor is a claim about what the database contains. Overwriting
/// it while it says "rebuild due" or "tail replay due" re-baselines a diverged
/// database to converged and makes the divergence unrecoverable — there is no
/// second record of the gap. Refusing is the only safe answer: the log is
/// intact, so the repair is always still available.
#[derive(Debug, thiserror::Error)]
#[error("refusing to write: the database is not converged with the event log — {reason}")]
pub struct NotConverged {
    pub reason: String,
}

/// True when `err` is (or wraps) [`NotConverged`].
pub fn is_not_converged(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotConverged>().is_some()
}

/// The event log could not be read from the cursor onward.
///
/// Distinguished from every other replay failure so recovery can give it row
/// 6's disposition — warn and defer — instead of propagating a parse error out
/// of every write entry point.
#[derive(Debug, thiserror::Error)]
#[error("cannot read the event log past offset {offset}: {detail}")]
pub struct LogUnreadable {
    pub offset: u64,
    pub detail: String,
}

/// True when `err` is (or wraps) [`LogUnreadable`].
pub fn is_log_unreadable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<LogUnreadable>().is_some()
}

/// The D3 recovery decision. Eight table rows, four outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Row 8 — `committed_len == offset`.
    NoOp,
    /// Row 7 — `committed_len > offset`: replay the tail, then advance.
    ReplayTail { from: u64, to: u64 },
    /// Rows 1-5.
    FullRebuild(RebuildReason),
    /// Row 6 — the log is unreadable past the cursor. Defer with a warning
    /// rather than take down every entry point.
    Defer(String),
    /// The log file is absent while the cursor claims a non-zero offset.
    ///
    /// Split out of [`Decision::Defer`] because it is the one "cannot tell"
    /// state where continuing is destructive rather than merely uninformed: the
    /// append path CREATES a missing log, so the next write would resurrect it
    /// holding only that batch and stamp the cursor converged against it. Every
    /// earlier entry would then be unbacked by the log, `inspect` would report
    /// NoOp, and the next full rebuild would delete them.
    LogMissing(PathBuf),
}

impl Decision {
    /// Whether the database is materially behind (or ahead of) the log.
    pub fn is_behind(&self) -> bool {
        !matches!(self, Decision::NoOp)
    }

    /// Whether a write must be refused rather than allowed to overwrite the
    /// cursor.
    ///
    /// Only the rows that say the cursor is a WRONG claim about the log block.
    ///
    /// Everything except a converged database and an obsolete schema stamp
    /// blocks.
    ///
    /// [`Decision::Defer`] blocks. Applying a batch while events before the
    /// damage are durable but unapplied would materialize a set that is no
    /// prefix of the log at all — the committed prefix plus a later, disjoint
    /// batch — so the database would be ahead of the durable log in one place
    /// and behind it in another. Not advancing the cursor is not enough to fix
    /// that; only refusing is.
    ///
    /// [`Decision::LogMissing`] blocks: the append path would recreate the log
    /// from nothing and the cursor would then be stamped converged against a
    /// log holding one batch, orphaning every earlier entry.
    ///
    /// [`RebuildReason::SchemaObsolete`] does not block: the cursor is still an
    /// accurate claim about how much of the log is applied, and only derived
    /// state (cue rows, the embedding vintage) is missing. Blocking it would
    /// make a pre-v3 database unwritable under `KB_NO_EMBED`, where the upgrade
    /// rebuild deliberately defers to avoid dropping embeddings.
    pub fn blocks_writes(&self) -> bool {
        !matches!(
            self,
            Decision::NoOp | Decision::FullRebuild(RebuildReason::SchemaObsolete)
        )
    }

    /// One clause naming why, for an error message or a staleness note.
    pub fn describe(&self) -> String {
        match self {
            Decision::NoOp => "converged".to_string(),
            Decision::ReplayTail { from, to } => {
                format!("the database is behind the event log, {from} of {to} bytes applied")
            }
            Decision::FullRebuild(reason) => reason.as_str().to_string(),
            Decision::Defer(message) => message.clone(),
            Decision::LogMissing(path) => {
                format!("the event log is missing at {}", path.display())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// kb_meta rows
// ---------------------------------------------------------------------------

/// Whether the database holds any entry rows.
///
/// Conservative on error: an unreadable `entries` table reads as empty, which
/// only ever admits a write that the write itself would fail on anyway.
fn has_entries(conn: &Connection) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM entries)", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0)
        > 0
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM kb_meta WHERE key=?1",
            params![key],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

/// Read the three cursor rows. `None` when any of them is absent — a partial
/// cursor is treated as no cursor, which routes to the full-rebuild row.
pub fn read(conn: &Connection) -> Result<Option<Cursor>> {
    let (Some(generation), Some(offset), Some(tail_sha)) = (
        meta(conn, APPLIED_LOG_GENERATION)?,
        meta(conn, APPLIED_LOG_OFFSET)?,
        meta(conn, APPLIED_LOG_TAIL_SHA)?,
    ) else {
        return Ok(None);
    };
    let (Ok(generation), Ok(offset)) = (generation.parse::<u64>(), offset.parse::<u64>()) else {
        return Ok(None);
    };
    Ok(Some(Cursor {
        generation,
        offset,
        tail_sha,
    }))
}

/// Write the three cursor rows. The caller owns the transaction: this is only
/// ever correct inside the same transaction as the apply it describes.
pub fn write(conn: &Connection, cursor: &Cursor) -> Result<()> {
    for (key, value) in [
        (APPLIED_LOG_GENERATION, cursor.generation.to_string()),
        (APPLIED_LOG_OFFSET, cursor.offset.to_string()),
        (APPLIED_LOG_TAIL_SHA, cursor.tail_sha.clone()),
    ] {
        conn.execute(
            "INSERT OR REPLACE INTO kb_meta(key, value) VALUES(?1, ?2)",
            params![key, value],
        )?;
    }
    Ok(())
}

/// Seed a brand-new database's cursor at "nothing applied yet".
///
/// A fresh database is empty by construction, so offset 0 is the truthful
/// cursor: recovery then replays the whole log incrementally instead of taking
/// the cursorless full-rebuild row. Generation 0 is deliberately pessimistic —
/// a fresh database in a compacted repository takes one full rebuild rather
/// than trusting a generation it never observed.
pub fn seed_fresh(conn: &Connection) -> Result<()> {
    write(
        conn,
        &Cursor {
            generation: 0,
            offset: 0,
            tail_sha: empty_tail_sha(),
        },
    )
}

// ---------------------------------------------------------------------------
// The log generation sidecar
// ---------------------------------------------------------------------------

fn sidecar(events_path: &Path, suffix: &str) -> PathBuf {
    let name = events_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent-kb-events.jsonl");
    events_path.with_file_name(format!("{name}.{suffix}"))
}

/// Path of the generation counter that lives beside the log.
pub fn generation_path(events_path: &Path) -> PathBuf {
    sidecar(events_path, "generation")
}

/// Read the log's generation. An absent or unparseable sidecar reads as 0,
/// which is the generation every never-compacted log has always had.
pub fn read_generation(events_path: &Path) -> u64 {
    fs::read_to_string(generation_path(events_path))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Bump the log's generation and make the bump durable.
///
/// Compaction calls this **under the same lock as its rename**, so no reader
/// can observe a rewritten log still carrying the pre-rewrite generation.
pub fn bump_generation(events_path: &Path) -> Result<u64> {
    let next = read_generation(events_path).saturating_add(1);
    let path = generation_path(events_path);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = sidecar(events_path, "generation.tmp");
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("create generation sidecar {}", tmp.display()))?;
        use std::io::Write;
        writeln!(f, "{next}")?;
        f.sync_data()?;
    }
    fs::rename(&tmp, &path)
        .with_context(|| format!("install generation sidecar {}", path.display()))?;
    sync_parent_dir(&path)?;
    Ok(next)
}

// ---------------------------------------------------------------------------
// The bounded tail hash
// ---------------------------------------------------------------------------

fn hex(digest: impl AsRef<[u8]>) -> String {
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn empty_tail_sha() -> String {
    hex(Sha256::digest([]))
}

/// Bounded fingerprint of the log prefix `[0, offset)`.
///
/// Covers the prefix length plus two anchored [`TAIL_SHA_WINDOW`] windows, one
/// at byte 0 and one ending at `offset`. Both anchors matter: a rewrite that
/// removes lines early shifts every byte after it, so the head window catches
/// what a tail-only window would sail past, and binding the length in means a
/// length-changing rewrite cannot match on coincidentally identical windows.
///
/// Errors when the log is shorter than `offset`; callers classify that as
/// [`RebuildReason::OffsetBeyondLog`] before ever reaching here.
pub fn tail_sha(events_path: &Path, offset: u64) -> Result<String> {
    if offset == 0 {
        return Ok(empty_tail_sha());
    }
    let mut file = fs::File::open(events_path).with_context(|| {
        format!(
            "open events {} for the prefix fingerprint",
            events_path.display()
        )
    })?;
    let len = file.metadata()?.len();
    anyhow::ensure!(
        len >= offset,
        "tail_sha: {} is {len} bytes, shorter than the cursor offset {offset}",
        events_path.display()
    );
    let window = offset.min(TAIL_SHA_WINDOW);
    let mut hasher = Sha256::new();
    hasher.update(offset.to_le_bytes());
    for start in [0, offset - window] {
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0_u8; window as usize];
        file.read_exact(&mut buf)?;
        hasher.update(&buf);
    }
    Ok(hex(hasher.finalize()))
}

// ---------------------------------------------------------------------------
// The poison / dead-letter sidecar
// ---------------------------------------------------------------------------

/// One record the apply path could not digest.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DeadLetterRecord {
    pub attempts: u32,
    pub quarantined: bool,
    pub last_error: String,
    pub quarantined_at: Option<String>,
    pub event: Value,
}

/// The dead-letter ledger, keyed by [`fingerprint`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DeadLetter {
    pub records: BTreeMap<String, DeadLetterRecord>,
}

/// Path of the dead-letter sidecar beside the log.
pub fn dead_letter_path(events_path: &Path) -> PathBuf {
    sidecar(events_path, "deadletter")
}

/// Stable identity of an event line. `serde_json::Value` maps are ordered, so
/// re-serializing a parsed event is canonical and survives compaction, which
/// moves lines but never edits them.
pub fn fingerprint(event: &Value) -> String {
    hex(Sha256::digest(event.to_string().as_bytes()))
}

impl DeadLetter {
    pub fn load(events_path: &Path) -> Self {
        fs::read(dead_letter_path(events_path))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    /// Durably: the attempt counter is what bounds a poison record's retries
    /// across process restarts, so it has to survive the crash that a retry
    /// loop is most likely to hit. Same tmp, sync, rename, sync-parent order as
    /// [`bump_generation`].
    pub fn save(&self, events_path: &Path) -> Result<()> {
        let path = dead_letter_path(events_path);
        let tmp = sidecar(events_path, "deadletter.tmp");
        {
            let mut file = fs::File::create(&tmp)
                .with_context(|| format!("create dead-letter sidecar {}", tmp.display()))?;
            use std::io::Write;
            file.write_all(&serde_json::to_vec_pretty(self)?)
                .with_context(|| format!("write dead-letter sidecar {}", tmp.display()))?;
            file.sync_data()
                .with_context(|| format!("sync dead-letter sidecar {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("install dead-letter sidecar {}", path.display()))?;
        sync_parent_dir(&path)?;
        Ok(())
    }

    /// Fingerprints that materialization must skip.
    pub fn quarantined(&self) -> HashSet<String> {
        self.records
            .iter()
            .filter(|(_, r)| r.quarantined)
            .map(|(k, _)| k.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Detection (no lock, no mutation)
// ---------------------------------------------------------------------------

/// Classify the database against the log per the D3 eight-row table.
///
/// Pure detection: no lock, no write, no error. A read-only surface calls this
/// and warns; a write surface calls it and repairs (ADR-7 — reads never
/// recover, and never take the write lock).
///
/// Row order deviates from the plan's table in one place: `offset >
/// committed_len` is evaluated **before** the tail-hash row, because a tail
/// hash cannot be computed at an offset the log does not reach. Both rows lead
/// to a full rebuild, so the decision is unchanged.
pub fn inspect(conn: &Connection, paths: &config::Paths) -> Decision {
    let cursor = match read(conn) {
        Ok(Some(cursor)) => cursor,
        Ok(None) => return Decision::FullRebuild(RebuildReason::CursorMissing),
        // Defer cause 1 of 4: the cursor row itself is unreadable.
        Err(error) => {
            return Decision::Defer(format!(
                "the applied cursor cannot be read from {}: {error}. Run `kb rebuild`.",
                paths.db.display()
            ))
        }
    };
    if !paths.events.exists() && (cursor.offset > 0 || has_entries(conn)) {
        // Reached only with a cursor row in hand: `read` above returns early
        // when there is none, and a cursorless database is row 1 — a full
        // rebuild — which is what a database that never had a cursor needs.
        //
        // The log is not there to compare against. Recovery must not rebuild —
        // that is the unreachable-layout hazard `rebuild` already refuses to
        // auto-repair, because it would drop every entry the vanished log
        // covered — but a write must not proceed either: the append path
        // recreates a missing log, and stamping the cursor against a one-batch
        // log orphans everything that came before.
        //
        // The offset alone is not enough to spot that. An offset-zero cursor on
        // a populated database is a real shape — a database seeded before its
        // first replay, or one written by a binary that predates the cursor —
        // and it is exactly the state with the most to lose. What must stay
        // permitted is the genuinely first write in an empty repository, where
        // no log yet is the normal condition rather than a vanished one.
        return Decision::LogMissing(paths.events.clone());
    }
    let committed_len = match events::committed_len(&paths.events) {
        Ok(committed_len) => committed_len,
        // Defer cause 2 of 4: the log's committed length cannot be determined.
        Err(error) => {
            return Decision::Defer(format!(
                "the event log at {} cannot be read: {error}. Repair the log, or \
                 run `kb rebuild`.",
                paths.events.display()
            ))
        }
    };
    if cursor.generation != read_generation(&paths.events) {
        return Decision::FullRebuild(RebuildReason::GenerationMismatch);
    }
    if cursor.offset > committed_len {
        return Decision::FullRebuild(RebuildReason::OffsetBeyondLog);
    }
    match tail_sha(&paths.events, cursor.offset) {
        Ok(sha) if sha == cursor.tail_sha => {}
        Ok(_) => return Decision::FullRebuild(RebuildReason::TailShaMismatch),
        // Defer cause 3 of 4: the prefix the cursor names cannot be hashed.
        Err(error) => {
            return Decision::Defer(format!(
                "the event log prefix at offset {} cannot be hashed: {error}. \
                 Repair the log, or run `kb rebuild`.",
                cursor.offset
            ))
        }
    }
    if committed_len > cursor.offset {
        // `committed_len` takes an intact-span shortcut, so it can report a
        // length for a log whose MIDDLE is malformed. Read the tail before
        // promising a replay of it: that read is bounded by the bytes appended
        // since the last apply, which is the cost D3 budgets for, and it is what
        // keeps row 6 reachable now that the shortcut exists.
        return match events::read_events_from_offset(&paths.events, cursor.offset) {
            Ok(tail) => Decision::ReplayTail {
                from: cursor.offset,
                to: tail.committed_len,
            },
            // Defer cause 4 of 4: a line past the cursor is unreadable. Name
            // the byte the operator has to look at, not the whole log.
            Err(error) => {
                let damage = events::first_unreadable_offset(&paths.events, cursor.offset);
                let where_ = match damage {
                    Some(offset) => format!("from byte {offset}"),
                    None => format!("past byte {}", cursor.offset),
                };
                Decision::Defer(format!(
                    "the event log at {} is unreadable {where_}: {error}. Repair \
                     that line, or run `kb rebuild`.",
                    paths.events.display()
                ))
            }
        };
    }
    // Last, deliberately. This is the one decision that does NOT block a write,
    // so any blocking condition evaluated after it would be masked on every
    // database whose stamp is stale — which is every pre-v3 one.
    if !db::schema_is_current(conn) {
        return Decision::FullRebuild(RebuildReason::SchemaObsolete);
    }
    Decision::NoOp
}

/// The one-line staleness note a read-only surface prints.
///
/// Reads serve what they have. They never take the write lock and never
/// repair — under `open_ro`'s `PRAGMA query_only` they could not anyway.
pub fn warn_if_behind(conn: &Connection, paths: &config::Paths) {
    let decision = inspect(conn, paths);
    if !decision.is_behind() {
        return;
    }
    let detail = decision.describe();
    eprintln!(
        "kb: WARNING serving possibly stale results — {detail}. Run `kb rebuild` \
         (or any write command) to converge."
    );
}

// ---------------------------------------------------------------------------
// The one write helper
// ---------------------------------------------------------------------------

fn verify_lock(lock: &Lock, paths: &config::Paths) -> Result<()> {
    let expected = fs::canonicalize(&paths.lock).with_context(|| {
        format!(
            "canonicalize write lock {} (the applied-cursor write path requires a live lock guard)",
            paths.lock.display()
        )
    })?;
    anyhow::ensure!(
        lock.path() == expected,
        "applied cursor: the supplied lock guards {}, but this repository's write lock is {}",
        lock.path().display(),
        expected.display()
    );
    Ok(())
}

/// Append a batch, sync it, apply it, and advance the cursor — as one unit.
///
/// Every production writer goes through here. See [`append_and_apply_with`]
/// for the variant that lets a caller join the cursor's transaction.
pub fn append_and_apply(
    lock: &Lock,
    conn: &Connection,
    paths: &config::Paths,
    embedder: &dyn Embedder,
    batch: &[Value],
) -> Result<()> {
    append_and_apply_with(lock, conn, paths, embedder, batch, |_| Ok(()))
}

/// [`append_and_apply`], plus caller work inside the same transaction.
///
/// D3 owns the outer transaction; a caller that needs its own writes to commit
/// with the apply (C2's `A1` audit record) joins it here rather than opening a
/// nested transaction, which SQLite rejects.
pub fn append_and_apply_with<T>(
    lock: &Lock,
    conn: &Connection,
    paths: &config::Paths,
    embedder: &dyn Embedder,
    batch: &[Value],
    inside: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    verify_lock(lock, paths)?;

    // A write may only start from a converged database. Without this the helper
    // appends and then stamps {current generation, new EOF, new tail hash} over
    // whatever the cursor said, so a database that was one `kb compact` or one
    // crashed writer behind is silently re-baselined to converged and the
    // divergence becomes unrecoverable. Three routes reach here diverged: a
    // long-lived MCP server that recovered only at startup, a CLI dispatch whose
    // recovery failed and only warned, and the KB_NO_EMBED rebuild defer. Each
    // now gets a refusal naming the repair instead of silent corruption.
    let decision = inspect(conn, paths);
    if decision.blocks_writes() {
        return Err(NotConverged {
            reason: decision.describe(),
        }
        .into());
    }

    // An empty batch appends nothing and moves no cursor, but the caller still
    // gets its transaction — A1's audit record has verdicts with no expire.
    if batch.is_empty() {
        let tx = conn.unchecked_transaction()?;
        return match inside(conn) {
            Ok(value) => {
                tx.commit()?;
                Ok(value)
            }
            Err(error) => {
                let _ = tx.rollback();
                Err(error)
            }
        };
    }

    // Before BEGIN: every embedding this batch will need. apply_event embeds
    // inside its savepoint, and a write transaction must never be held across
    // a model call.
    let prefetched = PrefetchedEmbedder::prefetch(embedder, db::embed_texts_for_batch(batch))?;

    // JSONL-first, and durable before any DB write (D1 + D2). The returned
    // length is the log's committed_len: the span just written closed cleanly
    // and everything before it was already committed.
    let committed_len = events::append_events_batch(&paths.events, batch)?;
    kill_point(KillPoint::AfterLogBatch);
    kill_point(KillPoint::BeforeApply);

    let cursor = Cursor {
        generation: read_generation(&paths.events),
        offset: committed_len,
        tail_sha: tail_sha(&paths.events, committed_len)?,
    };

    let tx = conn.unchecked_transaction()?;
    prefetched.seal();
    let outcome = (|| -> Result<T> {
        for event in batch {
            db::apply_event(conn, &prefetched, event)?;
        }
        let value = inside(conn)?;
        write(conn, &cursor)?;
        Ok(value)
    })();
    match outcome {
        Ok(value) => {
            tx.commit()?;
            kill_point(KillPoint::AfterApply);
            Ok(value)
        }
        Err(error) => {
            let _ = tx.rollback();
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Tail replay (the repair half of rows 7)
// ---------------------------------------------------------------------------

/// Replay the log from the cursor to the log's committed end, then advance the
/// cursor — all in one transaction.
///
/// A record that fails deterministically is retried up to
/// [`POISON_MAX_ATTEMPTS`] times across the whole system (the count is durable
/// in the dead-letter sidecar, so process restarts do not reset it) and is then
/// quarantined: the cursor advances past it and materialization skips it
/// forever. Without that, one bad event bricks every entry point (D3, Principle
/// 4; `DurableBatch.tla` `Quarantine`).
///
/// Returns the number of events applied.
pub fn replay_tail_locked(
    lock: &Lock,
    conn: &Connection,
    paths: &config::Paths,
    embedder: &dyn Embedder,
) -> Result<usize> {
    verify_lock(lock, paths)?;
    let Some(cursor) = read(conn)? else {
        anyhow::bail!("applied cursor: replay_tail_locked called on a cursorless database");
    };
    let tail = events::read_events_from_offset(&paths.events, cursor.offset).map_err(|error| {
        LogUnreadable {
            offset: cursor.offset,
            detail: format!("{error:#}"),
        }
    })?;
    let committed_len = tail.committed_len;
    if committed_len <= cursor.offset {
        return Ok(0);
    }
    let generation = read_generation(&paths.events);
    let tail_hash = tail_sha(&paths.events, committed_len)?;

    // Log occurrence indices for run_id-less legacy `run_history` events. Their
    // synthetic key needs an ordinal that is a function of the LOG, not of how
    // much of it happens to be materialized already — otherwise replaying an
    // already-applied one mints a fresh key and duplicates the row (T3 handed
    // this case to T4 explicitly, `db.rs` `synthetic_run_key`). The prefix scan
    // only happens when the tail actually contains such an event, which for any
    // log written by `run.rs` or `mcp.rs` is never.
    let occurrences = legacy_run_occurrences(&paths.events, cursor.offset, &tail.events)?;

    let mut ledger = DeadLetter::load(&paths.events);
    // Each round either applies the whole tail or dead-letters one more record,
    // and a record is dead-lettered after at most POISON_MAX_ATTEMPTS rounds.
    let max_rounds = tail.events.len() * POISON_MAX_ATTEMPTS as usize + 1;
    for _ in 0..max_rounds {
        let skip = ledger.quarantined();
        let live: Vec<&Value> = tail
            .events
            .iter()
            .filter(|event| !skip.contains(&fingerprint(event)))
            .collect();
        let prefetched = PrefetchedEmbedder::prefetch_deferring_errors(
            embedder,
            db::embed_texts_for_batch_refs(live.iter().copied()),
        );
        let tx = conn.unchecked_transaction()?;
        prefetched.seal();
        let mut failed: Option<(Value, anyhow::Error)> = None;
        for (index, event) in tail.events.iter().enumerate() {
            if skip.contains(&fingerprint(event)) {
                continue;
            }
            if let Err(error) =
                db::apply_event_at(conn, &prefetched, event, occurrences.get(&index).copied())
            {
                failed = Some((event.clone(), error));
                break;
            }
        }
        match failed {
            None => {
                write(
                    conn,
                    &Cursor {
                        generation,
                        offset: committed_len,
                        tail_sha: tail_hash,
                    },
                )?;
                tx.commit()?;
                return Ok(live.len());
            }
            Some((event, error)) => {
                let _ = tx.rollback();
                let key = fingerprint(&event);
                let record = ledger.records.entry(key.clone()).or_default();
                record.attempts = record.attempts.saturating_add(1);
                record.last_error = error.to_string();
                record.event = event.clone();
                if record.attempts >= POISON_MAX_ATTEMPTS {
                    record.quarantined = true;
                    record.quarantined_at = Some(chrono::Utc::now().to_rfc3339());
                    eprintln!(
                        "kb: ERROR quarantining an event after {} failed apply attempts: {error}. \
                         It is recorded in {} and will be skipped by every replay; the applied \
                         cursor advances past it so the knowledge base stays usable.",
                        record.attempts,
                        dead_letter_path(&paths.events).display()
                    );
                } else {
                    eprintln!(
                        "kb: WARNING apply failed during recovery (attempt {} of {}): {error}",
                        record.attempts, POISON_MAX_ATTEMPTS
                    );
                }
                ledger.save(&paths.events)?;
            }
        }
    }
    anyhow::bail!(
        "applied cursor: tail replay did not converge after {max_rounds} rounds — see {}",
        dead_letter_path(&paths.events).display()
    )
}

/// Occurrence index, within the whole log, of each run_id-less legacy
/// `run_history` event in `tail` — keyed by its position in `tail`.
fn legacy_run_occurrences(
    events_path: &Path,
    from: u64,
    tail: &[Value],
) -> Result<std::collections::HashMap<usize, u64>> {
    let mut hashes: Vec<(usize, String)> = Vec::new();
    for (index, event) in tail.iter().enumerate() {
        if let Some(hash) = db::legacy_run_content_hash(event) {
            hashes.push((index, hash));
        }
    }
    let mut seen: BTreeMap<String, u64> = BTreeMap::new();
    if !hashes.is_empty() && from > 0 {
        for event in events::read_events_prefix(events_path, from)?.events {
            if let Some(hash) = db::legacy_run_content_hash(&event) {
                *seen.entry(hash).or_default() += 1;
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    for (index, hash) in hashes {
        let slot = seen.entry(hash).or_default();
        out.insert(index, *slot);
        *slot += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_generation_starts_at_zero_and_bumps_durably() {
        let dir = tempdir().unwrap();
        let events = dir.path().join("agent-kb-events.jsonl");
        fs::write(&events, b"").unwrap();
        assert_eq!(read_generation(&events), 0);
        assert_eq!(bump_generation(&events).unwrap(), 1);
        assert_eq!(read_generation(&events), 1);
        assert_eq!(bump_generation(&events).unwrap(), 2);
        assert_eq!(read_generation(&events), 2);
    }

    #[test]
    fn test_prefix_fingerprint_anchors_at_both_ends_and_binds_the_length() {
        let dir = tempdir().unwrap();
        let events = dir.path().join("agent-kb-events.jsonl");
        // Long enough that the two windows do not overlap.
        let body = vec![b'a'; (TAIL_SHA_WINDOW * 3) as usize];
        fs::write(&events, &body).unwrap();
        let full = tail_sha(&events, body.len() as u64).unwrap();

        for index in [0_usize, body.len() - 1] {
            let mut mutated = body.clone();
            mutated[index] = b'b';
            fs::write(&events, &mutated).unwrap();
            assert_ne!(
                tail_sha(&events, mutated.len() as u64).unwrap(),
                full,
                "a change at byte {index} must move the fingerprint"
            );
        }

        // A length change alone moves it, even though both windows still read
        // as an unbroken run of the same byte.
        fs::write(&events, vec![b'a'; body.len() - 1]).unwrap();
        assert_ne!(tail_sha(&events, (body.len() - 1) as u64).unwrap(), full);

        // The middle stays uncovered on purpose: that is the generation
        // counter's job, and widening the windows is never the answer.
        let mut mutated = body.clone();
        mutated[(TAIL_SHA_WINDOW + 16) as usize] = b'b';
        fs::write(&events, &mutated).unwrap();
        assert_eq!(tail_sha(&events, mutated.len() as u64).unwrap(), full);
    }

    #[test]
    fn test_fingerprint_is_stable_across_reparse() {
        let event = serde_json::json!({"action":"upsert","table":"entries","id":"x"});
        let round_tripped: Value = serde_json::from_str(&event.to_string()).unwrap();
        assert_eq!(fingerprint(&event), fingerprint(&round_tripped));
    }

    /// The attempt counter is what bounds a poison record's retries across
    /// process restarts, so it has to be on disk before the process that wrote
    /// it can end. The child exits abruptly the instant save returns.
    #[test]
    fn test_dead_letter_survives_an_abrupt_exit_after_save() {
        use std::process::Command;

        if let Ok(root) = std::env::var("KB_DEADLETTER_CHILD_ROOT") {
            let events = Path::new(&root).join("agent-kb-events.jsonl");
            let mut ledger = DeadLetter::default();
            ledger.records.insert(
                "poison".to_string(),
                DeadLetterRecord {
                    attempts: POISON_MAX_ATTEMPTS,
                    quarantined: true,
                    last_error: "embedder is down".to_string(),
                    ..Default::default()
                },
            );
            ledger.save(&events).unwrap();
            std::process::exit(137);
        }

        let dir = tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("test_dead_letter_survives_an_abrupt_exit_after_save")
            .arg("--nocapture")
            .env("KB_DEADLETTER_CHILD_ROOT", dir.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(137));

        let events = dir.path().join("agent-kb-events.jsonl");
        let loaded = DeadLetter::load(&events);
        let record = loaded
            .records
            .get("poison")
            .expect("the ledger must survive the child's exit");
        assert_eq!(record.attempts, POISON_MAX_ATTEMPTS);
        assert!(record.quarantined);
        assert!(
            !dead_letter_path(&events).with_extension("").exists()
                || !sidecar(&events, "deadletter.tmp").exists(),
            "the staging file must not be left behind"
        );
    }

    #[test]
    fn test_dead_letter_round_trips_and_reports_quarantined() {
        let dir = tempdir().unwrap();
        let events = dir.path().join("agent-kb-events.jsonl");
        let mut ledger = DeadLetter::default();
        ledger.records.insert(
            "abc".to_string(),
            DeadLetterRecord {
                attempts: POISON_MAX_ATTEMPTS,
                quarantined: true,
                ..Default::default()
            },
        );
        ledger.records.insert(
            "def".to_string(),
            DeadLetterRecord {
                attempts: 1,
                quarantined: false,
                ..Default::default()
            },
        );
        ledger.save(&events).unwrap();
        let loaded = DeadLetter::load(&events);
        assert_eq!(loaded.quarantined(), HashSet::from(["abc".to_string()]));
    }
}

#[cfg(test)]
mod crash_tests {
    //! Kill-after-append, before-apply — one case per production writer.
    //!
    //! `kill_point` calls `process::exit`, so each case re-execs this unit-test
    //! binary as a child with `KB_CRASH_AFTER` armed and inspects the wreckage
    //! from the parent. It lives here rather than in `tests/` because
    //! `kill_point` compiles to a no-op outside the library's own `cfg(test)`.

    use super::*;
    use crate::commands::add::acquire_lock;
    use crate::components::embedder::NoopEmbedder;
    use crate::components::kb_core::{self, AddArgs};
    use crate::config::Paths;
    use serde_json::json;
    use std::process::Command;

    /// The ten production writers collapse to six distinct event shapes
    /// reaching `append_and_apply`; each is killed between the durable append
    /// and the apply.
    ///
    /// The first four drive their production command, so a regression inside
    /// one of those commands fails this test. `kb_core::add` covers `kb add`,
    /// `kb ingest` and MCP `kb_add`; `run`, `test_add` and `expire` each cover
    /// their CLI command and its MCP twin, which build the same event. The last
    /// two are event shapes rather than commands: `citation_healed` and
    /// `evidence_expire` come from `stale_check` and `migrate_citations`, whose
    /// production paths need a git repository and a relocated file that a crash
    /// child cannot cheaply stage — their cursor behaviour is asserted in those
    /// modules' own tests instead.
    const WRITERS: [&str; 6] = [
        "kb_core_add",
        "expire",
        "run",
        "test_add",
        "citation_healed",
        "evidence_expire",
    ];

    fn add_args(id: &str) -> AddArgs {
        AddArgs {
            id: id.to_string(),
            path: format!("p/{id}"),
            summary: format!("summary {id}"),
            content: format!("content {id}"),
            tags: json!(["t"]),
            version_ref: Some("abc123".to_string()),
            permanent: false,
            replace_path: false,
            kind: "belief".to_string(),
            evidence_status: "n/a".to_string(),
            evidence_rows: vec![],
            ts: "2026-09-05T00:00:00Z".to_string(),
            session: "test".to_string(),
            session_id: None,
            expire_reason: "replaced".to_string(),
            dedup_cutoff: None,
            cues: vec![],
        }
    }

    fn live_ids(paths: &Paths) -> Vec<String> {
        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM entries WHERE is_stale=0 ORDER BY id")
            .unwrap();
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        ids
    }

    /// The in-child half: appends through the writer under test and never
    /// returns, because the `BeforeApply` kill point exits with 137.
    fn crash_child(writer: &str, root: &Path) {
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;
        let lock = acquire_lock(&paths.lock).unwrap();
        let conn = db::open_rw(&paths, &lock).unwrap();
        let batch: Vec<Value> = match writer {
            "kb_core_add" => {
                kb_core::add_locked(&lock, &conn, &paths, &embedder, add_args("crash-add"))
                    .unwrap();
                return;
            }
            "expire" => {
                drop(conn);
                drop(lock);
                crate::commands::expire::Expire {
                    id: "seed-1".to_string(),
                    reason: Some("crash test".to_string()),
                    force: false,
                }
                .execute_with(&paths, &embedder)
                .unwrap();
                return;
            }
            "run" => {
                drop(conn);
                drop(lock);
                crate::commands::run::Run {
                    test_id: "t1".to_string(),
                    result: "pass".to_string(),
                    adapter: Some("rust_tool".to_string()),
                    detail: None,
                }
                .execute_with_paths(&paths, &embedder)
                .unwrap();
                return;
            }
            "test_add" => {
                drop(conn);
                drop(lock);
                crate::commands::test_add::TestAdd {
                    app: "app".to_string(),
                    name: "n".to_string(),
                    protocol: "browser".to_string(),
                    config: "{}".to_string(),
                    id: Some("tc-crash".to_string()),
                    version_ref: None,
                }
                .execute_with_paths(&paths)
                .unwrap();
                return;
            }
            "citation_healed" => vec![events::citation_healed_event(
                "seed-1",
                "ev-1",
                "old/path.rs",
                "new/path.rs",
                "deadbeef",
                None,
            )],
            "evidence_expire" => {
                vec![events::evidence_expire_event(
                    "seed-1",
                    "ev-1",
                    "crash test",
                )]
            }
            other => panic!("unknown writer {other}"),
        };
        append_and_apply(&lock, &conn, &paths, &embedder, &batch).unwrap();
    }

    /// The T4 headline, enumerated over every writer: killed between the
    /// durable append and the apply, the next write path converges the
    /// database with no manual `kb rebuild`.
    #[test]
    fn test_kill_before_apply_converges_on_the_next_write_for_every_writer() {
        if let Ok(writer) = std::env::var("KB_CRASH_WRITER") {
            let root = std::env::var("KB_CRASH_ROOT").unwrap();
            crash_child(&writer, Path::new(&root));
            return;
        }

        for writer in WRITERS {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
            let paths = Paths::from_root(dir.path());

            // Seed a live entry so the expire/heal writers have a parent to
            // target (the evidence arms are orphan-tolerant on apply, so a bare
            // entry is enough) and a test case so run_history's foreign key
            // resolves.
            kb_core::add(&paths, &NoopEmbedder, add_args("seed-1")).unwrap();
            {
                let lock = acquire_lock(&paths.lock).unwrap();
                let conn = db::open_rw(&paths, &lock).unwrap();
                let seed_case = json!({
                    "action": "upsert", "table": "test_cases",
                    "id": "t1", "app": "app", "name": "n", "protocol": "rust_tool",
                    "config": "{}", "version_ref": null, "ts": "2026-09-05T00:00:00Z",
                });
                append_and_apply(&lock, &conn, &paths, &NoopEmbedder, &[seed_case]).unwrap();
            }
            let before = events::read_events(&paths.events).unwrap().committed_len;

            let status = Command::new(std::env::current_exe().unwrap())
                .arg("test_kill_before_apply_converges_on_the_next_write_for_every_writer")
                .arg("--nocapture")
                .env("KB_CRASH_WRITER", writer)
                .env("KB_CRASH_ROOT", dir.path())
                .env("KB_CRASH_AFTER", KillPoint::BeforeApply.as_str())
                .env("KB_NO_EMBED", "1")
                .status()
                .unwrap();
            assert_eq!(
                status.code(),
                Some(137),
                "{writer}: the child did not reach the BeforeApply kill point"
            );

            // The log grew and is durable; the database did not follow.
            let after = events::read_events(&paths.events).unwrap().committed_len;
            assert!(after > before, "{writer}: the append did not survive");
            {
                let conn = db::open_unchecked_for_test(&paths.db).unwrap();
                assert_eq!(
                    read(&conn).unwrap().unwrap().offset,
                    before,
                    "{writer}: the cursor must still name the pre-crash boundary"
                );
                assert_eq!(
                    inspect(&conn, &paths),
                    Decision::ReplayTail {
                        from: before,
                        to: after
                    },
                    "{writer}: the gap must be detected as a tail replay"
                );
                if writer == "kb_core_add" {
                    let rows: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM entries WHERE id='crash-add'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(rows, 0, "a killed batch must leave no partial rows");
                }
            }

            // A write path converges it. No `kb rebuild` is run here.
            crate::commands::rebuild::recover_if_needed(&paths, &NoopEmbedder).unwrap();
            let conn = db::open_unchecked_for_test(&paths.db).unwrap();
            assert_eq!(
                inspect(&conn, &paths),
                Decision::NoOp,
                "{writer}: recovery did not converge the database"
            );
            assert!(
                DeadLetter::load(&paths.events).records.is_empty(),
                "{writer}: recovery quarantined a record instead of applying it"
            );
            if writer == "kb_core_add" {
                assert_eq!(
                    live_ids(&paths),
                    vec!["crash-add".to_string(), "seed-1".to_string()]
                );
            }
        }
    }
}
