//! `reembed` subcommand — backfill missing embeddings

use crate::commands::add::make_embedder;
use crate::components::{db, embedder};
use crate::config;
use crate::models::f32s_to_f16_blob;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::params;

/// Re-embed entries that are missing embeddings (e.g. written with KB_NO_EMBED=1)
#[derive(Command, Debug, Parser)]
pub struct Reembed {
    /// Show what would be re-embedded without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Skip entries where path+summary+content exceeds this char limit (default: 1800)
    #[arg(long, default_value_t = 1800)]
    pub max_chars: usize,
}

impl Runnable for Reembed {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Reembed {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let emb = make_embedder(&paths);
        self.execute_with(&paths, emb.as_ref())
    }

    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let conn = db::open_db(&paths.db)?;

        // Find non-stale entries with no embedding row
        let mut stmt = conn.prepare(
            "SELECT e.rowid, e.id, e.path, e.summary, e.content, e.tags
             FROM entries e
             WHERE e.is_stale = 0
               AND e.rowid NOT IN (SELECT rowid FROM entries_emb)",
        )?;

        let candidates: Vec<(i64, String, String, String, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Size filter runs on the ACTUAL embed text for the active mode —
        // in Abstraction mode content is excluded and tags included, so a
        // path+summary+content heuristic would mis-skip (review finding).
        let mode = db::EmbedTextMode::from_env();
        let to_embed: Vec<_> = candidates
            .iter()
            .filter(|(_, _, path, summary, content, tags)| {
                db::entry_embed_text(mode, path, summary, content, tags).len() <= self.max_chars
            })
            .collect();

        let skipped_size = candidates.len() - to_embed.len();

        if self.dry_run {
            println!(
                "dry-run: {} entries to re-embed, {} skipped (exceeds {} chars)",
                to_embed.len(),
                skipped_size,
                self.max_chars
            );
            for (_, id, path, _, _, _) in &to_embed {
                println!("  would embed: [{path}] id={id}");
            }
            return Ok(());
        }

        if embedder.is_noop() {
            eprintln!("reembed: KB_NO_EMBED is set — skipping (no embedder available)");
            return Ok(());
        }

        let mut done = 0usize;
        let mut failed = 0usize;

        db::check_embed_mode_vintage(&conn, mode);
        for (rowid, id, path, summary, content, tags) in &to_embed {
            let text = db::entry_embed_text(mode, path, summary, content, tags);
            match embedder.embed(&text) {
                Ok(emb_vec) if emb_vec.iter().all(|x| x.is_finite()) => {
                    let blob = f32s_to_f16_blob(&emb_vec);
                    conn.execute(
                        "INSERT OR REPLACE INTO entries_emb(rowid, embedding) VALUES(?1, ?2)",
                        params![rowid, blob],
                    )?;
                    done += 1;
                }
                Ok(_) => {
                    eprintln!("  skip {id}: embedder returned a non-finite component");
                    failed += 1;
                }
                Err(e) => {
                    eprintln!("  skip {id}: {e}");
                    failed += 1;
                }
            }
        }

        println!("reembed: {done} embedded, {failed} failed, {skipped_size} skipped (too large)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::add::Add;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_cmd_reembed_dry_run_no_writes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Add an entry (NoopEmbedder → no embedding written)
        let add_cmd = Add {
            path: "src/auth.rs".to_string(),
            summary: "auth module".to_string(),
            content: "handles JWT tokens".to_string(),
            tags: "auth".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("re-test-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // Dry-run must not error
        let cmd = Reembed {
            dry_run: true,
            max_chars: 1800,
        };
        cmd.execute_with(&paths, &embedder).unwrap();
    }

    #[test]
    fn test_cmd_reembed_skips_large_entries() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Add a large entry (2000 chars of content)
        let large_content = "x".repeat(2000);
        let add_cmd = Add {
            path: "docs/large.md".to_string(),
            summary: "large doc".to_string(),
            content: large_content,
            tags: "docs".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("re-large-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // Reembed with max_chars=100 → large entry must be skipped without error
        let cmd = Reembed {
            dry_run: true,
            max_chars: 100,
        };
        cmd.execute_with(&paths, &embedder).unwrap();
    }
}
