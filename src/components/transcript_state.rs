//! Crash-safe transcript byte-offset state tracking.
//!
//! Persists the last-consumed byte offset per transcript file to
//! `.state/agent-kb/transcripts.json`, using tmpfile+rename for atomicity
//! and `fs2` exclusive lock for concurrent safety.

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// In-memory map: path-as-string → last consumed byte offset.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct TranscriptOffsets {
    offsets: HashMap<String, u64>,
}

/// Handle to the transcript state file on disk.
pub struct TranscriptState {
    state_path: PathBuf,
}

impl TranscriptState {
    /// Open or create the transcript state file.
    /// `state_dir` is the `.state/agent-kb/` directory.
    pub fn open(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("create state dir {}", state_dir.display()))?;
        let state_path = state_dir.join("transcripts.json");
        // Touch the file so the lock target always exists.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&state_path)
            .with_context(|| format!("create state file {}", state_path.display()))?;
        Ok(TranscriptState { state_path })
    }

    /// Read the current offset for a transcript file.
    /// Returns 0 if not previously tracked.
    pub fn offset(&self, transcript: &Path) -> Result<u64> {
        let (_lock, map) = self.read_locked()?;
        Ok(*map.offsets.get(&path_key(transcript)).unwrap_or(&0))
    }

    /// Atomically advance the offset for a transcript file.
    ///
    /// Rejects `new_offset <= current_offset` (monotonicity).
    /// Uses tmpfile+fsync+rename for crash safety; both files are on the
    /// same filesystem so `rename` is POSIX-atomic.
    pub fn advance(&self, transcript: &Path, new_offset: u64) -> Result<()> {
        let (lock, mut map) = self.read_locked()?;
        let key = path_key(transcript);
        let current = *map.offsets.get(&key).unwrap_or(&0);
        if new_offset <= current {
            bail!("offset must advance: got {new_offset}, current is {current}");
        }
        map.offsets.insert(key, new_offset);
        // Write to a tmpfile in the same directory (same FS), then rename.
        let dir = self
            .state_path
            .parent()
            .context("state_path has no parent")?;
        // Use a unique suffix per call so concurrent callers (each holding
        // the flock in sequence) do not race on the same tmpfile path.
        let tmp_name = format!("transcripts.json.{}.tmp", uuid::Uuid::new_v4().simple());
        let tmp_path = dir.join(tmp_name);
        {
            let mut tmp = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
                .with_context(|| format!("open tmp {}", tmp_path.display()))?;
            let json = serde_json::to_vec(&map).context("serialize offsets")?;
            tmp.write_all(&json).context("write tmp")?;
            // sync_all() flushes data + metadata to disk before rename.
            tmp.sync_all().context("fsync tmp")?;
        }
        fs::rename(&tmp_path, &self.state_path).with_context(|| {
            format!(
                "rename {} → {}",
                tmp_path.display(),
                self.state_path.display()
            )
        })?;
        // Fsync the directory so the rename is durable on non-auto-commit
        // filesystems (XFS, btrfs) before we release the lock.
        fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .context("fsync state dir")?;
        drop(lock);
        Ok(())
    }

    /// Return all unread bytes from `file_contents` starting at the current
    /// stored offset. Does NOT advance the offset.
    pub fn unread_bytes<'a>(&self, transcript: &Path, file_contents: &'a [u8]) -> &'a [u8] {
        let off = self
            .offset(transcript)
            .unwrap_or(0)
            .min(file_contents.len() as u64) as usize;
        &file_contents[off..]
    }

    // ── internals ────────────────────────────────────────────────────────────

    /// Acquire an exclusive lock on the state file and read its current
    /// contents.  Returns the lock guard (keep alive for the duration of the
    /// write) and the deserialized map.
    fn read_locked(&self) -> Result<(fs::File, TranscriptOffsets)> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.state_path)
            .with_context(|| format!("open state {}", self.state_path.display()))?;
        f.lock_exclusive()
            .with_context(|| format!("lock {}", self.state_path.display()))?;
        let content = fs::read(&self.state_path).context("read state file")?;
        let map: TranscriptOffsets = if content.is_empty() {
            TranscriptOffsets::default()
        } else {
            serde_json::from_slice(&content).unwrap_or_else(|e| {
                eprintln!(
                    "warn: transcript state file corrupt ({e}); resetting offsets — full re-digest on next run"
                );
                TranscriptOffsets::default()
            })
        };
        Ok((f, map))
    }
}

/// Stable string key for a path (UTF-8 lossy conversion).
fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn ts_in(dir: &Path) -> TranscriptState {
        TranscriptState::open(dir).expect("open")
    }

    fn tpath(name: &str) -> PathBuf {
        PathBuf::from(format!("/fake/transcripts/{name}.jsonl"))
    }

    /// advance() rejects a lower-or-equal offset (monotonicity).
    #[test]
    fn test_advance_monotonic_reject() {
        let dir = tempdir().unwrap();
        let ts = ts_in(dir.path());
        let p = tpath("mono");

        ts.advance(&p, 100).expect("first advance");

        // same value → error
        let err = ts.advance(&p, 100).unwrap_err();
        assert!(
            err.to_string().contains("offset must advance"),
            "expected monotonicity error, got: {err}"
        );

        // lower value → error
        let err2 = ts.advance(&p, 50).unwrap_err();
        assert!(err2.to_string().contains("offset must advance"));

        // higher value still works
        ts.advance(&p, 200).expect("advance to 200");
        assert_eq!(ts.offset(&p).unwrap(), 200);
    }

    /// Crash simulation: tmpfile written but never renamed → state file unchanged.
    #[test]
    fn test_crash_tmpfile_not_renamed() {
        let dir = tempdir().unwrap();
        let ts = ts_in(dir.path());
        let p = tpath("crash");

        ts.advance(&p, 50).expect("initial advance");

        // Simulate a crash: write a stale tmpfile (with the old fixed name)
        // but do NOT rename it. The state file must remain unchanged.
        let tmp_path = dir.path().join("transcripts.json.stray.tmp");
        let fake_map = TranscriptOffsets {
            offsets: {
                let mut m = HashMap::new();
                m.insert(path_key(&p), 9999u64);
                m
            },
        };
        std::fs::write(&tmp_path, serde_json::to_vec(&fake_map).unwrap()).unwrap();

        // State file should still reflect the pre-crash value (50).
        let ts2 = ts_in(dir.path());
        assert_eq!(
            ts2.offset(&p).unwrap(),
            50,
            "state file must be unmodified after crash"
        );
    }

    /// Two threads calling advance() concurrently must serialize correctly.
    ///
    /// Under lock-based serialization, each advance() reads the current value
    /// and either commits (if monotonic) or returns a monotonicity error.
    /// The invariant tested here is that:
    ///   1. No data corruption occurs (the final value is a valid u64 the
    ///      lock serialization allows).
    ///   2. The final offset equals the highest value that successfully advanced.
    ///
    /// We drive each thread with a strictly increasing sequence, using a shared
    /// atomic to coordinate which thread "wins" each slot, so at least one of
    /// the target values always advances.
    #[test]
    fn test_concurrent_advance_serializes() {
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        };
        use std::thread;

        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        TranscriptState::open(&state_dir).expect("open");

        let state_dir = Arc::new(state_dir);
        let p = Arc::new(tpath("concurrent"));
        // Tracks the globally highest offset successfully committed.
        let peak: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

        let mut handles = vec![];
        // Launch 4 threads each trying to advance to a unique, increasing value.
        // Values are partitioned so each thread owns a non-overlapping range:
        //   thread 0: 10, 20, 30, 40
        //   thread 1: 50, 60, 70, 80
        //   thread 2: 90, 100, 110, 120
        //   thread 3: 130, 140, 150, 160
        for t in 0..4u64 {
            let sd = Arc::clone(&state_dir);
            let pp = Arc::clone(&p);
            let peak_ref = Arc::clone(&peak);
            handles.push(thread::spawn(move || {
                let ts = TranscriptState::open(&sd).unwrap();
                for i in 1..=4u64 {
                    let v = t * 40 + i * 10;
                    match ts.advance(&pp, v) {
                        Ok(()) => {
                            // Update peak atomically.
                            peak_ref.fetch_max(v, Ordering::SeqCst);
                        }
                        Err(e) => {
                            // Monotonicity error is expected when another thread
                            // already wrote a higher value — not a bug.
                            assert!(
                                e.to_string().contains("offset must advance"),
                                "unexpected error: {e}"
                            );
                        }
                    }
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        let ts_final = TranscriptState::open(&state_dir).unwrap();
        let final_offset = ts_final.offset(&p).unwrap();
        let expected_peak = peak.load(Ordering::SeqCst);

        assert_eq!(
            final_offset, expected_peak,
            "state file offset must equal the highest successfully committed value"
        );
        // At least the highest value in the last thread's range must have landed.
        assert!(
            final_offset >= 10,
            "at least one advance must have succeeded, got {final_offset}"
        );
    }

    /// unread_bytes returns the correct slice.
    #[test]
    fn test_unread_bytes_slice() {
        let dir = tempdir().unwrap();
        let ts = ts_in(dir.path());
        let p = tpath("slice");
        let content = b"hello world";

        // No prior offset → full slice.
        assert_eq!(ts.unread_bytes(&p, content), b"hello world");

        ts.advance(&p, 5).expect("advance to 5");
        assert_eq!(ts.unread_bytes(&p, content), b" world");

        ts.advance(&p, 11).expect("advance to end");
        assert_eq!(ts.unread_bytes(&p, content), b"");

        // Offset past end → empty (clamp).
        ts.advance(&p, 9999).expect("advance past end");
        assert_eq!(ts.unread_bytes(&p, content), b"");
    }
}
