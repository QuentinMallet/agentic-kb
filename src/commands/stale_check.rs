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
///      `commits` plus (if `blame`) commit SHAs discovered from
///      `git blame --porcelain` over each input file, then find KB entries
///      recorded at those SHAs.
///
/// The `unreachable` bucket is reserved for the T5 work (br-yyb.6); it is
/// always empty in the current implementation but the field is materialised
/// here so callers can already wire it through their renderers.
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
    for file in files {
        let rel_path = normalize_path(file, repo_root);
        let like_safe = rel_path.replace('%', "\\%").replace('_', "\\_");
        let mut stmt = conn.prepare(
            "SELECT id, summary, version_ref, path FROM entries
             WHERE (path = ?1 OR path LIKE '%' || ?2 || '%' ESCAPE '\\' OR ?1 LIKE '%' || path)
               AND version_ref IS NOT NULL AND is_stale = 0",
        )?;
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map(params![rel_path, like_safe], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3).unwrap_or_else(|_| rel_path.clone()),
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

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
            all_commits.extend(extract_blame_shas(file, repo_root));
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

/// Extract unique commit SHAs referenced in `git blame --porcelain` output for a file.
///
/// In porcelain format each blame hunk opens with a 40-hex-char SHA followed by
/// a space and line numbers.  Content lines start with a tab — they won't match.
pub fn extract_blame_shas(file: &str, repo_root: Option<&Path>) -> HashSet<String> {
    let mut cmd = std::process::Command::new("git");
    if let Some(root) = repo_root {
        cmd.arg("-C").arg(root);
    }
    cmd.args(["blame", "--porcelain", file]);
    match cmd.output() {
        Ok(out) if out.status.success() => parse_blame_shas(&out.stdout),
        _ => HashSet::new(),
    }
}

/// Parse SHAs from raw `git blame --porcelain` stdout bytes.
///
/// Uses byte-level filtering so that header lines containing multi-byte UTF-8
/// (e.g. an em-dash in a commit `summary`) cannot panic when slicing the
/// 40-byte SHA prefix.
pub fn parse_blame_shas(stdout: &[u8]) -> HashSet<String> {
    stdout
        .split(|&b| b == b'\n')
        .filter(|line| {
            line.len() > 40
                && line[..40].iter().all(u8::is_ascii_hexdigit)
                && line[40] == b' '
        })
        .map(|line| {
            // The 40-byte slice is guaranteed ASCII by the predicate above,
            // so from_utf8_lossy is a zero-cost conversion here.
            String::from_utf8_lossy(&line[..40]).into_owned()
        })
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

    /// Regression test for the UTF-8 panic introduced in 0ee49f5.
    ///
    /// Real-world `git blame --porcelain` output contains header lines like
    /// `summary feat: Clever Cloud deployment — strip ...` where byte 40 falls
    /// inside the em-dash multibyte sequence. The previous `line[..40]` str
    /// slice panicked: "end byte index 40 is not a char boundary; it is inside
    /// '—' (bytes 38..41)".
    #[test]
    fn parse_blame_shas_does_not_panic_on_multibyte_header_lines() {
        // Synthetic porcelain blob: one valid SHA hunk header, plus headers
        // whose first 40 bytes straddle a multi-byte char.
        let blob = concat!(
            "a1b2c3d4e5f6789012345678901234567890abcd 1 1 1\n",
            "author Test\n",
            // 40th byte falls inside the em-dash (3 bytes).
            "summary feat: Clever Cloud deployment — strip Horde/libcluster, add CC config\n",
            // 40th byte is inside the leading multi-byte glyph.
            "previous 0123456789012345678901234567890123456789 src/€uro_check.rs\n",
            "filename foo.rs\n",
            "\tactual line content\n",
        )
        .as_bytes();

        let shas = parse_blame_shas(blob);
        assert_eq!(shas.len(), 1, "expected only the SHA header to match");
        assert!(shas.contains("a1b2c3d4e5f6789012345678901234567890abcd"));
    }

    #[test]
    fn parse_blame_shas_collects_unique_shas() {
        let blob = concat!(
            "a1b2c3d4e5f6789012345678901234567890abcd 1 1 1\n",
            "\tline 1\n",
            "a1b2c3d4e5f6789012345678901234567890abcd 2 2\n",
            "\tline 2\n",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef 3 3 1\n",
            "\tline 3\n",
        )
        .as_bytes();

        let shas = parse_blame_shas(blob);
        assert_eq!(shas.len(), 2);
        assert!(shas.contains("a1b2c3d4e5f6789012345678901234567890abcd"));
        assert!(shas.contains("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"));
    }

    #[test]
    fn parse_blame_shas_rejects_non_hex_prefixes() {
        // Lines whose first 40 chars contain a non-hex byte must be ignored,
        // even when the 41st byte is a space.
        let blob = b"NOT_A_SHA_NOT_A_SHA_NOT_A_SHA_NOT_A_SHA_ 1 1 1\nignored\n";
        assert!(parse_blame_shas(blob).is_empty());
    }

    #[test]
    fn parse_blame_shas_handles_empty_input() {
        assert!(parse_blame_shas(b"").is_empty());
    }
}
