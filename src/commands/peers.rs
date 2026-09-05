//! `peers` subcommand — manage peer repo graph edges

use crate::commands::add::acquire_lock;
use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
use anyhow::Context;
use clap::Parser;
use rusqlite::params;
use serde_json::json;

// ---------------------------------------------------------------------------
// Top-level enum
// ---------------------------------------------------------------------------

/// Manage peer repo graph edges
#[derive(clap::Parser, Command, Debug, Runnable)]
pub enum Peers {
    /// Add a peer relationship to another repo
    Add(PeersAdd),
    /// List peer relationships for the local repo
    List(PeersList),
    /// Remove a peer relationship by ID
    Remove(PeersRemove),
    /// Show all peer relationships involving a given repo path
    Show(PeersShow),
    /// Bulk-import peer relationships from a JSON seed file (idempotent)
    Import(PeersImport),
    /// Manage directed peer edges (add/list/remove/cleanup-epic)
    #[command(subcommand)]
    Edge(PeersEdge),
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The source repo identity used to key peer graph edges.
///
/// Always `paths.root` -- the same layout-aware repository root the MCP
/// port uses for `handle_kb_peers_add` -- rather than a CWD-based
/// `config::git_repo_root()` git subprocess (which could disagree, e.g.
/// inside a managed `.state` git worktree) or an unconditional two-parent
/// walk from the db path (wrong for the legacy layout). One repository must
/// register under one identity regardless of whether it's addressed from
/// the CLI or the port.
fn detect_source_repo(paths: &config::Paths) -> String {
    paths.root.to_string_lossy().to_string()
}

fn maybe_sweep_expired_peers(conn: &rusqlite::Connection, did_mutate: bool) -> anyhow::Result<()> {
    if did_mutate {
        db::sweep_expired_peers(conn)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PeersAdd
// ---------------------------------------------------------------------------

/// Add a peer relationship to another repo
#[derive(Command, Debug, Parser)]
pub struct PeersAdd {
    /// Target repo path
    pub target_repo: String,

    /// Graph type: epic or dep
    #[arg(long = "type", name = "graph-type")]
    pub graph_type: String,

    /// Epic slug (optional)
    #[arg(long)]
    pub epic_slug: Option<String>,

    /// TTL in days (optional)
    #[arg(long)]
    pub ttl_days: Option<u32>,
}

impl Runnable for PeersAdd {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersAdd {
    pub fn execute(&self) -> anyhow::Result<()> {
        if self.graph_type != "epic" && self.graph_type != "dep" {
            anyhow::bail!("--type must be 'epic' or 'dep', got '{}'", self.graph_type);
        }

        let paths = config::Paths::discover()?;
        let lock = acquire_lock(&paths.lock)?;
        let conn = db::open_rw(&paths, &lock)?;
        let source_repo = detect_source_repo(&paths);
        let now = chrono::Utc::now().to_rfc3339();

        // Compute optional expires_at via SQLite datetime arithmetic.
        let expires_at: Option<String> = if let Some(days) = self.ttl_days {
            let val: String = conn.query_row(
                "SELECT datetime('now', ?1)",
                params![format!("+{days} days")],
                |r| r.get(0),
            )?;
            Some(val)
        } else {
            None
        };

        // Find or create a matching graph row.
        let graph_id: String = {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM graphs WHERE graph_type=?1 AND source_repo=?2 AND \
                     (epic_slug IS ?3 OR (epic_slug IS NULL AND ?3 IS NULL))",
                    params![self.graph_type, source_repo, self.epic_slug],
                    |r| r.get(0),
                )
                .optional()?;

            match existing {
                Some(id) => id,
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO graphs (id, graph_type, epic_slug, source_repo, \
                         created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            id,
                            self.graph_type,
                            self.epic_slug,
                            source_repo,
                            now,
                            expires_at,
                        ],
                    )?;
                    id
                }
            }
        };

        // Insert the peer edge.
        let peer_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO peers (id, graph_id, source_repo, target_repo, edge_type, \
             epic_slug, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, 'member', ?5, ?6, ?7)",
            params![
                peer_id,
                graph_id,
                source_repo,
                self.target_repo,
                self.epic_slug,
                now,
                expires_at,
            ],
        )?;
        maybe_sweep_expired_peers(&conn, true)?;

        println!("{peer_id}");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeersList
// ---------------------------------------------------------------------------

/// List peer relationships for the local repo
#[derive(Command, Debug, Parser)]
pub struct PeersList {
    /// Filter by graph type (epic or dep)
    #[arg(long = "type", name = "graph-type")]
    pub graph_type: Option<String>,
}

impl Runnable for PeersList {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersList {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let conn = db::open_ro(&paths.db)?;
        let source_repo = detect_source_repo(&paths);

        let rows = query_peers_for_repo(&conn, &source_repo, self.graph_type.as_deref())?;
        println!("{}", serde_json::to_string_pretty(&rows)?);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeersRemove
// ---------------------------------------------------------------------------

/// Remove a peer relationship by ID
#[derive(Command, Debug, Parser)]
pub struct PeersRemove {
    /// Peer edge ID to remove
    pub peer_id: String,
}

impl Runnable for PeersRemove {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersRemove {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let lock = acquire_lock(&paths.lock)?;
        let conn = db::open_rw(&paths, &lock)?;

        let deleted = conn.execute("DELETE FROM peers WHERE id=?1", params![self.peer_id])?;

        // Delete orphaned graphs (graphs with no remaining peer edges).
        conn.execute(
            "DELETE FROM graphs WHERE id NOT IN (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL)",
            [],
        )?;
        maybe_sweep_expired_peers(&conn, deleted > 0)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeersShow
// ---------------------------------------------------------------------------

/// Show all peer relationships involving a given repo path
#[derive(Command, Debug, Parser)]
pub struct PeersShow {
    /// Repo path to query (source or target)
    pub repo_path: String,
}

impl Runnable for PeersShow {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersShow {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let conn = db::open_ro(&paths.db)?;

        // Try as-is and canonicalized path.
        let canonical = std::fs::canonicalize(&self.repo_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| self.repo_path.clone());

        let mut rows = query_peers_by_either_repo(&conn, &self.repo_path)?;
        if canonical != self.repo_path {
            let mut canon_rows = query_peers_by_either_repo(&conn, &canonical)?;
            // Deduplicate by id.
            let existing_ids: std::collections::HashSet<String> = rows
                .iter()
                .filter_map(|v| v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                .collect();
            canon_rows.retain(|v| {
                v.get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| !existing_ids.contains(s))
                    .unwrap_or(true)
            });
            rows.extend(canon_rows);
        }

        println!("{}", serde_json::to_string_pretty(&rows)?);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shared query helpers
// ---------------------------------------------------------------------------

fn query_peers_for_repo(
    conn: &rusqlite::Connection,
    source_repo: &str,
    graph_type_filter: Option<&str>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let sql = format!(
        "SELECT p.id, p.source_repo, p.target_repo, \
         g.graph_type, p.epic_slug, p.created_at, p.expires_at \
         FROM peers p LEFT JOIN graphs g ON p.graph_id = g.id \
         WHERE p.source_repo = ?1 \
         AND {} \
         AND (?2 IS NULL OR g.graph_type = ?2)",
        db::live_peer_predicate("p"),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![source_repo, graph_type_filter], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, src, tgt, gtype, slug, created, expires) = row?;
        out.push(json!({
            "id": id,
            "source_repo": src,
            "target_repo": tgt,
            "graph_type": gtype,
            "epic_slug": slug,
            "created_at": created,
            "expires_at": expires,
        }));
    }
    Ok(out)
}

fn query_peers_by_either_repo(
    conn: &rusqlite::Connection,
    repo_path: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let sql = format!(
        "SELECT p.id, p.source_repo, p.target_repo, \
         g.graph_type, p.epic_slug, p.created_at, p.expires_at \
         FROM peers p LEFT JOIN graphs g ON p.graph_id = g.id \
         WHERE (p.source_repo = ?1 OR p.target_repo = ?1) \
         AND {}",
        db::live_peer_predicate("p"),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![repo_path], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, src, tgt, gtype, slug, created, expires) = row?;
        out.push(json!({
            "id": id,
            "source_repo": src,
            "target_repo": tgt,
            "graph_type": gtype,
            "epic_slug": slug,
            "created_at": created,
            "expires_at": expires,
        }));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// PeersImport
// ---------------------------------------------------------------------------

/// Bulk-import peer relationships from a JSON seed file (idempotent)
#[derive(Command, Debug, Parser)]
pub struct PeersImport {
    /// Path to JSON seeds file: array of {source_repo, target_repo, graph_type, epic_slug?, ttl_days?}
    pub seeds_file: String,
}

impl Runnable for PeersImport {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

#[derive(serde::Deserialize)]
struct PeerSeedEntry {
    source_repo: String,
    target_repo: String,
    graph_type: String,
    epic_slug: Option<String>,
    ttl_days: Option<u32>,
}

impl PeersImport {
    pub fn execute(&self) -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        use std::fs;

        let file_bytes = fs::read(&self.seeds_file)
            .with_context(|| format!("read seeds file: {}", self.seeds_file))?;

        // Stamp check — skip entirely if this exact file content was already imported.
        let mut hasher = Sha256::new();
        hasher.update(&file_bytes);
        let hash_hex = format!("{:x}", hasher.finalize());

        let paths = config::Paths::discover()?;
        let stamp_path = paths
            .db
            .parent()
            .map(|p| p.join(format!("peers-import-{hash_hex}.stamp")))
            .ok_or_else(|| anyhow::anyhow!("cannot derive stamp path from db path"))?;

        if stamp_path.exists() {
            println!("0");
            return Ok(());
        }

        let entries: Vec<PeerSeedEntry> = serde_json::from_slice(&file_bytes)
            .with_context(|| format!("parse seeds file: {}", self.seeds_file))?;

        let lock = acquire_lock(&paths.lock)?;
        let conn = db::open_rw(&paths, &lock)?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut added = 0usize;

        for entry in &entries {
            if entry.graph_type != "epic" && entry.graph_type != "dep" {
                anyhow::bail!("--type must be 'epic' or 'dep', got '{}'", entry.graph_type);
            }

            // Waiver site 6: duplicate suppression must see expired-but-
            // present rows too because the peers table has no UNIQUE constraint.
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM peers WHERE source_repo=?1 AND target_repo=?2 \
                     AND edge_type='member' \
                     AND (epic_slug IS ?3 OR (epic_slug IS NULL AND ?3 IS NULL))",
                    params![entry.source_repo, entry.target_repo, entry.epic_slug],
                    |r| r.get(0),
                )
                .optional()?;
            if existing.is_some() {
                continue;
            }

            // Compute optional expires_at.
            let expires_at: Option<String> = if let Some(days) = entry.ttl_days {
                let val: String = conn.query_row(
                    "SELECT datetime('now', ?1)",
                    params![format!("+{days} days")],
                    |r| r.get(0),
                )?;
                Some(val)
            } else {
                None
            };

            // Find or create a matching graph row.
            let graph_id: String = {
                let existing_graph: Option<String> = conn
                    .query_row(
                        "SELECT id FROM graphs WHERE graph_type=?1 AND source_repo=?2 AND \
                         (epic_slug IS ?3 OR (epic_slug IS NULL AND ?3 IS NULL))",
                        params![entry.graph_type, entry.source_repo, entry.epic_slug],
                        |r| r.get(0),
                    )
                    .optional()?;

                match existing_graph {
                    Some(id) => id,
                    None => {
                        let id = uuid::Uuid::new_v4().to_string();
                        conn.execute(
                            "INSERT INTO graphs (id, graph_type, epic_slug, source_repo, \
                             created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                id,
                                entry.graph_type,
                                entry.epic_slug,
                                entry.source_repo,
                                now,
                                expires_at,
                            ],
                        )?;
                        id
                    }
                }
            };

            let peer_id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO peers (id, graph_id, source_repo, target_repo, edge_type, \
                 epic_slug, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, 'member', ?5, ?6, ?7)",
                params![
                    peer_id,
                    graph_id,
                    entry.source_repo,
                    entry.target_repo,
                    entry.epic_slug,
                    now,
                    expires_at,
                ],
            )?;
            added += 1;
        }
        maybe_sweep_expired_peers(&conn, added > 0)?;

        // Write stamp file so re-runs are skipped.
        fs::write(&stamp_path, "")?;

        println!("{added}");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeersEdge (sub-command router)
// ---------------------------------------------------------------------------

/// Manage directed peer edges (add/list/remove/cleanup-epic)
#[derive(clap::Parser, Command, Debug, Runnable)]
pub enum PeersEdge {
    /// Add a directed edge between two repos
    Add(PeersEdgeAdd),
    /// List edges (optionally filtered by epic slug)
    List(PeersEdgeList),
    /// Remove an edge by ID (idempotent)
    Remove(PeersEdgeRemove),
    /// Delete all edges for an epic slug and orphan-clean graphs
    CleanupEpic(PeersEdgeCleanupEpic),
}

// ---------------------------------------------------------------------------
// PeersEdgeAdd
// ---------------------------------------------------------------------------

/// Add a directed edge between two repos
#[derive(Command, Debug, Parser)]
pub struct PeersEdgeAdd {
    /// Source repo path
    pub source_repo: String,

    /// Target repo path
    pub target_repo: String,

    /// Edge type (e.g. epic or dep)
    #[arg(long = "type", name = "edge-type")]
    pub edge_type: String,

    /// Epic slug (optional)
    #[arg(long)]
    pub epic_slug: Option<String>,

    /// TTL in days (optional)
    #[arg(long)]
    pub ttl_days: Option<u32>,
}

impl Runnable for PeersEdgeAdd {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersEdgeAdd {
    pub fn execute(&self) -> anyhow::Result<()> {
        if self.edge_type != "epic" && self.edge_type != "dep" {
            anyhow::bail!("--type must be 'epic' or 'dep', got '{}'", self.edge_type);
        }

        let paths = config::Paths::discover()?;
        let lock = acquire_lock(&paths.lock)?;
        let conn = db::open_rw(&paths, &lock)?;
        let now = chrono::Utc::now().to_rfc3339();

        let expires_at: Option<String> = if let Some(days) = self.ttl_days {
            let val: String = conn.query_row(
                "SELECT datetime('now', ?1)",
                params![format!("+{days} days")],
                |r| r.get(0),
            )?;
            Some(val)
        } else {
            None
        };

        // Find or create a matching graph row.
        let graph_id: String = {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT id FROM graphs WHERE graph_type=?1 AND source_repo=?2 AND \
                     (epic_slug IS ?3 OR (epic_slug IS NULL AND ?3 IS NULL))",
                    params![self.edge_type, self.source_repo, self.epic_slug],
                    |r| r.get(0),
                )
                .optional()?;

            match existing {
                Some(id) => id,
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO graphs (id, graph_type, epic_slug, source_repo, \
                         created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            id,
                            self.edge_type,
                            self.epic_slug,
                            self.source_repo,
                            now,
                            expires_at,
                        ],
                    )?;
                    id
                }
            }
        };

        let peer_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO peers (id, graph_id, source_repo, target_repo, edge_type, \
             epic_slug, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                peer_id,
                graph_id,
                self.source_repo,
                self.target_repo,
                self.edge_type,
                self.epic_slug,
                now,
                expires_at,
            ],
        )?;
        maybe_sweep_expired_peers(&conn, true)?;

        println!("{peer_id}");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeersEdgeList
// ---------------------------------------------------------------------------

/// List edges (optionally filtered by epic slug)
#[derive(Command, Debug, Parser)]
pub struct PeersEdgeList {
    /// Filter by epic slug (optional)
    #[arg(long)]
    pub epic_slug: Option<String>,
}

impl Runnable for PeersEdgeList {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersEdgeList {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let conn = db::open_ro(&paths.db)?;
        let out = query_peer_edges(&conn, self.epic_slug.as_deref())?;

        println!("{}", serde_json::to_string_pretty(&out)?);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeersEdgeRemove
// ---------------------------------------------------------------------------

/// Remove an edge by ID (idempotent)
#[derive(Command, Debug, Parser)]
pub struct PeersEdgeRemove {
    /// Edge ID to remove
    pub edge_id: String,
}

impl Runnable for PeersEdgeRemove {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersEdgeRemove {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let lock = acquire_lock(&paths.lock)?;
        let conn = db::open_rw(&paths, &lock)?;

        let deleted = conn.execute("DELETE FROM peers WHERE id=?1", params![self.edge_id])?;

        // Delete orphaned graphs (graphs with no remaining peer edges).
        conn.execute(
            "DELETE FROM graphs WHERE id NOT IN (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL)",
            [],
        )?;
        maybe_sweep_expired_peers(&conn, deleted > 0)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeersEdgeCleanupEpic
// ---------------------------------------------------------------------------

/// Delete all edges for an epic slug and orphan-clean graphs
#[derive(Command, Debug, Parser)]
pub struct PeersEdgeCleanupEpic {
    /// Epic slug whose edges should be deleted
    pub slug: String,
}

impl Runnable for PeersEdgeCleanupEpic {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl PeersEdgeCleanupEpic {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let lock = acquire_lock(&paths.lock)?;
        let conn = db::open_rw(&paths, &lock)?;

        let deleted = conn.execute("DELETE FROM peers WHERE epic_slug = ?1", params![self.slug])?;

        // Delete orphaned graphs (graphs with no remaining peer edges).
        conn.execute(
            "DELETE FROM graphs WHERE id NOT IN (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL)",
            [],
        )?;
        maybe_sweep_expired_peers(&conn, deleted > 0)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Optional helper trait import (rusqlite)
// ---------------------------------------------------------------------------

use rusqlite::OptionalExtension;

fn query_peer_edges(
    conn: &rusqlite::Connection,
    epic_slug: Option<&str>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let sql = format!(
        "SELECT p.id, p.source_repo, p.target_repo, \
         g.graph_type, p.epic_slug, p.created_at, p.expires_at \
         FROM peers p LEFT JOIN graphs g ON p.graph_id = g.id \
         WHERE {} AND (?1 IS NULL OR p.epic_slug = ?1)",
        db::live_peer_predicate("p"),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![epic_slug], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, src, tgt, gtype, slug, created, expires) = row?;
        out.push(json!({
            "id": id,
            "source_repo": src,
            "target_repo": tgt,
            "graph_type": gtype,
            "epic_slug": slug,
            "created_at": created,
            "expires_at": expires,
        }));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::db;
    use crate::config::Paths;
    use rusqlite::params;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command as Cmd;
    use tempfile::tempdir;

    struct CwdGuard(PathBuf);

    impl CwdGuard {
        fn set(dir: &std::path::Path) -> Self {
            let orig = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self(orig)
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    fn setup_repo() -> (tempfile::TempDir, Paths) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "T"],
        ] {
            Cmd::new("git")
                .args(&args)
                .current_dir(root)
                .output()
                .unwrap();
        }
        fs::write(root.join("README"), "init").unwrap();
        Cmd::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Cmd::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        (dir, paths)
    }

    // Helper: insert a graph + peer edge directly into an in-memory DB.
    fn insert_peer(
        conn: &rusqlite::Connection,
        source: &str,
        target: &str,
        graph_type: &str,
        epic_slug: Option<&str>,
        expires_at: Option<&str>,
    ) -> String {
        let graph_id = uuid::Uuid::new_v4().to_string();
        let now = "2024-01-01T00:00:00Z";
        conn.execute(
            "INSERT INTO graphs (id, graph_type, epic_slug, source_repo, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![graph_id, graph_type, epic_slug, source, now, expires_at],
        )
        .unwrap();

        let peer_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO peers (id, graph_id, source_repo, target_repo, edge_type, \
             epic_slug, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, 'member', ?5, ?6, ?7)",
            params![peer_id, graph_id, source, target, epic_slug, now, expires_at],
        )
        .unwrap();

        peer_id
    }

    fn insert_peer_row(
        conn: &rusqlite::Connection,
        source: &str,
        target: &str,
        graph_type: &str,
        epic_slug: Option<&str>,
        expires_at: Option<&str>,
    ) -> String {
        insert_peer(conn, source, target, graph_type, epic_slug, expires_at)
    }

    #[test]
    fn test_peers_add_list_remove_roundtrip() {
        let conn = db::open_db_memory().unwrap();

        // Insert peer edge: source="r1", target="r2", graph_type="epic", epic_slug=Some("s1"), no TTL
        let peer_id = insert_peer(&conn, "r1", "r2", "epic", Some("s1"), None);

        // SELECT from peers WHERE source_repo="r1" -> 1 row
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE source_repo = ?1",
                params!["r1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "should find 1 peer edge for source r1");

        // DELETE that row
        conn.execute("DELETE FROM peers WHERE id = ?1", params![peer_id])
            .unwrap();

        // SELECT again -> 0 rows
        let count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE source_repo = ?1",
                params!["r1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count_after, 0, "peer edge must be gone after delete");

        // No phantom rows: total peers count = 0
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 0, "peers table must be empty after delete");
    }

    #[test]
    fn test_cleanup_epic_correctness() {
        let conn = db::open_db_memory().unwrap();

        // Insert 2 edges with epic_slug="s1", 1 with epic_slug="s2"
        insert_peer(&conn, "r1", "r2", "epic", Some("s1"), None);
        insert_peer(&conn, "r1", "r3", "epic", Some("s1"), None);
        insert_peer(&conn, "r1", "r4", "epic", Some("s2"), None);

        // Delete all s1 edges
        conn.execute("DELETE FROM peers WHERE epic_slug = ?1", params!["s1"])
            .unwrap();

        let s1_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE epic_slug = ?1",
                params!["s1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(s1_count, 0, "all s1 edges must be deleted");

        let s2_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE epic_slug = ?1",
                params!["s2"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(s2_count, 1, "s2 edge must survive s1 cleanup");
    }

    #[test]
    fn test_peers_read_helpers_filter_expired_rows_without_deleting_them() {
        let (_dir, paths) = setup_repo();
        db::open_or_init(&paths).unwrap();
        let root = paths
            .db
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let canonical_root = std::fs::canonicalize(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let target_repo = "/tmp/peer-live";
        let conn = db::open_unchecked_for_test(&paths.db).unwrap();

        insert_peer_row(
            &conn,
            &canonical_root,
            "/tmp/peer-expired",
            "epic",
            Some("ttl"),
            Some("2000-01-01 00:00:00"),
        );
        insert_peer_row(
            &conn,
            &canonical_root,
            target_repo,
            "epic",
            Some("ttl"),
            None,
        );

        let list_rows = query_peers_for_repo(&conn, &canonical_root, None).unwrap();
        assert_eq!(list_rows.len(), 1, "list must hide the expired peer");
        assert_eq!(list_rows[0]["target_repo"], target_repo);

        let show_rows = query_peers_by_either_repo(&conn, &canonical_root).unwrap();
        assert_eq!(show_rows.len(), 1, "show must hide the expired peer");
        assert_eq!(show_rows[0]["target_repo"], target_repo);

        let edge_rows = query_peer_edges(&conn, None).unwrap();
        assert_eq!(edge_rows.len(), 1, "edge-list must hide the expired peer");
        assert_eq!(edge_rows[0]["target_repo"], target_repo);

        let physical_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            physical_rows, 2,
            "the expired peer must remain physically present until a locked sweep deletes it"
        );
    }

    #[test]
    fn test_peers_import_skips_expired_but_present_duplicate() {
        let (_dir, paths) = setup_repo();
        let root = paths
            .db
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let canonical_root = std::fs::canonicalize(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let _cwd = CwdGuard::set(root);
        db::open_or_init(&paths).unwrap();
        let conn = db::open_unchecked_for_test(&paths.db).unwrap();

        insert_peer_row(
            &conn,
            &canonical_root,
            "/tmp/existing-peer",
            "dep",
            Some("seed-slug"),
            Some("2000-01-01 00:00:00"),
        );
        drop(conn);

        let seeds_file = paths
            .db
            .parent()
            .unwrap()
            .join("expired-duplicate-seeds.json");
        fs::write(
            &seeds_file,
            serde_json::to_vec(&serde_json::json!([{
                "source_repo": canonical_root,
                "target_repo": "/tmp/existing-peer",
                "graph_type": "dep",
                "epic_slug": "seed-slug"
            }]))
            .unwrap(),
        )
        .unwrap();

        PeersImport {
            seeds_file: seeds_file.to_string_lossy().to_string(),
        }
        .execute()
        .unwrap();

        let conn = db::open_unchecked_for_test(&paths.db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE target_repo='/tmp/existing-peer'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "duplicate suppression must treat an expired-but-present peer as already existing"
        );
    }

    #[test]
    fn test_ttl_sweep_idempotency() {
        let conn = db::open_db_memory().unwrap();

        // Insert edge with expires_at in the past
        insert_peer(
            &conn,
            "r1",
            "r2",
            "epic",
            Some("s1"),
            Some("2020-01-01T00:00:00Z"),
        );

        // Insert edge with no expiry (NULL)
        insert_peer(&conn, "r1", "r3", "dep", None, None);

        // First sweep: expired edge gone, NULL-expiry edge present
        db::sweep_expired_peers(&conn).unwrap();

        let expired_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE target_repo = ?1",
                params!["r2"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(expired_count, 0, "expired peer edge must be swept");

        let null_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE target_repo = ?1",
                params!["r3"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_count, 1, "NULL-expiry edge must survive sweep");

        // Second sweep: NULL-expiry edge still present, count unchanged
        db::sweep_expired_peers(&conn).unwrap();

        let null_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE target_repo = ?1",
                params!["r3"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            null_count_after, 1,
            "second sweep must not remove NULL-expiry edge"
        );

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "only 1 edge must remain after two sweeps");
    }

    #[test]
    fn test_cleanup_epic_removes_all_slug_edges() {
        let conn = db::open_db_memory().unwrap();

        // Insert 5 edges all with epic_slug="test_slug"
        for i in 0..5 {
            insert_peer(
                &conn,
                &format!("src-{i}"),
                &format!("tgt-{i}"),
                "epic",
                Some("test_slug"),
                None,
            );
        }

        // Cleanup: delete all edges for epic_slug + orphan-clean graphs
        conn.execute(
            "DELETE FROM peers WHERE epic_slug = ?1",
            params!["test_slug"],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM graphs WHERE id NOT IN \
             (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL)",
            [],
        )
        .unwrap();

        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE epic_slug = ?1",
                params!["test_slug"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "all test_slug edges must be removed");

        // Graphs table should also be empty after orphan cleanup
        let graph_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM graphs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(graph_count, 0, "orphaned graphs must be removed");
    }

    #[test]
    fn test_traversal_terminates_on_cycle() {
        use std::collections::{HashMap, HashSet, VecDeque};

        // Build adjacency list with cycle: r1->r2, r2->r3, r3->r1
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        adj.entry("r1").or_default().push("r2");
        adj.entry("r2").or_default().push("r3");
        adj.entry("r3").or_default().push("r1");

        // BFS with visited set — must terminate
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        queue.push_back("r1");

        while let Some(node) = queue.pop_front() {
            if visited.contains(node) {
                continue;
            }
            visited.insert(node);
            if let Some(neighbors) = adj.get(node) {
                for &n in neighbors {
                    queue.push_back(n);
                }
            }
        }

        // All 3 nodes reachable, traversal terminated (no infinite loop)
        assert_eq!(
            visited.len(),
            3,
            "BFS must visit all 3 nodes in cycle graph"
        );
        assert!(visited.contains("r1"));
        assert!(visited.contains("r2"));
        assert!(visited.contains("r3"));
    }

    // -----------------------------------------------------------------------
    // Proptest suite
    // -----------------------------------------------------------------------

    proptest::proptest! {
        /// Roundtrip: insert a peer edge, verify it exists, delete it, verify
        /// the peers and graphs tables are both empty (no phantom rows).
        #[test]
        fn proptest_peer_add_list_remove_roundtrip(
            target in "[a-z]{3,8}",
            graph_type in proptest::sample::select(vec!["epic", "dep"]),
        ) {
            use proptest::prelude::*;
            let conn = db::open_db_memory().unwrap();
            let peer_id = insert_peer(&conn, "source", &target, graph_type, None, None);

            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM peers WHERE id = ?1",
                    params![peer_id],
                    |r| r.get(0),
                )
                .unwrap();
            prop_assert_eq!(count, 1, "peer must exist after insert");

            conn.execute("DELETE FROM peers WHERE id = ?1", params![peer_id])
                .unwrap();
            // Orphan-clean graphs
            conn.execute(
                "DELETE FROM graphs WHERE id NOT IN \
                 (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL)",
                [],
            )
            .unwrap();

            let count_after: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM peers WHERE id = ?1",
                    params![peer_id],
                    |r| r.get(0),
                )
                .unwrap();
            prop_assert_eq!(count_after, 0, "peer must be gone after delete");

            let graph_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM graphs", [], |r| r.get(0))
                .unwrap();
            prop_assert_eq!(graph_count, 0, "orphaned graph must be removed");
        }

        /// Cycle detection: inserting a cycle of n edges and running
        /// sweep_expired_peers() terminates and leaves all non-expired edges intact.
        #[test]
        fn proptest_cycle_detection_sweep_terminates(
            n in 2usize..5usize,
        ) {
            use proptest::prelude::*;
            let conn = db::open_db_memory().unwrap();

            // Build a cycle: r0->r1->r2->...->r(n-1)->r0 (no expires_at)
            for i in 0..n {
                let src = format!("repo-{}", i);
                let tgt = format!("repo-{}", (i + 1) % n);
                insert_peer(&conn, &src, &tgt, "dep", None, None);
            }

            let before: i64 = conn
                .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
                .unwrap();
            prop_assert_eq!(before, n as i64, "all cycle edges must be present before sweep");

            // sweep_expired_peers must terminate (no infinite loop on cyclic graph)
            db::sweep_expired_peers(&conn).unwrap();

            let after: i64 = conn
                .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
                .unwrap();
            prop_assert_eq!(after, n as i64, "non-expired cycle edges must survive sweep");
        }

        /// TTL sweep idempotency: sweeping twice yields the same count as sweeping once.
        /// Expired edges are removed on first sweep; live edges survive both sweeps.
        #[test]
        fn proptest_ttl_sweep_idempotency(
            n_expired in 0usize..4usize,
            n_live in 0usize..4usize,
        ) {
            use proptest::prelude::*;
            let conn = db::open_db_memory().unwrap();

            for i in 0..n_expired {
                insert_peer(
                    &conn,
                    &format!("exp-src-{}", i),
                    &format!("exp-tgt-{}", i),
                    "dep",
                    None,
                    Some("2020-01-01T00:00:00Z"),
                );
            }
            for i in 0..n_live {
                insert_peer(
                    &conn,
                    &format!("live-src-{}", i),
                    &format!("live-tgt-{}", i),
                    "epic",
                    None,
                    None,
                );
            }

            db::sweep_expired_peers(&conn).unwrap();
            let count_after_first: i64 = conn
                .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
                .unwrap();

            db::sweep_expired_peers(&conn).unwrap();
            let count_after_second: i64 = conn
                .query_row("SELECT COUNT(*) FROM peers", [], |r| r.get(0))
                .unwrap();

            prop_assert_eq!(count_after_first, count_after_second, "sweep is idempotent");
            prop_assert_eq!(count_after_first, n_live as i64, "only live edges survive");
        }

        /// Cleanup-epic: after deleting all edges for a slug, zero rows remain for
        /// that slug and edges for other slugs are unaffected.
        #[test]
        fn proptest_cleanup_epic_removes_all_slug_edges(
            slug in "[a-z]{3,8}",
            n_slug in 1usize..5usize,
            n_other in 0usize..3usize,
        ) {
            use proptest::prelude::*;
            let conn = db::open_db_memory().unwrap();

            for i in 0..n_slug {
                insert_peer(
                    &conn,
                    &format!("src-{}", i),
                    &format!("tgt-{}", i),
                    "epic",
                    Some(&slug),
                    None,
                );
            }
            for i in 0..n_other {
                insert_peer(
                    &conn,
                    &format!("other-src-{}", i),
                    &format!("other-tgt-{}", i),
                    "epic",
                    Some("other"),
                    None,
                );
            }

            conn.execute(
                "DELETE FROM peers WHERE epic_slug = ?1",
                params![slug],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM graphs WHERE id NOT IN \
                 (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL)",
                [],
            )
            .unwrap();

            let slug_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM peers WHERE epic_slug = ?1",
                    params![slug],
                    |r| r.get(0),
                )
                .unwrap();
            prop_assert_eq!(slug_count, 0, "all slug edges must be removed");

            let other_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM peers WHERE epic_slug = 'other'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            prop_assert_eq!(other_count, n_other as i64, "other slug edges must survive");
        }
    }
}
