//! `peers` subcommand — manage peer repo graph edges

use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Detect the source repo path from the DB path or fall back to cwd.
///
/// Convention: db lives at `<repo>/.state/agent-kb/agent-kb.db` (or via the
/// `agent-kb/` symlink). Walking two parents from the db file therefore gives
/// the repo root. We prefer the git-reported toplevel when available so the
/// path is canonical.
fn detect_source_repo(db_path: &std::path::Path) -> String {
    // Try git first — most reliable canonical path.
    if let Some(root) = config::git_repo_root() {
        return root.to_string_lossy().to_string();
    }
    // Fall back to walking up from the DB path.
    db_path
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
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
        let conn = db::open_db(&paths.db)?;
        let source_repo = detect_source_repo(&paths.db);
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
        let conn = db::open_db(&paths.db)?;
        let source_repo = detect_source_repo(&paths.db);

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
        let conn = db::open_db(&paths.db)?;

        conn.execute("DELETE FROM peers WHERE id=?1", params![self.peer_id])?;

        // Delete orphaned graphs (graphs with no remaining peer edges).
        conn.execute(
            "DELETE FROM graphs WHERE id NOT IN (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL)",
            [],
        )?;

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
        let conn = db::open_db(&paths.db)?;

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
    let sql = "SELECT p.id, p.source_repo, p.target_repo, \
               g.graph_type, p.epic_slug, p.created_at, p.expires_at \
               FROM peers p LEFT JOIN graphs g ON p.graph_id = g.id \
               WHERE p.source_repo = ?1 \
               AND (?2 IS NULL OR g.graph_type = ?2)";

    let mut stmt = conn.prepare(sql)?;
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
    let sql = "SELECT p.id, p.source_repo, p.target_repo, \
               g.graph_type, p.epic_slug, p.created_at, p.expires_at \
               FROM peers p LEFT JOIN graphs g ON p.graph_id = g.id \
               WHERE p.source_repo = ?1 OR p.target_repo = ?1";

    let mut stmt = conn.prepare(sql)?;
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
// Optional helper trait import (rusqlite)
// ---------------------------------------------------------------------------

use rusqlite::OptionalExtension;
