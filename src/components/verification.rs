//! Evidence verification: HEAD-byte-hash check.
//!
//! Per locked decision L1: verified = sha256(bytes of citation_path at
//! current HEAD over recorded byte range) == citation_hash. Detects F1
//! truth decay loudly. citation_sha is stored for provenance only.

use crate::models::Evidence;
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Safely join `rel` onto `repo_root`, rejecting any path that escapes the root.
///
/// Rejects: absolute paths, any `..` / root / prefix components.
/// Canonicalizes both sides and verifies containment.
/// Returns `None` on any rejection or I/O error during canonicalization.
fn safe_join(repo_root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    for c in rel_path.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
            _ => {}
        }
    }
    let candidate = repo_root.join(rel_path);
    let canon_root = repo_root.canonicalize().ok()?;
    let canon_cand = candidate.canonicalize().ok()?;
    if !canon_cand.starts_with(&canon_root) {
        return None;
    }
    Some(canon_cand)
}

/// Parse a citation_path of shape "src/foo.rs:42-58" into (path, start, end).
/// Returns Err if format is invalid. Byte offsets, NOT line numbers.
/// (Plan locks this: "byte range" semantics per spec AC15.)
fn parse_citation_path(s: &str) -> Result<(&str, usize, usize)> {
    let colon = s
        .rfind(':')
        .ok_or_else(|| anyhow::anyhow!("citation_path missing ':' separator: {s:?}"))?;
    let file_part = &s[..colon];
    let range_part = &s[colon + 1..];
    let dash = range_part
        .find('-')
        .ok_or_else(|| anyhow::anyhow!("citation_path range missing '-': {s:?}"))?;
    let start: usize = range_part[..dash]
        .parse()
        .map_err(|_| anyhow::anyhow!("citation_path start is not a number: {s:?}"))?;
    let end: usize = range_part[dash + 1..]
        .parse()
        .map_err(|_| anyhow::anyhow!("citation_path end is not a number: {s:?}"))?;
    if start > end {
        bail!("citation_path start > end: {s:?}");
    }
    Ok((file_part, start, end))
}

/// Strip a `"sha256:"` prefix if present, returning the bare hex string.
fn strip_hash_prefix(h: &str) -> &str {
    h.strip_prefix("sha256:").unwrap_or(h)
}

/// Verify a single evidence row against the file at HEAD.
///
/// Returns:
/// - `Ok(true)`  if the file exists at `citation_path`, the byte range is
///                in bounds, and sha256(bytes) == citation_hash.
/// - `Ok(false)` if file missing, range out of bounds, or hash mismatch.
///   Never panics. Never propagates I/O errors as Err — those are folded
///   into `Ok(false)` per AC16.
/// - `Err(e)`    only for malformed inputs (e.g. invalid citation_path
///                format that's a programming bug, not a runtime concern).
pub fn verify_evidence(ev: &Evidence, repo_root: &Path) -> Result<bool> {
    // Only "code" kind is verified in Phase 1. Other kinds deferred to Phase 2.
    // TODO(Phase 2): add test/command/user/derived verification paths.
    if ev.kind != "code" {
        return Ok(false);
    }

    let raw_path = match &ev.citation_path {
        Some(p) => p,
        None => return Ok(false),
    };

    let (file_rel, start, end) = parse_citation_path(raw_path)?;

    let file_abs = match safe_join(repo_root, file_rel) {
        Some(p) => p,
        None => return Ok(false), // path traversal or escape attempt → false per AC16
    };
    let bytes = match fs::read(&file_abs) {
        Ok(b) => b,
        Err(_) => return Ok(false), // missing file or I/O error → false per AC16
    };

    if end > bytes.len() || start > bytes.len() {
        return Ok(false); // range out of bounds
    }

    let slice = &bytes[start..end];
    let mut hasher = Sha256::new();
    hasher.update(slice);
    let computed = format!("{:x}", hasher.finalize());

    let expected = strip_hash_prefix(&ev.citation_hash);
    Ok(computed.eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_evidence(
        citation_path: Option<String>,
        citation_hash: String,
        kind: &str,
    ) -> Evidence {
        Evidence {
            id: "ev-test".to_string(),
            entry_id: "entry-test".to_string(),
            kind: kind.to_string(),
            citation_path,
            citation_sha: None,
            citation_hash,
            citation_excerpt: None,
            derived_from: None,
            recorded_at: None,
        }
    }

    fn hash_bytes(b: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(b);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn test_verify_evidence_hash_match() {
        let mut tmp = NamedTempFile::new().unwrap();
        let content = b"Hello, world! This is test content.";
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();

        let start = 7usize;
        let end = 13usize;
        let expected_hash = hash_bytes(&content[start..end]);

        let file_name = tmp.path().file_name().unwrap().to_string_lossy().to_string();
        let dir = tmp.path().parent().unwrap();
        let citation_path = format!("{}:{}-{}", file_name, start, end);

        let ev = make_evidence(Some(citation_path), expected_hash, "code");
        assert_eq!(verify_evidence(&ev, dir).unwrap(), true);
    }

    #[test]
    fn test_verify_evidence_hash_mismatch() {
        let mut tmp = NamedTempFile::new().unwrap();
        let content = b"Hello, world! This is test content.";
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();

        let file_name = tmp.path().file_name().unwrap().to_string_lossy().to_string();
        let dir = tmp.path().parent().unwrap();
        let citation_path = format!("{}:0-5", file_name);

        // Wrong hash
        let ev = make_evidence(Some(citation_path), "sha256:deadbeef".to_string(), "code");
        assert_eq!(verify_evidence(&ev, dir).unwrap(), false);
    }

    #[test]
    fn test_verify_evidence_missing_file() {
        let ev = make_evidence(
            Some("nonexistent/path/file.rs:0-10".to_string()),
            "sha256:anything".to_string(),
            "code",
        );
        // Must return false, not panic or Err
        assert_eq!(verify_evidence(&ev, Path::new("/tmp")).unwrap(), false);
    }

    #[test]
    fn test_verify_evidence_out_of_range() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"short file").unwrap();
        tmp.flush().unwrap();

        let file_name = tmp.path().file_name().unwrap().to_string_lossy().to_string();
        let dir = tmp.path().parent().unwrap();
        // File is 10 bytes; citation claims 1000-2000
        let citation_path = format!("{}:1000-2000", file_name);

        let ev = make_evidence(Some(citation_path), "sha256:anything".to_string(), "code");
        assert_eq!(verify_evidence(&ev, dir).unwrap(), false);
    }

    #[test]
    fn test_parse_citation_path_valid() {
        let (path, start, end) = parse_citation_path("src/foo.rs:42-58").unwrap();
        assert_eq!(path, "src/foo.rs");
        assert_eq!(start, 42);
        assert_eq!(end, 58);
    }

    #[test]
    fn test_parse_citation_path_valid_nested() {
        // Path with colons in directory name (unusual but valid)
        let (path, start, end) = parse_citation_path("a/b/c.rs:0-100").unwrap();
        assert_eq!(path, "a/b/c.rs");
        assert_eq!(start, 0);
        assert_eq!(end, 100);
    }

    #[test]
    fn test_parse_citation_path_malformed_no_colon() {
        assert!(parse_citation_path("src/foo.rs").is_err());
    }

    #[test]
    fn test_parse_citation_path_malformed_no_dash() {
        assert!(parse_citation_path("src/foo.rs:42").is_err());
    }

    #[test]
    fn test_parse_citation_path_malformed_non_numeric() {
        assert!(parse_citation_path("src/foo.rs:abc-def").is_err());
    }

    #[test]
    fn test_verify_evidence_hash_with_prefix() {
        // sha256: prefix must be stripped and still match
        let mut tmp = NamedTempFile::new().unwrap();
        let content = b"prefixed hash test";
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();

        let file_name = tmp.path().file_name().unwrap().to_string_lossy().to_string();
        let dir = tmp.path().parent().unwrap();
        let citation_path = format!("{}:0-{}", file_name, content.len());

        let bare_hash = hash_bytes(content);
        let prefixed_hash = format!("sha256:{bare_hash}");

        let ev = make_evidence(Some(citation_path), prefixed_hash, "code");
        assert_eq!(verify_evidence(&ev, dir).unwrap(), true);
    }

    #[test]
    fn test_verify_evidence_rejects_absolute_path() {
        use std::fs as stdfs;
        let dir = tempfile::tempdir().unwrap();
        // Ensure /etc/passwd is not read; citation_path points outside repo_root.
        let ev = make_evidence(
            Some("/etc/passwd:0-10".to_string()),
            "sha256:anything".to_string(),
            "code",
        );
        // Must return Ok(false) — no panic, no Err, no read outside repo.
        assert_eq!(verify_evidence(&ev, dir.path()).unwrap(), false);
        // Confirm we did not create any artifact inside the tempdir from this call.
        assert_eq!(stdfs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn test_verify_evidence_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        // Create a file outside the tempdir to make sure it can't be reached.
        let outer = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outer.path(), b"secret").unwrap();

        let ev = make_evidence(
            Some("../escape.txt:0-5".to_string()),
            "sha256:anything".to_string(),
            "code",
        );
        assert_eq!(verify_evidence(&ev, dir.path()).unwrap(), false);
    }

    #[test]
    fn test_verify_evidence_rejects_root_path() {
        let dir = tempfile::tempdir().unwrap();
        let ev = make_evidence(
            Some("/:0-1".to_string()),
            "sha256:anything".to_string(),
            "code",
        );
        assert_eq!(verify_evidence(&ev, dir.path()).unwrap(), false);
    }

    #[test]
    fn test_safe_join_accepts_repo_relative() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("probe.txt");
        std::fs::write(&file_path, b"data").unwrap();

        let result = safe_join(dir.path(), "probe.txt");
        assert!(result.is_some());
        // Canonical path must be inside the tempdir.
        let canon = result.unwrap();
        assert!(canon.starts_with(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn test_verify_evidence_non_code_kind() {
        // Non-code kinds return false (Phase 2 TODO)
        let ev = make_evidence(
            Some("src/foo.rs:0-10".to_string()),
            "sha256:anything".to_string(),
            "test",
        );
        assert_eq!(verify_evidence(&ev, Path::new("/tmp")).unwrap(), false);
    }
}
