//! Evidence verification: HEAD-byte-hash check, plus citation relocation.
//!
//! Per locked decision L1: verified = sha256(bytes of citation_path at
//! current HEAD over recorded byte range) == citation_hash. Detects F1
//! truth decay loudly. citation_sha is stored for provenance only.
//!
//! When the hash no longer matches, the citation may have MOVED rather than
//! decayed. [`RelocationPolicy`] decides whether to look for it. Relocation is
//! deliberately conservative (`agent-kb/tla/CitationRelocation.tla`,
//! `.omc/plans/kb-delivery.md` §6 S1): a strong excerpt and exactly one
//! candidate, or nothing. It reports where the code went; it never claims the
//! recorded hash still describes it.

use crate::models::{Evidence, VerificationStatus};
use anyhow::{anyhow, bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
#[cfg(not(target_os = "linux"))]
use std::sync::Once;

/// Maximum file size allowed for verification: 64 MiB.
/// Per D3 (2026-09-02), whole-file citations use this as their only size cap;
/// explicit ranges remain separately capped by [`MAX_RANGE_BYTES`].
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum range size allowed for verification: 4 MiB.
/// Per D3 (2026-09-02), this applies only to explicit `file:start-end`
/// citations; whole-file citations are bounded only by [`MAX_FILE_BYTES`].
const MAX_RANGE_BYTES: u64 = 4 * 1024 * 1024;

/// Fixed buffer size for streaming citation bytes into the hasher.
const HASH_READ_BUFFER_BYTES: usize = 64 * 1024;

/// Hard per-relocation scan budget: 32 MiB of file content read across the
/// whole search. Bounds the worst single unit of work handed to a verification
/// worker (pre-mortem S2 head-of-line blocking).
pub const MAX_RELOCATION_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// Minimum excerpt length, in bytes, before a relocation search may run.
pub const MIN_EXCERPT_BYTES: usize = 64;

/// Minimum number of lines an excerpt must span before a relocation search may
/// run. Both floors apply; a long single line is still too weak.
pub const MIN_EXCERPT_LINES: usize = 2;

/// Directory names never descended into during a repo-wide relocation search.
const EXCLUDED_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// How hard to look for a citation whose hash no longer matches.
///
/// This is a REQUIRED argument of [`verify_evidence`] with no `Default` impl,
/// on purpose: a default would let a future call site opt into filesystem
/// walks silently (plan §10, rejected alternatives).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocationPolicy {
    /// Never search. A mismatched citation is simply unverified.
    Never,
    /// Search the cited file only (catches in-file moves).
    FileOnly,
    /// Search the cited file, then the repo (catches cross-file moves).
    FileThenRepo,
}

/// Why a citation came back unverified. Reported, never guessed around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnverifiedReason {
    /// Only `kind = "code"` is verified in Phase 1.
    NotCodeKind,
    /// Evidence row carries no `citation_path`.
    MissingCitationPath,
    /// `citation_path` escapes the repo root.
    PathEscape,
    /// No file at `citation_path`.
    FileMissing,
    /// Cited byte range lies outside the file.
    RangeOutOfBounds,
    /// File exceeds `MAX_FILE_BYTES`.
    FileTooLarge,
    /// Cited range exceeds `MAX_RANGE_BYTES`.
    RangeTooLarge,
    /// I/O error while reading the cited range.
    ReadError,
    /// The file and range are intact but the bytes no longer hash to the
    /// recorded value.
    HashMismatch,
    /// Excerpt is below `MIN_EXCERPT_BYTES` or `MIN_EXCERPT_LINES` (or absent),
    /// so no relocation search was attempted.
    ExcerptTooWeak,
    /// The excerpt was not found in scope.
    NoCandidate,
    /// The excerpt was found in more than one place; the multiplicity is
    /// reported rather than resolved by guessing (pre-mortem S1).
    NonUnique {
        /// Number of candidate matches found before the search stopped.
        candidates: usize,
    },
    /// The relocation search hit `MAX_RELOCATION_SCAN_BYTES`.
    ScanCapExceeded,
}

impl UnverifiedReason {
    /// Stable machine-readable spelling for CLI/JSON surfaces.
    pub fn as_str(&self) -> &'static str {
        match self {
            UnverifiedReason::NotCodeKind => "not_code_kind",
            UnverifiedReason::MissingCitationPath => "missing_citation_path",
            UnverifiedReason::PathEscape => "path_escape",
            UnverifiedReason::FileMissing => "file_missing",
            UnverifiedReason::RangeOutOfBounds => "range_out_of_bounds",
            UnverifiedReason::FileTooLarge => "file_too_large",
            UnverifiedReason::RangeTooLarge => "range_too_large",
            UnverifiedReason::ReadError => "read_error",
            UnverifiedReason::HashMismatch => "hash_mismatch",
            UnverifiedReason::ExcerptTooWeak => "excerpt_too_weak",
            UnverifiedReason::NoCandidate => "no_candidate",
            UnverifiedReason::NonUnique { .. } => "non_unique",
            UnverifiedReason::ScanCapExceeded => "scan_cap_exceeded",
        }
    }
}

/// Result of one verification pass over one evidence row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationOutcome {
    /// Where this row landed in the status lattice.
    pub status: VerificationStatus,
    /// Proposed new `citation_path` (`file:start-end`) — `Some` exactly when
    /// `status == Relocated`. Writing it is a separate, opt-in decision
    /// (`relocation_autoheal`); computing it is not.
    pub relocated_to: Option<String>,
    /// `Some` exactly when `status == Unverified`.
    pub reason: Option<UnverifiedReason>,
}

impl VerificationOutcome {
    fn verified() -> Self {
        Self {
            status: VerificationStatus::Verified,
            relocated_to: None,
            reason: None,
        }
    }

    fn relocated(new_path: String) -> Self {
        Self {
            status: VerificationStatus::Relocated,
            relocated_to: Some(new_path),
            reason: None,
        }
    }

    fn unverified(reason: UnverifiedReason) -> Self {
        Self {
            status: VerificationStatus::Unverified,
            relocated_to: None,
            reason: Some(reason),
        }
    }

    /// True only for [`VerificationStatus::Verified`] — a relocated row is not
    /// a verified row.
    pub fn is_verified(&self) -> bool {
        self.status == VerificationStatus::Verified
    }
}

/// Safely join `rel` onto `repo_root`, rejecting any path that escapes the root.
///
/// Rejects: absolute paths, any `..` / root / prefix components.
/// Canonicalizes both sides and verifies containment.
/// Returns `None` on any rejection or I/O error during canonicalization.
pub(crate) fn safe_join(repo_root: &Path, rel: &str) -> Option<PathBuf> {
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

pub(crate) struct CitationHash {
    pub(crate) sha256_hex: String,
    pub(crate) file_size: u64,
}

/// The only place a citation_path string is constructed.
///
/// Before the optional-range parser lands, `range = None` must keep emitting
/// the legacy whole-file workaround form `path:0-file_size`. After `.3` lands,
/// the `None` arm becomes a one-line change to return `rel_path.to_string()`.
pub fn format_citation_path(
    rel_path: &str,
    range: Option<(usize, usize)>,
    file_size: u64,
) -> String {
    match range {
        Some((start, end)) => format!("{rel_path}:{start}-{end}"),
        None => format!("{rel_path}:0-{file_size}"),
    }
}

pub fn compute_citation_hash(
    repo_root: &Path,
    rel_path: &str,
    range: Option<(usize, usize)>,
) -> Result<String> {
    Ok(format!(
        "sha256:{}",
        compute_citation_hash_and_size(repo_root, rel_path, range)?.sha256_hex
    ))
}

pub(crate) fn compute_citation_hash_and_size(
    repo_root: &Path,
    rel_path: &str,
    range: Option<(usize, usize)>,
) -> Result<CitationHash> {
    hash_citation_bytes(repo_root, rel_path, range).map_err(|reason| {
        let detail = match reason {
            UnverifiedReason::FileTooLarge => format!(
                "file exceeds MAX_FILE_BYTES ({} bytes)",
                MAX_FILE_BYTES
            ),
            UnverifiedReason::RangeTooLarge => format!(
                "range exceeds MAX_RANGE_BYTES ({} bytes)",
                MAX_RANGE_BYTES
            ),
            _ => reason.as_str().to_string(),
        };
        anyhow!("compute citation hash for {rel_path:?}: {detail}")
    })
}

fn hash_citation_bytes(
    repo_root: &Path,
    file_rel: &str,
    range: Option<(usize, usize)>,
) -> std::result::Result<CitationHash, UnverifiedReason> {
    let file_abs = match safe_join(repo_root, file_rel) {
        Some(p) => p,
        None => {
            return Err(if repo_root.join(file_rel).exists() {
                UnverifiedReason::PathEscape
            } else {
                UnverifiedReason::FileMissing
            })
        }
    };

    let mut file = match File::open(&file_abs) {
        Ok(f) => f,
        Err(_) => return Err(UnverifiedReason::FileMissing),
    };

    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(_) => return Err(UnverifiedReason::ReadError),
    };
    if !metadata.is_file() {
        return Err(UnverifiedReason::FileMissing);
    }
    if !opened_file_within_repo(&file, &file_abs, repo_root) {
        return Err(UnverifiedReason::ReadError);
    }

    let file_size = metadata.len();
    if file_size > MAX_FILE_BYTES {
        return Err(UnverifiedReason::FileTooLarge);
    }

    let (start, end) = match range {
        Some((start, end)) => {
            if end < start {
                return Err(UnverifiedReason::RangeOutOfBounds);
            }
            if start as u64 > file_size || end as u64 > file_size {
                return Err(UnverifiedReason::RangeOutOfBounds);
            }
            let range_size = (end - start) as u64;
            if range_size > MAX_RANGE_BYTES {
                return Err(UnverifiedReason::RangeTooLarge);
            }
            (start, end)
        }
        None => (0usize, file_size as usize),
    };

    if file.seek(SeekFrom::Start(start as u64)).is_err() {
        return Err(UnverifiedReason::ReadError);
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; HASH_READ_BUFFER_BYTES];
    let mut remaining = (end - start) as u64;

    while remaining > 0 {
        let chunk_len = remaining.min(HASH_READ_BUFFER_BYTES as u64) as usize;
        let read = match file.read(&mut buffer[..chunk_len]) {
            Ok(read) => read,
            Err(_) => return Err(UnverifiedReason::ReadError),
        };
        if read == 0 {
            return Err(UnverifiedReason::ReadError);
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }

    Ok(CitationHash {
        sha256_hex: format!("{:x}", hasher.finalize()),
        file_size,
    })
}

/// Outcome of the direct hash check at the recorded citation path.
enum HashCheck {
    /// sha256(bytes at citation_path) == citation_hash.
    Match,
    /// File and range are readable but the bytes hash to something else.
    Mismatch,
    /// The check could not run; carries the reason it could not.
    Failed(UnverifiedReason),
}

/// Hash the recorded byte range at `file_rel` and compare against `expected`.
///
/// Never propagates I/O errors as `Err` — those are folded into
/// [`HashCheck::Failed`] per AC16.
fn hash_check_at_citation(
    repo_root: &Path,
    file_rel: &str,
    start: usize,
    end: usize,
    expected: &str,
) -> HashCheck {
    let computed = match hash_citation_bytes(repo_root, file_rel, Some((start, end))) {
        Ok(computed) => computed.sha256_hex,
        Err(reason) => return HashCheck::Failed(reason),
    };

    if computed.eq_ignore_ascii_case(strip_hash_prefix(expected)) {
        HashCheck::Match
    } else {
        HashCheck::Mismatch
    }
}

#[cfg(not(target_os = "linux"))]
static DESCRIPTOR_CONTAINMENT_DEGRADED_NOTE: Once = Once::new();

fn opened_file_within_repo(file: &File, file_abs: &Path, repo_root: &Path) -> bool {
    let canonical_root = match repo_root.canonicalize() {
        Ok(root) => root,
        Err(_) => return false,
    };

    #[cfg(target_os = "linux")]
    {
        let fd = file.as_raw_fd();
        let resolved = match std::fs::read_link(format!("/proc/self/fd/{fd}")) {
            Ok(path) => path,
            Err(_) => return false,
        };
        return resolved.starts_with(&canonical_root);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        DESCRIPTOR_CONTAINMENT_DEGRADED_NOTE.call_once(|| {
            eprintln!(
                "verification: descriptor containment degraded on this platform; falling back to canonicalized path checks"
            );
        });
        let resolved = match file_abs.canonicalize() {
            Ok(path) => path,
            Err(_) => return false,
        };
        return resolved.starts_with(&canonical_root);
    }
}

/// An excerpt is strong enough to relocate from only if it clears BOTH floors.
///
/// A long single line (a minified bundle, a wide table row) is common enough
/// across a repo that uniqueness there is luck, not evidence.
fn excerpt_is_strong(excerpt: &str) -> bool {
    excerpt.len() >= MIN_EXCERPT_BYTES && excerpt.lines().count() >= MIN_EXCERPT_LINES
}

/// Verify a single evidence row against the file at HEAD, optionally searching
/// for a citation that moved.
///
/// The direct hash check runs first and short-circuits: a matching hash is
/// `Verified` and NO relocation search is performed, which is what makes
/// `NoHealOnVerified` structural rather than a guard (spec, `Verify` action).
///
/// `policy` is required and has no default — see [`RelocationPolicy`].
///
/// Returns `Err` only for a malformed `citation_path` (a programming bug, not
/// a runtime condition); every runtime failure folds into
/// [`VerificationStatus::Unverified`] with a reason.
pub fn verify_evidence(
    ev: &Evidence,
    repo_root: &Path,
    policy: RelocationPolicy,
) -> Result<VerificationOutcome> {
    // Only "code" kind is verified in Phase 1. Other kinds deferred to Phase 2.
    // TODO(Phase 2): add test/command/user/derived verification paths.
    if ev.kind != "code" {
        return Ok(VerificationOutcome::unverified(
            UnverifiedReason::NotCodeKind,
        ));
    }

    let raw_path = match &ev.citation_path {
        Some(p) => p,
        None => {
            return Ok(VerificationOutcome::unverified(
                UnverifiedReason::MissingCitationPath,
            ))
        }
    };

    let (file_rel, start, end) = parse_citation_path(raw_path)?;

    let decayed = match hash_check_at_citation(repo_root, file_rel, start, end, &ev.citation_hash) {
        HashCheck::Match => return Ok(VerificationOutcome::verified()),
        HashCheck::Mismatch => UnverifiedReason::HashMismatch,
        HashCheck::Failed(reason) => reason,
    };

    if policy == RelocationPolicy::Never {
        return Ok(VerificationOutcome::unverified(decayed));
    }

    if matches!(
        decayed,
        UnverifiedReason::PathEscape | UnverifiedReason::FileMissing
    ) {
        return Ok(VerificationOutcome::unverified(decayed));
    }

    let excerpt = match &ev.citation_excerpt {
        Some(e) if excerpt_is_strong(e) => e.as_str(),
        _ => {
            return Ok(VerificationOutcome::unverified(
                if decayed == UnverifiedReason::HashMismatch {
                    UnverifiedReason::ExcerptTooWeak
                } else {
                    decayed
                },
            ))
        }
    };

    let candidate = match search_for_excerpt(repo_root, file_rel, excerpt, policy) {
        ExcerptSearch::Unique(c) => c,
        ExcerptSearch::NotFound => {
            return Ok(VerificationOutcome::unverified(
                UnverifiedReason::NoCandidate,
            ))
        }
        ExcerptSearch::NonUnique(candidates) => {
            return Ok(VerificationOutcome::unverified(
                UnverifiedReason::NonUnique { candidates },
            ))
        }
        ExcerptSearch::CapExceeded => {
            return Ok(VerificationOutcome::unverified(
                UnverifiedReason::ScanCapExceeded,
            ))
        }
    };

    // Reconstruct the range by anchoring the ORIGINAL range length at the match
    // offset. When the excerpt is the head of the cited range — the shape kb_add
    // records — this reproduces the original range exactly, so a later pass can
    // re-hash it against the unchanged stored hash. When it is not, the range is
    // wrong and that pass simply will not verify: the excerpt match is never
    // carried over as an assertion about the full range.
    let new_start = candidate.offset;
    let new_end = new_start + (end - start);
    if new_end as u64 > candidate.file_size {
        return Ok(VerificationOutcome::unverified(
            UnverifiedReason::RangeOutOfBounds,
        ));
    }

    let new_path = format_citation_path(
        &candidate.rel_path,
        Some((new_start, new_end)),
        candidate.file_size,
    );
    if new_path == *raw_path {
        // The excerpt is still exactly where it was; the range's bytes changed
        // around it. That is decay, not a move.
        return Ok(VerificationOutcome::unverified(decayed));
    }

    Ok(VerificationOutcome::relocated(new_path))
}

/// A single location the excerpt was found at.
struct Candidate {
    /// Repo-relative path, `/`-separated.
    rel_path: String,
    /// Byte offset of the match within the file.
    offset: usize,
    /// Size of the containing file, for range-overflow checking.
    file_size: u64,
}

enum ExcerptSearch {
    Unique(Candidate),
    NotFound,
    /// Two or more matches; carries the count observed before stopping.
    NonUnique(usize),
    /// `MAX_RELOCATION_SCAN_BYTES` reached.
    CapExceeded,
}

/// Look for `excerpt`, first in the cited file, then (under
/// [`RelocationPolicy::FileThenRepo`]) across the repo.
///
/// Stops as soon as a second candidate is seen: the answer "not unique" needs
/// no further evidence, and stopping bounds the work.
fn search_for_excerpt(
    repo_root: &Path,
    cited_rel: &str,
    excerpt: &str,
    policy: RelocationPolicy,
) -> ExcerptSearch {
    let needle = excerpt.as_bytes();
    let mut budget = MAX_RELOCATION_SCAN_BYTES;

    // -- the cited file first: an in-file move is the cheap, common case --
    if let Some(abs) = safe_join(repo_root, cited_rel) {
        match scan_file(&abs, repo_root, needle, &mut budget) {
            FileScan::CapExceeded => return ExcerptSearch::CapExceeded,
            FileScan::Hits { count, first, size } if count > 0 => {
                if count > 1 {
                    return ExcerptSearch::NonUnique(count);
                }
                return ExcerptSearch::Unique(Candidate {
                    rel_path: normalize_rel(cited_rel),
                    offset: first,
                    file_size: size,
                });
            }
            _ => {}
        }
    }

    if policy != RelocationPolicy::FileThenRepo {
        return ExcerptSearch::NotFound;
    }

    // -- repo walk --
    let canon_root = match repo_root.canonicalize() {
        Ok(r) => r,
        Err(_) => return ExcerptSearch::NotFound,
    };
    let excluded = excluded_names(&canon_root);
    let mut found: Option<Candidate> = None;
    let mut total = 0usize;
    let mut stack = vec![canon_root.clone()];

    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
            Err(_) => continue,
        };
        // Deterministic traversal order: the same tree must always yield the
        // same candidate, or `prop_relocation_is_idempotent` is a lie.
        entries.sort();

        for path in entries {
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if excluded.contains(&name) {
                continue;
            }
            // Do not follow symlinks: they invite cycles and escapes.
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }

            match scan_file(&path, &canon_root, needle, &mut budget) {
                FileScan::CapExceeded => return ExcerptSearch::CapExceeded,
                FileScan::Hits { count, first, size } if count > 0 => {
                    total += count;
                    if total > 1 {
                        return ExcerptSearch::NonUnique(total);
                    }
                    let rel = path
                        .strip_prefix(&canon_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    found = Some(Candidate {
                        rel_path: rel,
                        offset: first,
                        file_size: size,
                    });
                }
                _ => {}
            }
        }
    }

    match found {
        Some(c) => ExcerptSearch::Unique(c),
        None => ExcerptSearch::NotFound,
    }
}

enum FileScan {
    Hits {
        count: usize,
        first: usize,
        size: u64,
    },
    Skipped,
    CapExceeded,
}

/// Read `path` and count non-overlapping occurrences of `needle`, charging the
/// bytes read against `budget`.
fn scan_file(path: &Path, repo_root: &Path, needle: &[u8], budget: &mut u64) -> FileScan {
    // Reject links immediately before opening, then validate the opened object.
    // All subsequent metadata and bytes come from this descriptor: the path is
    // never reopened after the containment check.
    let path_meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return FileScan::Skipped,
    };
    if path_meta.file_type().is_symlink() || !path_meta.is_file() {
        return FileScan::Skipped;
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return FileScan::Skipped,
    };
    let meta = match file.metadata() {
        Ok(m) if m.is_file() => m,
        _ => return FileScan::Skipped,
    };
    if !opened_file_within_repo(&file, path, repo_root) {
        return FileScan::Skipped;
    }
    let size = meta.len();
    if size > MAX_FILE_BYTES || (size as usize) < needle.len() {
        return FileScan::Skipped;
    }
    if size > *budget {
        return FileScan::CapExceeded;
    }
    *budget -= size;

    let mut bytes = Vec::with_capacity(size as usize);
    if file.take(size + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 != size {
        return FileScan::Skipped;
    }
    let (count, first) = count_occurrences(&bytes, needle);
    FileScan::Hits {
        count,
        first: first.unwrap_or(0),
        size,
    }
}

/// Count non-overlapping occurrences of `needle` in `hay`, returning the count
/// and the first offset.
///
/// Deliberately a plain first-byte-skip scan: the inner comparison only runs on
/// a first-byte hit, and both the file size and the total scan are already
/// capped, so a substring-search dependency would buy nothing here.
fn count_occurrences(hay: &[u8], needle: &[u8]) -> (usize, Option<usize>) {
    if needle.is_empty() || hay.len() < needle.len() {
        return (0, None);
    }
    let lead = needle[0];
    let mut count = 0usize;
    let mut first = None;
    let mut i = 0usize;
    while i + needle.len() <= hay.len() {
        if hay[i] == lead && &hay[i..i + needle.len()] == needle {
            if first.is_none() {
                first = Some(i);
            }
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    (count, first)
}

/// Names excluded from the repo walk: the hard-coded set plus the plain
/// directory/file names listed in the repo-root `.gitignore`.
///
/// Only literal names are honoured — a line with a glob, a `!` negation, or an
/// interior `/` is ignored rather than half-interpreted. Full gitignore
/// semantics are out of scope; the hard-coded set carries the guarantee.
fn excluded_names(repo_root: &Path) -> HashSet<String> {
    let mut names: HashSet<String> = EXCLUDED_DIRS.iter().map(|s| s.to_string()).collect();
    if let Ok(content) = std::fs::read_to_string(repo_root.join(".gitignore")) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let candidate = line.trim_end_matches('/');
            if candidate.is_empty()
                || candidate.contains('/')
                || candidate.contains('*')
                || candidate.contains('?')
                || candidate.contains('[')
            {
                continue;
            }
            names.insert(candidate.to_string());
        }
    }
    names
}

/// Normalize a repo-relative citation path to `/`-separated form.
fn normalize_rel(rel: &str) -> String {
    rel.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_evidence(citation_path: Option<String>, citation_hash: String, kind: &str) -> Evidence {
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

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();
        let citation_path = format!("{}:{}-{}", file_name, start, end);

        let ev = make_evidence(Some(citation_path), expected_hash, "code");
        assert_eq!(
            verify_evidence(&ev, dir, RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            true
        );
    }

    #[test]
    fn test_verify_evidence_hash_mismatch() {
        let mut tmp = NamedTempFile::new().unwrap();
        let content = b"Hello, world! This is test content.";
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();
        let citation_path = format!("{}:0-5", file_name);

        // Wrong hash
        let ev = make_evidence(Some(citation_path), "sha256:deadbeef".to_string(), "code");
        assert_eq!(
            verify_evidence(&ev, dir, RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
    }

    #[test]
    fn test_verify_evidence_missing_file() {
        let ev = make_evidence(
            Some("nonexistent/path/file.rs:0-10".to_string()),
            "sha256:anything".to_string(),
            "code",
        );
        // Must return false, not panic or Err
        assert_eq!(
            verify_evidence(&ev, Path::new("/tmp"), RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
    }

    #[test]
    fn test_verify_evidence_out_of_range() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"short file").unwrap();
        tmp.flush().unwrap();

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();
        // File is 10 bytes; citation claims 1000-2000
        let citation_path = format!("{}:1000-2000", file_name);

        let ev = make_evidence(Some(citation_path), "sha256:anything".to_string(), "code");
        assert_eq!(
            verify_evidence(&ev, dir, RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
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

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();
        let citation_path = format!("{}:0-{}", file_name, content.len());

        let bare_hash = hash_bytes(content);
        let prefixed_hash = format!("sha256:{bare_hash}");

        let ev = make_evidence(Some(citation_path), prefixed_hash, "code");
        assert_eq!(
            verify_evidence(&ev, dir, RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            true
        );
    }

    #[test]
    fn test_hash_check_at_citation_matches_expected_ranges() {
        let mut tmp = NamedTempFile::new().unwrap();
        let content =
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ--tail";
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();

        for (start, end) in [
            (0usize, 8usize),
            (10usize, 36usize),
            (content.len() - 6, content.len()),
            (0usize, content.len()),
        ] {
            let expected_hash = hash_bytes(&content[start..end]);
            assert!(matches!(
                hash_check_at_citation(dir, &file_name, start, end, &expected_hash),
                HashCheck::Match
            ));
        }
    }

    #[test]
    fn test_hash_check_at_citation_matches_empty_file_range() {
        let tmp = NamedTempFile::new().unwrap();
        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();
        let expected_hash = hash_bytes(b"");

        assert!(matches!(
            hash_check_at_citation(dir, &file_name, 0, 0, &expected_hash),
            HashCheck::Match
        ));
    }

    #[test]
    fn test_hash_check_at_citation_large_range_crosses_chunk_boundaries() {
        let mut tmp = NamedTempFile::new().unwrap();
        let content: Vec<u8> = (0..(300 * 1024)).map(|i| (i % 251) as u8).collect();
        tmp.write_all(&content).unwrap();
        tmp.flush().unwrap();

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();
        let start = 17usize;
        let end = start + (300 * 1024) - 123;
        let expected_hash = hash_bytes(&content[start..end]);

        assert!(matches!(
            hash_check_at_citation(dir, &file_name, start, end, &expected_hash),
            HashCheck::Match
        ));
    }

    #[test]
    fn test_hash_check_at_citation_shrinking_file_folds_to_read_error() {
        let mut tmp = NamedTempFile::new().unwrap();
        let content = vec![b'x'; 128 * 1024];
        tmp.write_all(&content).unwrap();
        tmp.flush().unwrap();

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();
        let expected_hash = hash_bytes(&content);

        std::fs::OpenOptions::new()
            .write(true)
            .open(tmp.path())
            .unwrap()
            .set_len(1024)
            .unwrap();

        // Shrink happens before the read starts, so the metadata bounds check
        // catches it as RangeOutOfBounds. A shrink mid-read (after bounds are
        // checked but before all bytes are consumed) would instead surface as
        // ReadError from the short read; that path isn't deterministically
        // reachable here without fault injection.
        assert!(matches!(
            hash_check_at_citation(dir, &file_name, 0, content.len(), &expected_hash),
            HashCheck::Failed(UnverifiedReason::RangeOutOfBounds)
        ));
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
        assert_eq!(
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
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
        assert_eq!(
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
    }

    #[test]
    fn test_verify_evidence_rejects_root_path() {
        let dir = tempfile::tempdir().unwrap();
        let ev = make_evidence(
            Some("/:0-1".to_string()),
            "sha256:anything".to_string(),
            "code",
        );
        assert_eq!(
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
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
        assert_eq!(
            verify_evidence(&ev, Path::new("/tmp"), RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
    }

    #[test]
    fn test_verify_evidence_rejects_oversized_range() {
        // Range larger than MAX_RANGE_BYTES (4 MiB) must be rejected.
        let mut tmp = NamedTempFile::new().unwrap();
        let content = b"small file";
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();

        // Request a range of 5 MiB (larger than MAX_RANGE_BYTES)
        // This citation claims bytes 0 to 5_242_880, but the file is only 10 bytes.
        let citation_path = format!("{}:0-5242880", file_name);

        let ev = make_evidence(Some(citation_path), "sha256:anything".to_string(), "code");
        assert_eq!(
            verify_evidence(&ev, dir, RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
    }

    #[test]
    fn test_verify_evidence_rejects_out_of_bounds_end() {
        // Citation with end beyond file size must be rejected.
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(&vec![0u8; 100]).unwrap(); // 100 bytes
        tmp.flush().unwrap();

        let file_name = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let dir = tmp.path().parent().unwrap();

        // Citation claims bytes 0-200, but file is only 100 bytes.
        let citation_path = format!("{}:0-200", file_name);

        let ev = make_evidence(Some(citation_path), "sha256:anything".to_string(), "code");
        assert_eq!(
            verify_evidence(&ev, dir, RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false
        );
    }

    /// The relocation scan budget is a hard cap, not a hint: a single file
    /// larger than the remaining budget stops the search rather than being
    /// read (pre-mortem S2 — the worst single unit of work must stay bounded).
    ///
    /// Uses a sparse file so no 32 MiB is actually written.
    #[test]
    fn test_scan_file_refuses_to_exceed_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.txt");
        let f = File::create(&big).unwrap();
        f.set_len(MAX_RELOCATION_SCAN_BYTES + 1).unwrap();
        drop(f);

        let mut budget = MAX_RELOCATION_SCAN_BYTES;
        assert!(matches!(
            scan_file(&big, dir.path(), b"needle", &mut budget),
            FileScan::CapExceeded
        ));
        assert_eq!(
            budget, MAX_RELOCATION_SCAN_BYTES,
            "budget must not be spent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_scan_file_rejects_symlink_to_outside_target() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside secret needle").unwrap();
        let candidate = repo.path().join("candidate.txt");
        std::fs::write(&candidate, b"inside needle").unwrap();

        let mut budget = 1024;
        assert!(matches!(
            scan_file(&candidate, repo.path(), b"needle", &mut budget),
            FileScan::Hits { count: 1, .. }
        ));

        std::fs::remove_file(&candidate).unwrap();
        symlink(outside.path(), &candidate).unwrap();
        let mut budget = 1024;
        assert!(matches!(
            scan_file(&candidate, repo.path(), b"outside secret", &mut budget),
            FileScan::Skipped
        ));
        assert_eq!(
            budget, 1024,
            "the outside target must not be charged or read"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_scan_file_swap_to_symlink_never_matches_outside_content() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside secret needle").unwrap();
        let candidate = repo.path().join("candidate.txt");
        std::fs::write(&candidate, b"inside only").unwrap();

        let path_meta = std::fs::symlink_metadata(&candidate).unwrap();
        assert!(path_meta.is_file());
        std::fs::remove_file(&candidate).unwrap();
        symlink(outside.path(), &candidate).unwrap();

        let mut budget = 1024;
        assert!(matches!(
            scan_file(&candidate, repo.path(), b"outside secret", &mut budget),
            FileScan::Skipped
        ));
        assert_eq!(
            budget, 1024,
            "candidate count must remain unchanged on swap"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_containment_rejects_sibling_prefix_path() {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir().unwrap();
        let repo = sandbox.path().join("repo");
        let sibling = sandbox.path().join("repo-sibling");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&sibling).unwrap();

        let sibling_file = sibling.join("secret.txt");
        let sibling_content = concat!(
            "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\n",
            "sibling bytes must never verify through a repo-contained symlink\n"
        )
        .as_bytes();
        std::fs::write(&sibling_file, sibling_content).unwrap();

        let repo_link = repo.join("linked.txt");
        symlink(&sibling_file, &repo_link).unwrap();

        let expected_hash = hash_bytes(sibling_content);
        let ev = Evidence {
            id: "ev-test".to_string(),
            entry_id: "entry-test".to_string(),
            kind: "code".to_string(),
            citation_path: Some(format!("linked.txt:0-{}", sibling_content.len())),
            citation_sha: None,
            citation_hash: expected_hash,
            citation_excerpt: Some(String::from_utf8(sibling_content.to_vec()).unwrap()),
            derived_from: None,
            recorded_at: None,
        };

        let outcome = verify_evidence(&ev, &repo, RelocationPolicy::FileOnly).unwrap();
        assert_eq!(outcome.status, VerificationStatus::Unverified);
        // PathEscape now propagates directly instead of falling through to excerpt search.
        assert_eq!(outcome.reason, Some(UnverifiedReason::PathEscape));
        assert_eq!(outcome.relocated_to, None);
        assert!(!outcome.is_verified(), "sibling content must never verify");
    }

    #[cfg(unix)]
    #[test]
    fn test_opened_descriptor_containment_rejects_sibling() {
        let sandbox = tempfile::tempdir().unwrap();
        let repo = sandbox.path().join("repo");
        let sibling = sandbox.path().join("repo-sibling");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&sibling).unwrap();

        let repo_file = repo.join("inside.txt");
        let sibling_file = sibling.join("outside.txt");
        std::fs::write(&repo_file, b"inside").unwrap();
        std::fs::write(&sibling_file, b"outside").unwrap();

        let inside = std::fs::File::open(&repo_file).unwrap();
        let outside = std::fs::File::open(&sibling_file).unwrap();

        assert!(opened_file_within_repo(&inside, &repo_file, &repo));
        assert!(!opened_file_within_repo(&outside, &sibling_file, &repo));
    }

    #[test]
    fn test_count_occurrences_is_non_overlapping_and_reports_first() {
        assert_eq!(count_occurrences(b"aaaa", b"aa"), (2, Some(0)));
        assert_eq!(count_occurrences(b"xxabab", b"ab"), (2, Some(2)));
        assert_eq!(count_occurrences(b"abc", b"zz"), (0, None));
        assert_eq!(count_occurrences(b"ab", b"abcdef"), (0, None));
    }

    #[test]
    fn test_excerpt_floors_are_conjunctive() {
        let long_one_line = "a".repeat(MIN_EXCERPT_BYTES * 2);
        assert!(!excerpt_is_strong(&long_one_line));
        let short_two_lines = "a\nb";
        assert!(!excerpt_is_strong(short_two_lines));
        let ok = format!("{}\n{}", "a".repeat(MIN_EXCERPT_BYTES), "b");
        assert!(excerpt_is_strong(&ok));
    }

    /// File larger than MAX_FILE_BYTES is rejected.
    ///
    /// Uses `File::set_len` to create a sparse file — O(1) on ext4/btrfs/tmpfs
    /// (no 64 MiB of bytes actually allocated), so the test is fast.
    #[test]
    fn test_verify_evidence_rejects_oversized_file() {
        use std::fs::File;
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.bin");
        let f = File::create(&file_path).unwrap();
        // 1 byte past the cap — fastest way to trip the metadata().len() check.
        f.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(f);

        // Citation range is within the file but the file itself is oversized.
        let ev = make_evidence(
            Some("big.bin:0-10".to_string()),
            "sha256:anything".to_string(),
            "code",
        );
        assert_eq!(
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never)
                .unwrap()
                .is_verified(),
            false,
            "files larger than MAX_FILE_BYTES must return Ok(false)"
        );
    }
}
