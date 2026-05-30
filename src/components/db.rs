//! Database operations

use crate::components::embedder::Embedder;
use crate::models::{blob_to_f32s, cosine_similarity, f32s_to_blob};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;

/// Compute the `evidence_status` value for an entry based on its kind and
/// the number of evidence rows currently linked to it.
///
/// Rules (L2 soft-mandate):
/// - kind IN ('observation','belief','procedure') AND evidence_count > 0 → 'present'
/// - kind IN ('observation','belief','procedure') AND evidence_count = 0 → 'missing'
/// - all other kinds (convention, memory, or unknown) → 'n/a'
/// - Legacy entries that have never had an explicit kind event retain their
///   column DEFAULT ('belief'), so they will compute 'missing' once evidence
///   processing begins.  Callers that want to preserve 'n/a' for truly legacy
///   rows should check the current status before calling this helper.
pub fn compute_evidence_status(conn: &Connection, entry_id: &str) -> Result<String> {
    let kind: String = conn
        .query_row(
            "SELECT COALESCE(kind, 'belief') FROM entries WHERE id=?1",
            params![entry_id],
            |r| r.get(0),
        )
        .with_context(|| format!("compute_evidence_status: entry not found: {entry_id}"))?;

    let evidence_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM evidence WHERE entry_id=?1",
        params![entry_id],
        |r| r.get(0),
    )?;

    let status = match kind.as_str() {
        "observation" | "belief" | "procedure" => {
            if evidence_count > 0 {
                "present"
            } else {
                "missing"
            }
        }
        _ => "n/a",
    };
    Ok(status.to_string())
}

/// Open (or create) the SQLite database at the given path.
pub fn open_db(db_path: &Path) -> Result<Connection> {
    if let Some(p) = db_path.parent() {
        fs::create_dir_all(p)?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("open DB {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// Open an in-memory database with the full schema.
pub fn open_db_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// Create all required tables if they don't exist.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS entries (
            id          TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            summary     TEXT NOT NULL CHECK(LENGTH(summary) <= 200),
            content     TEXT NOT NULL CHECK(LENGTH(content) <= 10000),
            tags        TEXT NOT NULL,
            version_ref TEXT,
            is_stale    INTEGER DEFAULT 0,
            permanent   INTEGER DEFAULT 0,
            created_at  TEXT DEFAULT (datetime('now')),
            updated_at  TEXT DEFAULT (datetime('now'))
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
            id UNINDEXED, path, summary, content, tags
        );

        CREATE TABLE IF NOT EXISTS entries_emb (
            rowid    INTEGER PRIMARY KEY,
            embedding BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS test_cases (
            id          TEXT PRIMARY KEY,
            app         TEXT NOT NULL,
            name        TEXT NOT NULL,
            protocol    TEXT NOT NULL,
            config      TEXT NOT NULL,
            version_ref TEXT,
            is_stale    INTEGER DEFAULT 0,
            created_at  TEXT DEFAULT (datetime('now')),
            updated_at  TEXT DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS run_history (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            test_id  TEXT NOT NULL REFERENCES test_cases(id),
            result   TEXT NOT NULL CHECK(result IN ('pass','fail')),
            adapter  TEXT,
            detail   TEXT,
            ts       TEXT DEFAULT (datetime('now')),
            run_id   TEXT
        );

        -- T3 (br-yyb.4): index supporting the fast-path exact-match lookup in
        -- stale-check.  `path = ?` queries use this index; the substring
        -- fallback (`path LIKE '%x%'` and `? LIKE '%' || path`) still scans.
        -- Additive: `IF NOT EXISTS` keeps existing DBs working without a
        -- schema_version bump.
        CREATE INDEX IF NOT EXISTS idx_entries_path ON entries(path);
        "#,
    )?;
    // Migration: add `permanent` column to existing DBs that pre-date this field.
    // SQLite does not support `ADD COLUMN IF NOT EXISTS` before 3.37; ignore "duplicate column" error.
    let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN permanent INTEGER DEFAULT 0;");
    // Migration: add `kind` and `evidence_status` columns (Phase 1 defensibility).
    // Legacy entries default to kind='belief', evidence_status='n/a' via column DEFAULT.
    let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN kind TEXT DEFAULT 'belief';");
    let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN evidence_status TEXT DEFAULT 'n/a';");
    // New tables for evidence and audit runs (additive; no-op on already-migrated DBs).
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS evidence (
            id               TEXT PRIMARY KEY,
            entry_id         TEXT NOT NULL,
            kind             TEXT NOT NULL CHECK(kind IN ('code','test','command','user','derived')),
            citation_path    TEXT,
            citation_sha     TEXT,
            citation_hash    TEXT NOT NULL,
            citation_excerpt TEXT,
            derived_from     TEXT,
            recorded_at      TEXT DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_evidence_entry_id ON evidence(entry_id);

        CREATE TABLE IF NOT EXISTS audit_runs (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id     TEXT NOT NULL,
            audited_at   TEXT DEFAULT (datetime('now')),
            verdict      TEXT NOT NULL CHECK(verdict IN ('true','false')),
            evidence_ref TEXT
        );
        "#,
    )?;
    Ok(())
}

/// Apply a single event to the database.
pub fn apply_event(
    conn: &Connection,
    embedder: &dyn Embedder,
    event: &serde_json::Value,
) -> Result<()> {
    let action = event["action"].as_str().unwrap_or("");
    let table = event["table"].as_str().unwrap_or("");

    match (action, table) {
        ("upsert", "entries") => {
            let id = event["id"].as_str().context("missing id")?;
            let path = event["path"].as_str().context("missing path")?;
            let summary = event["summary"].as_str().context("missing summary")?;
            let content = event["content"].as_str().context("missing content")?;
            let tags = event["tags"].to_string();
            let version_ref = event["version_ref"].as_str();
            let ts = event["ts"].as_str().unwrap_or("");
            let permanent = event["permanent"].as_bool().unwrap_or(false) as i32;
            let is_stale = event["is_stale"].as_bool().unwrap_or(false) as i32;
            // Legacy events without kind/evidence_status fields default to
            // 'belief' / 'n/a' — matching the column DEFAULT for pre-migration rows.
            let kind = event["kind"].as_str().unwrap_or("belief");
            let evidence_status = event["evidence_status"].as_str().unwrap_or("n/a");

            conn.execute(
                "INSERT INTO entries(id, path, summary, content, tags, version_ref, permanent, is_stale, kind, evidence_status, created_at, updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)
                 ON CONFLICT(id) DO UPDATE SET
                   path=excluded.path, summary=excluded.summary,
                   content=excluded.content, tags=excluded.tags,
                   version_ref=excluded.version_ref,
                   permanent=excluded.permanent,
                   is_stale=excluded.is_stale,
                   kind=excluded.kind,
                   evidence_status=excluded.evidence_status,
                   updated_at=excluded.updated_at",
                params![id, path, summary, content, tags, version_ref, permanent, is_stale, kind, evidence_status, ts],
            )?;

            // Stale entries: clean up FTS/embeddings so they don't appear in search
            if is_stale == 1 {
                conn.execute("DELETE FROM entries_fts WHERE id=?1", params![id])?;
                return Ok(());
            }

            let rowid: i64 = conn.query_row(
                "SELECT rowid FROM entries WHERE id=?1",
                params![id],
                |r| r.get(0),
            )?;

            // Sync FTS5
            conn.execute("DELETE FROM entries_fts WHERE id=?1", params![id])?;
            conn.execute(
                "INSERT INTO entries_fts(id, path, summary, content, tags)
                 VALUES(?1,?2,?3,?4,?5)",
                params![id, path, summary, content, tags],
            )?;

            // Sync embedding store
            if !embedder.is_noop() {
                let text = format!("{} {} {}", path, summary, content);
                let emb = embedder.embed(&text)?;
                let blob = f32s_to_blob(&emb);
                conn.execute(
                    "INSERT OR REPLACE INTO entries_emb(rowid, embedding) VALUES(?1,?2)",
                    params![rowid, blob],
                )?;
            }
        }

        ("expire", "entries") => {
            let id = event["id"].as_str().context("missing id")?;
            conn.execute(
                "UPDATE entries SET is_stale=1, updated_at=datetime('now') WHERE id=?1",
                params![id],
            )?;
            // Remove from FTS so expired entries don't appear in search
            conn.execute("DELETE FROM entries_fts WHERE id=?1", params![id])?;
        }

        ("upsert", "test_cases") => {
            let id = event["id"].as_str().context("missing id")?;
            let app = event["app"].as_str().context("missing app")?;
            let name = event["name"].as_str().context("missing name")?;
            let protocol = event["protocol"].as_str().context("missing protocol")?;
            let config = event["config"].as_str().context("missing config")?;
            let version_ref = event["version_ref"].as_str();
            let ts = event["ts"].as_str().unwrap_or("");
            conn.execute(
                "INSERT INTO test_cases(id,app,name,protocol,config,version_ref,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
                 ON CONFLICT(id) DO UPDATE SET
                   app=excluded.app, name=excluded.name, protocol=excluded.protocol,
                   config=excluded.config, version_ref=excluded.version_ref,
                   is_stale=0, updated_at=excluded.updated_at",
                params![id, app, name, protocol, config, version_ref, ts],
            )?;
        }

        ("insert", "run_history") => {
            let test_id = event["test_id"].as_str().context("missing test_id")?;
            let result = event["result"].as_str().context("missing result")?;
            let adapter = event["adapter"].as_str();
            let detail = event["detail"].as_str();
            let ts = event["ts"].as_str().unwrap_or("");
            let run_id = event["run_id"].as_str();
            conn.execute(
                "INSERT INTO run_history(test_id,result,adapter,detail,ts,run_id)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![test_id, result, adapter, detail, ts, run_id],
            )?;
        }

        ("evidence_add", "evidence") => {
            let ev = &event["evidence"];
            let ev_id = ev["id"].as_str().context("evidence_add: missing evidence.id")?;
            let entry_id = event["entry_id"].as_str().context("evidence_add: missing entry_id")?;

            // Orphan-tolerant: if the parent entry doesn't exist, skip silently.
            let entry_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE id=?1",
                    params![entry_id],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if !entry_exists {
                return Ok(());
            }

            let kind = ev["kind"].as_str().context("evidence_add: missing evidence.kind")?;
            let citation_path = ev["citation_path"].as_str();
            let citation_sha = ev["citation_sha"].as_str();
            let citation_hash = ev["citation_hash"]
                .as_str()
                .context("evidence_add: missing evidence.citation_hash")?;
            let citation_excerpt = ev["citation_excerpt"].as_str();
            let derived_from = ev["derived_from"].as_str();
            let recorded_at = ev["recorded_at"].as_str();

            conn.execute(
                "INSERT OR IGNORE INTO evidence(id, entry_id, kind, citation_path, citation_sha, citation_hash, citation_excerpt, derived_from, recorded_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![ev_id, entry_id, kind, citation_path, citation_sha, citation_hash, citation_excerpt, derived_from, recorded_at],
            )?;

            // Update evidence_status on the parent entry via the soft-mandate helper.
            // Preserve 'n/a' for truly legacy entries (those that have no explicit kind
            // event — detected by kind column still holding the column default 'belief'
            // AND evidence_status still holding 'n/a' AND this being the first evidence row).
            let current_status: String = conn
                .query_row(
                    "SELECT COALESCE(evidence_status, 'n/a') FROM entries WHERE id=?1",
                    params![entry_id],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| "n/a".to_string());
            // Only update if the entry was already explicitly tracked (status != 'n/a')
            // OR if after this insert there is more than 1 evidence row (meaning the
            // entry had prior evidence, so it was already out of legacy-untouched state).
            let ev_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM evidence WHERE entry_id=?1",
                params![entry_id],
                |r| r.get(0),
            )?;
            if current_status != "n/a" || ev_count > 1 {
                let new_status = compute_evidence_status(conn, entry_id)?;
                conn.execute(
                    "UPDATE entries SET evidence_status=?1, updated_at=datetime('now') WHERE id=?2",
                    params![new_status, entry_id],
                )?;
            }
        }

        ("evidence_expire", "evidence") => {
            let ev_id = event["evidence_id"].as_str().context("evidence_expire: missing evidence_id")?;
            let entry_id = event["entry_id"].as_str().context("evidence_expire: missing entry_id")?;

            conn.execute(
                "DELETE FROM evidence WHERE id=?1 AND entry_id=?2",
                params![ev_id, entry_id],
            )?;

            // Recompute evidence_status if the parent entry exists.
            let entry_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE id=?1",
                    params![entry_id],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if entry_exists {
                let new_status = compute_evidence_status(conn, entry_id)?;
                conn.execute(
                    "UPDATE entries SET evidence_status=?1, updated_at=datetime('now') WHERE id=?2",
                    params![new_status, entry_id],
                )?;
            }
        }

        _ => {} // unknown event — skip silently
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared search infrastructure
// ---------------------------------------------------------------------------

/// Options for hybrid FTS5 + semantic search.
pub struct SearchOptions {
    pub limit: usize,
    pub do_fts: bool,
    pub do_semantic: bool,
    /// Only return entries whose path starts with this prefix.
    pub path_prefix: Option<String>,
    /// Only return entries that have this exact tag.
    pub tag_filter: Option<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: true,
            path_prefix: None,
            tag_filter: None,
        }
    }
}

/// A single result entry returned by `search_entries`.
pub struct SearchEntry {
    pub id: String,
    pub path: String,
    pub summary: String,
    pub content: String,
    /// Raw JSON array string (e.g. `["rust","test"]`).
    pub tags: String,
    /// Relevance score (cosine similarity for semantic, 1.0 for FTS).
    pub score: f32,
    /// `"fts"` or `"semantic"`.
    pub source: &'static str,
}

/// Shared hybrid search used by both the CLI and MCP handler.
///
/// FTS queries are wrapped in double-quotes to enable phrase search and prevent
/// FTS5 operator injection (e.g. unexpected `AND`/`OR` parsing).
pub fn search_entries(
    conn: &Connection,
    embedder: &dyn Embedder,
    query: &str,
    opts: &SearchOptions,
) -> Result<Vec<SearchEntry>> {
    let mut entries: Vec<SearchEntry> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    if opts.do_fts {
        // Quote each whitespace-delimited term individually to prevent FTS5
        // operator injection (AND/OR/NOT) while allowing multi-term recall.
        // "auth security" matches entries with both words anywhere, not just
        // as an exact phrase.
        let safe_query: String = query
            .split_whitespace()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        let mut stmt = conn.prepare(
            "SELECT e.id, e.path, e.summary, e.content, e.tags
             FROM entries_fts f
             JOIN entries e ON e.id = f.id
             WHERE f.entries_fts MATCH ?1
               AND e.is_stale = 0
               AND (?2 IS NULL OR e.path LIKE (?2 || '%'))
               AND (?3 IS NULL OR EXISTS (SELECT 1 FROM json_each(e.tags) WHERE value = ?3))
             ORDER BY rank
             LIMIT ?4",
        )?;
        let rows: Vec<_> = stmt
            .query_map(
                params![safe_query, opts.path_prefix, opts.tag_filter, opts.limit as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )?
            .filter_map(|r| r.ok())
            .collect();

        for (id, path, summary, content, tags) in rows {
            seen_ids.insert(id.clone());
            entries.push(SearchEntry {
                id,
                path,
                summary,
                content,
                tags,
                score: 1.0,
                source: "fts",
            });
        }
    }

    if opts.do_semantic && !embedder.is_noop() {
        let q_emb = embedder.embed(query)?;
        let mut stmt = conn.prepare(
            "SELECT e.id, e.path, e.summary, e.content, e.tags, emb.embedding
             FROM entries_emb emb
             JOIN entries e ON e.rowid = emb.rowid
             WHERE e.is_stale = 0
               AND (?1 IS NULL OR e.path LIKE (?1 || '%'))
               AND (?2 IS NULL OR EXISTS (SELECT 1 FROM json_each(e.tags) WHERE value = ?2))",
        )?;
        // TODO: O(n) brute-force scan — replace with ANN index (e.g. sqlite-vss) when entry count exceeds ~10k
        let mut candidates: Vec<(f32, String, String, String, String, String)> = stmt
            .query_map(params![opts.path_prefix, opts.tag_filter], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .map(|(id, path, summary, content, tags, blob)| {
                let emb_vec = blob_to_f32s(&blob);
                let sim = cosine_similarity(&q_emb, &emb_vec);
                (sim, id, path, summary, content, tags)
            })
            .collect();

        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        for (sim, id, path, summary, content, tags) in candidates.into_iter().take(opts.limit) {
            if seen_ids.contains(&id) {
                continue;
            }
            entries.push(SearchEntry {
                id,
                path,
                summary,
                content,
                tags,
                score: sim,
                source: "semantic",
            });
        }

        // Re-sort hybrid results by score descending and cap at limit
        if opts.do_fts {
            entries.sort_by(|a, b| {
                b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
            });
            entries.truncate(opts.limit);
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::embedder::NoopEmbedder;

    #[test]
    fn test_db_ensure_schema_creates_tables() {
        let conn = open_db_memory().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"entries".to_string()));
        assert!(tables.contains(&"test_cases".to_string()));
        assert!(tables.contains(&"run_history".to_string()));
        assert!(tables.contains(&"entries_emb".to_string()));
        assert!(tables.contains(&"evidence".to_string()));
        assert!(tables.contains(&"audit_runs".to_string()));
    }

    #[test]
    fn test_apply_event_legacy_entries_get_belief_and_na() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // Replay a legacy upsert event with no kind or evidence_status fields.
        let legacy1 = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "legacy1",
            "path": "old/path.md",
            "summary": "legacy entry one",
            "content": "some old content",
            "tags": ["old"],
            "ts": "2023-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &legacy1).unwrap();

        let (kind, evidence_status): (String, String) = conn
            .query_row(
                "SELECT COALESCE(kind,'belief'), COALESCE(evidence_status,'n/a') FROM entries WHERE id='legacy1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "belief", "legacy entry must default to kind='belief'");
        assert_eq!(evidence_status, "n/a", "legacy entry must default to evidence_status='n/a'");

        // Replay a second legacy entry.
        let legacy2 = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "legacy2",
            "path": "old/other.md",
            "summary": "legacy entry two",
            "content": "more old content",
            "tags": [],
            "ts": "2023-01-02T00:00:00Z"
        });
        apply_event(&conn, &embedder, &legacy2).unwrap();

        // audit_runs table must be untouched — zero rows (L4 boundary: audit_runs
        // is DB-only, never written by event replay).
        let audit_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(audit_count, 0, "audit_runs must be untouched after legacy event replay (L4 boundary)");
    }

    #[test]
    fn test_apply_event_upsert_entry() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;
        let event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "e1",
            "path": "src/lib.rs",
            "summary": "test summary",
            "content": "test content",
            "tags": ["rust"],
            "version_ref": "abc123",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &event).unwrap();

        let (path, summary): (String, String) = conn
            .query_row(
                "SELECT path, summary FROM entries WHERE id='e1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path, "src/lib.rs");
        assert_eq!(summary, "test summary");

        // Check FTS entry exists
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries_fts WHERE id='e1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_count, 1);
    }

    #[test]
    fn test_apply_event_expire() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // First upsert
        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "e1",
            "path": "src/lib.rs",
            "summary": "test",
            "content": "content",
            "tags": [],
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        // Then expire
        let expire = serde_json::json!({
            "action": "expire",
            "table": "entries",
            "id": "e1"
        });
        apply_event(&conn, &embedder, &expire).unwrap();

        let is_stale: i64 = conn
            .query_row("SELECT is_stale FROM entries WHERE id='e1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(is_stale, 1);
    }

    #[test]
    fn test_apply_event_expire_cleans_fts() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "fts1", "path": "src/auth.rs", "summary": "auth module",
            "content": "handles JWT tokens", "tags": ["auth"],
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        // Verify FTS entry exists before expire
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries_fts WHERE id='fts1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 1);

        let expire = serde_json::json!({
            "action": "expire", "table": "entries", "id": "fts1"
        });
        apply_event(&conn, &embedder, &expire).unwrap();

        // FTS entry must be gone after expire
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries_fts WHERE id='fts1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 0, "expire must remove entry from FTS index");
    }

    #[test]
    fn test_apply_event_upsert_with_is_stale_true() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // Upsert with is_stale=true (as produced by compact after expire)
        let event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "stale1",
            "path": "src/lib.rs",
            "summary": "stale entry",
            "content": "content",
            "tags": ["rust"],
            "version_ref": "abc123",
            "ts": "2024-01-01T00:00:00Z",
            "is_stale": true
        });
        apply_event(&conn, &embedder, &event).unwrap();

        // Entry must be stale in DB
        let is_stale: i64 = conn
            .query_row(
                "SELECT is_stale FROM entries WHERE id='stale1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_stale, 1, "upsert with is_stale=true must persist staleness");

        // FTS must NOT contain the stale entry
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries_fts WHERE id='stale1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_count, 0, "stale entries must not appear in FTS index");
    }

    #[test]
    fn test_apply_event_upsert_without_is_stale_defaults_false() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // Old-format event without is_stale field
        let event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "old1",
            "path": "src/lib.rs",
            "summary": "old entry",
            "content": "content",
            "tags": ["rust"],
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &event).unwrap();

        let is_stale: i64 = conn
            .query_row(
                "SELECT is_stale FROM entries WHERE id='old1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_stale, 0, "events without is_stale must default to active");
    }
}
