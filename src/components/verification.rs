//! Evidence verification: HEAD-byte-hash check, plus citation relocation.
//!
//! Per locked decision L1: verified = sha256(bytes of citation_path at
//! current HEAD over recorded byte range) == citation_hash. Detects F1
//! truth decay loudly. citation_sha is stored for provenance only.
//!
//! When the hash no longer matches, the citation may have MOVED rather than
//! decayed. [`RelocationPolicy`] decides whether to look for it. Relocation is
//! deliberately conservative (`.state/agent-kb/tla/CitationRelocation.tla`,
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
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
///
/// `.state` holds the KB's own managed files (`agent-kb.db`,
/// `agent-kb-events.jsonl`, ...) on the canonical layout; `agent-kb` holds
/// the same files directly at the repository root on the tolerated legacy
/// layout (`<root>/agent-kb/agent-kb.db`, no `.state` wrapper). Both store
/// every recorded `citation_excerpt` verbatim as row/event data. Without
/// these exclusions a repo-wide scan matches its own database as a second
/// "candidate" location for any excerpt the KB has ever recorded, turning a
/// legitimate unique relocation into a false `NonUnique`.
const EXCLUDED_DIRS: [&str; 5] = [".git", "target", "node_modules", ".state", "agent-kb"];

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
    /// Stored `citation_path` does not satisfy the citation grammar.
    MalformedCitationPath,
    /// `citation_path` escapes the repo root.
    PathEscape,
    /// A component of `citation_path` is a symbolic link.
    SymlinkPathRejected,
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
            UnverifiedReason::MalformedCitationPath => "malformed_citation",
            UnverifiedReason::PathEscape => "path_escape",
            UnverifiedReason::SymlinkPathRejected => "symlink_path_rejected",
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
/// Walks existing components without following symbolic links.
/// Returns `None` on any rejection or filesystem error.
pub(crate) fn safe_join(repo_root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    let mut candidate = repo_root.to_path_buf();
    for c in rel_path.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
            Component::CurDir => continue,
            Component::Normal(name) => {
                candidate.push(name);
                if std::fs::symlink_metadata(&candidate)
                    .ok()?
                    .file_type()
                    .is_symlink()
                {
                    return None;
                }
            }
        }
    }
    Some(candidate)
}

/// Parse a whole-file `path` or ranged `path:start-end` citation.
/// Explicit ranges use byte offsets, NOT line numbers.
pub(crate) fn parse_citation_path(s: &str) -> Result<(&str, Option<(usize, usize)>)> {
    if s.is_empty() {
        bail!("citation_path file part must not be empty: {s:?}");
    }
    let Some(colon) = s.rfind(':') else {
        return Ok((s, None));
    };
    let file_part = &s[..colon];
    if file_part.is_empty() {
        bail!("citation_path file part must not be empty: {s:?}");
    }
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
    if start >= end {
        bail!("citation_path start must be less than end: {s:?}");
    }
    Ok((file_part, Some((start, end))))
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
/// `file_size` is retained for API compatibility with callers that compute it;
/// whole-file citations deliberately carry no size anchor.
pub fn format_citation_path(
    rel_path: &str,
    range: Option<(usize, usize)>,
    _file_size: u64,
) -> String {
    match range {
        Some((start, end)) => format!("{rel_path}:{start}-{end}"),
        None => rel_path.to_string(),
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
            UnverifiedReason::FileTooLarge => {
                format!("file exceeds MAX_FILE_BYTES ({} bytes)", MAX_FILE_BYTES)
            }
            UnverifiedReason::RangeTooLarge => {
                format!("range exceeds MAX_RANGE_BYTES ({} bytes)", MAX_RANGE_BYTES)
            }
            _ => reason.as_str().to_string(),
        };
        anyhow!("compute citation hash for {rel_path:?}: {detail}")
    })
}

pub(crate) fn open_citation_descriptor(repo_root: &Path, file_rel: &str) -> Result<File> {
    if syntactically_escapes_root(file_rel) {
        bail!("citation path escapes repository root");
    }
    let file_abs = repo_root.join(file_rel);
    let file = open_citation_file(repo_root, Path::new(file_rel))
        .map_err(|error| anyhow!("open citation file {file_rel:?}: {error}"))?;
    if !opened_file_within_repo(&file, &file_abs, repo_root) {
        bail!("opened citation file is outside repository root");
    }
    Ok(file)
}

pub(crate) fn compute_citation_hash_and_size_from(
    file: &File,
    rel_path: &str,
    range: Option<(usize, usize)>,
) -> Result<CitationHash> {
    hash_citation_bytes_from(file, range).map_err(|reason| {
        let detail = match reason {
            UnverifiedReason::FileTooLarge => {
                format!("file exceeds MAX_FILE_BYTES ({} bytes)", MAX_FILE_BYTES)
            }
            UnverifiedReason::RangeTooLarge => {
                format!("range exceeds MAX_RANGE_BYTES ({} bytes)", MAX_RANGE_BYTES)
            }
            UnverifiedReason::RangeOutOfBounds => match (range, file.metadata()) {
                (Some((_, end)), Ok(metadata)) if end as u64 > metadata.len() => {
                    format!("end offset {end} exceeds file size {}", metadata.len())
                }
                _ => reason.as_str().to_string(),
            },
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
    let reporter = NoopCapabilityReporter;
    let already_emitted = AtomicBool::new(true);
    Verifier::with_capabilities(
        repo_root,
        VerificationCapabilities::platform_default(),
        &reporter,
        &already_emitted,
    )
    .hash_citation_bytes(file_rel, range)
}

fn hash_citation_bytes_from(
    file: &File,
    range: Option<(usize, usize)>,
) -> std::result::Result<CitationHash, UnverifiedReason> {
    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(_) => return Err(UnverifiedReason::ReadError),
    };
    if !metadata.is_file() {
        return Err(UnverifiedReason::FileMissing);
    }

    let file_size = metadata.len();
    if file_size > MAX_FILE_BYTES {
        return Err(UnverifiedReason::FileTooLarge);
    }

    let (start, end) = match range {
        Some((start, end)) => {
            if end <= start {
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

    let mut reader = file;
    if reader.seek(SeekFrom::Start(start as u64)).is_err() {
        return Err(UnverifiedReason::ReadError);
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; HASH_READ_BUFFER_BYTES];
    let mut remaining = (end - start) as u64;

    while remaining > 0 {
        let chunk_len = remaining.min(HASH_READ_BUFFER_BYTES as u64) as usize;
        let read = match reader.read(&mut buffer[..chunk_len]) {
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

/// Open a citation relative to an already-open repository directory.
///
/// Linux resolves the complete path atomically with `openat2`. Older kernels
/// and other Unix targets use a deliberately stricter component walk which
/// rejects symlinks at every level with `O_NOFOLLOW`.
#[cfg(unix)]
fn open_citation_file(repo_root: &Path, rel_path: &Path) -> rustix::io::Result<File> {
    open_citation_file_with_resolver(repo_root, rel_path, Resolver::platform_default())
}

#[cfg(unix)]
fn open_citation_file_with_resolver(
    repo_root: &Path,
    rel_path: &Path,
    resolver: Resolver,
) -> rustix::io::Result<File> {
    #[cfg(target_os = "linux")]
    if resolver == Resolver::Openat2 {
        use rustix::fs::{open, openat2, Mode, OFlags, ResolveFlags};

        let root = open(
            repo_root,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        match openat2(
            &root,
            rel_path,
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
        ) {
            Ok(fd) => return Ok(File::from(fd)),
            Err(rustix::io::Errno::NOSYS) => {}
            Err(error) => return Err(error),
        }
    }

    open_citation_file_fallback(repo_root, rel_path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolver {
    #[cfg(target_os = "linux")]
    Openat2,
    Fallback,
}

impl Resolver {
    fn platform_default() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Openat2
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::Fallback
        }
    }
}

#[cfg(unix)]
fn open_failure_reason(error: rustix::io::Errno, resolver: Resolver) -> UnverifiedReason {
    if error == rustix::io::Errno::LOOP
        || (resolver == Resolver::Fallback && error == rustix::io::Errno::NOTDIR)
    {
        UnverifiedReason::SymlinkPathRejected
    } else {
        UnverifiedReason::FileMissing
    }
}

#[cfg(not(unix))]
fn open_failure_reason(_error: std::io::Error, _resolver: Resolver) -> UnverifiedReason {
    UnverifiedReason::FileMissing
}

#[cfg(not(unix))]
fn open_citation_file(_repo_root: &Path, _rel_path: &Path) -> std::io::Result<File> {
    // The supported deployment targets are Unix. Fail closed rather than
    // reintroducing a pathname-first open on targets without openat.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "descriptor-relative citation opens require openat",
    ))
}

#[cfg(not(unix))]
fn open_citation_file_with_resolver(
    repo_root: &Path,
    rel_path: &Path,
    _resolver: Resolver,
) -> std::io::Result<File> {
    open_citation_file(repo_root, rel_path)
}

#[cfg(unix)]
fn open_citation_file_fallback(repo_root: &Path, rel_path: &Path) -> rustix::io::Result<File> {
    use rustix::fs::{open, openat, Mode, OFlags};

    let mut current: OwnedFd = open(
        repo_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut components = rel_path.components().peekable();
    if components.peek().is_none() {
        return Err(rustix::io::Errno::NOENT);
    }

    while let Some(component) = components.next() {
        let name = match component {
            Component::Normal(name) => name,
            Component::CurDir => continue,
            _ => return Err(rustix::io::Errno::XDEV),
        };
        let is_last = components.peek().is_none();
        let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        if !is_last {
            flags |= OFlags::DIRECTORY;
        }
        current = openat(&current, name, flags, Mode::empty())?;
    }

    Ok(File::from(current))
}

fn syntactically_escapes_root(rel: &str) -> bool {
    let path = Path::new(rel);
    if path.is_absolute() {
        return true;
    }
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth == 0 => return true,
            Component::ParentDir => depth -= 1,
            Component::RootDir | Component::Prefix(_) => return true,
            Component::CurDir => {}
        }
    }
    false
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
#[cfg(test)]
fn hash_check_at_citation(
    repo_root: &Path,
    file_rel: &str,
    range: Option<(usize, usize)>,
    expected: &str,
) -> HashCheck {
    let reporter = NoopCapabilityReporter;
    let already_emitted = AtomicBool::new(true);
    Verifier::with_capabilities(
        repo_root,
        VerificationCapabilities::platform_default(),
        &reporter,
        &already_emitted,
    )
    .hash_check_at_citation(file_rel, range, expected)
}

const DESCRIPTOR_CONTAINMENT_DEGRADED_NOTE: &str =
    "verification: descriptor containment degraded on this platform; falling back to canonicalized path checks";

static DESCRIPTOR_CONTAINMENT_DEGRADED_NOTE_EMITTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerificationCapabilities {
    descriptor_containment_degraded: bool,
    resolver: Resolver,
}

impl VerificationCapabilities {
    fn platform_default() -> Self {
        Self {
            descriptor_containment_degraded: cfg!(not(target_os = "linux")),
            resolver: Resolver::platform_default(),
        }
    }
}

fn hash_check_from(file: &File, range: Option<(usize, usize)>, expected: &str) -> HashCheck {
    let computed = match hash_citation_bytes_from(file, range) {
        Ok(computed) => computed.sha256_hex,
        Err(reason) => return HashCheck::Failed(reason),
    };
    if computed.eq_ignore_ascii_case(strip_hash_prefix(expected)) {
        HashCheck::Match
    } else {
        HashCheck::Mismatch
    }
}

trait CapabilityReporter {
    fn report_descriptor_containment_degraded(&self);
}

struct StderrCapabilityReporter;

impl CapabilityReporter for StderrCapabilityReporter {
    fn report_descriptor_containment_degraded(&self) {
        eprintln!("{DESCRIPTOR_CONTAINMENT_DEGRADED_NOTE}");
    }
}

struct NoopCapabilityReporter;

impl CapabilityReporter for NoopCapabilityReporter {
    fn report_descriptor_containment_degraded(&self) {}
}

struct Verifier<'a, R: CapabilityReporter> {
    repo_root: &'a Path,
    capabilities: VerificationCapabilities,
    reporter: &'a R,
    capability_notice_emitted: &'a AtomicBool,
}

impl<'a, R: CapabilityReporter> Verifier<'a, R> {
    fn new(repo_root: &'a Path, reporter: &'a R) -> Self {
        Self::with_capabilities(
            repo_root,
            VerificationCapabilities::platform_default(),
            reporter,
            &DESCRIPTOR_CONTAINMENT_DEGRADED_NOTE_EMITTED,
        )
    }

    fn with_capabilities(
        repo_root: &'a Path,
        capabilities: VerificationCapabilities,
        reporter: &'a R,
        capability_notice_emitted: &'a AtomicBool,
    ) -> Self {
        let verifier = Self {
            repo_root,
            capabilities,
            reporter,
            capability_notice_emitted,
        };
        verifier.emit_capability_notice();
        verifier
    }

    fn emit_capability_notice(&self) {
        if self.capabilities.descriptor_containment_degraded
            && self
                .capability_notice_emitted
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            self.reporter.report_descriptor_containment_degraded();
        }
    }

    fn opened_file_within_repo(&self, file: &File, file_abs: &Path) -> bool {
        let canonical_root = match self.repo_root.canonicalize() {
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
            let resolved = match file_abs.canonicalize() {
                Ok(path) => path,
                Err(_) => return false,
            };
            return resolved.starts_with(&canonical_root);
        }
    }

    fn hash_citation_bytes(
        &self,
        file_rel: &str,
        range: Option<(usize, usize)>,
    ) -> std::result::Result<CitationHash, UnverifiedReason> {
        if syntactically_escapes_root(file_rel) {
            return Err(UnverifiedReason::PathEscape);
        }
        let file_abs = self.repo_root.join(file_rel);
        let mut file = match open_citation_file_with_resolver(
            self.repo_root,
            Path::new(file_rel),
            self.capabilities.resolver,
        ) {
            Ok(f) => f,
            Err(error) => return Err(open_failure_reason(error, self.capabilities.resolver)),
        };

        if !self.opened_file_within_repo(&file, &file_abs) {
            return Err(UnverifiedReason::ReadError);
        }
        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(_) => return Err(UnverifiedReason::ReadError),
        };
        if !metadata.is_file() {
            return Err(UnverifiedReason::FileMissing);
        }

        let file_size = metadata.len();
        if file_size > MAX_FILE_BYTES {
            return Err(UnverifiedReason::FileTooLarge);
        }

        let (start, end) = match range {
            Some((start, end)) => {
                if end <= start {
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

    fn hash_check_at_citation(
        &self,
        file_rel: &str,
        range: Option<(usize, usize)>,
        expected: &str,
    ) -> HashCheck {
        let computed = match self.hash_citation_bytes(file_rel, range) {
            Ok(computed) => computed.sha256_hex,
            Err(reason) => return HashCheck::Failed(reason),
        };

        if computed.eq_ignore_ascii_case(strip_hash_prefix(expected)) {
            HashCheck::Match
        } else {
            HashCheck::Mismatch
        }
    }

    fn scan_file(&self, path: &Path, needle: &[u8], budget: &mut u64) -> FileScan {
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
        if !self.opened_file_within_repo(&file, path) {
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

    fn search_for_excerpt(
        &self,
        cited_rel: &str,
        excerpt: &str,
        policy: RelocationPolicy,
    ) -> ExcerptSearch {
        let needle = excerpt.as_bytes();
        let mut budget = MAX_RELOCATION_SCAN_BYTES;
        let mut found: Option<Candidate> = None;
        let mut total = 0usize;
        let mut cited_identity = None;

        if let Some(abs) = safe_join(self.repo_root, cited_rel) {
            cited_identity = FileIdentity::of(&abs).ok();
            match self.scan_file(&abs, needle, &mut budget) {
                FileScan::CapExceeded => return ExcerptSearch::CapExceeded,
                FileScan::Hits { count, first, size } if count > 0 => {
                    if count > 1 {
                        return ExcerptSearch::NonUnique(count);
                    }
                    total = count;
                    found = Some(Candidate {
                        rel_path: normalize_rel(cited_rel),
                        offset: first,
                        file_size: size,
                    });
                }
                _ => {}
            }
        }

        if policy != RelocationPolicy::FileThenRepo {
            return match found {
                Some(candidate) => ExcerptSearch::Unique(candidate),
                None => ExcerptSearch::NotFound,
            };
        }

        let canon_root = match self.repo_root.canonicalize() {
            Ok(r) => r,
            Err(_) => return ExcerptSearch::NotFound,
        };
        let excluded = excluded_names(&canon_root);
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
                if cited_identity
                    .is_some_and(|identity| FileIdentity::of(&path).ok() == Some(identity))
                {
                    continue;
                }

                match self.scan_file(&path, needle, &mut budget) {
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

    fn verify_evidence(&self, ev: &Evidence, policy: RelocationPolicy) -> VerificationOutcome {
        if ev.kind != "code" {
            return VerificationOutcome::unverified(UnverifiedReason::NotCodeKind);
        }

        let raw_path = match &ev.citation_path {
            Some(p) => p,
            None => return VerificationOutcome::unverified(UnverifiedReason::MissingCitationPath),
        };

        let (file_rel, range) = match parse_citation_path(raw_path) {
            Ok(parsed) => parsed,
            Err(_) => {
                return VerificationOutcome::unverified(UnverifiedReason::MalformedCitationPath)
            }
        };

        let decayed = match self.hash_check_at_citation(file_rel, range, &ev.citation_hash) {
            HashCheck::Match => return VerificationOutcome::verified(),
            HashCheck::Mismatch => UnverifiedReason::HashMismatch,
            HashCheck::Failed(reason) => reason,
        };

        if policy == RelocationPolicy::Never {
            return VerificationOutcome::unverified(decayed);
        }

        if matches!(
            decayed,
            UnverifiedReason::PathEscape
                | UnverifiedReason::SymlinkPathRejected
                | UnverifiedReason::FileMissing
        ) {
            return VerificationOutcome::unverified(decayed);
        }

        let excerpt = match &ev.citation_excerpt {
            Some(e) if excerpt_is_strong(e) => e.as_str(),
            _ => {
                return VerificationOutcome::unverified(
                    if decayed == UnverifiedReason::HashMismatch {
                        UnverifiedReason::ExcerptTooWeak
                    } else {
                        decayed
                    },
                )
            }
        };

        let candidate = match self.search_for_excerpt(file_rel, excerpt, policy) {
            ExcerptSearch::Unique(c) => c,
            ExcerptSearch::NotFound => {
                return VerificationOutcome::unverified(UnverifiedReason::NoCandidate)
            }
            ExcerptSearch::NonUnique(candidates) => {
                return VerificationOutcome::unverified(UnverifiedReason::NonUnique { candidates })
            }
            ExcerptSearch::CapExceeded => {
                return VerificationOutcome::unverified(UnverifiedReason::ScanCapExceeded)
            }
        };

        let new_range = range.map(|(start, end)| {
            let new_start = candidate.offset;
            (new_start, new_start + (end - start))
        });
        if new_range.is_some_and(|(_, new_end)| new_end as u64 > candidate.file_size) {
            return VerificationOutcome::unverified(UnverifiedReason::RangeOutOfBounds);
        }

        let new_path = format_citation_path(&candidate.rel_path, new_range, candidate.file_size);
        if new_path == *raw_path {
            return VerificationOutcome::unverified(decayed);
        }

        VerificationOutcome::relocated(new_path)
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
/// Every failure, including malformed stored citation data, folds into
/// [`VerificationStatus::Unverified`] with a reason.
pub fn verify_evidence(
    ev: &Evidence,
    repo_root: &Path,
    policy: RelocationPolicy,
) -> VerificationOutcome {
    let raw_path = match &ev.citation_path {
        Some(p) => p,
        None => return VerificationOutcome::unverified(UnverifiedReason::MissingCitationPath),
    };

    let (file_rel, _) = match parse_citation_path(raw_path) {
        Ok(parsed) => parsed,
        Err(_) => return VerificationOutcome::unverified(UnverifiedReason::MalformedCitationPath),
    };

    if syntactically_escapes_root(file_rel) {
        return VerificationOutcome::unverified(UnverifiedReason::PathEscape);
    }
    let file_abs = repo_root.join(file_rel);
    let file = match open_citation_file(repo_root, Path::new(file_rel)) {
        Ok(file) => file,
        Err(error) => {
            return VerificationOutcome::unverified(open_failure_reason(
                error,
                Resolver::platform_default(),
            ))
        }
    };
    if !opened_file_within_repo(&file, &file_abs, repo_root) {
        return VerificationOutcome::unverified(UnverifiedReason::ReadError);
    }
    verify_evidence_from(&file, ev, repo_root, policy)
}

/// Verify evidence using an already-open citation descriptor.
///
/// Cite callers retain this descriptor through hashing, self-check, and the
/// final pathname identity check. This provides snapshot consistency: the
/// emitted hash describes bytes actually read from one open file. The stored
/// `(citation_path, citation_hash)` pair cannot be atomic against a rename
/// after that identity check; C3 explicitly accepts that residual window.
///
/// Constructs a [`Verifier`] up front so the descriptor-containment-degraded
/// capability note (if any) is emitted once, at verifier init, regardless of
/// how this evidence row resolves — never as a side effect of the relocation
/// search reaching a particular file.
pub fn verify_evidence_from(
    file: &File,
    ev: &Evidence,
    repo_root: &Path,
    policy: RelocationPolicy,
) -> VerificationOutcome {
    let reporter = StderrCapabilityReporter;
    let verifier = Verifier::new(repo_root, &reporter);

    if ev.kind != "code" {
        return VerificationOutcome::unverified(UnverifiedReason::NotCodeKind);
    }
    let raw_path = match &ev.citation_path {
        Some(p) => p,
        None => return VerificationOutcome::unverified(UnverifiedReason::MissingCitationPath),
    };
    let (file_rel, range) = match parse_citation_path(raw_path) {
        Ok(parsed) => parsed,
        Err(_) => return VerificationOutcome::unverified(UnverifiedReason::MalformedCitationPath),
    };

    let decayed = match hash_check_from(file, range, &ev.citation_hash) {
        HashCheck::Match => return VerificationOutcome::verified(),
        HashCheck::Mismatch => UnverifiedReason::HashMismatch,
        HashCheck::Failed(reason) => reason,
    };

    if policy == RelocationPolicy::Never {
        return VerificationOutcome::unverified(decayed);
    }

    if matches!(
        decayed,
        UnverifiedReason::PathEscape
            | UnverifiedReason::SymlinkPathRejected
            | UnverifiedReason::FileMissing
    ) {
        return VerificationOutcome::unverified(decayed);
    }

    let excerpt = match &ev.citation_excerpt {
        Some(e) if excerpt_is_strong(e) => e.as_str(),
        _ => {
            return VerificationOutcome::unverified(if decayed == UnverifiedReason::HashMismatch {
                UnverifiedReason::ExcerptTooWeak
            } else {
                decayed
            })
        }
    };

    let candidate = match verifier.search_for_excerpt(file_rel, excerpt, policy) {
        ExcerptSearch::Unique(c) => c,
        ExcerptSearch::NotFound => {
            return VerificationOutcome::unverified(UnverifiedReason::NoCandidate)
        }
        ExcerptSearch::NonUnique(candidates) => {
            return VerificationOutcome::unverified(UnverifiedReason::NonUnique { candidates })
        }
        ExcerptSearch::CapExceeded => {
            return VerificationOutcome::unverified(UnverifiedReason::ScanCapExceeded)
        }
    };

    // Explicit ranges retain their original length at the match offset. A
    // whole-file citation has no length anchor and therefore relocates to the
    // candidate's bare path. In both cases a later pass must re-hash against
    // the unchanged stored hash before it can become Verified.
    let new_range = range.map(|(start, end)| {
        let new_start = candidate.offset;
        (new_start, new_start + (end - start))
    });
    if new_range.is_some_and(|(_, new_end)| new_end as u64 > candidate.file_size) {
        return VerificationOutcome::unverified(UnverifiedReason::RangeOutOfBounds);
    }

    let new_path = format_citation_path(&candidate.rel_path, new_range, candidate.file_size);
    if new_path == *raw_path {
        // The excerpt is still exactly where it was; the range's bytes changed
        // around it. That is decay, not a move.
        return VerificationOutcome::unverified(decayed);
    }

    VerificationOutcome::relocated(new_path)
}

fn opened_file_within_repo(file: &File, file_abs: &Path, repo_root: &Path) -> bool {
    let reporter = NoopCapabilityReporter;
    let already_emitted = AtomicBool::new(true);
    Verifier::with_capabilities(
        repo_root,
        VerificationCapabilities::platform_default(),
        &reporter,
        &already_emitted,
    )
    .opened_file_within_repo(file, file_abs)
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

/// Stable filesystem identity for excluding an already-scanned file from a
/// repository walk. V3's write path can reuse this instead of treating path
/// spellings as object identity.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl FileIdentity {
    /// Resolve `path` and return its `(st_dev, st_ino)` identity.
    pub(crate) fn of(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
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
///
/// Budget decision: exhaustion before any candidate and exhaustion after one
/// candidate both return [`ExcerptSearch::CapExceeded`]. The latter must not
/// degrade to `Unique`: in the TLA refinement, `candidates` is the saturating
/// `min(actual repo-wide overlapping match locations, MaxCandidates)`.
/// `CapExceeded` maps to an `Unverified` outcome, but has no single
/// `candidates` image because the unscanned bytes leave the actual repo-wide
/// count unknown (its model image is the set of such unverified states).
#[cfg(test)]
fn search_for_excerpt(
    repo_root: &Path,
    cited_rel: &str,
    excerpt: &str,
    policy: RelocationPolicy,
) -> ExcerptSearch {
    let reporter = NoopCapabilityReporter;
    let already_emitted = AtomicBool::new(true);
    Verifier::with_capabilities(
        repo_root,
        VerificationCapabilities::platform_default(),
        &reporter,
        &already_emitted,
    )
    .search_for_excerpt(cited_rel, excerpt, policy)
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

/// Read `path` and count overlapping occurrences of `needle`, charging the
/// bytes read against `budget`.
#[cfg(test)]
fn scan_file(path: &Path, repo_root: &Path, needle: &[u8], budget: &mut u64) -> FileScan {
    let reporter = NoopCapabilityReporter;
    let already_emitted = AtomicBool::new(true);
    Verifier::with_capabilities(
        repo_root,
        VerificationCapabilities::platform_default(),
        &reporter,
        &already_emitted,
    )
    .scan_file(path, needle, budget)
}

/// Count overlapping occurrences of `needle` in `hay`, returning the count
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
            i += 1;
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
    use proptest::prelude::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tempfile::NamedTempFile;

    struct RecordingCapabilityReporter {
        calls: AtomicUsize,
    }

    impl RecordingCapabilityReporter {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl CapabilityReporter for RecordingCapabilityReporter {
        fn report_descriptor_containment_degraded(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

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
    fn test_verifier_init_emits_identical_capability_output_for_existing_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("present.rs"), b"present").unwrap();

        let existing = make_evidence(
            Some("present.rs".to_string()),
            hash_bytes(b"present"),
            "code",
        );
        let missing = make_evidence(
            Some("missing.rs".to_string()),
            hash_bytes(b"missing"),
            "code",
        );

        let existing_reporter = RecordingCapabilityReporter::new();
        let existing_notice = AtomicBool::new(false);
        let existing_verifier = Verifier::with_capabilities(
            dir.path(),
            VerificationCapabilities {
                descriptor_containment_degraded: true,
                resolver: Resolver::platform_default(),
            },
            &existing_reporter,
            &existing_notice,
        );
        let _ = existing_verifier.verify_evidence(&existing, RelocationPolicy::Never);

        let missing_reporter = RecordingCapabilityReporter::new();
        let missing_notice = AtomicBool::new(false);
        let missing_verifier = Verifier::with_capabilities(
            dir.path(),
            VerificationCapabilities {
                descriptor_containment_degraded: true,
                resolver: Resolver::platform_default(),
            },
            &missing_reporter,
            &missing_notice,
        );
        let _ = missing_verifier.verify_evidence(&missing, RelocationPolicy::Never);

        assert_eq!(existing_reporter.call_count(), 1);
        assert_eq!(missing_reporter.call_count(), 1);
    }

    #[test]
    fn test_verifier_init_emits_capability_warning_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("present.rs"), b"present").unwrap();

        let reporter = RecordingCapabilityReporter::new();
        let notice = AtomicBool::new(false);
        let verifier = Verifier::with_capabilities(
            dir.path(),
            VerificationCapabilities {
                descriptor_containment_degraded: true,
                resolver: Resolver::platform_default(),
            },
            &reporter,
            &notice,
        );
        let existing = make_evidence(
            Some("present.rs".to_string()),
            hash_bytes(b"present"),
            "code",
        );
        let missing = make_evidence(
            Some("missing.rs".to_string()),
            hash_bytes(b"missing"),
            "code",
        );

        let _ = verifier.verify_evidence(&existing, RelocationPolicy::Never);
        let _ = verifier.verify_evidence(&missing, RelocationPolicy::Never);
        let second = Verifier::with_capabilities(
            dir.path(),
            VerificationCapabilities {
                descriptor_containment_degraded: true,
                resolver: Resolver::platform_default(),
            },
            &reporter,
            &notice,
        );
        let _ = second.verify_evidence(&existing, RelocationPolicy::Never);

        assert_eq!(reporter.call_count(), 1);
    }

    #[test]
    fn test_verifier_init_suppresses_capability_warning_when_not_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let reporter = RecordingCapabilityReporter::new();
        let notice = AtomicBool::new(false);

        let _ = Verifier::with_capabilities(
            dir.path(),
            VerificationCapabilities {
                descriptor_containment_degraded: false,
                resolver: Resolver::platform_default(),
            },
            &reporter,
            &notice,
        );

        assert_eq!(reporter.call_count(), 0);
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
            verify_evidence(&ev, dir, RelocationPolicy::Never).is_verified(),
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
            verify_evidence(&ev, dir, RelocationPolicy::Never).is_verified(),
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
            verify_evidence(&ev, Path::new("/tmp"), RelocationPolicy::Never).is_verified(),
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
            verify_evidence(&ev, dir, RelocationPolicy::Never).is_verified(),
            false
        );
    }

    #[test]
    fn test_parse_citation_path_valid() {
        let (path, range) = parse_citation_path("src/foo.rs:42-58").unwrap();
        assert_eq!(path, "src/foo.rs");
        assert_eq!(range, Some((42, 58)));
    }

    #[test]
    fn test_parse_citation_path_valid_nested() {
        // Path with colons in directory name (unusual but valid)
        let (path, range) = parse_citation_path("a/b/c.rs:0-100").unwrap();
        assert_eq!(path, "a/b/c.rs");
        assert_eq!(range, Some((0, 100)));
    }

    #[test]
    fn test_parse_citation_path_whole_file() {
        assert_eq!(
            parse_citation_path("src/foo.rs").unwrap(),
            ("src/foo.rs", None)
        );
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
    fn test_parse_citation_path_rejects_empty_file_and_empty_range() {
        assert!(parse_citation_path("").is_err());
        assert!(parse_citation_path(":0-4").is_err());
        assert!(parse_citation_path("src/foo.rs:4-4").is_err());
    }

    #[test]
    fn test_parse_citation_path_colon_edge_rules() {
        assert!(parse_citation_path("a:b.rs").is_err());
        assert_eq!(
            parse_citation_path("weird:name.rs:0-42").unwrap(),
            ("weird:name.rs", Some((0, 42)))
        );
    }

    proptest! {
        #[test]
        fn proptest_parse_citation_path_round_trips(
            path in "[a-zA-Z0-9_./]{1,40}",
            start in 0usize..10_000,
            width in 1usize..10_000,
        ) {
            let end = start + width;
            let ranged = format!("{path}:{start}-{end}");
            prop_assert_eq!(
                parse_citation_path(&ranged).unwrap(),
                (path.as_str(), Some((start, end)))
            );
            prop_assert_eq!(parse_citation_path(&path).unwrap(), (path.as_str(), None));
        }
    }

    #[test]
    fn test_verify_evidence_whole_file_match_mismatch_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("full.rs"), b"whole file bytes\n").unwrap();
        std::fs::write(dir.path().join("empty.rs"), b"").unwrap();

        let matching = make_evidence(
            Some("full.rs".to_string()),
            hash_bytes(b"whole file bytes\n"),
            "code",
        );
        assert!(verify_evidence(&matching, dir.path(), RelocationPolicy::Never).is_verified());

        let mismatching = make_evidence(
            Some("full.rs".to_string()),
            hash_bytes(b"different"),
            "code",
        );
        assert_eq!(
            verify_evidence(&mismatching, dir.path(), RelocationPolicy::Never).reason,
            Some(UnverifiedReason::HashMismatch)
        );

        let empty = make_evidence(Some("empty.rs".to_string()), hash_bytes(b""), "code");
        assert!(verify_evidence(&empty, dir.path(), RelocationPolicy::Never).is_verified());
    }

    #[test]
    fn test_verify_evidence_whole_file_directory_is_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let ev = make_evidence(Some("src".to_string()), hash_bytes(b""), "code");
        assert_eq!(
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never).reason,
            Some(UnverifiedReason::FileMissing)
        );
    }

    #[test]
    fn test_verify_evidence_escape_reason_does_not_disclose_existence() {
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(parent.path().join("outside-existing-file"), b"secret").unwrap();

        for path in ["../outside-existing-file", "../outside-nonexistent-file"] {
            let ev = make_evidence(Some(path.to_string()), hash_bytes(b"secret"), "code");
            assert_eq!(
                verify_evidence(&ev, &repo, RelocationPolicy::Never).reason,
                Some(UnverifiedReason::PathEscape),
                "{path}"
            );
        }
    }

    #[test]
    fn test_verify_evidence_rejects_absolute_path_as_escape() {
        let repo = tempfile::tempdir().unwrap();
        let ev = make_evidence(
            Some(
                repo.path()
                    .join("missing.rs")
                    .to_string_lossy()
                    .into_owned(),
            ),
            hash_bytes(b""),
            "code",
        );
        assert_eq!(
            verify_evidence(&ev, repo.path(), RelocationPolicy::Never).reason,
            Some(UnverifiedReason::PathEscape)
        );
    }

    #[test]
    fn test_verify_evidence_normal_missing_file_is_file_missing() {
        let repo = tempfile::tempdir().unwrap();
        let ev = make_evidence(Some("src/missing.rs".to_string()), hash_bytes(b""), "code");
        assert_eq!(
            verify_evidence(&ev, repo.path(), RelocationPolicy::Never).reason,
            Some(UnverifiedReason::FileMissing)
        );
    }

    #[test]
    fn test_verify_evidence_malformed_path_folds_to_reason() {
        let ev = make_evidence(
            Some("src/foo.rs:not-a-range".to_string()),
            hash_bytes(b""),
            "code",
        );
        let outcome = verify_evidence(&ev, Path::new("/tmp"), RelocationPolicy::Never);
        assert_eq!(
            outcome.reason,
            Some(UnverifiedReason::MalformedCitationPath)
        );
        assert_eq!(outcome.reason.unwrap().as_str(), "malformed_citation");
    }

    #[test]
    fn test_verify_evidence_relocates_whole_file_to_bare_path() {
        let dir = tempfile::tempdir().unwrap();
        let excerpt = concat!(
            "fn uniquely_relocated_whole_file() {\n",
            "    let enough_bytes = \"make this excerpt strong and unique in the repository\";\n",
            "    println!(\"{enough_bytes}\");\n",
            "}\n"
        );
        std::fs::write(dir.path().join("old.rs"), b"changed bytes\n").unwrap();
        std::fs::write(dir.path().join("new.rs"), excerpt).unwrap();
        let mut ev = make_evidence(
            Some("old.rs".to_string()),
            hash_bytes(excerpt.as_bytes()),
            "code",
        );
        ev.citation_excerpt = Some(excerpt.to_string());

        let outcome = verify_evidence(&ev, dir.path(), RelocationPolicy::FileThenRepo);
        assert_eq!(outcome.status, VerificationStatus::Relocated);
        assert_eq!(outcome.relocated_to.as_deref(), Some("new.rs"));
    }

    #[test]
    fn test_repo_search_rejects_match_in_cited_and_other_file_as_non_unique() {
        let dir = tempfile::tempdir().unwrap();
        let excerpt = concat!(
            "fn duplicated_relocation_candidate() {\n",
            "    let marker = \"this excerpt is deliberately strong and duplicated\";\n",
            "}\n"
        );
        std::fs::write(
            dir.path().join("cited.rs"),
            format!("changed prefix\n{excerpt}"),
        )
        .unwrap();
        std::fs::write(dir.path().join("other.rs"), excerpt).unwrap();
        let mut evidence = make_evidence(
            Some(format!("cited.rs:0-{}", excerpt.len())),
            hash_bytes(excerpt.as_bytes()),
            "code",
        );
        evidence.citation_excerpt = Some(excerpt.to_string());

        let outcome = verify_evidence(&evidence, dir.path(), RelocationPolicy::FileThenRepo);
        assert_eq!(outcome.status, VerificationStatus::Unverified);
        assert_eq!(
            outcome.reason,
            Some(UnverifiedReason::NonUnique { candidates: 2 })
        );
        assert_eq!(outcome.relocated_to, None);
    }

    #[test]
    fn test_repo_search_cap_after_one_candidate_is_cap_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let excerpt = concat!(
            "fn candidate_before_budget_exhaustion() {\n",
            "    let marker = \"the first candidate must never become false unique\";\n",
            "}\n"
        );
        std::fs::write(dir.path().join("a-cited.rs"), excerpt).unwrap();
        let oversized = File::create(dir.path().join("z-oversized.rs")).unwrap();
        oversized.set_len(MAX_RELOCATION_SCAN_BYTES).unwrap();

        assert!(matches!(
            search_for_excerpt(
                dir.path(),
                "a-cited.rs",
                excerpt,
                RelocationPolicy::FileThenRepo,
            ),
            ExcerptSearch::CapExceeded
        ));
    }

    proptest! {
        #[test]
        fn proptest_repo_search_is_unique_iff_one_file_identity_has_a_match(
            copies in 0usize..5,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let excerpt = concat!(
                "fn property_relocation_candidate() {\n",
                "    let marker = \"generated trees count each matching file identity once\";\n",
                "}\n"
            );
            let cited_rel = if copies == 0 {
                std::fs::write(dir.path().join("cited.rs"), b"changed\n").unwrap();
                "cited.rs"
            } else {
                for i in 0..copies {
                    std::fs::write(dir.path().join(format!("copy-{i}.rs")), excerpt).unwrap();
                }
                std::fs::hard_link(
                    dir.path().join("copy-0.rs"),
                    dir.path().join("cited-link.rs"),
                ).unwrap();
                "cited-link.rs"
            };

            let result = search_for_excerpt(
                dir.path(), cited_rel, excerpt, RelocationPolicy::FileThenRepo,
            );
            prop_assert_eq!(matches!(&result, ExcerptSearch::Unique(_)), copies == 1);
            if copies > 1 {
                prop_assert!(matches!(&result, ExcerptSearch::NonUnique(_)));
            }
        }
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
            verify_evidence(&ev, dir, RelocationPolicy::Never).is_verified(),
            true
        );
    }

    #[test]
    fn test_hash_check_at_citation_matches_expected_ranges() {
        let mut tmp = NamedTempFile::new().unwrap();
        let content = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ--tail";
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
                hash_check_at_citation(dir, &file_name, Some((start, end)), &expected_hash),
                HashCheck::Match
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_hash_check_rejects_symlink_to_outside_without_reading_target() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let secret = b"outside bytes must never be hashed";
        let outside = parent.path().join("outside.txt");
        std::fs::write(&outside, secret).unwrap();
        symlink(&outside, repo.join("citation.txt")).unwrap();

        assert!(matches!(
            hash_check_at_citation(&repo, "citation.txt", None, &hash_bytes(secret)),
            HashCheck::Failed(UnverifiedReason::SymlinkPathRejected)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symlink_citations_are_rejected_by_openat2_and_fallback_resolvers() {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir().unwrap();
        let repo = sandbox.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("inside.txt"), b"inside").unwrap();
        std::fs::write(sandbox.path().join("outside.txt"), b"outside").unwrap();

        for (link, target) in [
            ("inside-link.txt", repo.join("inside.txt")),
            ("outside-link.txt", sandbox.path().join("outside.txt")),
        ] {
            symlink(target, repo.join(link)).unwrap();
            for resolver in [Resolver::Openat2, Resolver::Fallback] {
                let reporter = NoopCapabilityReporter;
                let emitted = AtomicBool::new(true);
                let verifier = Verifier::with_capabilities(
                    &repo,
                    VerificationCapabilities {
                        descriptor_containment_degraded: false,
                        resolver,
                    },
                    &reporter,
                    &emitted,
                );
                let evidence = make_evidence(
                    Some(link.to_string()),
                    hash_bytes(if link.starts_with("inside") {
                        b"inside"
                    } else {
                        b"outside"
                    }),
                    "code",
                );
                let outcome = verifier.verify_evidence(&evidence, RelocationPolicy::FileThenRepo);
                assert_eq!(
                    outcome.reason,
                    Some(UnverifiedReason::SymlinkPathRejected),
                    "resolver={resolver:?}, link={link}"
                );
                assert_eq!(outcome.relocated_to, None);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_descriptor_walk_fallback_rejects_mid_path_symlink() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("citation.txt"), b"secret").unwrap();
        symlink(&outside, repo.join("linked-dir")).unwrap();

        assert!(open_citation_file_fallback(&repo, Path::new("linked-dir/citation.txt")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_descriptor_walk_fallback_reads_normal_file() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/citation.txt"), b"inside").unwrap();

        let mut file =
            open_citation_file_fallback(repo.path(), Path::new("src/citation.txt")).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"inside");
    }

    #[test]
    fn test_hash_check_at_citation_rejects_empty_explicit_range() {
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
            hash_check_at_citation(dir, &file_name, Some((0, 0)), &expected_hash),
            HashCheck::Failed(UnverifiedReason::RangeOutOfBounds)
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
            hash_check_at_citation(dir, &file_name, Some((start, end)), &expected_hash),
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
            hash_check_at_citation(dir, &file_name, Some((0, content.len())), &expected_hash),
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
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never).is_verified(),
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
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never).is_verified(),
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
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never).is_verified(),
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
        assert_eq!(result.unwrap(), file_path);
    }

    #[cfg(unix)]
    #[test]
    fn safe_join_rejects_symlink_components_and_parent_components() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("plain/nested")).unwrap();
        symlink(dir.path().join("plain"), dir.path().join("sym")).unwrap();

        assert_eq!(
            safe_join(dir.path(), "plain/nested"),
            Some(dir.path().join("plain/nested"))
        );
        assert!(safe_join(dir.path(), "sym/nested").is_none());
        assert!(safe_join(dir.path(), "plain/../plain").is_none());
    }

    #[cfg(unix)]
    proptest! {
        #[test]
        fn prop_safe_join_accepts_exactly_existing_plain_relative_paths(
            components in prop::collection::vec(
                prop_oneof![Just("a"), Just("b"), Just(".."), Just("sym")],
                0..8,
            )
        ) {
            use std::os::unix::fs::symlink;

            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let mut prefix = dir.path().to_path_buf();
            for component in &components {
                match *component {
                    "a" | "b" => {
                        prefix.push(*component);
                        std::fs::create_dir_all(&prefix).unwrap();
                    }
                    "sym" => {
                        let link = prefix.join("sym");
                        if !link.exists() {
                            symlink(outside.path(), link).unwrap();
                        }
                        break;
                    }
                    ".." => break,
                    _ => unreachable!(),
                }
            }
            let rel = components.join("/");
            let expected = !components.iter().any(|component| *component == ".." || *component == "sym")
                && dir.path().join(&rel).exists();
            prop_assert_eq!(safe_join(dir.path(), &rel).is_some(), expected);
        }
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
            verify_evidence(&ev, Path::new("/tmp"), RelocationPolicy::Never).is_verified(),
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
            verify_evidence(&ev, dir, RelocationPolicy::Never).is_verified(),
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
            verify_evidence(&ev, dir, RelocationPolicy::Never).is_verified(),
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

        let outcome = verify_evidence(&ev, &repo, RelocationPolicy::FileOnly);
        assert_eq!(outcome.status, VerificationStatus::Unverified);
        // Non-disclosure rule (bd-eho3): reporting SymlinkPathRejected here
        // does not create an existence oracle for the sibling target. The
        // fact being disclosed is that `linked.txt`, a component that lives
        // inside the repo, is itself a symlink -- metadata read from within
        // the repository, not from wherever the link resolves to. Whether
        // the target exists, and whether it is inside or outside the repo,
        // never affects this reason, so no side channel about the outside
        // world is opened. Contrast with PathEscape/FileMissing folding,
        // which exists precisely to avoid disclosing outside-repo state.
        assert_eq!(outcome.reason, Some(UnverifiedReason::SymlinkPathRejected));
        assert_eq!(outcome.relocated_to, None);
        assert!(!outcome.is_verified(), "sibling content must never verify");
    }

    #[cfg(unix)]
    #[test]
    fn relocation_scan_skips_symlinked_candidates_and_never_auto_heals_them() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let excerpt = concat!(
            "a strong relocation excerpt must be long enough to search\n",
            "and must contain enough lines to satisfy the verifier\n"
        );
        std::fs::write(dir.path().join("cited.rs"), b"changed bytes").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target.rs"), excerpt).unwrap();
        symlink(
            outside.path().join("target.rs"),
            dir.path().join("candidate.rs"),
        )
        .unwrap();

        let mut evidence = make_evidence(
            Some("cited.rs".to_string()),
            hash_bytes(b"original bytes"),
            "code",
        );
        evidence.citation_excerpt = Some(excerpt.to_string());
        let skipped = verify_evidence(&evidence, dir.path(), RelocationPolicy::FileThenRepo);
        assert_eq!(skipped.status, VerificationStatus::Unverified);
        assert_eq!(skipped.reason, Some(UnverifiedReason::NoCandidate));
        assert_eq!(
            skipped.relocated_to, None,
            "no relocation means no heal plan"
        );
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
    fn test_count_occurrences_counts_overlapping_periodic_multiline_excerpt() {
        let period = b"abcdefghijklmnopqrstuvwxyzABCDE\n";
        let needle = [period.as_slice(), period.as_slice()].concat();
        let hay = [period.as_slice(), period.as_slice(), period.as_slice()].concat();
        assert!(needle.len() >= MIN_EXCERPT_BYTES);
        assert_eq!(count_occurrences(&hay, &needle), (2, Some(0)));
    }

    #[test]
    fn test_count_occurrences_reports_first() {
        assert_eq!(count_occurrences(b"aaaa", b"aa"), (3, Some(0)));
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
            verify_evidence(&ev, dir.path(), RelocationPolicy::Never).is_verified(),
            false,
            "files larger than MAX_FILE_BYTES must return Ok(false)"
        );
    }
}
