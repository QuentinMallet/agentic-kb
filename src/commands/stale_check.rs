//! `stale-check` subcommand
//!
//! Both the CLI subcommand and the MCP `kb_stale_check` handler call into the
//! shared [`run_stale_check`] helper.  The CLI renders [`StaleCheckReport`] to
//! stdout; the MCP handler serialises it to JSON.  No SQL or git subprocess
//! invocation should be duplicated between the two call sites.

#![allow(deprecated)] // db::open_db (ADR-1) — remaining call sites migrate in C2/L1b, L2, L3, L1c
use crate::components::cursor;
use crate::components::db;
use crate::components::verification::{verify_evidence, RelocationPolicy};
use crate::config;
use crate::models::VerificationStatus;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;

/// A KB entry whose file has changed since the entry's `version_ref`.
#[derive(Debug, Clone)]
pub struct StaleEntry {
    pub id: String,
    pub path: String,
    pub summary: String,
    pub version_ref: String,
    pub commits_behind: usize,
}

/// A KB entry whose `version_ref` matches a commit we are surfacing for review
/// (either an explicit `--commits` value or one discovered via `--blame`).
#[derive(Debug, Clone)]
pub struct ReviewEntry {
    pub id: String,
    pub path: String,
    pub summary: String,
    pub version_ref: String,
}

/// A KB entry whose `version_ref` cannot be resolved from the current HEAD
/// (deleted branch, garbage-collected commit, KB on an orphan branch).
///
/// Distinguished from "not stale" so callers can flag it for manual review
/// instead of silently treating it as current.  Currently always empty; T5
/// (br-yyb.6) will populate it.
#[derive(Debug, Clone)]
pub struct UnreachableEntry {
    pub id: String,
    pub path: String,
    pub summary: String,
    pub version_ref: String,
}

/// Bucketed result of a stale-check run.  CLI renders to STALE/REVIEW/UNKNOWN
/// lines; MCP renders to JSON arrays.
///
/// **Bucket precedence (each entry appears in at most one bucket).** The
/// orchestrator processes Pass 1 (file-based) then Pass 2 (commit-based),
/// and tracks already-bucketed ids in a `seen_ids` set.  First write wins,
/// so the precedence is:
///
///   1. `unreachable` — Pass 1 routes an entry here whenever its
///      `version_ref` cannot be resolved from HEAD; Pass 2 does the same
///      for explicit/blame SHAs that no longer exist locally.
///   2. `stale` — Pass 1 promotes here only for refs that ARE reachable
///      and have `commits_behind > 0` for the stored path.
///   3. `review` — Pass 2 lands the remainder (reachable refs that match
///      the explicit/blame commit set and have not already been bucketed
///      by Pass 1).
///
/// As a consequence: an entry that both has a changed file AND matches an
/// explicit/blame SHA will only appear in `stale` (Pass 1 fires first); an
/// entry with an unreachable ref will only appear in `unreachable`,
/// regardless of which pass surfaced it.
#[derive(Debug, Default)]
pub struct StaleCheckReport {
    pub stale: Vec<StaleEntry>,
    pub review: Vec<ReviewEntry>,
    pub unreachable: Vec<UnreachableEntry>,
    /// Citation-level relocation findings (V1).  Empty unless the caller asked
    /// for a relocation policy other than [`RelocationPolicy::Never`].  Rendered
    /// on its own line prefixes, disjoint from the `^STALE` contract that the
    /// machines_conf hooks grep (plan N5).
    pub relocation: Vec<RelocationEntry>,
    /// Number of input files checked (echoed back in the MCP response).
    pub checked: usize,
}

/// One evidence row whose citation no longer hashes at its recorded path.
///
/// `Relocated` means the excerpt was found at exactly one new location;
/// `Unverified` means it was not, and `reason` says why.  `Verified` rows are
/// not reported — there is nothing to act on.
#[derive(Debug, Clone)]
pub struct RelocationEntry {
    pub entry_id: String,
    pub evidence_id: String,
    pub status: VerificationStatus,
    /// The `path:start-end` citation as currently recorded.
    pub old_path: String,
    /// Proposed citation, `Some` exactly when `status == Relocated`.
    pub new_path: Option<String>,
    /// Machine-readable reason, `Some` exactly when `status == Unverified`.
    pub reason: Option<&'static str>,
    /// Whether a `citation_healed` event was written for this row.  Always
    /// false unless `relocation_autoheal = true` in `kb.toml`.
    pub healed: bool,
}

/// Check if KB entries for given files (or commits) are stale.
///
/// Output line prefixes:
///   STALE   — entry's file has changed since its recorded version_ref
///   REVIEW  — entry was recorded at one of the supplied commit SHAs
///   UNKNOWN — entry's recorded version_ref is unreachable from current HEAD
///             (deleted branch, garbage-collected commit, orphan-branch KB)
///   RELOCATED / UNVERIFIED — citation-level relocation findings when
///             `--relocate` is not `never`
///
/// Two lookup modes (can be combined):
///   1. File-based: pass file paths; finds entries whose KB path overlaps
///      and checks whether the file changed since the entry was recorded.
///      Unreachable refs are surfaced as UNKNOWN, not silently skipped.
///   2. Commit-based: pass `--commits` or `--blame` to find entries
///      recorded *at* specific commit SHAs.  With `--blame`, the SHA set
///      is "commits that touched the file" (`git log --pretty=%H -- file`),
///      not the file's full blame line history.
///
/// Relocation is report-only by default. A successful relocation writes a
/// `citation_healed` event only when `relocation_autoheal = true` in
/// `kb.toml`.
#[derive(Command, Debug, Parser)]
pub struct StaleCheck {
    /// File paths to check for stale KB entries (by path match + git log)
    #[arg(required = false)]
    pub files: Vec<String>,

    /// Also check entries recorded at these exact commit SHAs (comma-separated)
    #[arg(long = "commits", value_delimiter = ',')]
    pub commits: Vec<String>,

    /// Discover commit SHAs from the input files' commit history
    /// (`git log --pretty=%H -- file`) and surface KB entries recorded at
    /// those commits for review
    #[arg(long, default_value_t = false)]
    pub blame: bool,

    /// How hard to look for citations that moved: `never` (default),
    /// `file` (search only the cited file), `file-then-repo` (fall back to a
    /// repo walk). When enabled, emits `RELOCATED` and `UNVERIFIED` lines in
    /// addition to entry-level `STALE` / `REVIEW` / `UNKNOWN`. Auto-heal
    /// remains gated by `relocation_autoheal = true` in `kb.toml`.
    #[arg(long = "relocate", value_enum, default_value_t = RelocateArg::Never)]
    pub relocate: RelocateArg,
}

/// CLI spelling of [`RelocationPolicy`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum RelocateArg {
    Never,
    File,
    FileThenRepo,
}

impl From<RelocateArg> for RelocationPolicy {
    fn from(a: RelocateArg) -> Self {
        match a {
            RelocateArg::Never => RelocationPolicy::Never,
            RelocateArg::File => RelocationPolicy::FileOnly,
            RelocateArg::FileThenRepo => RelocationPolicy::FileThenRepo,
        }
    }
}

impl Runnable for StaleCheck {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl StaleCheck {
    /// Execute the stale-check command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let report = self.execute_with(&paths)?;
        render_cli(&report);
        Ok(())
    }

    /// Execute against explicit paths (exposed for testing).
    ///
    /// `repo_root` resolves from `paths.root` -- the same layout-aware root
    /// `add`/`cite` hash evidence against and the MCP port passes -- rather
    /// than a CWD-based `config::git_repo_root()` git subprocess. Those can
    /// silently disagree: a process cwd inside a managed `.state` git
    /// worktree has its own git toplevel, so `git_repo_root()` would resolve
    /// to the worktree while `paths.root` (and the port) resolve to the
    /// outer repo -- meaning `--relocate` would rewrite evidence hashes
    /// against the wrong repository.
    pub fn execute_with(&self, paths: &config::Paths) -> anyhow::Result<StaleCheckReport> {
        if self.files.is_empty() && self.commits.is_empty() && !self.blame {
            anyhow::bail!("provide at least one file path or --commits");
        }

        let conn = db::open_db(&paths.db)?;
        let repo_root = Some(paths.root.clone());
        let policy: RelocationPolicy = self.relocate.into();

        let mut report = run_stale_check(
            &conn,
            &self.files,
            &self.commits,
            self.blame,
            repo_root.as_deref(),
            policy,
        )?;

        // P4: relocation computes and reports by default.  Rewriting a citation
        // is a separate, off-by-default decision, and even then it writes the
        // path only — never the stored hash.
        let cfg = config::KbConfig::from_paths(paths);
        if cfg.relocation_autoheal && policy != RelocationPolicy::Never {
            heal_relocations(paths, &mut report)?;
        }

        Ok(report)
    }
}

/// Write one `citation_healed` event per relocated row and apply it.
///
/// The event log is the durable substrate (P2/F1): the DB update alone would
/// not survive `kb rebuild`.  Runs under the same flock as every other writer.
/// Opens its own mutating connection rather than reusing the caller's read
/// connection: a mutation must be performed on a handle obtained with the lock
/// in hand (ADR-1, principle 2).
fn heal_relocations(paths: &config::Paths, report: &mut StaleCheckReport) -> anyhow::Result<()> {
    use crate::commands::add::acquire_lock;
    use crate::components::embedder::NoopEmbedder;
    use crate::components::events;

    let version_ref = config::git_head_sha();
    let lock = acquire_lock(&paths.lock)?;
    let conn = &db::open_rw(paths, &lock)?;

    for r in report.relocation.iter_mut() {
        let new_path = match &r.new_path {
            Some(p) if r.status == VerificationStatus::Relocated => p.clone(),
            _ => continue,
        };
        // The stored hash is read back and echoed into the event unchanged;
        // it is audit data, never an input to the UPDATE.
        let Some(citation_hash): Option<String> = conn
            .query_row(
                "SELECT citation_hash FROM evidence WHERE id=?1 AND entry_id=?2",
                params![r.evidence_id, r.entry_id],
                |row| row.get(0),
            )
            .optional()?
        else {
            continue;
        };
        let event = events::citation_healed_event(
            &r.entry_id,
            &r.evidence_id,
            &r.old_path,
            &new_path,
            &citation_hash,
            version_ref.as_deref(),
        );
        // Writer 5 of 10.
        cursor::append_and_apply(&lock, conn, paths, &NoopEmbedder, &[event])?;
        r.healed = true;
    }
    Ok(())
}

/// Shared orchestration for the CLI subcommand and the MCP `kb_stale_check`
/// handler.  Performs two passes:
///
///   1. File-based path matching → [`StaleCheckReport::stale`]:  for each
///      input file, find KB entries whose `path` overlaps, then count commits
///      touching that path since the entry's recorded `version_ref`.  Entries
///      with one or more such commits are flagged stale.
///
///   2. Commit-based lookup → [`StaleCheckReport::review`]:  collect explicit
///      `commits` plus (if `blame`) commit SHAs from
///      [`commits_that_touched_file`] over each input file, then find KB
///      entries recorded at those SHAs.
///
/// The `unreachable` bucket is reserved for the T5 work (br-yyb.6); it is
/// always empty in the current implementation but the field is materialised
/// here so callers can already wire it through their renderers.
///
/// T3 (br-yyb.4): per-file path matching is split into a fast-path exact
/// match (uses `idx_entries_path`) plus a substring fallback (full scan).
/// Both prepared statements are hoisted out of the file loop so SQLite
/// statement compilation runs once per call, not once per input file.
///
/// The third pass is citation relocation (V1).  It is read-only: it computes
/// statuses and proposed paths, and never writes.  `relocation` is a required
/// argument with no default — passing [`RelocationPolicy::Never`] (the only
/// value that touches no file content) has to be a choice, not an omission.
pub fn run_stale_check(
    conn: &Connection,
    files: &[String],
    explicit_commits: &[String],
    blame: bool,
    repo_root: Option<&Path>,
    relocation: RelocationPolicy,
) -> anyhow::Result<StaleCheckReport> {
    use std::collections::HashMap;

    let mut report = StaleCheckReport {
        checked: files.len(),
        ..Default::default()
    };
    let mut seen_ids: HashSet<String> = HashSet::new();
    // T4 (br-yyb.5): memoise `commits_since` results by (version_ref, path).
    // Many KB entries record the same `version_ref` (e.g. when a coding session
    // recorded a batch of entries at one HEAD), and they often share a stored
    // path.  Without memoisation the call fans out one git subprocess per
    // matched row.  With memoisation it collapses to one subprocess per
    // distinct (version_ref, path) tuple — observed reduction on normatix:
    // ~60 entries → 4 distinct tuples → 4 subprocesses.
    let mut count_cache: HashMap<(String, String), Option<usize>> = HashMap::new();
    // T5 (br-yyb.6): memoise `ref_exists` results so unreachable refs are
    // probed once per distinct version_ref across all matched rows.
    let mut ref_cache: HashMap<String, bool> = HashMap::new();

    // -- Pass 1: file-based path matching --
    //
    // Two prepared statements, hoisted outside the file loop:
    //  * `fast_path` — `path = ?1` — uses idx_entries_path (O(log N) lookup).
    //  * `substring_fallback` — leading-wildcard LIKE — still scans, but only
    //    runs when the exact-match fast-path returns nothing.  This preserves
    //    the original "input path contains entry path as a suffix" semantics
    //    without paying for a full scan when the exact match already
    //    answered the query.
    let mut fast_path = conn.prepare(
        "SELECT id, summary, version_ref, path FROM entries
         WHERE path = ?1
           AND version_ref IS NOT NULL AND is_stale = 0",
    )?;
    let mut substring_fallback = conn.prepare(
        "SELECT id, summary, version_ref, path FROM entries
         WHERE (path LIKE '%' || ?1 || '%' ESCAPE '\\' OR ?2 LIKE '%' || path)
           AND path != ?2
           AND version_ref IS NOT NULL AND is_stale = 0",
    )?;

    for file in files {
        let rel_path = normalize_path(file, repo_root);
        let like_safe = rel_path.replace('%', "\\%").replace('_', "\\_");

        // Collect rows from both queries: fast-path first (index hit), then
        // fallback for substring/suffix matches the exact-match cannot reach.
        let mut rows: Vec<(String, String, String, String)> = fast_path
            .query_map(params![rel_path], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3).unwrap_or_else(|_| rel_path.clone()),
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        rows.extend(
            substring_fallback
                .query_map(params![like_safe, rel_path], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3).unwrap_or_else(|_| rel_path.clone()),
                    ))
                })?
                .filter_map(|r| r.ok()),
        );

        for (id, summary, version_ref, stored_path) in rows {
            if seen_ids.contains(&id) {
                continue;
            }
            // T5 (br-yyb.6): probe ref existence first.  If the recorded
            // `version_ref` is unreachable (deleted branch, GC'd commit,
            // orphan-branch KB), surface in the `unreachable` bucket instead
            // of letting `commits_since` return `None` (indistinguishable
            // from "no commits since" at the call site).
            let reachable = *ref_cache
                .entry(version_ref.clone())
                .or_insert_with(|| ref_exists(&version_ref, repo_root));
            if !reachable {
                seen_ids.insert(id.clone());
                report.unreachable.push(UnreachableEntry {
                    id,
                    path: stored_path,
                    summary,
                    version_ref,
                });
                continue;
            }
            let key = (version_ref.clone(), stored_path.clone());
            let count = *count_cache
                .entry(key)
                .or_insert_with(|| commits_since(&version_ref, &stored_path, repo_root));
            if let Some(count) = count {
                if count > 0 {
                    seen_ids.insert(id.clone());
                    report.stale.push(StaleEntry {
                        id,
                        path: stored_path,
                        summary,
                        version_ref,
                        commits_behind: count,
                    });
                }
            }
        }
    }

    // -- Pass 2: commit-based lookup --
    let mut all_commits: HashSet<String> = explicit_commits.iter().cloned().collect();
    if blame {
        for file in files {
            all_commits.extend(commits_that_touched_file(file, repo_root));
        }
    }

    if !all_commits.is_empty() {
        let commits: Vec<String> = all_commits.into_iter().collect();
        let placeholders = (1..=commits.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, summary, version_ref, path FROM entries
             WHERE version_ref IN ({placeholders}) AND is_stale = 0"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map(rusqlite::params_from_iter(commits.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        for (id, summary, version_ref, stored_path) in rows {
            if seen_ids.contains(&id) {
                continue;
            }
            // I1 (post-impl review): Pass 2 also routes unreachable refs to
            // the `unreachable` bucket.  An explicit `--commits` SHA or a
            // blame-discovered SHA may point at a commit that no longer
            // exists in the local repo (typo, remote-only branch, GC).
            // Matching entries should be flagged unreachable, not surfaced
            // as if their context were intact.
            let reachable = *ref_cache
                .entry(version_ref.clone())
                .or_insert_with(|| ref_exists(&version_ref, repo_root));
            seen_ids.insert(id.clone());
            if reachable {
                report.review.push(ReviewEntry {
                    id,
                    path: stored_path,
                    summary,
                    version_ref,
                });
            } else {
                report.unreachable.push(UnreachableEntry {
                    id,
                    path: stored_path,
                    summary,
                    version_ref,
                });
            }
        }
    }

    // -- Pass 3: citation relocation --
    if relocation != RelocationPolicy::Never {
        report.relocation = relocation_pass(conn, &report, repo_root, relocation)?;
    }

    Ok(report)
}

/// Compute relocation status for every evidence row of every entry already
/// surfaced by passes 1 and 2.
///
/// Scoping to the surfaced entries is what keeps this bounded: a stale-check
/// over three edited files verifies the citations of the handful of entries
/// that mention them, not the whole KB.
fn relocation_pass(
    conn: &Connection,
    report: &StaleCheckReport,
    repo_root: Option<&Path>,
    policy: RelocationPolicy,
) -> anyhow::Result<Vec<RelocationEntry>> {
    let root = match repo_root {
        Some(r) => r,
        // Without a repo root there is nothing to hash against, and guessing
        // one would search an arbitrary directory.
        None => return Ok(vec![]),
    };

    let mut entry_ids: Vec<String> = Vec::new();
    entry_ids.extend(report.stale.iter().map(|e| e.id.clone()));
    entry_ids.extend(report.review.iter().map(|e| e.id.clone()));
    entry_ids.extend(report.unreachable.iter().map(|e| e.id.clone()));
    if entry_ids.is_empty() {
        return Ok(vec![]);
    }

    let evidence_map = db::fetch_evidence_for_entries(conn, &entry_ids)?;
    let mut out = Vec::new();
    for entry_id in &entry_ids {
        let rows = match evidence_map.get(entry_id) {
            Some(r) => r,
            None => continue,
        };
        for ev in rows {
            let outcome = verify_evidence(ev, root, policy);
            if outcome.status == VerificationStatus::Verified {
                continue;
            }
            out.push(RelocationEntry {
                entry_id: entry_id.clone(),
                evidence_id: ev.id.clone(),
                status: outcome.status,
                old_path: ev.citation_path.clone().unwrap_or_default(),
                new_path: outcome.relocated_to,
                reason: outcome.reason.as_ref().map(|r| r.as_str()),
                healed: false,
            });
        }
    }
    Ok(out)
}

/// Build the CLI's line-oriented rendering of a [`StaleCheckReport`].
///
/// Separated from printing so the line contract — which the machines_conf edit
/// hook greps (`^STALE`) — is assertable in a test.
fn render_lines(report: &StaleCheckReport) -> Vec<String> {
    let mut lines = Vec::new();
    for e in &report.stale {
        let plural = if e.commits_behind == 1 { "" } else { "s" };
        lines.push(format!(
            "STALE  [{}] {}  id={}  recorded-at={}  ({} commit{} ago)",
            e.path, e.summary, e.id, e.version_ref, e.commits_behind, plural
        ));
    }
    for e in &report.review {
        lines.push(format!(
            "REVIEW [{}] {}  id={}  recorded-at={}  (matches blame/commit)",
            e.path, e.summary, e.id, e.version_ref
        ));
    }
    for e in &report.unreachable {
        lines.push(format!(
            "UNKNOWN [{}] {}  id={}  recorded-at={}  (version_ref unreachable from HEAD)",
            e.path, e.summary, e.id, e.version_ref
        ));
    }
    // V1 relocation lines.  Distinct prefixes, deliberately NOT `STALE`: the
    // machines_conf edit hook greps `^STALE` and must not start seeing
    // citation-level findings as entry-level staleness (plan N5).
    for r in &report.relocation {
        match r.status {
            VerificationStatus::Relocated => lines.push(format!(
                "RELOCATED [{}] -> [{}]  id={}  ev={}  ({})",
                r.old_path,
                r.new_path.as_deref().unwrap_or("?"),
                r.entry_id,
                r.evidence_id,
                if r.healed { "healed" } else { "report-only" }
            )),
            VerificationStatus::Unverified => lines.push(format!(
                "UNVERIFIED [{}]  id={}  ev={}  reason={}",
                r.old_path,
                r.entry_id,
                r.evidence_id,
                r.reason.unwrap_or("unknown")
            )),
            // Verified rows are filtered out before they reach the report.
            VerificationStatus::Verified => {}
        }
    }
    if lines.is_empty() {
        lines.push("ok".to_string());
    }
    lines
}

/// Render a [`StaleCheckReport`] to stdout in the CLI's line-oriented format.
fn render_cli(report: &StaleCheckReport) {
    for line in render_lines(report) {
        println!("{line}");
    }
}

/// Returns the set of commit SHAs that touched `file` in its full history.
///
/// Replaces the previous `extract_blame_shas` (T2, br-yyb.3): porcelain blame
/// emitted SHAs from every line of history including unrelated commits that
/// happened to author a line via merge resolution, inflating the input to the
/// downstream `version_ref IN (...)` query for large files.  `git log
/// --pretty=%H -- <file>` returns exactly the commits that modified the file,
/// matching the docstring of the `blame=true` mode ("surface KB entries that
/// were current when the changed code was written").
///
/// Output is one 40-hex-char SHA per line; we collect into a `HashSet` to dedupe
/// merge SHAs that touched the file via multiple paths.
pub fn commits_that_touched_file(file: &str, repo_root: Option<&Path>) -> HashSet<String> {
    let mut cmd = std::process::Command::new("git");
    if let Some(root) = repo_root {
        cmd.arg("-C").arg(root);
    }
    cmd.args(["log", "--pretty=%H", "--", file]);
    match cmd.output() {
        Ok(out) if out.status.success() => parse_log_hashes(&out.stdout),
        _ => HashSet::new(),
    }
}

/// Parse 40-hex SHAs from `git log --pretty=%H` stdout.
///
/// Each output line is exactly one SHA (no headers, no content).  Defensive
/// byte-level filtering rejects anything that is not a clean 40-hex-char line,
/// so an unexpected git version or locale cannot crash the helper.
pub fn parse_log_hashes(stdout: &[u8]) -> HashSet<String> {
    stdout
        .split(|&b| b == b'\n')
        .filter(|line| line.len() == 40 && line.iter().all(u8::is_ascii_hexdigit))
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

/// Relativize an absolute path against the repo root (no-op for relative paths).
///
/// **Note on encoding.** Input is already `&str`, so the relative-path
/// no-op case is fully lossless.  In the absolute-path branch the path is
/// strip-prefixed and then converted back via `to_string_lossy`, which
/// replaces invalid UTF-8 byte sequences with `U+FFFD`.  This is
/// intentional and safe for stale-check: KB entry paths are stored as
/// UTF-8, so a path containing non-UTF-8 bytes can never match an
/// existing entry regardless of how it is normalised — the lossy
/// substitution only changes the query string for paths that would have
/// returned zero rows anyway.
pub fn normalize_path(file: &str, repo_root: Option<&Path>) -> String {
    if let Some(root) = repo_root {
        let p = Path::new(file);
        if p.is_absolute() {
            return p
                .strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned();
        }
    }
    file.to_string()
}

/// Returns the number of commits touching `path` since `version_ref`, or
/// `None` if git failed or the range is empty.
///
/// T4 (br-yyb.5): uses `git rev-list --count VERSION..HEAD -- path` which
/// returns a single integer line — no commit object formatting, no message
/// materialisation — instead of `git log --oneline` (the prior implementation
/// formatted every commit just to count newlines).
///
/// Callers that have many entries sharing the same `(version_ref, path)`
/// tuple should dedupe before invoking this; see [`run_stale_check`].
pub fn commits_since(version_ref: &str, path: &str, repo_root: Option<&Path>) -> Option<usize> {
    let mut cmd = std::process::Command::new("git");
    if let Some(root) = repo_root {
        cmd.arg("-C").arg(root);
    }
    cmd.args([
        "rev-list",
        "--count",
        &format!("{version_ref}..HEAD"),
        "--",
        path,
    ]);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let count_str = std::str::from_utf8(&out.stdout).ok()?.trim();
    count_str.parse::<usize>().ok()
}

/// Returns whether `version_ref` exists in the local repository.
///
/// T5 (br-yyb.6): used by [`run_stale_check`] to distinguish "no commits
/// since" (ref reachable, file unchanged) from "ref unreachable" (recorded
/// SHA gone — deleted branch, orphan-branch KB, garbage-collected commit).
/// The latter is surfaced via [`StaleCheckReport::unreachable`] instead of
/// being silently treated as not-stale.
///
/// Implementation: `git cat-file -e <ref>` — cheap probe, no object payload
/// emitted, single fork+exec per distinct `version_ref` when memoised by
/// the caller.
pub fn ref_exists(version_ref: &str, repo_root: Option<&Path>) -> bool {
    let mut cmd = std::process::Command::new("git");
    if let Some(root) = repo_root {
        cmd.arg("-C").arg(root);
    }
    cmd.args(["cat-file", "-e", version_ref]);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// br-<stale-check-repo-root>: `execute_with` must relocate against
    /// `paths.root`, not a CWD-based `config::git_repo_root()` git
    /// subprocess -- the two can silently disagree (e.g. a process cwd
    /// inside a managed `.state` git worktree has its own git toplevel).
    ///
    /// Construction: `old.rs` has changed bytes (hash mismatch against the
    /// stored citation), but `new.rs` under the same root holds the exact
    /// recorded excerpt, verbatim and unique enough that it cannot exist
    /// anywhere under the test runner's own cwd. If relocation resolves
    /// against `paths.root`, `FileThenRepo` finds `new.rs` and reports
    /// `Relocated`. If it fell back to (or ignored) some other root, the
    /// excerpt would not be found and the row would stay `Unverified`.
    /// The commit SHA is fictitious and unresolvable, which only routes the
    /// entry to the `unreachable` bucket -- `relocation_pass` still checks
    /// evidence for every bucket, including `unreachable`.
    ///
    /// The sqlite db itself lives in a separate directory from `paths.root`
    /// (a real repo's db normally nests under root/.state/, but nothing here
    /// exercises that nesting): `FileThenRepo` walks the whole of
    /// `paths.root`, and the recorded excerpt is also stored verbatim as the
    /// `citation_excerpt` column value, so a db file living inside the
    /// scanned tree would itself be a second (non-unique) match.
    #[test]
    fn execute_with_relocates_against_paths_root_not_git_repo_root() {
        use sha2::{Digest, Sha256};

        let content_tmp = TempDir::new().unwrap();
        let state_tmp = TempDir::new().unwrap();
        let root = content_tmp.path();
        let state_dir = state_tmp.path();
        let paths = config::Paths {
            root: root.to_path_buf(),
            lock: state_dir.join(".lock"),
            events: state_dir.join("agent-kb-events.jsonl"),
            db: state_dir.join("agent-kb.db"),
            fastembed_cache: state_dir.join("fastembed-cache"),
            compact_state: state_dir.join("compact-state.json"),
            query_hits: state_dir.join("query-hits.db"),
        };
        db::open_or_init(&paths).unwrap();
        let conn = db::open_db(&paths.db).unwrap();

        let excerpt = concat!(
            "fn uniquely_relocated_for_stale_check_repo_root_regression_test() {\n",
            "    let enough_bytes = \"make this excerpt strong and unique in the repository\";\n",
            "    println!(\"{enough_bytes}\");\n",
            "}\n"
        );
        std::fs::write(root.join("old.rs"), b"changed bytes\n").unwrap();
        std::fs::write(root.join("new.rs"), excerpt).unwrap();
        let hash = format!("sha256:{:x}", Sha256::digest(excerpt.as_bytes()));

        conn.execute(
            "INSERT INTO entries (id, path, summary, content, tags, version_ref, is_stale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                "e1",
                "old.rs",
                "test entry",
                "body",
                "[]",
                "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence(
                id, entry_id, kind, citation_path, citation_hash, citation_excerpt, recorded_at
             ) VALUES (?1, ?2, 'code', ?3, ?4, ?5, '2024-01-01T00:00:00Z')",
            params!["ev1", "e1", "old.rs", &hash, excerpt],
        )
        .unwrap();
        drop(conn);

        let cmd = StaleCheck {
            files: vec![],
            commits: vec!["deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string()],
            blame: false,
            relocate: RelocateArg::FileThenRepo,
        };

        let report = cmd.execute_with(&paths).unwrap();

        assert_eq!(
            report.unreachable.len(),
            1,
            "the fictitious commit SHA must route e1 to unreachable, not vanish"
        );
        assert_eq!(
            report.relocation.len(),
            1,
            "relocation_pass must still check unreachable-bucket evidence"
        );
        assert_eq!(report.relocation[0].status, VerificationStatus::Relocated);
        assert_eq!(
            report.relocation[0].new_path.as_deref(),
            Some("new.rs"),
            "relocation must find new.rs under paths.root"
        );
    }

    #[test]
    fn parse_log_hashes_collects_unique_shas() {
        let blob = concat!(
            "a1b2c3d4e5f6789012345678901234567890abcd\n",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
            "a1b2c3d4e5f6789012345678901234567890abcd\n", // dupe
        )
        .as_bytes();
        let shas = parse_log_hashes(blob);
        assert_eq!(shas.len(), 2);
        assert!(shas.contains("a1b2c3d4e5f6789012345678901234567890abcd"));
        assert!(shas.contains("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"));
    }

    #[test]
    fn parse_log_hashes_rejects_malformed_lines() {
        let blob = concat!(
            "a1b2c3d4e5f6789012345678901234567890abcd\n", // ok
            "TOOSHORT\n",                                 // wrong length
            "a1b2c3d4e5f6789012345678901234567890abcd abc def\n", // trailing junk
            "ZZb2c3d4e5f6789012345678901234567890abcd\n", // non-hex
        )
        .as_bytes();
        let shas = parse_log_hashes(blob);
        assert_eq!(shas.len(), 1);
        assert!(shas.contains("a1b2c3d4e5f6789012345678901234567890abcd"));
    }

    #[test]
    fn parse_log_hashes_handles_empty_input() {
        assert!(parse_log_hashes(b"").is_empty());
    }

    /// Run a git command in a tempdir, panicking on failure.  Used by the
    /// fixture-based integration tests below.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git available");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Regression test for T2 (br-yyb.3): `blame=true` semantics.
    ///
    /// Build a tempdir repo with file history vs unrelated history; assert
    /// that `commits_that_touched_file` returns exactly the commits that
    /// touched the file, *not* every commit in the repository.
    #[test]
    fn commits_that_touched_file_only_returns_commits_touching_that_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);

        // 3 commits touching foo.rs
        for i in 1..=3 {
            std::fs::write(dir.join("foo.rs"), format!("foo v{i}\n")).unwrap();
            git(dir, &["add", "foo.rs"]);
            git(dir, &["commit", "-q", "-m", &format!("foo {i}")]);
        }
        // 2 unrelated commits touching bar.rs only
        for i in 1..=2 {
            std::fs::write(dir.join("bar.rs"), format!("bar v{i}\n")).unwrap();
            git(dir, &["add", "bar.rs"]);
            git(dir, &["commit", "-q", "-m", &format!("bar {i}")]);
        }

        let foo_shas = commits_that_touched_file("foo.rs", Some(dir));
        assert_eq!(
            foo_shas.len(),
            3,
            "exactly the 3 foo.rs commits should appear, not the 5 total"
        );

        let bar_shas = commits_that_touched_file("bar.rs", Some(dir));
        assert_eq!(bar_shas.len(), 2);

        // foo and bar SHAs are disjoint sets
        assert!(foo_shas.is_disjoint(&bar_shas));
    }

    /// Regression test for T3 (br-yyb.4): the fast-path query must use the
    /// `idx_entries_path` index instead of a full table scan.
    #[test]
    fn fast_path_query_uses_idx_entries_path() {
        let conn = db::open_db_memory().expect("memory db");
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT id, summary, version_ref, path FROM entries
                 WHERE path = ?1
                   AND version_ref IS NOT NULL AND is_stale = 0",
                params!["x"],
                |r| r.get::<_, String>(3),
            )
            .expect("explain query plan");
        assert!(
            plan.contains("idx_entries_path"),
            "expected idx_entries_path in query plan, got: {plan}"
        );
    }

    /// Regression test for T4 (br-yyb.5): `commits_since` returns an integer
    /// from `git rev-list --count`, not a hand-counted log line set.
    #[test]
    fn commits_since_counts_commits_between_ref_and_head() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);

        // c1: baseline
        std::fs::write(dir.join("foo.rs"), "v1\n").unwrap();
        git(dir, &["add", "foo.rs"]);
        git(dir, &["commit", "-q", "-m", "c1"]);
        let c1 = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        // Two more commits touching foo.rs
        for tag in ["c2", "c3"] {
            std::fs::write(dir.join("foo.rs"), format!("v{tag}\n")).unwrap();
            git(dir, &["add", "foo.rs"]);
            git(dir, &["commit", "-q", "-m", tag]);
        }
        // One commit touching only bar.rs (must NOT be counted for foo.rs)
        std::fs::write(dir.join("bar.rs"), "x\n").unwrap();
        git(dir, &["add", "bar.rs"]);
        git(dir, &["commit", "-q", "-m", "bar"]);

        assert_eq!(commits_since(&c1, "foo.rs", Some(dir)), Some(2));
        assert_eq!(commits_since(&c1, "bar.rs", Some(dir)), Some(1));
    }

    #[test]
    fn commits_since_returns_none_for_unknown_ref() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("x"), "x").unwrap();
        git(dir, &["add", "x"]);
        git(dir, &["commit", "-q", "-m", "seed"]);

        // SHA that does not exist in this repo → git rev-list exits non-zero
        // → caller distinguishes via Option<usize>::None.
        let ghost = "0000000000000000000000000000000000000000";
        assert_eq!(commits_since(ghost, "x", Some(dir)), None);
    }

    /// Regression test for T5 (br-yyb.6): an entry whose `version_ref`
    /// cannot be resolved in the local repo is surfaced in the `unreachable`
    /// bucket rather than silently treated as not-stale.
    #[test]
    fn run_stale_check_routes_unreachable_version_ref_to_unreachable_bucket() {
        use crate::components::db::open_db_memory;
        use rusqlite::params;

        // Fixture repo: one commit, one tracked file.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("seed.rs"), "x").unwrap();
        git(dir, &["add", "seed.rs"]);
        git(dir, &["commit", "-q", "-m", "seed"]);

        // KB: one entry whose version_ref is a SHA that does not exist in
        // the fixture repo.  The path matches `seed.rs` exactly so the
        // fast-path query returns the row, then ref_exists denies it.
        let conn = open_db_memory().unwrap();
        let ghost = "0000000000000000000000000000000000000000";
        conn.execute(
            "INSERT INTO entries (id, path, summary, content, tags, version_ref, is_stale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params!["e1", "seed.rs", "test entry", "body", "[]", ghost],
        )
        .unwrap();

        let report = run_stale_check(
            &conn,
            &["seed.rs".to_string()],
            &[],
            false,
            Some(dir),
            RelocationPolicy::Never,
        )
        .unwrap();

        assert_eq!(report.stale.len(), 0, "must not be marked stale");
        assert_eq!(report.review.len(), 0);
        assert_eq!(report.unreachable.len(), 1, "must land in unreachable");
        assert_eq!(report.unreachable[0].id, "e1");
        assert_eq!(report.unreachable[0].version_ref, ghost);
    }

    /// I1 (post-impl review): Pass-2 commit-based lookup also routes
    /// unreachable refs to the `unreachable` bucket.  Without this, an
    /// explicit `--commits` SHA that no longer exists locally would
    /// silently surface the matched entry as `review`, hiding the fact
    /// that the referenced commit is gone.
    #[test]
    fn run_stale_check_routes_unreachable_explicit_commit_to_unreachable_bucket() {
        use crate::components::db::open_db_memory;
        use rusqlite::params;

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("x"), "x").unwrap();
        git(dir, &["add", "x"]);
        git(dir, &["commit", "-q", "-m", "seed"]);

        let conn = open_db_memory().unwrap();
        let ghost = "0000000000000000000000000000000000000000";
        conn.execute(
            "INSERT INTO entries (id, path, summary, content, tags, version_ref, is_stale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params!["e1", "x", "test entry", "body", "[]", ghost],
        )
        .unwrap();

        let report = run_stale_check(
            &conn,
            &[],                  // no files — Pass 2 only
            &[ghost.to_string()], // explicit unreachable commit
            false,
            Some(dir),
            RelocationPolicy::Never,
        )
        .unwrap();

        assert_eq!(report.review.len(), 0, "must not be marked review");
        assert_eq!(report.stale.len(), 0);
        assert_eq!(report.unreachable.len(), 1, "must land in unreachable");
        assert_eq!(report.unreachable[0].id, "e1");
    }

    /// Reachable refs do not get routed to `unreachable`.
    #[test]
    fn run_stale_check_does_not_flag_reachable_ref_as_unreachable() {
        use crate::components::db::open_db_memory;
        use rusqlite::params;

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("seed.rs"), "x").unwrap();
        git(dir, &["add", "seed.rs"]);
        git(dir, &["commit", "-q", "-m", "seed"]);
        let head = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let conn = open_db_memory().unwrap();
        conn.execute(
            "INSERT INTO entries (id, path, summary, content, tags, version_ref, is_stale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params!["e1", "seed.rs", "test entry", "body", "[]", &head],
        )
        .unwrap();

        let report = run_stale_check(
            &conn,
            &["seed.rs".to_string()],
            &[],
            false,
            Some(dir),
            RelocationPolicy::Never,
        )
        .unwrap();

        // Ref is at HEAD with no diverging commits — not stale, not unreachable.
        assert_eq!(report.unreachable.len(), 0);
        assert_eq!(report.stale.len(), 0);
    }

    #[test]
    fn commits_that_touched_file_handles_missing_file_gracefully() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("seed"), "x").unwrap();
        git(dir, &["add", "seed"]);
        git(dir, &["commit", "-q", "-m", "seed"]);

        // git log -- nonexistent returns empty stdout with success exit
        let shas = commits_that_touched_file("does_not_exist.rs", Some(dir));
        assert!(shas.is_empty());
    }

    #[cfg(test)]
    mod proptest_stale_check {
        use super::*;
        use proptest::prelude::*;
        use std::collections::HashSet;

        /// Invariant: STALE, REVIEW, UNREACHABLE buckets are pairwise disjoint.
        /// All entry IDs appear in exactly zero or one bucket.
        proptest! {
            #[test]
            fn prop_stale_check_buckets_are_disjoint(
                num_files in 0usize..=3,
            ) {
                use crate::components::db::open_db_memory;
                use rusqlite::params;

                let conn = open_db_memory().expect("memory db");

                // Insert a small set of entries with various paths and refs.
                let entry_ids = vec!["e1", "e2", "e3"];
                let paths = vec!["a.rs", "b.rs", "c.rs"];
                let shas = vec![
                    "a1b2c3d4e5f6789012345678901234567890abcd",
                    "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                    "0000000000000000000000000000000000000000",
                ];

                for (i, entry_id) in entry_ids.iter().enumerate() {
                    let path = paths[i % paths.len()];
                    let version_ref = shas[i % shas.len()];
                    conn.execute(
                        "INSERT INTO entries (id, path, summary, content, tags, version_ref, is_stale)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                        params![entry_id, path, "test", "body", "[]", version_ref],
                    )
                    .expect("insert entry");
                }

                // Run stale_check with a subset of files
                let files_to_check: Vec<String> = (0..num_files)
                    .map(|i| paths[i % paths.len()].to_string())
                    .collect();

                let report = run_stale_check(
                    &conn,
                    &files_to_check,
                    &[],
                    false,
                    None,
                    RelocationPolicy::Never,
                )
                .expect("run_stale_check");

                // Collect all IDs from each bucket
                let stale_ids: HashSet<String> =
                    report.stale.iter().map(|e| e.id.clone()).collect();
                let review_ids: HashSet<String> =
                    report.review.iter().map(|e| e.id.clone()).collect();
                let unreachable_ids: HashSet<String> =
                    report.unreachable.iter().map(|e| e.id.clone()).collect();

                // Invariant: buckets are pairwise disjoint
                prop_assert!(
                    stale_ids.is_disjoint(&review_ids),
                    "stale and review must be disjoint"
                );
                prop_assert!(
                    stale_ids.is_disjoint(&unreachable_ids),
                    "stale and unreachable must be disjoint"
                );
                prop_assert!(
                    review_ids.is_disjoint(&unreachable_ids),
                    "review and unreachable must be disjoint"
                );

                // Invariant: no entry appears twice (union has same cardinality as sum)
                let total = stale_ids.len() + review_ids.len() + unreachable_ids.len();
                let union_size = stale_ids
                    .union(&review_ids)
                    .cloned()
                    .collect::<HashSet<_>>()
                    .union(&unreachable_ids)
                    .count();
                prop_assert_eq!(
                    total, union_size,
                    "entry appears in multiple buckets"
                );
            }
        }
    }

    // -- V1 relocation surface --

    fn relocation_entry(status: VerificationStatus) -> RelocationEntry {
        RelocationEntry {
            entry_id: "entry-1".to_string(),
            evidence_id: "ev-1".to_string(),
            status,
            old_path: "src/old.rs:0-66".to_string(),
            new_path: Some("src/new.rs:11-77".to_string()),
            reason: Some("non_unique"),
            healed: false,
        }
    }

    /// The `^STALE` grep contract the machines_conf edit hook depends on (plan
    /// N5) must not start matching citation-level relocation findings.
    #[test]
    fn relocation_lines_never_match_the_stale_contract() {
        let report = StaleCheckReport {
            stale: vec![StaleEntry {
                id: "e1".to_string(),
                path: "src/a.rs".to_string(),
                summary: "sum".to_string(),
                version_ref: "abc".to_string(),
                commits_behind: 2,
            }],
            relocation: vec![
                relocation_entry(VerificationStatus::Relocated),
                relocation_entry(VerificationStatus::Unverified),
            ],
            ..Default::default()
        };

        let lines = render_lines(&report);
        let stale_lines: Vec<&String> = lines.iter().filter(|l| l.starts_with("STALE")).collect();
        assert_eq!(
            stale_lines.len(),
            1,
            "exactly the one real stale entry may match ^STALE: {lines:?}"
        );
        assert!(lines
            .iter()
            .any(|l| l.starts_with("RELOCATED [src/old.rs:0-66] -> [src/new.rs:11-77]")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("UNVERIFIED") && l.contains("reason=non_unique")));
    }

    /// A report-only relocation says so; the heal marker is not cosmetic.
    #[test]
    fn relocation_line_distinguishes_healed_from_report_only() {
        let mut e = relocation_entry(VerificationStatus::Relocated);
        let report_only = render_lines(&StaleCheckReport {
            relocation: vec![e.clone()],
            ..Default::default()
        });
        assert!(report_only[0].ends_with("(report-only)"), "{report_only:?}");

        e.healed = true;
        let healed = render_lines(&StaleCheckReport {
            relocation: vec![e],
            ..Default::default()
        });
        assert!(healed[0].ends_with("(healed)"), "{healed:?}");
    }

    #[test]
    fn heal_relocations_skips_missing_evidence_rows_and_keeps_renderable_report() {
        use crate::components::events;
        use rusqlite::params;

        let dir = TempDir::new().unwrap();
        // On disk, not in memory: heal_relocations opens its own mutating
        // connection under the write lock, so the seed rows must live in the
        // repository's real database rather than a private handle.
        let (paths, conn) = db::test_db(dir.path());

        // Seeded through the applied-cursor writer, so the log exists and
        // matches: a populated database with no log at all is refused by the
        // write guard, which is the state C1/T4 exists to stop.
        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries", "id": "e1",
            "path": "seed.rs", "summary": "test entry", "content": "body",
            "tags": [], "version_ref": "deadbeef", "kind": "belief",
            "ts": "2024-01-01T00:00:00Z",
        });
        let live = crate::models::Evidence {
            id: "ev-live".to_string(),
            entry_id: "e1".to_string(),
            kind: "code".to_string(),
            citation_path: Some("seed.rs:0-10".to_string()),
            citation_sha: None,
            citation_hash: "sha256:live".to_string(),
            citation_excerpt: Some("strong excerpt".to_string()),
            derived_from: None,
            recorded_at: Some("2024-01-01T00:00:00Z".to_string()),
        };
        {
            let lock = crate::commands::add::acquire_lock(&paths.lock).unwrap();
            crate::components::cursor::append_and_apply(
                &lock,
                &conn,
                &paths,
                &crate::components::embedder::NoopEmbedder,
                &[upsert, events::evidence_add_event("e1", &live, None)],
            )
            .unwrap();
        }

        let mut report = StaleCheckReport {
            relocation: vec![
                RelocationEntry {
                    entry_id: "e1".to_string(),
                    evidence_id: "ev-missing".to_string(),
                    status: VerificationStatus::Relocated,
                    old_path: "seed.rs:10-20".to_string(),
                    new_path: Some("seed.rs:30-40".to_string()),
                    reason: None,
                    healed: false,
                },
                RelocationEntry {
                    entry_id: "e1".to_string(),
                    evidence_id: "ev-live".to_string(),
                    status: VerificationStatus::Relocated,
                    old_path: "seed.rs:0-10".to_string(),
                    new_path: Some("seed.rs:20-30".to_string()),
                    reason: None,
                    healed: false,
                },
            ],
            ..Default::default()
        };

        heal_relocations(&paths, &mut report).unwrap();

        assert!(!report.relocation[0].healed, "missing rows are skipped");
        assert!(report.relocation[1].healed, "remaining rows still heal");

        let lines = render_lines(&report);
        assert!(lines
            .iter()
            .any(|l| l.contains("seed.rs:10-20") && l.ends_with("(report-only)")));
        assert!(lines
            .iter()
            .any(|l| l.contains("seed.rs:0-10") && l.ends_with("(healed)")));

        let healed_events = events::read_events(&paths.events)
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e["action"] == "citation_healed")
            .count();
        assert_eq!(healed_events, 1, "only the surviving heal is appended");

        let healed_path: String = conn
            .query_row(
                "SELECT citation_path FROM evidence WHERE id=?1 AND entry_id=?2",
                params!["ev-live", "e1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(healed_path, "seed.rs:20-30");
    }

    #[test]
    fn run_stale_check_reports_malformed_citation_as_unverified() {
        use crate::components::db::open_db_memory;
        use rusqlite::params;

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("seed.rs"), "fn seed() {}\n").unwrap();
        git(dir, &["add", "seed.rs"]);
        git(dir, &["commit", "-q", "-m", "seed"]);
        let head = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(dir.join("seed.rs"), "fn seed() {}\nfn changed() {}\n").unwrap();
        git(dir, &["add", "seed.rs"]);
        git(dir, &["commit", "-q", "-m", "change seed"]);

        let conn = open_db_memory().unwrap();
        conn.execute(
            "INSERT INTO entries (id, path, summary, content, tags, version_ref, is_stale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params!["e1", "seed.rs", "test entry", "body", "[]", &head],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO evidence(
                id, entry_id, kind, citation_path, citation_hash, citation_excerpt, recorded_at
             ) VALUES (?1, ?2, 'code', ?3, ?4, ?5, '2024-01-01T00:00:00Z')",
            params![
                "ev-malformed",
                "e1",
                "seed.rs:not-a-range",
                "sha256:deadbeef",
                Some("fn seed() {}")
            ],
        )
        .unwrap();

        let report = run_stale_check(
            &conn,
            &["seed.rs".to_string()],
            &[],
            false,
            Some(dir),
            RelocationPolicy::FileOnly,
        )
        .unwrap();

        assert_eq!(
            report.stale.len(),
            1,
            "fixture must surface entry in pass 1"
        );
        assert_eq!(report.relocation.len(), 1, "malformed row must not vanish");
        let row = &report.relocation[0];
        assert_eq!(row.entry_id, "e1");
        assert_eq!(row.evidence_id, "ev-malformed");
        assert_eq!(row.status, VerificationStatus::Unverified);
        assert_eq!(row.reason, Some("malformed_citation"));

        let lines = render_lines(&report);
        assert!(lines
            .iter()
            .any(|l| l.starts_with("UNVERIFIED [seed.rs:not-a-range]")
                && l.contains("reason=malformed_citation")));
    }

    /// `--relocate never` (the default) runs no relocation pass at all.
    #[test]
    fn never_policy_leaves_the_relocation_bucket_empty() {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q"]);
        let conn = db::open_db_memory().unwrap();
        let report = run_stale_check(
            &conn,
            &["seed.rs".to_string()],
            &[],
            false,
            Some(dir.path()),
            RelocationPolicy::Never,
        )
        .unwrap();
        assert!(report.relocation.is_empty());
    }

    /// The CLI enum maps onto the engine's policy without a default.
    #[test]
    fn relocate_arg_maps_onto_policy() {
        assert_eq!(
            RelocationPolicy::from(RelocateArg::Never),
            RelocationPolicy::Never
        );
        assert_eq!(
            RelocationPolicy::from(RelocateArg::File),
            RelocationPolicy::FileOnly
        );
        assert_eq!(
            RelocationPolicy::from(RelocateArg::FileThenRepo),
            RelocationPolicy::FileThenRepo
        );
    }
}

#[cfg(test)]
mod heal_writer_tests {
    //! C1/T4: `heal_relocations` is a production applied-cursor writer. It
    //! needs a git repository and a relocated citation to reach, which is why
    //! it is exercised here rather than from `tests/applied_cursor.rs`.

    use super::*;
    use crate::commands::add::acquire_lock;
    use crate::components::embedder::NoopEmbedder;
    use crate::components::{cursor, events};
    use crate::config::Paths;
    use crate::models::{Evidence, VerificationStatus};

    #[test]
    fn test_heal_relocations_leaves_the_cursor_caught_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);

        // Seed an entry plus one evidence row, through the applied-cursor
        // writer so the repository starts converged.
        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries", "id": "entry-1",
            "path": "src/lib.rs", "summary": "entry", "content": "content",
            "tags": [], "kind": "observation", "evidence_status": "present",
            "ts": "2026-09-05T00:00:00Z",
        });
        let evidence = Evidence {
            id: "ev-1".to_string(),
            entry_id: "entry-1".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/old.rs".to_string()),
            citation_sha: None,
            citation_hash: "deadbeef".to_string(),
            citation_excerpt: None,
            derived_from: None,
            recorded_at: Some("2026-09-05T00:00:00Z".to_string()),
        };
        let evidence_event = events::evidence_add_event("entry-1", &evidence, None);
        {
            let lock = acquire_lock(&paths.lock).unwrap();
            let conn = db::open_rw(&paths, &lock).unwrap();
            cursor::append_and_apply(
                &lock,
                &conn,
                &paths,
                &NoopEmbedder,
                &[upsert, evidence_event],
            )
            .unwrap();
        }

        let mut report = StaleCheckReport {
            relocation: vec![RelocationEntry {
                entry_id: "entry-1".to_string(),
                evidence_id: "ev-1".to_string(),
                status: VerificationStatus::Relocated,
                old_path: "src/old.rs".to_string(),
                new_path: Some("src/new.rs".to_string()),
                reason: None,
                healed: false,
            }],
            ..Default::default()
        };
        heal_relocations(&paths, &mut report).unwrap();
        assert!(report.relocation[0].healed, "the row must be healed");

        let conn = db::open_ro(&paths.db).unwrap();
        assert_eq!(
            cursor::inspect(&conn, &paths),
            cursor::Decision::NoOp,
            "heal_relocations left the applied cursor behind the log"
        );
        let path: String = conn
            .query_row(
                "SELECT citation_path FROM evidence WHERE id='ev-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path, "src/new.rs");
    }
}
