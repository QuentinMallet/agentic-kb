//! Event log operations (JSONL append + read).
//!
//! # Line format (C1/D1)
//!
//! Every append — batch *and* single event — is wrapped in an in-band commit
//! envelope:
//!
//! ```jsonl
//! {"action":"batch_begin","batch_id":"<uuid>","n":3}
//! … the 3 event lines, unchanged …
//! {"action":"batch_commit","batch_id":"<uuid>","n":3}
//! ```
//!
//! A span counts as committed only when its `batch_commit` line is present
//! **and** newline-terminated. The reader rules are:
//!
//! | Log shape | Meaning |
//! |---|---|
//! | line outside any span (legacy log) | committed standalone event |
//! | span with a newline-terminated `batch_commit` | all its events committed |
//! | dangling `batch_begin` at EOF | uncommitted; dropped by every reader |
//! | dangling `batch_begin` mid-log | hard error, never a silent drop |
//! | `n` disagreeing with the observed line count | hard error |
//!
//! Marker lines are **not events**: they are consumed by the reader, never
//! returned, never reach `apply_event`, and never counted by rebuild or
//! compact. Logs written before this format contain no markers, so every one of
//! their lines is standalone-committed and replays unchanged — there is no
//! migration.
//!
//! Downgrading to a binary that predates the envelope: run `kb compact` first.
//! Compact rewrites the log from the reader's output, which is marker-free by
//! construction.

use crate::crash_sim::{kill_point, KillPoint};
use crate::models::Evidence;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// `action` value of the marker line that opens a commit span.
pub const BATCH_BEGIN: &str = "batch_begin";
/// `action` value of the marker line that closes a commit span.
pub const BATCH_COMMIT: &str = "batch_commit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TornTail {
    pub line: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadEvents {
    pub events: Vec<serde_json::Value>,
    pub torn_tail: Option<TornTail>,
    /// Byte offset one past the end of the last committed record, excluding any
    /// span left open at the tail.
    ///
    /// This is the only offset that may cross a process or phase boundary: no
    /// span straddles it, so the bytes before it can never be reinterpreted by
    /// bytes that arrive later (plan §4 Principle 3).
    pub committed_len: u64,
}

/// Depth of event-log flocks held by this process.
///
/// The repair path truncates uncommitted spans, so its "a dangling begin is
/// only ever at the tail" precondition is enforced rather than documented.
static LOG_LOCK_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Record that this process acquired the event-log flock.
pub fn note_log_lock_acquired() {
    LOG_LOCK_DEPTH.fetch_add(1, Ordering::SeqCst);
}

/// Record that this process released the event-log flock.
pub fn note_log_lock_released() {
    let _ = LOG_LOCK_DEPTH.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |depth| {
        Some(depth.saturating_sub(1))
    });
}

/// Whether this process currently holds the event-log flock.
pub fn log_lock_held() -> bool {
    LOG_LOCK_DEPTH.load(Ordering::SeqCst) > 0
}

enum EventLineParseError {
    Utf8(std::str::Utf8Error),
    Json(serde_json::Error),
}

fn parse_event_line(
    bytes: &[u8],
) -> std::result::Result<Option<serde_json::Value>, EventLineParseError> {
    let text = std::str::from_utf8(bytes).map_err(EventLineParseError::Utf8)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(EventLineParseError::Json)
}

/// Build an `evidence_add` event payload.
///
/// The caller is responsible for appending to the event log under the existing
/// flock (see ADR-B).  `version_ref` is the git HEAD SHA at the time of the
/// write, or `None` when unavailable.
pub fn evidence_add_event(
    entry_id: &str,
    evidence: &Evidence,
    version_ref: Option<&str>,
) -> serde_json::Value {
    let ts = chrono::Utc::now().to_rfc3339();
    serde_json::json!({
        "action": "evidence_add",
        "table": "evidence",
        "entry_id": entry_id,
        "evidence": {
            "id": evidence.id,
            "entry_id": evidence.entry_id,
            "kind": evidence.kind,
            "citation_path": evidence.citation_path,
            "citation_sha": evidence.citation_sha,
            "citation_hash": evidence.citation_hash,
            "citation_excerpt": evidence.citation_excerpt,
            "derived_from": evidence.derived_from,
            "recorded_at": evidence.recorded_at,
        },
        "version_ref": version_ref,
        "ts": ts,
    })
}

/// Build a `citation_healed` event payload — the durable record of a citation
/// that was repointed after a relocation search.
///
/// The event carries `citation_hash` for audit only: it is the UNCHANGED hash
/// recorded at `kb_add`, and `apply_event` never writes it. Relocation status
/// is computed at read time, so this event — not any row — is the trace of what
/// moved where (plan §6 S1 residual, spec `StoredHashImmutable`).
pub fn citation_healed_event(
    entry_id: &str,
    evidence_id: &str,
    old_path: &str,
    new_path: &str,
    citation_hash: &str,
    version_ref: Option<&str>,
) -> serde_json::Value {
    let ts = chrono::Utc::now().to_rfc3339();
    serde_json::json!({
        "action": "citation_healed",
        "table": "evidence",
        "entry_id": entry_id,
        "evidence_id": evidence_id,
        "old_path": old_path,
        "new_path": new_path,
        "citation_hash": citation_hash,
        "version_ref": version_ref,
        "ts": ts,
    })
}

/// Build an `evidence_expire` event payload.
pub fn evidence_expire_event(entry_id: &str, evidence_id: &str, reason: &str) -> serde_json::Value {
    let ts = chrono::Utc::now().to_rfc3339();
    serde_json::json!({
        "action": "evidence_expire",
        "table": "evidence",
        "entry_id": entry_id,
        "evidence_id": evidence_id,
        "reason": reason,
        "ts": ts,
    })
}

/// Write one commit span: `batch_begin`, the event lines verbatim, `batch_commit`.
fn write_span(f: &mut File, events: &[serde_json::Value]) -> Result<()> {
    let batch_id = uuid::Uuid::new_v4().to_string();
    let n = events.len();
    let marker = |action: &str| {
        serde_json::json!({ "action": action, "batch_id": batch_id, "n": n }).to_string()
    };
    writeln!(f, "{}", marker(BATCH_BEGIN))?;
    for event in events {
        writeln!(f, "{}", serde_json::to_string(event)?)?;
        kill_point(KillPoint::AfterLogLine);
    }
    writeln!(f, "{}", marker(BATCH_COMMIT))?;
    kill_point(KillPoint::AfterCommitMarker);
    Ok(())
}

/// Append multiple events to the JSONL log as one commit span.
///
/// The caller must hold the flock before calling. Nothing in the span is
/// reader-accepted until its `batch_commit` line lands with its newline, so an
/// interrupted append contributes zero events rather than a prefix.
/// Returns the log's `committed_len` after the append: the span just written
/// closed cleanly and everything before it was already committed, so it is the
/// file length. The applied cursor (C1/D3) records exactly this value, and
/// taking it from the writer keeps the cost of a write O(bytes appended)
/// rather than O(log size).
pub fn append_events_batch(events_path: &Path, events: &[serde_json::Value]) -> Result<u64> {
    append_events_batch_with_sync(
        events_path,
        events,
        File::sync_data,
        crate::components::fsync::sync_dir,
    )
}

/// Implementation seam used to prove sync ordering and failure behavior.
fn append_events_batch_with_sync<S, D>(
    events_path: &Path,
    events: &[serde_json::Value],
    mut sync_file: S,
    mut sync_dir: D,
) -> Result<u64>
where
    S: FnMut(&File) -> std::io::Result<()>,
    D: FnMut(&Path) -> Result<()>,
{
    if events.is_empty() {
        return Ok(read_events(events_path)?.committed_len);
    }
    let file_was_created = !events_path.exists();
    let parent = events_path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent_was_created = parent.is_some_and(|p| !p.exists());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    repair_uncommitted_tail_before_append(events_path)?;
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(events_path)
        .with_context(|| format!("open events {}", events_path.display()))?;
    write_span(&mut f, events)?;

    // Do not retry a failed sync and then trust a later success: on Linux an
    // fsync error may report and clear an earlier writeback error. Propagate the
    // first failure so callers cannot proceed to any DB apply.
    sync_file(&f).with_context(|| format!("sync event log {}", events_path.display()))?;

    if file_was_created {
        if let Some(parent) = parent {
            sync_dir(parent)?;
        }
    }
    if parent_was_created {
        if let Some(grandparent) = parent.and_then(Path::parent) {
            sync_dir(grandparent)?;
        }
    }
    // "AfterSync" means the complete log durability boundary: data plus any
    // directory entries created by this append are stable before DB apply.
    kill_point(KillPoint::AfterSync);
    Ok(f.metadata()?.len())
}

/// Append a single event to the JSONL log.
///
/// Single events are enveloped too. A lone `writeln!` is self-framing against a
/// *crash*, but not against a *write error*: the body write can succeed and the
/// newline write fail, and without a span the next append would classify that
/// complete-JSON tail as reader-accepted and promote an event the caller
/// reported as failed.
pub fn append_event(events_path: &Path, event: &serde_json::Value) -> Result<u64> {
    append_events_batch(events_path, std::slice::from_ref(event))
}

/// Preserve a torn final record using the event-log sidecar naming convention.
pub(crate) fn preserve_torn_tail(events_path: &Path, torn_tail: &[u8]) -> Result<PathBuf> {
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
    fs::write(&sidecar, torn_tail).with_context(|| {
        format!(
            "preserve torn tail from {} into {}",
            events_path.display(),
            sidecar.display()
        )
    })?;
    Ok(sidecar)
}

/// Whether the uncommitted tail begins with a `batch_begin` marker.
fn tail_opens_span(tail: &[u8]) -> bool {
    let first = tail.split(|byte| *byte == b'\n').next().unwrap_or_default();
    matches!(parse_event_line(first), Ok(Some(value)) if value["action"] == BATCH_BEGIN)
}

/// Bytes scanned backwards when checking whether the log ends on an intact span.
const TAIL_WINDOW: u64 = 64 * 1024;

/// Whether the log ends on an intact, newline-terminated commit span.
///
/// Every span this binary writes is appended to a log it has already scanned in
/// full, so an intact closing span at end of file means `committed_len == len`
/// without re-scanning. Anything else — a legacy tail, a torn tail, a dangling
/// span, or a line appended by a binary that predates the envelope — returns
/// false and falls through to the full span-aware scan, which is where the D7
/// hard errors live. That keeps the append path's cost bounded by the last span
/// rather than by the size of the log.
fn ends_on_intact_span(file: &mut File, len: u64) -> Result<bool> {
    let start = len.saturating_sub(TAIL_WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut window = vec![0_u8; (len - start) as usize];
    file.read_exact(&mut window)?;
    if !window.ends_with(b"\n") {
        return Ok(false);
    }
    let mut lines: Vec<&[u8]> = window[..window.len() - 1]
        .split(|byte| *byte == b'\n')
        .collect();
    if start > 0 && !lines.is_empty() {
        // The window may open mid-line; that partial line is not usable.
        lines.remove(0);
    }
    let Some(Ok(Some(commit))) = lines.last().map(|line| parse_event_line(line)) else {
        return Ok(false);
    };
    if commit["action"] != BATCH_COMMIT {
        return Ok(false);
    }
    let (Some(batch_id), Some(n)) = (commit["batch_id"].as_str(), commit["n"].as_u64()) else {
        return Ok(false);
    };
    let n = n as usize;
    if lines.len() < n + 2 {
        return Ok(false);
    }
    let body = &lines[lines.len() - n - 1..lines.len() - 1];
    if body.iter().any(|line| {
        matches!(parse_event_line(line), Ok(Some(value))
            if value["action"] == BATCH_BEGIN || value["action"] == BATCH_COMMIT)
    }) {
        return Ok(false);
    }
    let Ok(Some(begin)) = parse_event_line(lines[lines.len() - n - 2]) else {
        return Ok(false);
    };
    Ok(begin["action"] == BATCH_BEGIN
        && begin["batch_id"].as_str() == Some(batch_id)
        && begin["n"].as_u64() == Some(n as u64))
}

/// Drop everything past `committed_len` while the caller holds the event-log flock.
///
/// Two shapes of uncommitted tail exist and they are repaired differently:
///
/// * **A dangling span.** It was never reader-accepted, so truncating it is
///   sufficient and safe, and the sidecar must never block that: on ENOSPC — a
///   motivating fault for this whole change — a preserve-then-truncate order
///   would leave the dangling span in place. The sidecar is best-effort here.
/// * **A torn final line outside any span** (a legacy log, or a log this binary
///   has never appended to). Unchanged behaviour: valid UTF-8 that parses as a
///   complete JSON value is already reader-accepted and only needs its newline;
///   anything else is preserved in a sidecar and truncated.
///
/// The scan hard-errors on a mid-log dangling `batch_begin` or an `n` mismatch,
/// so a skewed-binary log stops the append loudly instead of being repaired
/// into something lossy.
fn repair_uncommitted_tail_before_append(events_path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(events_path)
        .with_context(|| format!("open events {} for tail repair", events_path.display()))?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    if ends_on_intact_span(&mut file, len)? {
        return Ok(());
    }

    let committed_len = read_events(events_path)?.committed_len;
    if committed_len < len {
        file.seek(SeekFrom::Start(committed_len))?;
        let mut tail = Vec::with_capacity((len - committed_len) as usize);
        file.read_to_end(&mut tail)?;
        if tail_opens_span(&tail) {
            if !log_lock_held() {
                anyhow::bail!(
                    "events: refusing to truncate the uncommitted span in {} without the \
                     event-log flock — every log-writing call site must hold it",
                    events_path.display()
                );
            }
            match preserve_torn_tail(events_path, &tail) {
                Ok(sidecar) => eprintln!(
                    "events: WARNING truncated an uncommitted span ({} bytes) from {}, preserved in {}",
                    tail.len(),
                    events_path.display(),
                    sidecar.display()
                ),
                Err(error) => eprintln!(
                    "events: WARNING truncated an uncommitted span ({} bytes) from {}; \
                     sidecar not written: {error}",
                    tail.len(),
                    events_path.display()
                ),
            }
            file.set_len(committed_len)?;
        } else {
            let sidecar = preserve_torn_tail(events_path, &tail)?;
            file.set_len(committed_len)?;
            eprintln!(
                "events: WARNING preserved torn final record ({} bytes) to {} and truncated {} before append",
                tail.len(),
                sidecar.display(),
                events_path.display()
            );
        }
    }

    // What survives may end on a reader-accepted record that never got its
    // newline. Give it one so the next span starts on its own line.
    let end = file.metadata()?.len();
    if end > 0 {
        file.seek(SeekFrom::Start(end - 1))?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// A span held open while its event lines accumulate.
struct OpenSpan {
    batch_id: String,
    n: usize,
    line: usize,
    pending: Vec<serde_json::Value>,
}

/// Result of one span-aware scan, with `committed` relative to where the scan started.
struct SpanScan {
    events: Vec<serde_json::Value>,
    torn_tail: Option<TornTail>,
    committed: u64,
}

fn marker_n(value: &serde_json::Value, action: &str, line: usize) -> Result<usize> {
    value["n"]
        .as_u64()
        .map(|n| n as usize)
        .ok_or_else(|| anyhow::anyhow!("events line {line}: {action} marker has no integer n"))
}

fn marker_batch_id(value: &serde_json::Value, action: &str, line: usize) -> Result<String> {
    let id = value["batch_id"].as_str().unwrap_or_default();
    if id.is_empty() {
        anyhow::bail!("events line {line}: {action} marker has no batch_id");
    }
    Ok(id.to_string())
}

/// Read events, honouring the D1 commit envelope.
///
/// Stops once `max` committed events have been collected and no span is open,
/// so a limit can never split a span.
fn scan_events<R: BufRead>(mut reader: R, max: usize) -> Result<SpanScan> {
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut open: Option<OpenSpan> = None;
    let mut committed = 0_u64;
    let mut offset = 0_u64;
    let mut line = 0_usize;
    let mut buf = Vec::new();

    loop {
        if events.len() >= max && open.is_none() {
            break;
        }
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        line += 1;
        offset += read as u64;
        let record_end = offset;
        let has_newline = buf.ends_with(b"\n");
        let chunk = if has_newline {
            &buf[..buf.len() - 1]
        } else {
            buf.as_slice()
        };
        let torn_tail = || TornTail {
            line,
            bytes: chunk.to_vec(),
        };

        let parsed = match parse_event_line(chunk) {
            Ok(parsed) => parsed,
            // A torn final chunk. Any span still open is uncommitted and dropped.
            Err(EventLineParseError::Utf8(_) | EventLineParseError::Json(_)) if !has_newline => {
                return Ok(SpanScan {
                    events,
                    torn_tail: Some(torn_tail()),
                    committed,
                });
            }
            Err(EventLineParseError::Utf8(e)) => {
                return Err(e).with_context(|| format!("decode events line {line}"));
            }
            Err(EventLineParseError::Json(e)) => {
                return Err(e).with_context(|| format!("parse events line {line}"));
            }
        };

        let Some(value) = parsed else {
            // Blank lines are skipped and never count toward a span's arity.
            if open.is_none() {
                committed = record_end;
            }
            continue;
        };

        match value["action"].as_str() {
            Some(BATCH_BEGIN) => {
                if let Some(previous) = &open {
                    anyhow::bail!(
                        "events line {line}: batch_begin while the span opened at line {} is still \
                         open — a mid-log dangling batch_begin is never dropped silently; run \
                         `kb compact` under this binary to produce a marker-free log",
                        previous.line
                    );
                }
                open = Some(OpenSpan {
                    batch_id: marker_batch_id(&value, BATCH_BEGIN, line)?,
                    n: marker_n(&value, BATCH_BEGIN, line)?,
                    line,
                    pending: Vec::new(),
                });
            }
            Some(BATCH_COMMIT) => {
                let Some(span) = open.take() else {
                    anyhow::bail!(
                        "events line {line}: batch_commit without a matching batch_begin"
                    );
                };
                let batch_id = marker_batch_id(&value, BATCH_COMMIT, line)?;
                if batch_id != span.batch_id {
                    anyhow::bail!(
                        "events line {line}: batch_commit batch_id {batch_id:?} does not match the \
                         batch_begin at line {} ({:?})",
                        span.line,
                        span.batch_id
                    );
                }
                let declared = marker_n(&value, BATCH_COMMIT, line)?;
                if declared != span.n || span.pending.len() != span.n {
                    anyhow::bail!(
                        "events line {line}: span n mismatch — batch_begin at line {} declared \
                         n={}, batch_commit declared n={declared}, {} event line(s) observed",
                        span.line,
                        span.n,
                        span.pending.len()
                    );
                }
                if !has_newline {
                    // The commit marker never got its newline: the span is uncommitted.
                    return Ok(SpanScan {
                        events,
                        torn_tail: None,
                        committed,
                    });
                }
                events.extend(span.pending);
                committed = record_end;
            }
            _ => match open.as_mut() {
                Some(span) => {
                    if span.pending.len() >= span.n {
                        anyhow::bail!(
                            "events line {line}: span n mismatch — batch_begin at line {} declared \
                             n={} but a further event line follows",
                            span.line,
                            span.n
                        );
                    }
                    span.pending.push(value);
                }
                None => {
                    events.push(value);
                    committed = record_end;
                }
            },
        }
    }

    // A span still open at EOF was never committed: drop it.
    Ok(SpanScan {
        events,
        torn_tail: None,
        committed,
    })
}

fn empty_read() -> ReadEvents {
    ReadEvents {
        events: vec![],
        torn_tail: None,
        committed_len: 0,
    }
}

/// The log's `committed_len` without materializing its events.
///
/// Takes the same intact-span shortcut the append path does: a log this binary
/// has already appended to ends on a closed span, so `committed_len` is the
/// file length and no scan is needed. Anything else — a legacy tail, a torn
/// tail, a dangling span — falls through to the full span-aware scan, which is
/// also where the D7 hard errors live.
///
/// The applied-cursor write guard calls this on every write, so the shortcut is
/// what keeps a write O(bytes appended) instead of O(log size).
pub fn committed_len(events_path: &Path) -> Result<u64> {
    if !events_path.exists() {
        return Ok(0);
    }
    let mut file = File::open(events_path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(0);
    }
    if ends_on_intact_span(&mut file, len)? {
        return Ok(len);
    }
    Ok(read_events(events_path)?.committed_len)
}

/// Read all events from a JSONL file.
pub fn read_events(events_path: &Path) -> Result<ReadEvents> {
    read_events_up_to(events_path, usize::MAX)
}

/// Read at most `max` complete events from a JSONL file.
///
/// The limit is applied only at span boundaries, so it can never expose a
/// partially-committed batch.
pub fn read_events_up_to(events_path: &Path, max: usize) -> Result<ReadEvents> {
    if !events_path.exists() {
        return Ok(empty_read());
    }
    let file = File::open(events_path)?;
    let scan = scan_events(BufReader::new(file), max)?;
    Ok(ReadEvents {
        events: scan.events,
        torn_tail: scan.torn_tail,
        committed_len: scan.committed,
    })
}

/// Read the byte prefix `[0, len)` of the log.
///
/// `len` must be a [`ReadEvents::committed_len`] value: no span straddles such
/// an offset, so the prefix is self-interpreting and the events it yields are
/// exactly the ones the snapshot saw.
pub fn read_events_prefix(events_path: &Path, len: u64) -> Result<ReadEvents> {
    if !events_path.exists() {
        return Ok(empty_read());
    }
    let file = File::open(events_path)?;
    let scan = scan_events(BufReader::new(file.take(len)), usize::MAX)?;
    Ok(ReadEvents {
        events: scan.events,
        torn_tail: scan.torn_tail,
        committed_len: scan.committed,
    })
}

/// Byte offset of the first line at or after `from` that the reader cannot
/// parse.
///
/// `None` when every line parses — the read may still have failed structurally
/// (a mid-log dangling `batch_begin`, an `n` mismatch), which is a property of
/// the span rather than of one line, and the caller then names the boundary it
/// started from instead.
///
/// Used to point an operator at the damage rather than at the whole log.
pub fn first_unreadable_offset(events_path: &Path, from: u64) -> Option<u64> {
    let mut file = File::open(events_path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut reader = BufReader::new(file);
    let mut offset = from;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf).ok()?;
        if read == 0 {
            return None;
        }
        let has_newline = buf.ends_with(b"\n");
        let chunk = if has_newline {
            &buf[..buf.len() - 1]
        } else {
            buf.as_slice()
        };
        // A torn final chunk is the ordinary uncommitted tail, not damage.
        if parse_event_line(chunk).is_err() && has_newline {
            return Some(offset);
        }
        offset += read as u64;
    }
}

/// Read complete events beginning at a known committed boundary.
///
/// Rebuild records this byte offset while holding the event-log flock and
/// verifies the bytes before it have not changed before using this reader. The
/// offset must be a span boundary: an offset inside a span would hide the
/// span's `batch_begin` and its remaining lines would read as standalone
/// committed events, which is exactly the half-applied batch this format
/// exists to prevent. Both halves of that are rejected — a non-record-boundary
/// offset here, and an unmatched `batch_commit` in the scan.
pub fn read_events_from_offset(events_path: &Path, offset: u64) -> Result<ReadEvents> {
    if !events_path.exists() {
        return Ok(empty_read());
    }
    let mut file = File::open(events_path)?;
    let len = file.metadata()?.len();
    if offset >= len {
        return Ok(ReadEvents {
            events: vec![],
            torn_tail: None,
            committed_len: offset,
        });
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0_u8; 1];
        file.read_exact(&mut previous)?;
        if previous[0] != b'\n' {
            anyhow::bail!(
                "read_events_from_offset: {offset} is not a record boundary in {}",
                events_path.display()
            );
        }
    }
    file.seek(SeekFrom::Start(offset))?;
    let scan = scan_events(BufReader::new(file), usize::MAX)?;
    Ok(ReadEvents {
        events: scan.events,
        torn_tail: scan.torn_tail,
        committed_len: offset + scan.committed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::io;
    use std::rc::Rc;
    use tempfile::tempdir;

    fn torn_sidecars(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("events.jsonl.torn-"))
            })
            .collect()
    }

    #[test]
    fn test_append_sync_precedes_caller_apply() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let order = Rc::new(RefCell::new(Vec::new()));
        let sync_order = Rc::clone(&order);

        append_events_batch_with_sync(
            &events_path,
            &[serde_json::json!({"action": "upsert", "id": "ordered"})],
            move |_| {
                sync_order.borrow_mut().push("sync_data");
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        // This represents the first operation the caller may perform after a
        // successful append; the append cannot return before the sync hook.
        order.borrow_mut().push("apply_event");

        assert_eq!(&*order.borrow(), &["sync_data", "apply_event"]);
    }

    #[test]
    fn test_sync_failure_returns_once_and_caller_applies_nothing() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let sync_attempts = Rc::new(RefCell::new(0));
        let attempts = Rc::clone(&sync_attempts);
        let mut db_writes = 0;

        let append = append_events_batch_with_sync(
            &events_path,
            &[serde_json::json!({"action": "upsert", "id": "sync-fail"})],
            move |_| {
                *attempts.borrow_mut() += 1;
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected sync failure",
                ))
            },
            |_| Ok(()),
        );
        if append.is_ok() {
            db_writes += 1;
        }

        assert!(append.unwrap_err().to_string().contains("sync event log"));
        assert_eq!(
            *sync_attempts.borrow(),
            1,
            "sync failure must not be retried"
        );
        assert_eq!(db_writes, 0, "a failed sync must prevent every DB write");
    }

    #[test]
    fn test_directory_syncs_only_for_created_file_or_directory() {
        let dir = tempdir().unwrap();
        let state = dir.path().join(".state");
        fs::create_dir(&state).unwrap();
        let log_dir = state.join("agent-kb");
        let events_path = log_dir.join("events.jsonl");

        let synced = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
        let record = Rc::clone(&synced);
        append_events_batch_with_sync(
            &events_path,
            &[serde_json::json!({"action": "upsert", "id": "first"})],
            |_| Ok(()),
            move |path| {
                record.borrow_mut().push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(&*synced.borrow(), &[log_dir.clone(), state.clone()]);

        synced.borrow_mut().clear();
        let record = Rc::clone(&synced);
        append_events_batch_with_sync(
            &events_path,
            &[serde_json::json!({"action": "upsert", "id": "second"})],
            |_| Ok(()),
            move |path| {
                record.borrow_mut().push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        assert!(
            synced.borrow().is_empty(),
            "existing entries need no directory sync"
        );

        fs::remove_file(&events_path).unwrap();
        let record = Rc::clone(&synced);
        append_events_batch_with_sync(
            &events_path,
            &[serde_json::json!({"action": "upsert", "id": "recreated"})],
            |_| Ok(()),
            move |path| {
                record.borrow_mut().push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(&*synced.borrow(), &[log_dir]);
    }

    #[test]
    fn test_append_and_read_events_roundtrip() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");

        let e1 = serde_json::json!({"action": "upsert", "table": "entries", "id": "1"});
        let e2 = serde_json::json!({"action": "expire", "table": "entries", "id": "2"});
        let e3 = serde_json::json!({"action": "insert", "table": "run_history", "test_id": "t1"});

        append_event(&events_path, &e1).unwrap();
        append_event(&events_path, &e2).unwrap();
        append_event(&events_path, &e3).unwrap();

        let result = read_events(&events_path).unwrap();
        assert!(result.torn_tail.is_none());
        assert_eq!(result.events.len(), 3);
        assert_eq!(result.events[0]["id"], "1");
        assert_eq!(result.events[1]["id"], "2");
        assert_eq!(result.events[2]["test_id"], "t1");
    }

    #[test]
    fn test_append_event_preserves_complete_json_without_newline_in_log() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let prior = serde_json::json!({"action": "upsert", "id": "prior"});
        let appended = serde_json::json!({"action": "upsert", "id": "appended"});
        fs::write(&events_path, serde_json::to_vec(&prior).unwrap()).unwrap();

        append_event(&events_path, &appended).unwrap();

        let result = read_events(&events_path).unwrap();
        assert_eq!(result.events, vec![prior, appended]);
        assert!(result.torn_tail.is_none());
        assert!(torn_sidecars(dir.path()).is_empty());
    }

    #[test]
    fn test_append_events_batch_preserves_complete_json_without_newline_in_log() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let prior = serde_json::json!({"action": "upsert", "id": "prior"});
        let appended = vec![
            serde_json::json!({"action": "upsert", "id": "one"}),
            serde_json::json!({"action": "upsert", "id": "two"}),
        ];
        fs::write(&events_path, serde_json::to_vec(&prior).unwrap()).unwrap();

        append_events_batch(&events_path, &appended).unwrap();

        let result = read_events(&events_path).unwrap();
        let mut expected = vec![prior];
        expected.extend(appended);
        assert_eq!(result.events, expected);
        assert!(result.torn_tail.is_none());
        assert!(torn_sidecars(dir.path()).is_empty());
    }

    #[test]
    fn test_append_event_repairs_and_preserves_torn_tail() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let first = serde_json::json!({"action": "upsert", "table": "entries", "id": "1"});
        let second = serde_json::json!({"action": "upsert", "table": "entries", "id": "2"});
        let torn = b"{\"action\":";
        fs::write(
            &events_path,
            format!("{}\n", serde_json::to_string(&first).unwrap()).as_bytes(),
        )
        .unwrap();
        let mut file = OpenOptions::new().append(true).open(&events_path).unwrap();
        file.write_all(torn).unwrap();

        append_event(&events_path, &second).unwrap();

        let result = read_events(&events_path).unwrap();
        assert_eq!(result.events, vec![first, second]);
        assert!(result.torn_tail.is_none());
        let sidecars = torn_sidecars(dir.path());
        assert_eq!(sidecars.len(), 1);
        assert_eq!(fs::read(&sidecars[0]).unwrap(), torn);
    }

    #[test]
    fn test_append_event_preserves_split_utf8_tail_in_sidecar() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let first = serde_json::json!({"action": "upsert", "id": "first"});
        let appended = serde_json::json!({"action": "upsert", "id": "appended"});
        let mut contents = serde_json::to_vec(&first).unwrap();
        contents.push(b'\n');
        let torn = b"{\"msg\":\"\xE2\x82";
        contents.extend_from_slice(torn);
        fs::write(&events_path, contents).unwrap();

        append_event(&events_path, &appended).unwrap();

        let result = read_events(&events_path).unwrap();
        assert_eq!(result.events, vec![first, appended]);
        assert!(result.torn_tail.is_none());
        let sidecars = torn_sidecars(dir.path());
        assert_eq!(sidecars.len(), 1);
        assert_eq!(fs::read(&sidecars[0]).unwrap(), torn);
    }

    #[test]
    fn test_append_event_never_removes_reader_accepted_tail() {
        let appended = serde_json::json!({"action": "upsert", "id": "appended"});
        for tail in [
            serde_json::to_vec(&serde_json::json!({"action": "upsert", "id": "accepted"})).unwrap(),
            b"{\"action\":".to_vec(),
        ] {
            let dir = tempdir().unwrap();
            let events_path = dir.path().join("events.jsonl");
            fs::write(&events_path, &tail).unwrap();
            let before = read_events(&events_path).unwrap();

            append_event(&events_path, &appended).unwrap();

            let after = read_events(&events_path).unwrap();
            let mut expected = before.events;
            expected.push(appended.clone());
            assert_eq!(after.events, expected);
            assert!(after.torn_tail.is_none());
        }
    }

    #[test]
    fn test_append_events_batch_repairs_torn_tail() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        fs::write(&events_path, b"{\"torn\":").unwrap();
        let events = vec![
            serde_json::json!({"action": "upsert", "id": "1"}),
            serde_json::json!({"action": "upsert", "id": "2"}),
        ];

        append_events_batch(&events_path, &events).unwrap();

        assert_eq!(read_events(&events_path).unwrap().events, events);
        let sidecar = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("events.jsonl.torn-"))
            })
            .expect("torn-tail sidecar");
        assert_eq!(fs::read(sidecar).unwrap(), b"{\"torn\":");
    }

    #[test]
    fn test_append_event_to_clean_file_creates_no_sidecar() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let events = vec![
            serde_json::json!({"action": "upsert", "id": "1"}),
            serde_json::json!({"action": "upsert", "id": "2"}),
        ];
        append_event(&events_path, &events[0]).unwrap();
        append_event(&events_path, &events[1]).unwrap();

        assert_eq!(read_events(&events_path).unwrap().events, events);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn test_append_event_to_empty_file() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        fs::write(&events_path, b"").unwrap();
        let event = serde_json::json!({"action": "upsert", "id": "1"});

        append_event(&events_path, &event).unwrap();

        assert_eq!(read_events(&events_path).unwrap().events, vec![event]);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn test_read_events_nonexistent_file() {
        let result = read_events(Path::new("/tmp/nonexistent-events.jsonl")).unwrap();
        assert!(result.events.is_empty());
        assert!(result.torn_tail.is_none());
    }

    #[test]
    fn test_read_events_tolerates_torn_final_json_line() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        fs::write(
            &events_path,
            b"{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"1\"}\n{\"action\":",
        )
        .unwrap();

        let result = read_events(&events_path).unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["id"], "1");
        let torn_tail = result.torn_tail.expect("torn tail must be reported");
        assert_eq!(torn_tail.line, 2);
        assert_eq!(torn_tail.bytes, b"{\"action\":");
    }

    #[test]
    fn test_read_events_tolerates_torn_final_utf8_line() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"1\"}\n");
        bytes.extend_from_slice(b"{\"msg\":\"");
        bytes.extend_from_slice(&[0xE2, 0x82]);
        fs::write(&events_path, bytes).unwrap();

        let result = read_events(&events_path).unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["id"], "1");
        let torn_tail = result.torn_tail.expect("torn tail must be reported");
        assert_eq!(torn_tail.line, 2);
        assert!(torn_tail.bytes.ends_with(&[0xE2, 0x82]));
    }

    #[test]
    fn test_read_events_errors_on_malformed_middle_line() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        fs::write(
            &events_path,
            b"{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"1\"}\n{\"action\":\n{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"2\"}\n",
        )
        .unwrap();

        let err = read_events(&events_path).unwrap_err();
        assert!(err.to_string().contains("parse events line 2"));
    }

    #[test]
    fn test_read_events_errors_on_invalid_utf8_middle_line() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"1\"}\n");
        bytes.extend_from_slice(b"{\"msg\":\"");
        bytes.extend_from_slice(&[0xE2, 0x82]);
        bytes.extend_from_slice(b"\"}\n");
        bytes.extend_from_slice(b"{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"2\"}\n");
        fs::write(&events_path, bytes).unwrap();

        let err = read_events(&events_path).unwrap_err();
        assert!(err.to_string().contains("decode events line 2"));
    }

    // -----------------------------------------------------------------
    // Crash harness (T1a). `kill_point` is armed by `cfg(test)`, which only
    // holds inside the library's own test build — these must live here, not in
    // an integration test, or the child never dies.
    // -----------------------------------------------------------------

    fn crash_log(root: &str) -> PathBuf {
        Path::new(root).join("events.jsonl")
    }

    fn spawn_crash_child(test_name: &str, case: &str, root: &Path, kill: KillPoint) -> Option<i32> {
        std::process::Command::new(std::env::current_exe().unwrap())
            .arg(test_name)
            .arg("--nocapture")
            .current_dir(root)
            .env("KB_CRASH_TEST_CASE", case)
            .env("KB_CRASH_TEST_ROOT", root)
            .env("KB_CRASH_AFTER", kill.to_string())
            .status()
            .unwrap()
            .code()
    }

    fn is_crash_child(case: &str) -> bool {
        std::env::var("KB_CRASH_TEST_CASE").ok().as_deref() == Some(case)
    }

    fn crash_upsert(id: &str) -> serde_json::Value {
        serde_json::json!({"action": "upsert", "table": "entries", "id": id})
    }

    #[test]
    fn test_crash_mid_batch_leaves_zero_reader_accepted_events() {
        if is_crash_child("mid-batch") {
            let root = std::env::var("KB_CRASH_TEST_ROOT").unwrap();
            append_events_batch(
                &crash_log(&root),
                &[crash_upsert("m0"), crash_upsert("m1"), crash_upsert("m2")],
            )
            .unwrap();
            panic!("child append returned without hitting the configured kill point");
        }

        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fs::write(&path, format!("{}\n", crash_upsert("pre-existing"))).unwrap();
        let committed_before = read_events(&path).unwrap().committed_len;

        let code = spawn_crash_child(
            "test_crash_mid_batch_leaves_zero_reader_accepted_events",
            "mid-batch",
            dir.path(),
            KillPoint::AfterLogLine,
        );
        assert_eq!(code, Some(137), "the child must die at the kill point");

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("m0"), "the partial span must be on disk");
        let read = read_events(&path).unwrap();
        assert_eq!(
            read.events,
            vec![crash_upsert("pre-existing")],
            "no event of the interrupted batch may be reader-accepted"
        );
        assert_eq!(read.committed_len, committed_before);
    }

    #[test]
    fn test_crash_after_commit_marker_leaves_the_whole_batch_committed() {
        if is_crash_child("after-commit") {
            let root = std::env::var("KB_CRASH_TEST_ROOT").unwrap();
            append_events_batch(&crash_log(&root), &[crash_upsert("c0"), crash_upsert("c1")])
                .unwrap();
            panic!("child append returned without hitting the configured kill point");
        }

        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");

        let code = spawn_crash_child(
            "test_crash_after_commit_marker_leaves_the_whole_batch_committed",
            "after-commit",
            dir.path(),
            KillPoint::AfterCommitMarker,
        );
        assert_eq!(code, Some(137), "the child must die at the kill point");

        let read = read_events(&path).unwrap();
        assert_eq!(read.events, vec![crash_upsert("c0"), crash_upsert("c1")]);
        assert_eq!(read.committed_len, fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn test_read_events_skips_blank_lines() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");
        fs::write(
            &events_path,
            b"\n  \n{\"action\":\"upsert\",\"table\":\"entries\",\"id\":\"1\"}\n\t\n",
        )
        .unwrap();

        let result = read_events(&events_path).unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0]["id"], "1");
        assert!(result.torn_tail.is_none());
    }
}
