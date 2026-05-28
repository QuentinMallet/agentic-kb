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

/// Check if kb entries for given files (or commits) are stale.
///
/// Two lookup modes:
///   1. File-based (existing): pass file paths; finds entries whose KB path
///      overlaps and checks whether the file changed since the entry was recorded.
///   2. Commit-based (new): pass `--commits` or `--blame` to find entries
///      recorded *at* specific commit SHAs — useful at the end of an epic to
///      surface KB entries that were current when the changed code was written.
#[derive(Command, Debug, Parser)]
pub struct StaleCheck {
    /// File paths to check for stale KB entries (by path match + git log)
    #[arg(required = false)]
    pub files: Vec<String>,

    /// Also check entries recorded at these exact commit SHAs (comma-separated)
    #[arg(long = "commits", value_delimiter = ',')]
    pub commits: Vec<String>,

    /// Run git blame on files to discover relevant commit SHAs, then surface
    /// KB entries recorded at those commits for review
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
    let mut report = StaleCheckReport {
        checked: files.len(),
        ..Default::default()
    };
    let mut seen_ids: HashSet<String> = HashSet::new();

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
            if let Some(count) = commits_since(&version_ref, &stored_path, repo_root) {
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

/// Returns the number of commits touching `path` since `version_ref`, or None
/// if git is unavailable / the range is invalid.
pub fn commits_since(version_ref: &str, path: &str, repo_root: Option<&Path>) -> Option<usize> {
    let mut cmd = std::process::Command::new("git");
    if let Some(root) = repo_root {
        cmd.arg("-C").arg(root);
    }
    cmd.args(["log", "--oneline", &format!("{version_ref}..HEAD"), "--", path]);
    let out = cmd.output().ok()?;
    if out.status.success() && !out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        Some(std::str::from_utf8(&out.stdout).unwrap_or("").lines().count())
    } else {
        None
    }
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
