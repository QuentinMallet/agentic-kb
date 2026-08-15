//! Best-effort query-hit telemetry in a dedicated SQLite file.
//!
//! This database is deliberately separate from the rebuildable entries database:
//! lock-free search writers must never hold a connection to the file swapped by
//! `rebuild`. Hits are operational telemetry only; they produce no JSONL events
//! and are neither a rebuild input nor part of backups.

use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;

const RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;
const MAX_ROWS: i64 = 200_000;

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(2_000))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS hits(
           id INTEGER PRIMARY KEY,
           entry_id TEXT NOT NULL,
           queried_at INTEGER NOT NULL,
           surface TEXT NOT NULL DEFAULT 'unknown'
         );
         CREATE INDEX IF NOT EXISTS idx_hits_entry_id ON hits(entry_id);",
    )
}

fn open(path: &Path, create: bool) -> rusqlite::Result<Connection> {
    if !create && !path.is_file() {
        return Err(rusqlite::Error::InvalidPath(path.to_path_buf()));
    }
    if create {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let conn = Connection::open(path)?;
    configure(&conn)?;
    Ok(conn)
}

fn drop_and_recreate(path: &Path) -> Option<Connection> {
    eprintln!(
        "kb: query-hit log open failed; dropping and recreating {}",
        path.display()
    );
    let _ = fs::remove_file(path);
    let wal = path.with_extension("db-wal");
    let shm = path.with_extension("db-shm");
    let _ = fs::remove_file(wal);
    let _ = fs::remove_file(shm);
    open(path, true).ok()
}

/// Record one row per returned entry. Every failure is swallowed.
pub fn record_hits(path: &Path, entry_ids: &[String], surface: &str) {
    if entry_ids.is_empty() {
        return;
    }
    let mut conn = match open(path, true) {
        Ok(conn) => conn,
        Err(_) => match drop_and_recreate(path) {
            Some(conn) => conn,
            None => return,
        },
    };
    let now = chrono::Utc::now().timestamp();
    let result = (|| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO hits(entry_id,queried_at,surface) VALUES(?1,?2,?3)",
            )?;
            for entry_id in entry_ids {
                insert.execute(params![entry_id, now, surface])?;
            }
        }
        tx.execute(
            "DELETE FROM hits WHERE queried_at < ?1",
            [now - RETENTION_SECONDS],
        )?;
        tx.execute(
            "DELETE FROM hits WHERE id IN
             (SELECT id FROM hits ORDER BY queried_at DESC, id DESC LIMIT -1 OFFSET ?1)",
            [MAX_ROWS],
        )?;
        tx.commit()
    })();
    if result.is_err() {
        let _ = drop_and_recreate(path);
    }
}

/// Return hit counts for traffic sampling. Missing, corrupt, or unreadable logs
/// return `None`, allowing the caller to degrade to its uniform arm.
pub fn counts(path: &Path) -> Option<Vec<(String, u64)>> {
    let conn = match open(path, false) {
        Ok(conn) => conn,
        Err(_) => {
            if path.exists() {
                let _ = drop_and_recreate(path);
            }
            return None;
        }
    };
    let result = (|| -> rusqlite::Result<Vec<(String, u64)>> {
        let mut stmt = conn.prepare("SELECT entry_id, COUNT(*) FROM hits GROUP BY entry_id")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    })();
    match result {
        Ok(counts) => Some(counts),
        Err(_) => {
            drop(conn);
            let _ = drop_and_recreate(path);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_prunes_and_caps() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hits.db");
        record_hits(&path, &["new".into()], "mcp");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO hits(entry_id,queried_at) VALUES('old',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<=200000)
             INSERT INTO hits(entry_id,queried_at) SELECT 'bulk',strftime('%s','now') FROM n",
            [],
        ).unwrap();
        drop(conn);
        record_hits(&path, &["last".into()], "test");
        let conn = Connection::open(&path).unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM hits", [], |r| r.get(0))
            .unwrap();
        let old: i64 = conn
            .query_row("SELECT COUNT(*) FROM hits WHERE entry_id='old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let surface: String = conn
            .query_row(
                "SELECT surface FROM hits WHERE entry_id='last'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, MAX_ROWS);
        assert_eq!(old, 0);
        assert_eq!(surface, "test");
    }

    #[test]
    fn corrupted_file_is_dropped_and_recreated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hits.db");
        fs::write(&path, b"not sqlite").unwrap();
        record_hits(&path, &["recovered".into()], "test");
        assert_eq!(counts(&path).unwrap(), vec![("recovered".into(), 1)]);
    }
}
