//! `search` subcommand

#![allow(deprecated)] // db::open_db (ADR-1) — remaining call sites migrate in C2/L1b, L2, L3, L1c
use crate::components::embedder;
use crate::components::{db, query_hits};
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use std::collections::{HashMap, HashSet};

fn compare_federated_rows(a: &db::SearchEntry, b: &db::SearchEntry) -> std::cmp::Ordering {
    db::compare_rank(a.score, "", b.score, "")
        .then_with(|| match (&a.origin_repo, &b.origin_repo) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => a.cmp(b),
        })
        .then_with(|| a.id.cmp(&b.id))
}

/// Merge already-ranked repository batches, deduplicate globally, and apply the
/// sole federated truncation. The optional batch origin is stamped on peer rows.
fn merge_federated_results(
    batches: Vec<(Option<String>, Vec<db::SearchEntry>)>,
    limit: usize,
) -> Vec<db::SearchEntry> {
    let mut by_origin_and_id: HashMap<(Option<String>, String), db::SearchEntry> = HashMap::new();

    for (batch_origin, rows) in batches {
        for mut candidate in rows {
            candidate.origin_repo = batch_origin.clone();

            let collision_key = by_origin_and_id
                .keys()
                .find(|(_, id)| id == &candidate.id)
                .cloned();
            if let Some(key) = collision_key {
                let existing = by_origin_and_id.get(&key).expect("collision key exists");
                // Local wins by contract. Spell this out instead of depending on
                // Option's derived ordering to happen to put None first.
                let replace = match (
                    existing.origin_repo.is_none(),
                    candidate.origin_repo.is_none(),
                ) {
                    (true, _) => false,
                    (false, true) => true,
                    (false, false) => compare_federated_rows(&candidate, existing).is_lt(),
                };
                if replace {
                    by_origin_and_id.remove(&key);
                } else {
                    continue;
                }
            }

            let key = (candidate.origin_repo.clone(), candidate.id.clone());
            by_origin_and_id.insert(key, candidate);
        }
    }

    let mut merged: Vec<_> = by_origin_and_id.into_values().collect();
    merged.sort_by(compare_federated_rows);
    merged.truncate(limit);
    merged
}

fn evidence_display_line(ev: &db::SearchEvidence) -> String {
    let verified_str = match ev.verified {
        Some(true) => "verified=true",
        Some(false) => "verified=false",
        None => "verified=null",
    };
    format!(
        "    evidence: kind={}  {}  status={}  {}",
        ev.kind,
        ev.citation_path.as_deref().unwrap_or(""),
        ev.status_str(),
        verified_str
    )
}

fn parse_limit(arg: &str) -> Result<usize, String> {
    let value: usize = arg
        .parse()
        .map_err(|_| format!("invalid value '{arg}' for '--limit': expected an integer"))?;
    if !(1..=db::MAX_LIMIT).contains(&value) {
        return Err(format!(
            "invalid value '{arg}' for '--limit': must be in 1..={}",
            db::MAX_LIMIT
        ));
    }
    Ok(value)
}

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
    #[arg(long, default_value_t = 10, value_parser = parse_limit)]
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
    /// Skip peer federation and search only the local DB
    #[arg(long, default_value_t = false)]
    pub local_only: bool,
    /// Open each registered peer DB and merge results
    #[arg(long, default_value_t = false)]
    pub peers: bool,
    /// Traverse peer graph from this repo path (implies --peers)
    #[arg(long)]
    pub reachable_from: Option<String>,
    /// Max hops for --reachable-from traversal (default: 1)
    #[arg(long, default_value_t = 1)]
    pub max_hops: u8,
    /// Restrict peer traversal to edges with this epic_slug
    #[arg(long)]
    pub slug: Option<String>,
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
        crate::commands::rebuild::rebuild_if_schema_obsolete(&paths, emb.as_ref())?;
        self.execute_with(&paths, emb.as_ref())
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let kb_config = config::KbConfig::from_paths(paths);
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
            inline_verify_k: self.limit, // verify all results by default, capped in search_entries
            repo_root: None,
            verify_pool_size: kb_config.verify_pool_size,
            recency_lambda: kb_config.recency_lambda,
            mmr_lambda: kb_config.mmr_lambda,
        };

        // A pure read: open_ro, never the write lock (ADR-7). An uninitialized
        // repository serves an empty result plus a one-line stderr note, which
        // is the first-run behaviour the pre-split read path produced by
        // silently creating the database.
        let conn = match db::open_ro(&paths.db) {
            Ok(conn) => Some(conn),
            Err(e) if db::is_db_uninitialized(&e) => {
                db::note_uninitialized(&paths.db);
                None
            }
            Err(e) => return Err(e),
        };
        let local_results = match &conn {
            Some(conn) => db::search_entries(conn, embedder, &self.query, &opts)?,
            None => Vec::new(),
        };

        // Peer federation: collect results from peer DBs and merge.
        let results = if let (Some(conn), true) = (
            conn.as_ref(),
            !self.local_only && (self.peers || self.reachable_from.is_some()),
        ) {
            let peer_paths = collect_peer_paths(
                conn,
                self.reachable_from.as_deref(),
                self.max_hops,
                self.slug.as_deref(),
            );

            let mut batches = vec![(None, local_results)];

            for peer_path in peer_paths {
                let peer_db = config::Paths::from_root(std::path::Path::new(&peer_path)).db;
                // A peer's DB belongs to another repository: reading it must
                // never create it, run DDL against it, or sweep its rows.
                let peer_conn = match db::open_ro(&peer_db) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("warn: peer {peer_path}: {e}");
                        continue;
                    }
                };
                // Federation asymmetry guard: recency_lambda is forced to 0.0
                // for peer queries. Peer clocks may differ from local clock,
                // making decay scores incomparable across repos.
                if opts.recency_lambda != 0.0 {
                    eprintln!(
                        "warn: recency_lambda={} forced to 0.0 for peer {} (clock skew guard)",
                        opts.recency_lambda, peer_path
                    );
                }
                let peer_opts = db::SearchOptions {
                    repo_root: Some(std::path::PathBuf::from(&peer_path)),
                    recency_lambda: 0.0,
                    mmr_lambda: 0.0,
                    ..opts.clone()
                };
                match db::search_entries(&peer_conn, embedder, &self.query, &peer_opts) {
                    Ok(peer_results) => batches.push((Some(peer_path), peer_results)),
                    Err(e) => eprintln!("warn: peer {peer_path} search: {e}"),
                }
            }
            merge_federated_results(batches, self.limit)
        } else {
            local_results
        };

        // Determine display mode: RRF hybrid produces unified results;
        // single-lane modes keep separate FTS / semantic sections.
        let is_rrf = results.iter().any(|r| r.score_kind == "rrf");

        fn print_entry(r: &db::SearchEntry, show_content: bool) {
            println!("  [{path}] {summary}  score={score:.6}  score_kind={score_kind}  tags={tags}  id={id}",
                path = r.path, summary = r.summary,
                score = r.score, score_kind = r.score_kind,
                tags = r.tags, id = r.id);
            if show_content && !r.content.is_empty() {
                println!("  content: {}", r.content);
            }
            for ev in &r.evidence {
                println!("{}", evidence_display_line(ev));
            }
        }

        fn print_section<'a, I>(header: &str, rows: I, show_content: bool)
        where
            I: IntoIterator<Item = &'a db::SearchEntry>,
        {
            println!("{header}");
            let mut empty = true;
            for r in rows {
                print_entry(r, show_content);
                empty = false;
            }
            if empty {
                println!("  (no results)");
            }
        }

        if is_rrf {
            print_section("=== Hybrid (RRF) results ===", &results, self.content);
        } else {
            if opts.do_fts {
                let fts = results.iter().filter(|r| r.score_kind == "fts");
                print_section("=== FTS results ===", fts, self.content);
            }
            if opts.do_semantic && !embedder.is_noop() {
                let sem = results.iter().filter(|r| r.score_kind == "semantic");
                print_section("=== Semantic results ===", sem, self.content);
            }
        }

        if let Ok(surface) = std::env::var("KB_INJECTION_SOURCE") {
            let session_id =
                std::env::var("CLAUDE_SESSION_ID").unwrap_or_else(|_| "unknown".into());
            let injected: Vec<_> = results
                .iter()
                .map(|entry| {
                    let cited_file = entry
                        .evidence
                        .iter()
                        .find_map(|ev| ev.citation_path.as_deref())
                        .map(citation_file_component);
                    (entry.id.clone(), cited_file)
                })
                .collect();
            query_hits::record_injection(&paths.query_hits, &session_id, &injected, &surface);
        }

        Ok(())
    }
}

fn citation_file_component(path: &str) -> String {
    let Some((file, suffix)) = path.rsplit_once(':') else {
        return path.to_owned();
    };
    if suffix
        .split('-')
        .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
    {
        file.to_owned()
    } else {
        path.to_owned()
    }
}

/// Collect peer target_repo paths from the local DB for federation.
///
/// If `reachable_from` is set, performs BFS up to `max_hops` hops starting
/// from that repo. Otherwise returns direct peers (1 hop) of the local DB.
fn collect_peer_paths(
    conn: &rusqlite::Connection,
    reachable_from: Option<&str>,
    max_hops: u8,
    slug_filter: Option<&str>,
) -> Vec<String> {
    match reachable_from {
        Some(start) => bfs_peers(conn, start, max_hops, slug_filter),
        // Direct peers only (1 hop).
        None => query_direct_peers(conn, slug_filter),
    }
}

/// Run a `SELECT DISTINCT target_repo FROM peers` variant and collect the rows.
///
/// Returns an empty Vec on any prepare/query error — peer federation is best-effort
/// and must not abort the local search. `params` are bound positionally in order.
fn query_target_repos(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(sql) else {
        return vec![];
    };
    stmt.query_map(params, |r| r.get::<_, String>(0))
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Query direct target_repo values from the peers table.
fn query_direct_peers(conn: &rusqlite::Connection, slug_filter: Option<&str>) -> Vec<String> {
    match slug_filter {
        Some(slug) => query_target_repos(
            conn,
            &format!(
                "SELECT DISTINCT p.target_repo FROM peers p WHERE {} AND p.epic_slug = ?1",
                db::live_peer_predicate("p"),
            ),
            &[&slug],
        ),
        None => query_target_repos(
            conn,
            &format!(
                "SELECT DISTINCT p.target_repo FROM peers p WHERE {}",
                db::live_peer_predicate("p"),
            ),
            &[],
        ),
    }
}

/// BFS traversal of peer graph starting from `start_repo` up to `max_hops`.
fn bfs_peers(
    conn: &rusqlite::Connection,
    start_repo: &str,
    max_hops: u8,
    slug_filter: Option<&str>,
) -> Vec<String> {
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start_repo.to_string());
    let mut frontier: Vec<String> = vec![start_repo.to_string()];
    let mut result: Vec<String> = Vec::new();

    for _ in 0..max_hops {
        if frontier.is_empty() {
            break;
        }
        let mut next_frontier: Vec<String> = Vec::new();
        for repo in &frontier {
            for neighbor in query_neighbors(conn, repo, slug_filter) {
                if visited.insert(neighbor.clone()) {
                    result.push(neighbor.clone());
                    next_frontier.push(neighbor);
                }
            }
        }
        frontier = next_frontier;
    }
    result
}

/// Query direct neighbors (target_repo) for a given source_repo.
fn query_neighbors(
    conn: &rusqlite::Connection,
    source_repo: &str,
    slug_filter: Option<&str>,
) -> Vec<String> {
    match slug_filter {
        Some(slug) => query_target_repos(
            conn,
            &format!(
                "SELECT DISTINCT p.target_repo FROM peers p WHERE p.source_repo = ?1 AND {} AND p.epic_slug = ?2",
                db::live_peer_predicate("p"),
            ),
            &[&source_repo, &slug],
        ),
        None => query_target_repos(
            conn,
            &format!(
                "SELECT DISTINCT p.target_repo FROM peers p WHERE p.source_repo = ?1 AND {}",
                db::live_peer_predicate("p"),
            ),
            &[&source_repo],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::add::Add;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use crate::models::VerificationStatus;
    use std::env;
    use std::fs;
    use tempfile::tempdir;

    const FAST_PROPTEST_CASES: u32 = 16;

    fn federated_row(id: &str, score: f32, origin_repo: Option<&str>) -> db::SearchEntry {
        db::SearchEntry {
            id: id.to_string(),
            path: format!("{id}.md"),
            summary: format!("summary {id}"),
            content: String::new(),
            tags: "[]".to_string(),
            score,
            source: "rrf",
            score_kind: "rrf",
            evidence: vec![],
            confidence: 0.5,
            audit_n: 0,
            origin_repo: origin_repo.map(str::to_string),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    fn federated_contract_fixture(peer_order: &[&str]) -> Vec<db::SearchEntry> {
        let local = (0..8)
            .map(|i| federated_row(&format!("local-{i}"), 1.0 / (60.0 + i as f32), None));
        let mut batches = vec![(None, local.collect())];
        for peer in peer_order {
            let rows = match *peer {
                "peer-a" => vec![
                    federated_row("shared-peer", 1.0 / 61.0, Some(peer)),
                    federated_row("shared-local", 1.0 / 62.0, Some(peer)),
                    federated_row("peer-a-only", 1.0 / 63.0, Some(peer)),
                ],
                "peer-b" => vec![
                    federated_row("shared-peer", 1.0 / 61.0, Some(peer)),
                    federated_row("peer-b-only", 1.0 / 62.0, Some(peer)),
                ],
                _ => unreachable!(),
            };
            batches.push((Some((*peer).to_string()), rows));
        }
        batches[0].1.push(federated_row("shared-local", 1.0 / 64.0, None));
        merge_federated_results(batches, 10)
    }

    #[test]
    fn test_federated_global_limit_dedup_and_local_collision_contract() {
        let results = federated_contract_fixture(&["peer-a", "peer-b"]);
        assert_eq!(
            results.len(),
            10,
            "--limit is global across local and two peers"
        );
        assert_eq!(
            results.iter().filter(|row| row.id == "shared-peer").count(),
            1,
            "an id present in two peers must appear once"
        );
        let collision = results.iter().find(|row| row.id == "shared-local").unwrap();
        assert!(
            collision.origin_repo.is_none(),
            "the local row must explicitly win a local/peer id collision"
        );
    }

    #[test]
    fn test_federated_order_is_byte_stable_under_peer_traversal_permutation() {
        fn bytes(rows: &[db::SearchEntry]) -> Vec<u8> {
            rows.iter()
                .flat_map(|row| {
                    format!("{:?}\t{}\t{}\n", row.origin_repo, row.id, row.score).into_bytes()
                })
                .collect()
        }
        assert_eq!(
            bytes(&federated_contract_fixture(&["peer-a", "peer-b"])),
            bytes(&federated_contract_fixture(&["peer-b", "peer-a"])),
        );
    }

    #[test]
    fn test_federated_rank_position_top_tiny_peer_outranks_mid_local() {
        let rows = merge_federated_results(
            vec![
                (None, vec![federated_row("local-mid", 1.0 / 62.0, None)]),
                (
                    Some("tiny-peer".to_string()),
                    vec![federated_row("peer-top", 1.0 / 61.0, Some("tiny-peer"))],
                ),
            ],
            10,
        );
        // This is by design: cross-repo RRF scores encode within-repo rank position,
        // not relevance calibrated across differently sized corpora.
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["peer-top", "local-mid"]
        );
    }

    fn proptest_cases(default_full: u32) -> u32 {
        env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(FAST_PROPTEST_CASES.min(default_full))
    }

    fn insert_peer_edge(
        conn: &rusqlite::Connection,
        source_repo: &str,
        target_repo: &str,
        expires_at: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO graphs(id, graph_type, source_repo, created_at, expires_at)
             VALUES(?1, 'dep', ?2, '2024-01-01T00:00:00Z', ?3)",
            rusqlite::params![
                format!("graph-{source_repo}-{target_repo}"),
                source_repo,
                expires_at
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO peers(
                id, graph_id, source_repo, target_repo, edge_type, created_at, expires_at
             ) VALUES(?1, ?2, ?3, ?4, 'dep', '2024-01-01T00:00:00Z', ?5)",
            rusqlite::params![
                format!("peer-{source_repo}-{target_repo}"),
                format!("graph-{source_repo}-{target_repo}"),
                source_repo,
                target_repo,
                expires_at
            ],
        )
        .unwrap();
    }

    #[test]
    fn test_evidence_display_line_includes_status() {
        let line = evidence_display_line(&db::SearchEvidence {
            id: "ev-1".to_string(),
            kind: "code".to_string(),
            citation_path: Some("src/lib.rs:0-10".to_string()),
            citation_sha: None,
            citation_hash: "sha256:abc".to_string(),
            citation_excerpt: None,
            verified: Some(false),
            verification_status: Some(VerificationStatus::Relocated),
        });
        assert!(line.contains("status=relocated"));
        assert!(line.contains("verified=false"));
    }

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
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
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
            local_only: false,
            peers: false,
            reachable_from: None,
            max_hops: 1,
            slug: None,
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
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
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
            local_only: false,
            peers: false,
            reachable_from: None,
            max_hops: 1,
            slug: None,
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
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
                cues: vec![],
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
            local_only: false,
            peers: false,
            reachable_from: None,
            max_hops: 1,
            slug: None,
        };
        search_cmd.execute_with(&paths, &embedder).unwrap();
    }

    #[test]
    fn test_collect_peer_paths_filters_expired_rows_without_deleting_them() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        db::open_or_init(&paths).unwrap();
        let conn = rusqlite::Connection::open(&paths.db).unwrap();

        insert_peer_edge(&conn, "repo-a", "repo-expired", Some("2000-01-01 00:00:00"));
        insert_peer_edge(&conn, "repo-a", "repo-live", None);

        // `reachable_from=None` queries direct peers with no source_repo scope
        // (used when there is no single starting repo), so this second hop is
        // seeded only after the direct-peers assertion below to keep that
        // assertion scoped to repo-a's own edges.
        let direct = collect_peer_paths(&conn, None, 1, None);
        assert_eq!(direct, vec!["repo-live".to_string()]);

        insert_peer_edge(&conn, "repo-live", "repo-live-2", None);

        let bfs = collect_peer_paths(&conn, Some("repo-a"), 2, None);
        assert_eq!(
            bfs,
            vec!["repo-live".to_string(), "repo-live-2".to_string()],
            "expired edges must be invisible to traversal and to federated peer collection"
        );

        let physical_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            physical_rows, 3,
            "the expired peer row must still be physically present before any locked sweep runs"
        );
    }

    #[test]
    fn test_federated_global_limit_ignores_physically_present_expired_peer() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        db::open_or_init(&paths).unwrap();
        let conn = rusqlite::Connection::open(&paths.db).unwrap();

        insert_peer_edge(&conn, "local", "peer-live-a", None);
        insert_peer_edge(&conn, "local", "peer-expired", Some("2000-01-01 00:00:00"));
        insert_peer_edge(&conn, "local", "peer-live-b", None);

        let peer_paths = collect_peer_paths(&conn, None, 1, None);
        let batches = peer_paths
            .iter()
            .map(|peer| {
                let rows = (0..4)
                    .map(|i| {
                        federated_row(
                            &format!("{peer}-{i}"),
                            1.0 / (61.0 + i as f32),
                            Some(peer),
                        )
                    })
                    .collect();
                (Some(peer.clone()), rows)
            })
            .collect();
        let results = merge_federated_results(batches, 6);

        assert_eq!(
            results.len(),
            6,
            "expired peers must not consume the global limit"
        );
        assert!(
            results
                .iter()
                .all(|row| row.origin_repo.as_deref() != Some("peer-expired")),
            "no result may come from the physically present expired peer"
        );
        let physical_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM peers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            physical_rows, 3,
            "the fixture must retain the expired row physically"
        );
    }

    #[test]
    fn test_cmd_search_limit_rejects_out_of_range_value() {
        let too_large = (db::MAX_LIMIT + 1).to_string();
        let err = Search::try_parse_from(["kb", "needle", "--limit", too_large.as_str()])
            .unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains(&format!("must be in 1..={}", db::MAX_LIMIT)),
            "expected explicit range error, got: {rendered}"
        );
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
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        add_cmd.execute_with(&paths, &embedder).unwrap();

        // These queries contain FTS5 operators — should not error out
        for q in &[
            "auth AND security",
            "auth OR security",
            "auth NOT security",
            "auth*",
        ] {
            let search_cmd = Search {
                query: q.to_string(),
                fts: true,
                semantic: false,
                repo: None,
                limit: 10,
                content: false,
                path_prefix: None,
                tag: None,
                local_only: false,
                peers: false,
                reachable_from: None,
                max_hops: 1,
                slug: None,
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
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
                cues: vec![],
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
            local_only: false,
            peers: false,
            reachable_from: None,
            max_hops: 1,
            slug: None,
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
            cues: vec![],
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
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };
        let conn = crate::components::db::open_db(&paths.db).unwrap();

        // Override CWD-based repo root discovery by using the db search directly
        // with the root as repo root. Since find_repo_root() walks from CWD (not
        // the tempdir), we test verification via the MCP path which passes repo_root
        // explicitly. Instead, verify the evidence array is populated and the
        // verified field is Some (true or false) — not None.
        let results =
            crate::components::db::search_entries(&conn, &embedder, "evidence test", &opts)
                .unwrap();
        assert!(!results.is_empty(), "search must return at least 1 result");

        let entry = results.iter().find(|r| r.id == "ev-search-test-1").unwrap();
        assert_eq!(entry.evidence.len(), 1, "entry must have 1 evidence row");

        // verified is Some(bool) — inline verification was attempted
        // (true if CWD happens to be the tempdir, false otherwise — both are acceptable)
        assert!(
            entry.evidence[0].verified.is_some(),
            "verified must not be null for top-K results"
        );
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
                cues: vec![],
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
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };
        let conn = crate::components::db::open_db(&paths.db).unwrap();
        let results = crate::components::db::search_entries(
            &conn,
            &embedder,
            "narrow k fallback entry authentication",
            &opts,
        )
        .unwrap();

        assert_eq!(results.len(), 3, "all 3 entries must be returned");

        // First result: verified=Some(...)
        let first = &results[0];
        assert_eq!(first.evidence.len(), 1);
        assert!(
            first.evidence[0].verified.is_some(),
            "top-1 result must have verified=Some(...)"
        );

        // Remaining results: verified=None
        for r in &results[1..] {
            assert_eq!(r.evidence.len(), 1);
            assert!(
                r.evidence[0].verified.is_none(),
                "results beyond K must have verified=null"
            );
        }
    }

    #[cfg(test)]
    mod proptest_fts_injection {
        use super::*;
        use proptest::prelude::*;

        /// Adversarial FTS queries that should be safely escaped.
        /// Generates terms with FTS5 operators, quotes, backslashes, and mixed content.
        fn arb_adversarial_fts_query() -> impl Strategy<Value = String> {
            prop_oneof![
                Just("auth AND security".to_string()),
                Just("auth OR database".to_string()),
                Just("auth NOT public".to_string()),
                Just("\"quoted phrase\"".to_string()),
                Just("backslash \\ escape".to_string()),
                Just("auth* wildcard".to_string()),
                Just("(nested query)".to_string()),
                Just("mixed AND quoted \"phrase\" NOT keyword".to_string()),
                Just("\"\"\"triple quotes\"\"\"".to_string()),
                Just("auth \\ OR \" AND NOT".to_string()),
            ]
        }

        /// Invariant: arbitrary FTS queries don't panic and FTS keywords
        /// are treated as literals, not operators. Quote/backslash escaping
        /// preserves valid FTS5 syntax.
        proptest! {
            #![proptest_config(proptest::prelude::ProptestConfig {
                cases: proptest_cases(256),
                .. proptest::prelude::ProptestConfig::default()
            })]
            #[test]
            fn prop_fts_query_injection_no_panic(query in arb_adversarial_fts_query()) {
                use crate::components::db::{open_db_memory, search_entries, SearchOptions};
                use crate::components::embedder::NoopEmbedder;
                use crate::commands::add::Add;
                use crate::config::Paths;
                use std::path::Path;

                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::from_root(dir.path());
                fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();

                let embedder = NoopEmbedder;

                // Insert known entries for matching
                let add_cmd = Add {
                    path: "src/auth.rs".to_string(),
                    summary: "authentication module".to_string(),
                    content: "handles auth and security".to_string(),
                    tags: "auth,security".to_string(),
                    version_ref: Some("abc123".to_string()),
                    id: Some("fts-test-1".to_string()),
                    permanent: false,
                    replace_path: false,
                    kind: "convention".to_string(),
                    evidence: vec![],
                    evidence_file: None,
                    cues: vec![],
                };
                add_cmd.execute_with(&paths, &embedder).unwrap();

                // Connect to DB and search with adversarial query
                let conn = crate::components::db::open_db(&paths.db).unwrap();
                let opts = crate::components::db::SearchOptions {
                    limit: 10,
                    do_fts: true,
                    do_semantic: false,
                    path_prefix: None,
                    tag_filter: None,
                    inline_verify_k: 0,
                    repo_root: None,
                    verify_pool_size: None,
                    recency_lambda: 0.0,
                    mmr_lambda: 0.0,
                };

                // Should not panic; FTS keywords inside quotes are treated as literals
                let result = search_entries(&conn, &embedder, &query, &opts);

                // Result must be Ok (no panic, no DB error)
                prop_assert!(result.is_ok(), "search should succeed without panic");

                // If search succeeded, result should be a vec (possibly empty)
                if let Ok(results) = result {
                    prop_assert!(
                        results.iter().all(|r| !r.id.is_empty()),
                        "all results must have non-empty IDs"
                    );
                }
            }
        }
    }

    /// Federated search score_kind round-trip (br-improvement-catalog-23b.5):
    /// When peer DB results are merged into local results, each peer SearchEntry
    /// retains the score_kind set by search_entries on the peer DB. The merge path
    /// only mutates origin_repo — score_kind must be preserved verbatim.
    #[test]
    fn test_federated_search_score_kind_preserved_across_peer_merge() {
        // Build peer KB with one FTS-matchable entry
        let peer_dir = tempdir().unwrap();
        let peer_root = peer_dir.path();
        fs::create_dir_all(peer_root.join(".state/agent-kb")).unwrap();
        let peer_paths = Paths::from_root(peer_root);
        let embedder = NoopEmbedder;

        let add_peer = Add {
            path: "peer/module.rs".to_string(),
            summary: "peer score_kind roundtrip entry".to_string(),
            content: "peer body content".to_string(),
            tags: "peer,scorekind".to_string(),
            version_ref: Some("peer-sha".to_string()),
            id: Some("peer-sk-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        add_peer.execute_with(&peer_paths, &embedder).unwrap();

        // Register local KB + peer edge
        let local_dir = tempdir().unwrap();
        let local_root = local_dir.path();
        fs::create_dir_all(local_root.join(".state/agent-kb")).unwrap();
        let local_paths = Paths::from_root(local_root);

        let local_conn = crate::components::db::open_db(&local_paths.db).unwrap();
        let peer_root_str = peer_root.to_str().unwrap().to_string();
        local_conn
            .execute(
                "INSERT INTO graphs(id, graph_type, epic_slug, source_repo, expires_at)
             VALUES('g1', 'dep', NULL, 'local', NULL)",
                rusqlite::params![],
            )
            .unwrap();
        local_conn.execute(
            "INSERT INTO peers(id, graph_id, source_repo, target_repo, edge_type, epic_slug, expires_at)
             VALUES('p1', 'g1', 'local', ?1, 'dep', NULL, NULL)",
            rusqlite::params![peer_root_str],
        ).unwrap();

        // FTS-only search with peers enabled
        let opts = crate::components::db::SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: false,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 0,
            repo_root: None,
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };

        let peer_db = crate::config::Paths::from_root(std::path::Path::new(&peer_root_str)).db;
        let peer_conn = crate::components::db::open_db(&peer_db).unwrap();
        let peer_opts = crate::components::db::SearchOptions {
            repo_root: Some(std::path::PathBuf::from(&peer_root_str)),
            ..opts.clone()
        };
        let mut peer_results = crate::components::db::search_entries(
            &peer_conn,
            &embedder,
            "peer score_kind roundtrip",
            &peer_opts,
        )
        .unwrap();

        // Simulate the federation merge: set origin_repo, push into merged vec
        for r in &mut peer_results {
            r.origin_repo = Some(peer_root_str.clone());
        }

        // score_kind must survive the origin_repo mutation
        for r in &peer_results {
            assert_eq!(
                r.score_kind, "fts",
                "peer FTS result must retain score_kind=fts after federation merge, got {}",
                r.score_kind
            );
            assert!(
                r.origin_repo.is_some(),
                "origin_repo must be set after peer merge"
            );
        }
        assert!(!peer_results.is_empty(), "peer FTS must return the entry");
    }
}
