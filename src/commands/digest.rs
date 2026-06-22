//! `digest` — read unread transcript turns, synthesize a KB entry, advance offset.

use crate::commands::add::read_omc_session;
use crate::commands::add_validation::compute_evidence_status_write;
use crate::components::{embedder::NoopEmbedder, kb_core, redactor, transcript_state::TranscriptState};
use crate::config;
use anyhow::{Context, Result};
use sha2::{Digest as Sha2Digest, Sha256};
use std::path::Path;

pub struct DigestOutcome {
    pub turns_processed: usize,
    pub skipped_no_change: bool,
}

/// Read unread turns from `transcript_path` starting at the stored offset,
/// synthesize a digest, write to KB, and advance the offset.
///
/// Crash-safe: `advance()` is always the LAST step. A crash before it re-queues
/// the same turns on the next invocation (idempotent via hash check).
pub fn digest_session(
    session_id: &str,
    transcript_path: &Path,
    paths: &config::Paths,
) -> Result<DigestOutcome> {
    // Resolve state dir from KB_STATE_DIR env or default: same dir as DB.
    let state_dir = std::env::var("KB_STATE_DIR")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            paths
                .db
                .parent()
                .expect("db path has no parent")
                .to_path_buf()
        });

    let ts_state = TranscriptState::open(&state_dir)
        .with_context(|| format!("open transcript state in {}", state_dir.display()))?;

    let file_bytes = std::fs::read(transcript_path)
        .with_context(|| format!("read transcript {}", transcript_path.display()))?;

    let unread = ts_state.unread_bytes(transcript_path, &file_bytes);
    let new_offset = file_bytes.len() as u64;

    if unread.is_empty() {
        return Ok(DigestOutcome {
            turns_processed: 0,
            skipped_no_change: true,
        });
    }

    // Cap raw bytes before materialising as UTF-8 to bound memory on pathological
    // transcripts. 512 KiB is well above any realistic session.
    const MAX_DIGEST_BYTES: usize = 512 * 1024;
    let unread_capped = if unread.len() > MAX_DIGEST_BYTES {
        &unread[..MAX_DIGEST_BYTES]
    } else {
        unread
    };
    // Lossy so we never bail on binary garbage in tool output.
    let text = String::from_utf8_lossy(unread_capped).into_owned();

    // Split into turns by blank lines or literal `---` separator, cap at 500.
    let turns: Vec<String> = split_turns(&text, 500);
    let turns_processed = turns.len();

    // head: first 3 turns; tail: last 3 turns (non-overlapping with head).
    let head_end = turns.len().min(3);
    let tail_start = if turns.len() > 3 { turns.len() - 3 } else { head_end };
    let head_text = turns[..head_end].join("\n\n---\n\n");
    let tail_text = turns[tail_start..].join("\n\n---\n\n");

    // tool_calls: lines matching tool-use heuristic patterns.
    let tool_calls_text: String = text
        .lines()
        .filter(|line| {
            line.contains("Tool:")
                || line.contains("<tool_use>")
                || line.contains("fn_call:")
                || line.contains("Bash(")
                || line.contains("Read(")
                || line.contains("Write(")
                || line.contains("Edit(")
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Hash check — skip write if digest content unchanged.
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(head_text.as_bytes());
    hasher.update(b"|");
    hasher.update(tail_text.as_bytes());
    hasher.update(b"|");
    hasher.update(tool_calls_text.as_bytes());
    let digest_hash = format!("{:x}", hasher.finalize());

    let kb_path = format!("sessions/{session_id}/digest");
    if let Ok(existing_hash) = read_digest_hash(paths, &kb_path) {
        if existing_hash == digest_hash {
            // Content unchanged — advance offset then skip KB write.
            let current_offset = ts_state.offset(transcript_path).unwrap_or(0);
            if new_offset > current_offset {
                ts_state.advance(transcript_path, new_offset)?;
            }
            return Ok(DigestOutcome {
                turns_processed: 0,
                skipped_no_change: true,
            });
        }
    }

    // Build digest body and redact credentials.
    let digest_body = build_digest_body(session_id, &head_text, &tail_text, &tool_calls_text);
    let redacted_body = redactor::redact_str(&digest_body).into_owned();

    // Write KB entry.
    let id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();
    let (session, omc_session_id) = read_omc_session();
    let tags = serde_json::json!({"digest_hash": digest_hash});
    let kind = "memory";
    let evidence_rows: Vec<serde_json::Value> = vec![];
    let evidence_status = compute_evidence_status_write(kind, &evidence_rows);

    let emb = NoopEmbedder;
    kb_core::add(
        paths,
        &emb,
        kb_core::AddArgs {
            id,
            path: kb_path,
            summary: format!("Session digest for {session_id}"),
            content: redacted_body,
            tags,
            version_ref: None,
            permanent: false,
            replace_path: true,
            kind: kind.to_string(),
            evidence_status: evidence_status.to_string(),
            evidence_rows,
            ts,
            session,
            session_id: omc_session_id,
            expire_reason: format!("replaced by session digest for {session_id}"),
        },
    )?;

    // LAST step — crash before here re-queues turns on next run.
    ts_state.advance(transcript_path, new_offset)?;

    Ok(DigestOutcome {
        turns_processed,
        skipped_no_change: false,
    })
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Split `text` into turns separated by blank lines or `---` lines, capped at `max`.
fn split_turns(text: &str, max: usize) -> Vec<String> {
    let mut turns: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" {
            if !current.is_empty() {
                let turn = current.join("\n").trim().to_string();
                if !turn.is_empty() {
                    turns.push(turn);
                    if turns.len() >= max {
                        return turns;
                    }
                }
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    // Flush trailing turn.
    if !current.is_empty() {
        let turn = current.join("\n").trim().to_string();
        if !turn.is_empty() {
            turns.push(turn);
        }
    }
    turns
}

fn build_digest_body(
    session_id: &str,
    head_text: &str,
    tail_text: &str,
    tool_calls_text: &str,
) -> String {
    format!(
        "## Session Digest: {session_id}\n\n\
         ### Head (first turns)\n{head_text}\n\n\
         ### Tail (last turns)\n{tail_text}\n\n\
         ### Tool calls\n{tool_calls_text}"
    )
}

/// Read the `digest_hash` tag from the most recent non-stale entry at `kb_path`.
fn read_digest_hash(paths: &config::Paths, kb_path: &str) -> Result<String> {
    use crate::components::db;
    use rusqlite::params;

    let conn = db::open_db(&paths.db)?;
    let row: Option<String> = conn
        .query_row(
            "SELECT tags FROM entries WHERE path=?1 AND is_stale=0 ORDER BY created_at DESC LIMIT 1",
            params![kb_path],
            |r| r.get::<_, String>(0),
        )
        .ok();

    let tags_json = row.ok_or_else(|| anyhow::anyhow!("no existing digest entry"))?;
    let tags: serde_json::Value =
        serde_json::from_str(&tags_json).context("parse tags JSON")?;
    tags.get("digest_hash")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no digest_hash tag"))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::db;
    use crate::config::Paths;
    use std::fs;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, Paths) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(&root);
        (dir, paths)
    }

    fn write_transcript(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, content).unwrap();
        p
    }

    fn make_transcript_content(turns: usize) -> String {
        (0..turns)
            .map(|i| format!("Turn {i}: some content here for turn number {i}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Basic integration: 5 turns → entry written, turns_processed == 5.
    #[test]
    fn test_digest_session_basic() {
        let (dir, paths) = setup();
        let state_dir = paths.db.parent().unwrap().to_path_buf();
        std::env::set_var("KB_STATE_DIR", &state_dir);

        let content = make_transcript_content(5);
        let tp = write_transcript(&dir, "sess-basic.jsonl", &content);

        let outcome = digest_session("test-sess-basic", &tp, &paths).unwrap();
        assert_eq!(outcome.turns_processed, 5);
        assert!(!outcome.skipped_no_change);

        let conn = db::open_db(&paths.db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE path='sessions/test-sess-basic/digest' AND is_stale=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "digest entry must exist in KB");

        std::env::remove_var("KB_STATE_DIR");
    }

    /// Second run with no new content → skipped_no_change=true, no new events.
    #[test]
    fn test_digest_session_no_change() {
        let (dir, paths) = setup();
        let state_dir = paths.db.parent().unwrap().to_path_buf();
        std::env::set_var("KB_STATE_DIR", &state_dir);

        let content = make_transcript_content(5);
        let tp = write_transcript(&dir, "sess-nochange.jsonl", &content);

        let out1 = digest_session("test-sess-nochange", &tp, &paths).unwrap();
        assert!(!out1.skipped_no_change);

        let events_before = fs::read_to_string(&paths.events).unwrap().lines().count();

        // Second run: file unchanged → offset at end → empty unread.
        let out2 = digest_session("test-sess-nochange", &tp, &paths).unwrap();
        assert_eq!(out2.turns_processed, 0);
        assert!(out2.skipped_no_change);

        let events_after = fs::read_to_string(&paths.events).unwrap().lines().count();
        assert_eq!(events_before, events_after, "no new events on no-change run");

        std::env::remove_var("KB_STATE_DIR");
    }

    /// Hash-based idempotence: reset offset to simulate crash before advance().
    #[test]
    fn test_digest_session_hash_idempotence() {
        let (dir, paths) = setup();
        let state_dir = paths.db.parent().unwrap().to_path_buf();
        std::env::set_var("KB_STATE_DIR", &state_dir);

        let content = make_transcript_content(3);
        let tp = write_transcript(&dir, "sess-idem.jsonl", &content);

        let out1 = digest_session("test-sess-idem", &tp, &paths).unwrap();
        assert!(!out1.skipped_no_change);

        let events_before = fs::read_to_string(&paths.events).unwrap().lines().count();

        // Simulate crash: zero out stored offset so unread_bytes returns full content.
        let ts_path = state_dir.join("transcripts.json");
        fs::write(&ts_path, "{}").unwrap();

        let out2 = digest_session("test-sess-idem", &tp, &paths).unwrap();
        assert!(out2.skipped_no_change, "same content hash must skip write");

        let events_after = fs::read_to_string(&paths.events).unwrap().lines().count();
        assert_eq!(events_before, events_after, "no new KB events on hash-match skip");

        std::env::remove_var("KB_STATE_DIR");
    }

    /// Empty transcript → skipped_no_change=true immediately.
    #[test]
    fn test_digest_session_empty_transcript() {
        let (dir, paths) = setup();
        let state_dir = paths.db.parent().unwrap().to_path_buf();
        std::env::set_var("KB_STATE_DIR", &state_dir);

        let tp = write_transcript(&dir, "empty.jsonl", "");

        let out = digest_session("test-sess-empty", &tp, &paths).unwrap();
        assert_eq!(out.turns_processed, 0);
        assert!(out.skipped_no_change);

        std::env::remove_var("KB_STATE_DIR");
    }

    #[test]
    fn test_split_turns_basic() {
        let text = "A\n\nB\n\nC";
        let turns = split_turns(text, 500);
        assert_eq!(turns, vec!["A", "B", "C"]);
    }

    #[test]
    fn test_split_turns_separator() {
        let text = "A\n---\nB";
        let turns = split_turns(text, 500);
        assert_eq!(turns, vec!["A", "B"]);
    }

    #[test]
    fn test_split_turns_cap() {
        let text = (0..10).map(|i| format!("Turn {i}")).collect::<Vec<_>>().join("\n\n");
        let turns = split_turns(&text, 3);
        assert_eq!(turns.len(), 3);
    }
}
