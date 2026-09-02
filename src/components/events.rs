//! Event log operations (JSONL append + read)

use crate::models::Evidence;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TornTail {
    pub line: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadEvents {
    pub events: Vec<serde_json::Value>,
    pub torn_tail: Option<TornTail>,
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

/// Append multiple events to the JSONL log in one pass.
///
/// The caller must hold the flock before calling (same contract as
/// [`append_event`]).  Each event is written as a separate `writeln!` to
/// preserve the one-JSON-object-per-line invariant that `read_events` relies on.
pub fn append_events_batch(events_path: &Path, events: &[serde_json::Value]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    if let Some(p) = events_path.parent() {
        fs::create_dir_all(p)?;
    }
    repair_torn_tail_before_append(events_path)?;
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(events_path)
        .with_context(|| format!("open events {}", events_path.display()))?;
    for event in events {
        writeln!(f, "{}", serde_json::to_string(event)?)?;
    }
    Ok(())
}

/// Append a single event to the JSONL log.
pub fn append_event(events_path: &Path, event: &serde_json::Value) -> Result<()> {
    if let Some(p) = events_path.parent() {
        fs::create_dir_all(p)?;
    }
    repair_torn_tail_before_append(events_path)?;
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(events_path)
        .with_context(|| format!("open events {}", events_path.display()))?;
    writeln!(f, "{}", serde_json::to_string(event)?)?;
    Ok(())
}

/// Preserve a torn final record using the event-log sidecar naming convention.
pub(crate) fn preserve_torn_tail(
    events_path: &Path,
    torn_tail: &[u8],
) -> Result<PathBuf> {
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

fn repair_torn_tail_before_append(events_path: &Path) -> Result<()> {
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

    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }

    const SCAN_CHUNK: u64 = 8192;
    let mut cursor = len;
    let mut tail_start = 0_u64;
    let mut scan = vec![0_u8; SCAN_CHUNK as usize];
    while cursor > 0 {
        let start = cursor.saturating_sub(SCAN_CHUNK);
        let count = (cursor - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut scan[..count])?;
        if let Some(offset) = scan[..count].iter().rposition(|byte| *byte == b'\n') {
            tail_start = start + offset as u64 + 1;
            break;
        }
        cursor = start;
    }

    file.seek(SeekFrom::Start(tail_start))?;
    let mut bytes = Vec::with_capacity((len - tail_start) as usize);
    file.read_to_end(&mut bytes)?;
    let sidecar = preserve_torn_tail(events_path, &bytes)?;
    file.set_len(tail_start)?;
    eprintln!(
        "events: WARNING preserved torn final record ({} bytes) to {} and truncated {} before append",
        bytes.len(),
        sidecar.display(),
        events_path.display()
    );
    Ok(())
}

/// Read all events from a JSONL file.
pub fn read_events(events_path: &Path) -> Result<ReadEvents> {
    read_events_up_to(events_path, usize::MAX)
}

/// Read at most `max` complete events from a JSONL file.
///
/// This function stops only when `max` events have been collected or EOF is
/// reached. The "snapshot" guarantee used by Phase 2 of rebuild comes from the
/// caller passing the Phase-1 `snapshot_len` while holding the flock, not from
/// any byte-offset coordination inside this reader.
///
/// Soundness assumption: [`append_event`] and [`append_events_batch`] write the
/// serialized JSON bytes first and then the trailing newline to an unbuffered
/// `File`. A crash can therefore truncate only the final unterminated chunk; it
/// cannot produce a newline-terminated-but-partial JSON record in the middle of
/// the log.
pub fn read_events_up_to(events_path: &Path, max: usize) -> Result<ReadEvents> {
    if !events_path.exists() {
        return Ok(ReadEvents {
            events: vec![],
            torn_tail: None,
        });
    }
    let f = File::open(events_path)?;
    let mut reader = BufReader::new(f);
    let mut events = Vec::new();
    let mut buf = Vec::new();
    let mut line = 0usize;

    loop {
        if events.len() >= max {
            break;
        }
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        line += 1;
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
        let text = match std::str::from_utf8(chunk) {
            Ok(text) => text,
            Err(_) if !has_newline => {
                return Ok(ReadEvents {
                    events,
                    torn_tail: Some(torn_tail()),
                });
            }
            Err(e) => return Err(e).with_context(|| format!("decode events line {line}")),
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) if !has_newline => {
                return Ok(ReadEvents {
                    events,
                    torn_tail: Some(torn_tail()),
                });
            }
            Err(e) => return Err(e).with_context(|| format!("parse events line {line}")),
        };
        events.push(v);
    }
    Ok(ReadEvents {
        events,
        torn_tail: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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
        let sidecars: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("events.jsonl.torn-"))
            })
            .collect();
        assert_eq!(sidecars.len(), 1);
        assert_eq!(fs::read(&sidecars[0]).unwrap(), torn);
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
