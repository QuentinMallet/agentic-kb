//! Best-effort query-hit telemetry in a dedicated SQLite file.
//!
//! This database is deliberately separate from the rebuildable entries database:
//! lock-free search writers must never hold a connection to the file swapped by
//! `rebuild`. Hits are operational telemetry only; they produce no JSONL events
//! and are neither a rebuild input nor part of backups.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;
const MAX_ROWS: i64 = 200_000;

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

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
         CREATE INDEX IF NOT EXISTS idx_hits_entry_id ON hits(entry_id);
         CREATE TABLE IF NOT EXISTS injections(
           id INTEGER PRIMARY KEY,
           session_id TEXT NOT NULL,
           entry_id TEXT NOT NULL,
           cited_file TEXT,
           surface TEXT NOT NULL DEFAULT 'unknown',
           injected_at INTEGER NOT NULL,
           acted_on INTEGER DEFAULT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_injections_session_id ON injections(session_id);",
    )?;
    let has_acted_on = conn
        .prepare("PRAGMA table_info(injections)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == "acted_on");
    if !has_acted_on {
        conn.execute("ALTER TABLE injections ADD COLUMN acted_on INTEGER DEFAULT NULL", [])?;
    }
    Ok(())
}

/// Record one row per injected entry. Every failure is swallowed.
pub fn record_injection(
    path: &Path,
    session_id: &str,
    entries: &[(String, Option<String>)],
    surface: &str,
) {
    if entries.is_empty() {
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
    let session_id = truncate_utf8(session_id, 64);
    let surface = truncate_utf8(surface, 32);
    let result = (|| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO injections(session_id,entry_id,cited_file,surface,injected_at) \
                 VALUES(?1,?2,?3,?4,?5)",
            )?;
            for (entry_id, cited_file) in entries {
                let entry_id = truncate_utf8(entry_id, 512);
                let cited_file = cited_file.as_deref().map(|v| truncate_utf8(v, 512));
                insert.execute(params![session_id, entry_id, cited_file, surface, now])?;
            }
        }
        tx.execute(
            "DELETE FROM injections WHERE injected_at < ?1",
            [now - RETENTION_SECONDS],
        )?;
        tx.execute(
            "DELETE FROM injections WHERE id IN
             (SELECT id FROM injections ORDER BY injected_at DESC, id DESC LIMIT -1 OFFSET ?1)",
            [MAX_ROWS],
        )?;
        tx.commit()
    })();
    if result.is_err() {
        let _ = drop_and_recreate(path);
    }
}

fn is_tool_call(line: &str) -> bool {
    line.contains("Tool:")
        || line.contains("<tool_use>")
        || line.contains("fn_call:")
        || line.contains("Bash(")
        || line.contains("Read(")
        || line.contains("Write(")
        || line.contains("Edit(")
        || line.contains("\"type\":\"tool_use\"")
        || line.contains("\"type\": \"tool_use\"")
        || line.contains("\"tool_name\"")
}

/// Mark this session's injections according to references in newly-read tool-call turns.
/// Failures are swallowed; a prior match can never be changed back to unmatched.
pub fn record_acted_on(path: &Path, session_id: &str, transcript_bytes: &[u8]) {
    let session_id = truncate_utf8(session_id, 64);
    let mut conn = match open(path, false) {
        Ok(conn) => conn,
        Err(_) => {
            if path.exists() {
                let _ = drop_and_recreate(path);
            }
            return;
        }
    };
    let tool_text = String::from_utf8_lossy(transcript_bytes)
        .lines()
        .filter(|line| is_tool_call(line))
        .collect::<Vec<_>>()
        .join("\n");
    let result = (|| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        let rows: Vec<(i64, String, Option<String>)> = {
            let mut stmt = tx.prepare(
                "SELECT id,entry_id,cited_file FROM injections WHERE session_id=?1",
            )?;
            let collected: Vec<_> = stmt
                .query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            collected
        };
        for (id, entry_id, cited_file) in rows {
            let matched = tool_text.contains(&entry_id)
                || cited_file.as_deref().is_some_and(|file| !file.is_empty() && tool_text.contains(file));
            tx.execute(
                "UPDATE injections SET acted_on=MAX(COALESCE(acted_on,0),?1) WHERE id=?2",
                params![if matched { 1 } else { 0 }, id],
            )?;
        }
        tx.commit()
    })();
    if result.is_err() {
        let _ = drop_and_recreate(path);
    }
}

#[derive(Debug, Serialize)]
pub struct SurfaceTelemetry {
    pub count: i64,
    pub acted_on_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct InjectionTelemetry {
    pub total_injections: i64,
    pub acted_on_rate: Option<f64>,
    pub unknown_surface_rate: f64,
    pub per_surface: BTreeMap<String, SurfaceTelemetry>,
}

/// Aggregate injection telemetry. Missing/corrupt logs degrade to `None`.
pub fn injection_telemetry(path: &Path) -> Option<InjectionTelemetry> {
    let conn = match open(path, false) {
        Ok(conn) => conn,
        Err(_) => {
            if path.exists() {
                let _ = drop_and_recreate(path);
            }
            return None;
        }
    };
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM injections", [], |r| r.get(0)).ok()?;
    let (scanned, matched): (i64, i64) = conn.query_row(
        "SELECT COUNT(acted_on), COALESCE(SUM(acted_on),0) FROM injections",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).ok()?;
    let unknown: i64 = conn.query_row(
        "SELECT COUNT(*) FROM injections WHERE surface='unknown'", [], |r| r.get(0),
    ).ok()?;
    let mut per_surface = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT surface,COUNT(*),COUNT(acted_on),COALESCE(SUM(acted_on),0) \
         FROM injections GROUP BY surface ORDER BY surface",
    ).ok()?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))).ok()?;
    for (surface, count, surface_scanned, surface_matched) in rows.filter_map(Result::ok) {
        per_surface.insert(surface, SurfaceTelemetry {
            count,
            acted_on_rate: (surface_scanned > 0).then_some(surface_matched as f64 / surface_scanned as f64),
        });
    }
    Some(InjectionTelemetry {
        total_injections: total,
        acted_on_rate: (scanned > 0).then_some(matched as f64 / scanned as f64),
        unknown_surface_rate: if total == 0 { 0.0 } else { unknown as f64 / total as f64 },
        per_surface,
    })
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
    let surface = truncate_utf8(surface, 32);
    let result = (|| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO hits(entry_id,queried_at,surface) VALUES(?1,?2,?3)",
            )?;
            for entry_id in entry_ids {
                insert.execute(params![truncate_utf8(entry_id, 512), now, surface])?;
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

    #[test]
    fn records_injections_and_prunes_expired_rows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hits.db");
        record_injection(&path, "s1", &[("new".into(), Some("src/lib.rs".into()))], "cli");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO injections(session_id,entry_id,surface,injected_at) VALUES('s1','old','cli',0)",
            [],
        ).unwrap();
        drop(conn);
        record_injection(&path, "s1", &[("last".into(), None)], "cli");
        let conn = Connection::open(&path).unwrap();
        let rows: Vec<(String, Option<String>)> = conn.prepare(
            "SELECT entry_id,cited_file FROM injections ORDER BY id",
        ).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .map(Result::unwrap).collect();
        assert_eq!(rows, vec![("new".into(), Some("src/lib.rs".into())), ("last".into(), None)]);
    }

    #[test]
    fn acted_on_matching_is_idempotent_and_reported() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hits.db");
        record_injection(&path, "s1", &[
            ("entry-a".into(), Some("src/touched.rs".into())),
            ("entry-b".into(), Some("src/untouched.rs".into())),
        ], "unknown");
        let fixture = br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/touched.rs"}}]}}
{"type":"user","message":"entry-b outside a tool call"}
"#;
        record_acted_on(&path, "s1", fixture);
        record_acted_on(&path, "s1", br#"{"type":"tool_use","name":"Read","input":{"file_path":"other.rs"}}"#);
        let conn = Connection::open(&path).unwrap();
        let flags: Vec<i64> = conn.prepare("SELECT acted_on FROM injections ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(flags, vec![1, 0], "replay must not flip the prior match");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM injections", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2, "acted-on replay must not duplicate injections");
        drop(conn);
        let report = injection_telemetry(&path).unwrap();
        assert_eq!(report.total_injections, 2);
        assert_eq!(report.acted_on_rate, Some(0.5));
        assert_eq!(report.unknown_surface_rate, 1.0);
        assert_eq!(report.per_surface["unknown"].acted_on_rate, Some(0.5));
    }

    #[test]
    fn corrupt_db_is_recreated_for_injections() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hits.db");
        fs::write(&path, b"not sqlite").unwrap();
        record_injection(&path, "s1", &[("recovered".into(), None)], "test");
        assert_eq!(injection_telemetry(&path).unwrap().total_injections, 1);
    }

    #[test]
    fn oversized_fields_are_truncated_on_utf8_boundaries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hits.db");
        let surface = "é".repeat(40);
        let session = "é".repeat(50);
        let entry = "é".repeat(300);
        let cited = "é".repeat(300);
        record_injection(&path, &session, &[(entry.clone(), Some(cited))], &surface);
        record_hits(&path, &[entry], &surface);

        let conn = Connection::open(&path).unwrap();
        let injection_lengths: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT length(CAST(session_id AS BLOB)), length(CAST(entry_id AS BLOB)), \
                 length(CAST(cited_file AS BLOB)), length(CAST(surface AS BLOB)) FROM injections",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(injection_lengths, (64, 512, 512, 32));
        let hit_lengths: (i64, i64) = conn
            .query_row(
                "SELECT length(CAST(entry_id AS BLOB)), length(CAST(surface AS BLOB)) FROM hits",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(hit_lengths, (512, 32));
    }
}
