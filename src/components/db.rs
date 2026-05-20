//! Database operations

use crate::components::embedder::Embedder;
use crate::models::f32s_to_blob;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;

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

            conn.execute(
                "INSERT INTO entries(id, path, summary, content, tags, version_ref, created_at, updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
                 ON CONFLICT(id) DO UPDATE SET
                   path=excluded.path, summary=excluded.summary,
                   content=excluded.content, tags=excluded.tags,
                   version_ref=excluded.version_ref,
                   is_stale=0, updated_at=excluded.updated_at",
                params![id, path, summary, content, tags, version_ref, ts],
            )?;

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

        _ => {} // unknown event — skip silently
    }
    Ok(())
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
}
