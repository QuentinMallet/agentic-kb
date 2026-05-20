//! `stale-check` subcommand

use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::params;

/// Check if kb entries for given files are stale
#[derive(Command, Debug, Parser)]
pub struct StaleCheck {
    /// One or more file paths to check
    #[arg(required = true)]
    pub files: Vec<String>,
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
        let conn = db::open_db(&paths.db)?;
        let repo_root = config::git_repo_root();
        let mut found_any = false;

        for file in &self.files {
            let rel_path: String = if let Some(ref root) = repo_root {
                let p = std::path::Path::new(file);
                if p.is_absolute() {
                    p.strip_prefix(root)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    file.clone()
                }
            } else {
                file.clone()
            };

            let mut stmt = conn.prepare(
                "SELECT id, summary, version_ref, path FROM entries
                 WHERE (path = ?1 OR path LIKE '%' || ?1 OR ?1 LIKE '%' || path)
                   AND version_ref IS NOT NULL AND is_stale = 0",
            )?;
            let rows: Vec<_> = stmt
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

            for (id, summary, version_ref, stored_path) in rows {
                let mut cmd = std::process::Command::new("git");
                if let Some(ref root) = repo_root {
                    cmd.arg("-C").arg(root);
                }
                cmd.args([
                    "log",
                    "--oneline",
                    &format!("{}..HEAD", version_ref),
                    "--",
                    &stored_path,
                ]);
                if let Ok(out) = cmd.output() {
                    if out.status.success()
                        && !out.stdout.iter().all(|b| b.is_ascii_whitespace())
                    {
                        let count = std::str::from_utf8(&out.stdout)
                            .unwrap_or("")
                            .lines()
                            .count();
                        println!(
                            "STALE  [{stored_path}] {summary}  id={id}  recorded-at={version_ref}  ({count} commit{} ago)",
                            if count == 1 { "" } else { "s" }
                        );
                        found_any = true;
                    }
                }
            }
        }

        if !found_any {
            println!("ok");
        }
        Ok(())
    }
}
