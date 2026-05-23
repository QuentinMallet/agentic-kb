//! `stale-check` subcommand

use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::params;
use std::collections::HashSet;
use std::path::Path;

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
        let mut found_any = false;
        let mut seen_ids: HashSet<String> = HashSet::new();

        // -- File-based path matching (existing behaviour) --
        for file in &self.files {
            let rel_path = normalize_path(file, repo_root.as_deref());
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
                if let Some(count) = commits_since(&version_ref, &stored_path, repo_root.as_deref()) {
                    if count > 0 {
                        println!(
                            "STALE  [{stored_path}] {summary}  id={id}  recorded-at={version_ref}  ({count} commit{} ago)",
                            if count == 1 { "" } else { "s" }
                        );
                        seen_ids.insert(id);
                        found_any = true;
                    }
                }
            }
        }

        // -- Commit-based lookup (new behaviour) --
        let mut all_commits: HashSet<String> = self.commits.iter().cloned().collect();
        if self.blame {
            for file in &self.files {
                all_commits.extend(extract_blame_shas(file, repo_root.as_deref()));
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
                println!("REVIEW [{stored_path}] {summary}  id={id}  recorded-at={version_ref}  (matches blame/commit)");
                seen_ids.insert(id);
                found_any = true;
            }
        }

        if !found_any {
            println!("ok");
        }
        Ok(())
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
        Ok(out) if out.status.success() => std::str::from_utf8(&out.stdout)
            .unwrap_or("")
            .lines()
            .filter(|line| {
                line.len() > 40
                    && line[..40].chars().all(|c| c.is_ascii_hexdigit())
                    && line.as_bytes().get(40) == Some(&b' ')
            })
            .map(|line| line[..40].to_string())
            .collect(),
        _ => HashSet::new(),
    }
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
