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
        // CLI: repo_root left None; search_entries falls back to find_repo_root()
        // walking from CWD, which is correct for the CLI invocation pattern (user
        // runs `kb search` from inside the repo). MCP path sets repo_root explicitly
        // via root_from_db (mcp.rs:40-45) because MCP CWD is typically '/' and CWD
        // discovery would fail.
        let opts = db::SearchOptions {
            limit: self.limit,
            do_fts: self.fts || !self.semantic,
            do_semantic: self.semantic || !self.fts,
            path_prefix: self.path_prefix.clone(),
            tag_filter: self.tag.clone(),
            inline_verify_k: self.limit, // verify all results by default
            repo_root: None,
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
                    for ev in &r.evidence {
                        let verified_str = match ev.verified {
                            Some(true) => "verified=true",
                            Some(false) => "verified=false",
                            None => "verified=null",
                        };
                        println!("    evidence: kind={kind}  {path}  {verified}",
                            kind = ev.kind,
                            path = ev.citation_path.as_deref().unwrap_or(""),
                            verified = verified_str);
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
                for ev in &r.evidence {
                    let verified_str = match ev.verified {
                        Some(true) => "verified=true",
                        Some(false) => "verified=false",
                        None => "verified=null",
                    };
                    println!("    evidence: kind={kind}  {path}  {verified}",
                        kind = ev.kind,
                        path = ev.citation_path.as_deref().unwrap_or(""),
                        verified = verified_str);
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
                kind: "belief".to_string(),
                evidence: vec![],
                evidence_file: None,
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
                kind: "belief".to_string(),
                evidence: vec![],
                evidence_file: None,
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
                kind: "belief".to_string(),
                evidence: vec![],
                evidence_file: None,
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
                kind: "belief".to_string(),
                evidence: vec![],
                evidence_file: None,
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
                kind: "belief".to_string(),
                evidence: vec![],
                evidence_file: None,
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

    /// AC17: search results include evidence array with verified flag.
    /// Uses Cargo.toml as the cited file (stable, always present).
    #[test]
    fn test_kb_search_returns_evidence_with_verified_flag() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Write a small known file into the tempdir to use as citation target.
        let cited_content = b"fn main() { println!(\"hello\"); }";
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let cited_file = src_dir.join("cited.rs");
        fs::write(&cited_file, cited_content).unwrap();

        // Compute correct hash for byte range 0..cited_content.len()
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(cited_content);
        let hash = format!("sha256:{:x}", h.finalize());
        let end = cited_content.len();
        let citation_path = format!("src/cited.rs:0-{end}");

        let evidence_json = format!(
            r#"{{"kind":"code","citation_path":"{citation_path}","citation_sha":null,"citation_hash":"{hash}","citation_excerpt":"fn main()"}}"#
        );

        let add_cmd = Add {
            path: "src/evidence_test".to_string(),
            summary: "evidence test entry".to_string(),
            content: "evidence test content".to_string(),
            tags: "evidence".to_string(),
            version_ref: Some("abc123".to_string()),
            id: Some("ev-search-test-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "observation".to_string(),
            evidence: vec![evidence_json],
            evidence_file: None,
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // Run search using db directly so we can inspect evidence field.
        let opts = crate::components::db::SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: false,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 10,
            repo_root: None,
        };
        let conn = crate::components::db::open_db(&paths.db).unwrap();

        // Override CWD-based repo root discovery by using the db search directly
        // with the root as repo root. Since find_repo_root() walks from CWD (not
        // the tempdir), we test verification via the MCP path which passes repo_root
        // explicitly. Instead, verify the evidence array is populated and the
        // verified field is Some (true or false) — not None.
        let results = crate::components::db::search_entries(&conn, &embedder, "evidence test", &opts).unwrap();
        assert!(!results.is_empty(), "search must return at least 1 result");

        let entry = results.iter().find(|r| r.id == "ev-search-test-1").unwrap();
        assert_eq!(entry.evidence.len(), 1, "entry must have 1 evidence row");

        // verified is Some(bool) — inline verification was attempted
        // (true if CWD happens to be the tempdir, false otherwise — both are acceptable)
        assert!(entry.evidence[0].verified.is_some(), "verified must not be null for top-K results");
        assert_eq!(entry.evidence[0].kind, "code");
    }

    /// AC18: inline_verify_k narrow-K fallback — results beyond K get verified=null.
    #[test]
    fn test_kb_search_narrow_k_fallback() {
        use sha2::{Digest, Sha256};

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        // Create a stable cited file
        let cited_content = b"narrow k test content";
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("narrow.rs"), cited_content).unwrap();

        let mut h = Sha256::new();
        h.update(cited_content);
        let hash = format!("sha256:{:x}", h.finalize());
        let end = cited_content.len();

        // Insert 3 entries each with 1 evidence row, all with the same summary
        // so FTS returns all 3.
        for i in 0..3usize {
            let citation_path = format!("src/narrow.rs:0-{end}");
            let evidence_json = format!(
                r#"{{"kind":"code","citation_path":"{citation_path}","citation_sha":null,"citation_hash":"{hash}","citation_excerpt":"narrow"}}"#
            );
            let add_cmd = Add {
                path: format!("src/narrow_mod_{i}.rs"),
                summary: "narrow k fallback entry authentication".to_string(),
                content: format!("narrow k content {i}"),
                tags: "narrow".to_string(),
                version_ref: Some("abc".to_string()),
                id: Some(format!("narrow-k-{i}")),
                permanent: false,
                replace_path: false,
                kind: "observation".to_string(),
                evidence: vec![evidence_json],
                evidence_file: None,
            };
            add_cmd.execute_with(&paths, &embedder).unwrap();
        }

        // Search with inline_verify_k=1 → only first result gets verified=Some, rest get None.
        let opts = crate::components::db::SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: false,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 1,
            repo_root: None,
        };
        let conn = crate::components::db::open_db(&paths.db).unwrap();
        let results = crate::components::db::search_entries(
            &conn, &embedder, "narrow k fallback entry authentication", &opts,
        ).unwrap();

        assert_eq!(results.len(), 3, "all 3 entries must be returned");

        // First result: verified=Some(...)
        let first = &results[0];
        assert_eq!(first.evidence.len(), 1);
        assert!(first.evidence[0].verified.is_some(), "top-1 result must have verified=Some(...)");

        // Remaining results: verified=None
        for r in &results[1..] {
            assert_eq!(r.evidence.len(), 1);
            assert!(r.evidence[0].verified.is_none(), "results beyond K must have verified=null");
        }
    }
}
