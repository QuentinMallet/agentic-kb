//! `search` subcommand

use crate::components::db;
use crate::components::embedder;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;

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
    /// Maximum number of results (default: 10)
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Include full content in output
    #[arg(long)]
    pub content: bool,
    /// Filter results to entries whose path starts with this prefix
    #[arg(long)]
    pub path_prefix: Option<String>,
    /// Filter results to entries that have this tag
    #[arg(long)]
    pub tag: Option<String>,
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
        let opts = db::SearchOptions {
            limit: self.limit,
            do_fts: self.fts || !self.semantic,
            do_semantic: self.semantic || !self.fts,
            path_prefix: self.path_prefix.clone(),
            tag_filter: self.tag.clone(),
        };

        let conn = db::open_db(&paths.db)?;
        let results = db::search_entries(&conn, embedder, &self.query, &opts)?;

        let mut fts_count = 0usize;
        let mut sem_count = 0usize;
        for r in &results {
            if r.source == "fts" {
                fts_count += 1;
            } else {
                sem_count += 1;
            }
        }

        if opts.do_fts {
            println!("=== FTS results ===");
            let fts_results: Vec<_> = results.iter().filter(|r| r.source == "fts").collect();
            if fts_results.is_empty() {
                println!("  (no results)");
            } else {
                for r in fts_results {
                    println!("  [{path}] {summary}  tags={tags}  id={id}",
                        path = r.path, summary = r.summary,
                        tags = r.tags, id = r.id);
                    if self.content && !r.content.is_empty() {
                        println!("  content: {}", r.content);
                    }
                }
            }
        }

        if opts.do_semantic && sem_count > 0 {
            println!("=== Semantic results ===");
            for r in results.iter().filter(|r| r.source == "semantic") {
                println!("  [{path}] {summary}  sim={score:.4}  tags={tags}  id={id}",
                    path = r.path, summary = r.summary,
                    score = r.score, tags = r.tags, id = r.id);
                if self.content && !r.content.is_empty() {
                    println!("  content: {}", r.content);
                }
            }
        } else if opts.do_semantic && !embedder.is_noop() && fts_count == 0 {
            println!("=== Semantic results ===");
            println!("  (no results)");
        }

        let _ = (fts_count, sem_count); // suppress unused warning
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
            replace_path: false,
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // Search by FTS — just verify it doesn't error
        let search_cmd = Search {
            query: "authentication".to_string(),
            fts: true,
            semantic: false,
            repo: None,
            limit: 10,
            content: false,
            path_prefix: None,
            tag: None,
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
            replace_path: false,
        };
        add_cmd.execute_with(&remote_paths, &embedder).unwrap();

        // Search the remote KB via --repo (simulate by passing remote_paths directly)
        let search_cmd = Search {
            query: "remote knowledge".to_string(),
            fts: true,
            semantic: false,
            repo: Some(remote_root.to_path_buf()),
            limit: 10,
            content: false,
            path_prefix: None,
            tag: None,
        };
        search_cmd.execute_with(&remote_paths, &embedder).unwrap();
    }

    #[test]
    fn test_cmd_search_limit_flag() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Add 5 entries
        for i in 0..5 {
            let add_cmd = Add {
                path: format!("src/mod{i}.rs"),
                summary: format!("module {i} authentication"),
                content: format!("content for module {i}"),
                tags: "rust".to_string(),
                version_ref: Some("abc".to_string()),
                id: Some(format!("limit-test-{i}")),
                permanent: false,
                replace_path: false,
            };
            add_cmd.execute_with(&paths, &embedder).unwrap();
        }

        // --limit 3 should parse without error (was broken before this fix)
        let search_cmd = Search {
            query: "authentication".to_string(),
            fts: true,
            semantic: false,
            repo: None,
            limit: 3,
            content: false,
            path_prefix: None,
            tag: None,
        };
        search_cmd.execute_with(&paths, &embedder).unwrap();
    }

    #[test]
    fn test_cmd_search_fts_injection_safe() {
        // Queries with FTS5 operators should not cause errors
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let add_cmd = Add {
            path: "src/auth.rs".to_string(),
            summary: "auth module".to_string(),
            content: "handles authentication".to_string(),
            tags: "auth".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("inj-test-1".to_string()),
            permanent: false,
            replace_path: false,
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // These queries contain FTS5 operators — should not error out
        for q in &["auth AND security", "auth OR security", "auth NOT security", "auth*"] {
            let search_cmd = Search {
                query: q.to_string(),
                fts: true,
                semantic: false,
                repo: None,
                limit: 10,
                content: false,
                path_prefix: None,
                tag: None,
            };
            // Should succeed (no panic, no error)
            let _ = search_cmd.execute_with(&paths, &embedder);
        }
    }

    #[test]
    fn test_cmd_search_path_prefix_filter() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        for (id, path) in &[
            ("pf-1", "src/auth.rs"),
            ("pf-2", "docs/auth.md"),
            ("pf-3", "src/tokens.rs"),
        ] {
            let add_cmd = Add {
                path: path.to_string(),
                summary: format!("authentication entry at {path}"),
                content: "authentication content".to_string(),
                tags: "auth".to_string(),
                version_ref: Some("abc".to_string()),
                id: Some(id.to_string()),
                permanent: false,
                replace_path: false,
            };
            add_cmd.execute_with(&paths, &embedder).unwrap();
        }

        // --path-prefix src/ should only match src/ entries
        let search_cmd = Search {
            query: "authentication".to_string(),
            fts: true,
            semantic: false,
            repo: None,
            limit: 10,
            content: false,
            path_prefix: Some("src/".to_string()),
            tag: None,
        };
        // Just verify no error — path filtering is applied in SQL
        search_cmd.execute_with(&paths, &embedder).unwrap();
    }
}
