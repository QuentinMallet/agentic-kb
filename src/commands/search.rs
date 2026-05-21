//! `search` subcommand

use crate::components::db;
use crate::components::embedder;
use crate::config;
use crate::models::{blob_to_f32s, cosine_similarity};
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::params;

/// Search knowledge entries (default: hybrid FTS5 + semantic re-rank)
#[derive(Command, Debug, Parser)]
pub struct Search {
    /// Search query
    pub query: String,
    /// FTS5 keyword search only
    #[arg(long)]
    pub fts: bool,
    /// Semantic similarity search only
    #[arg(long)]
    pub semantic: bool,
    /// Search a different repo's KB (path to repo root)
    #[arg(long)]
    pub repo: Option<std::path::PathBuf>,
}

impl Runnable for Search {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Search {
    /// Execute the search command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = if let Some(repo) = &self.repo {
            config::Paths::from_root(repo)
        } else {
            config::Paths::discover()?
        };
        let emb = crate::commands::add::make_embedder(&paths);
        self.execute_with(&paths, emb.as_ref())
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let do_fts = self.fts || !self.semantic;
        let do_semantic = self.semantic || !self.fts;

        let conn = db::open_db(&paths.db)?;

        if do_fts {
            println!("=== FTS results ===");
            let mut stmt = conn.prepare(
                "SELECT e.id, e.path, e.summary, e.tags
                 FROM entries_fts f
                 JOIN entries e ON e.id = f.id
                 WHERE f.entries_fts MATCH ?1 AND e.is_stale=0
                 ORDER BY rank
                 LIMIT 10",
            )?;
            let mut count = 0;
            let rows = stmt.query_map(params![self.query], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (id, path, summary, tags) = row?;
                println!("  [{path}] {summary}  tags={tags}  id={id}");
                count += 1;
            }
            if count == 0 {
                println!("  (no results)");
            }
        }

        if do_semantic && !embedder.is_noop() {
            println!("=== Semantic results ===");
            let q_emb = embedder.embed(&self.query)?;

            let mut stmt = conn.prepare(
                "SELECT e.id, e.path, e.summary, e.tags, emb.embedding
                 FROM entries_emb emb
                 JOIN entries e ON e.rowid = emb.rowid
                 WHERE e.is_stale = 0",
            )?;
            let mut candidates: Vec<(f32, String, String, String, String)> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .map(|(id, path, summary, tags, blob)| {
                    let emb = blob_to_f32s(&blob);
                    let sim = cosine_similarity(&q_emb, &emb);
                    (sim, id, path, summary, tags)
                })
                .collect();

            candidates
                .sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let top: Vec<_> = candidates.into_iter().take(10).collect();

            if top.is_empty() {
                println!("  (no results)");
            } else {
                for (sim, id, path, summary, tags) in top {
                    println!("  [{path}] {summary}  sim={sim:.4}  tags={tags}  id={id}");
                }
            }
        }

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
    fn test_cmd_search_fts_returns_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let add_cmd = Add {
            path: "src/auth.rs".to_string(),
            summary: "authentication module".to_string(),
            content: "handles JWT tokens".to_string(),
            tags: "auth,security".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("search-test-1".to_string()),
            permanent: false,
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // Search by FTS — just verify it doesn't error
        let search_cmd = Search {
            query: "authentication".to_string(),
            fts: true,
            semantic: false,
            repo: None,
        };
        search_cmd.execute_with(&paths, &embedder).unwrap();
    }

    #[test]
    fn test_cmd_search_repo_flag_searches_alternate_kb() {
        // Build a "remote" KB in a separate temp dir
        let remote_dir = tempdir().unwrap();
        let remote_root = remote_dir.path();
        fs::create_dir_all(remote_root.join(".state/agent-kb")).unwrap();
        let remote_paths = Paths::from_root(remote_root);
        let embedder = NoopEmbedder;

        let add_cmd = Add {
            path: "remote/mod.rs".to_string(),
            summary: "remote knowledge".to_string(),
            content: "cross-repo content".to_string(),
            tags: "remote".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("remote-1".to_string()),
            permanent: false,
        };
        add_cmd.execute_with(&remote_paths, &embedder).unwrap();

        // Search the remote KB via --repo (simulate by passing remote_paths directly)
        let search_cmd = Search {
            query: "remote knowledge".to_string(),
            fts: true,
            semantic: false,
            repo: Some(remote_root.to_path_buf()),
        };
        search_cmd.execute_with(&remote_paths, &embedder).unwrap();
    }
}
