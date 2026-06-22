//! `digest` — read unread transcript turns, synthesize a KB entry, advance offset.
//!
//! This is a stub populated by Item 4 (T5).

use std::path::Path;

pub struct DigestOutcome {
    pub turns_processed: usize,
    pub skipped_no_change: bool,
}

/// Read unread turns from `transcript_path` starting at the stored offset,
/// synthesize a digest, write to KB, and advance the offset.
///
/// This is a stub — populated by Item 4 (T5).
pub fn digest_session(
    _session_id: &str,
    _transcript_path: &Path,
    _paths: &crate::config::Paths,
) -> anyhow::Result<DigestOutcome> {
    unimplemented!("digest_session: populated by Item 4")
}
