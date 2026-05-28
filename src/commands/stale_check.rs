//! `stale-check` subcommand
//!
//! Both the CLI subcommand and the MCP `kb_stale_check` handler call into the
//! shared [`run_stale_check`] helper.  The CLI renders [`StaleCheckReport`] to
//! stdout; the MCP handler serialises it to JSON.  No SQL or git subprocess
//! invocation should be duplicated between the two call sites.

use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::{params, Connection};
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
#[derive(Debug, Default)]
pub struct StaleCheckReport {
    pub stale: Vec<StaleEntry>,
    pub review: Vec<ReviewEntry>,
    pub unreachable: Vec<UnreachableEntry>,
    /// Number of input files checked (echoed back in the MCP response).
    pub checked: usize,
}

/// Check if KB entries for given files (or commits) are stale.
///
/// Output line prefixes:
///   STALE   — entry's file has changed since its recorded version_ref
///   REVIEW  — entry was recorded at one of the supplied commit SHAs
///   UNKNOWN — entry's recorded version_ref is unreachable from current HEAD
///             (deleted branch, garbage-collected commit, orphan-branch KB)
///
/// Two lookup modes (can be combined):
///   1. File-based: pass file paths; finds entries whose KB path overlaps
///      and checks whether the file changed since the entry was recorded.
///      Unreachable refs are surfaced as UNKNOWN, not silently skipped.
///   2. Commit-based: pass `--commits` or `--blame` to find entries
///      recorded *at* specific commit SHAs.  With `--blame`, the SHA set
///      is "commits that touched the file" (`git log --pretty=%H -- file`),
///      not the file's full blame line history.
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
        if self.files.is_empty() && self.commits.is_empty() && !self.blame {
            anyhow::bail!("provide at least one file path or --commits");
        }

        let paths = config::Paths::discover()?;
        let conn = db::open_db(&paths.db)?;
        let repo_root = config::git_repo_root();

        let report = run_stale_check(
            &conn,
            &self.files,
            &self.commits,
            self.blame,
            repo_root.as_deref(),
        )?;

        render_cli(&report);
        Ok(())
    }
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
pub fn run_stale_check(
    conn: &Connection,
    files: &[String],
    explicit_commits: &[String],
    blame: bool,
    repo_root: Option<&Path>,
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
            let count = *count_cache.entry(key).or_insert_with(|| {
                commits_since(&version_ref, &stored_path, repo_root)
            });
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
            seen_ids.insert(id.clone());
            report.review.push(ReviewEntry {
                id,
                path: stored_path,
                summary,
                version_ref,
            });
        }
    }

    Ok(report)
}

/// Render a [`StaleCheckReport`] to stdout in the CLI's line-oriented format.
fn render_cli(report: &StaleCheckReport) {
    let mut found_any = false;
    for e in &report.stale {
        let plural = if e.commits_behind == 1 { "" } else { "s" };
        println!(
            "STALE  [{}] {}  id={}  recorded-at={}  ({} commit{} ago)",
            e.path, e.summary, e.id, e.version_ref, e.commits_behind, plural
        );
        found_any = true;
    }
    for e in &report.review {
        println!(
            "REVIEW [{}] {}  id={}  recorded-at={}  (matches blame/commit)",
            e.path, e.summary, e.id, e.version_ref
        );
        found_any = true;
    }
    for e in &report.unreachable {
        println!(
            "UNKNOWN [{}] {}  id={}  recorded-at={}  (version_ref unreachable from HEAD)",
            e.path, e.summary, e.id, e.version_ref
        );
        found_any = true;
    }
    if !found_any {
        println!("ok");
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
pub fn normalize_path(file: &str, repo_root: Option<&Path>) -> String {
    if let Some(root) = repo_root {
        let p = Path::new(file);
        if p.is_absolute() {
            return p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned();
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
    cmd.args(["rev-list", "--count", &format!("{version_ref}..HEAD"), "--", path]);
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
            "a1b2c3d4e5f6789012345678901234567890abcd\n",        // ok
            "TOOSHORT\n",                                         // wrong length
            "a1b2c3d4e5f6789012345678901234567890abcd abc def\n", // trailing junk
            "ZZb2c3d4e5f6789012345678901234567890abcd\n",        // non-hex
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
            Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir).output().unwrap().stdout,
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
        )
        .unwrap();

        assert_eq!(report.stale.len(), 0, "must not be marked stale");
        assert_eq!(report.review.len(), 0);
        assert_eq!(report.unreachable.len(), 1, "must land in unreachable");
        assert_eq!(report.unreachable[0].id, "e1");
        assert_eq!(report.unreachable[0].version_ref, ghost);
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
            Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir).output().unwrap().stdout,
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
}
