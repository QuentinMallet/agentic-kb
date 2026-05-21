//! `mcp` subcommand — line-delimited JSON port protocol server
//!
//! Spawned by the Elixir PortManager GenServer. Speaks the agentic-kb port
//! protocol: line-delimited JSON over stdin/stdout (one JSON object per line).
//!
//! Protocol: see .omc/specs/agentic-kb-port-protocol.md

use crate::commands::add::{acquire_lock, make_embedder};
use crate::components::{db, embedder, events};
use crate::config;
use crate::models::{blob_to_f32s, cosine_similarity};
use abscissa_core::{Command, Runnable};
use anyhow::Result;
use clap::Parser;
use rusqlite::params;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// Run MCP port protocol server (line-delimited JSON over stdio)
#[derive(Command, Debug, Parser)]
pub struct Mcp {
    /// Path to agent-kb.db
    #[arg(long)]
    pub db: PathBuf,
}

impl Runnable for Mcp {
    fn run(&self) {
        if let Err(e) = self.execute() {
            let err = json!({"type":"error","code":"internal","message":e.to_string()});
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

/// Derive the repo root from the db path (<root>/agent-kb/agent-kb.db).
fn root_from_db(db: &Path) -> PathBuf {
    db.parent()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

impl Mcp {
    pub fn execute(&self) -> Result<()> {
        let root = root_from_db(&self.db);
        let paths = config::Paths::from_root(&root);
        // Override db path with the explicitly passed one (handles cases where
        // the symlink .state/agent-kb/agent-kb.db differs from the canonical path).
        let paths = config::Paths {
            db: self.db.clone(),
            ..paths
        };

        // Build embedder once; reused for all requests in this session.
        let emb = make_embedder(&paths);

        let ready = json!({
            "type": "ready",
            "version": "1.0",
            "db": self.db.to_string_lossy()
        });
        println!("{ready}");
        io::stdout().flush()?;

        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let response = handle_request(&line, &paths, emb.as_ref());
            println!("{response}");
            io::stdout().flush()?;
        }
        Ok(())
    }
}

fn handle_request(line: &str, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return json!({"type":"error","code":"parse_error","message":e.to_string()});
        }
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "search" => handle_search(&id, &req, paths, emb),
        "add" => handle_add(&id, &req, paths, emb),
        "import" => handle_import(&id, &req, paths, emb),
        "rebuild" => handle_rebuild(&id, paths, emb),
        _ => json!({
            "id": id,
            "type": "error",
            "code": "unknown_method",
            "message": format!("unknown method: {method}")
        }),
    }
}

fn handle_search(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let query = match req.get("query").and_then(|q| q.as_str()) {
        Some(q) => q.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing query"}),
    };
    let limit = req.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
    let mode = req.get("mode").and_then(|m| m.as_str()).unwrap_or("hybrid");

    let do_fts = mode == "fts" || mode == "hybrid";
    let do_semantic = mode == "semantic" || mode == "hybrid";

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let mut entries: Vec<Value> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    if do_fts {
        let mut stmt = match conn.prepare(
            "SELECT e.id, e.path, e.summary, e.content, e.tags
             FROM entries_fts f
             JOIN entries e ON e.id = f.id
             WHERE f.entries_fts MATCH ?1 AND e.is_stale=0
             ORDER BY rank
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
        };

        let rows: Vec<_> = match stmt.query_map(params![query, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        }) {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => vec![],
        };

        for (entry_id, path, summary, content, tags_str) in rows {
            let tags: Value = serde_json::from_str(&tags_str).unwrap_or(Value::Array(vec![]));
            seen_ids.insert(entry_id.clone());
            entries.push(json!({
                "path": path,
                "summary": summary,
                "content": content,
                "tags": tags,
                "score": 1.0,
                "id": entry_id,
                "source": "fts",
            }));
        }
    }

    if do_semantic && !emb.is_noop() {
        let q_emb = match emb.embed(&query) {
            Ok(e) => e,
            Err(_) => return json!({"id":id,"type":"result","entries":entries}),
        };

        let mut stmt = match conn.prepare(
            "SELECT e.id, e.path, e.summary, e.content, e.tags, emb.embedding
             FROM entries_emb emb
             JOIN entries e ON e.rowid = emb.rowid
             WHERE e.is_stale = 0",
        ) {
            Ok(s) => s,
            Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
        };

        let mut candidates: Vec<(f32, String, String, String, String, String)> = match stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                ))
            }) {
            Ok(mapped) => mapped
                .filter_map(|r| r.ok())
                .map(|(entry_id, path, summary, content, tags_str, blob)| {
                    let emb_vec = blob_to_f32s(&blob);
                    let sim = cosine_similarity(&q_emb, &emb_vec);
                    (sim, entry_id, path, summary, content, tags_str)
                })
                .collect(),
            Err(_) => vec![],
        };

        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        for (sim, entry_id, path, summary, content, tags_str) in candidates.into_iter().take(limit) {
            if seen_ids.contains(&entry_id) {
                continue;
            }
            let tags: Value = serde_json::from_str(&tags_str).unwrap_or(Value::Array(vec![]));
            entries.push(json!({
                "path": path,
                "summary": summary,
                "content": content,
                "tags": tags,
                "score": sim,
                "id": entry_id,
                "source": "semantic",
            }));
        }

        // Re-sort by score for hybrid mode
        entries.sort_by(|a, b| {
            let sa = a["score"].as_f64().unwrap_or(0.0);
            let sb = b["score"].as_f64().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        entries.truncate(limit);
    }

    json!({"id": id, "type": "result", "entries": entries})
}

fn handle_add(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let path = match req.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing path"}),
    };
    let summary = req.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let content = req.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tags = req.get("tags").cloned().unwrap_or(Value::Array(vec![]));

    let entry_id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();

    let event = json!({
        "action": "upsert",
        "table": "entries",
        "id": entry_id,
        "path": path,
        "summary": summary,
        "content": content,
        "tags": tags,
        "version_ref": null,
        "ts": ts,
        "session": "mcp",
    });

    let _lock = match acquire_lock(&paths.lock) {
        Ok(l) => l,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    if let Err(e) = events::append_event(&paths.events, &event) {
        return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
    }

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    if let Err(e) = db::apply_event(&conn, emb, &event) {
        return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
    }

    json!({"id": id, "type": "ok", "entry_id": entry_id})
}

fn handle_import(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let file_path = match req.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing path"}),
    };
    let upsert = req.get("upsert").and_then(|v| v.as_bool()).unwrap_or(false);

    let content = match std::fs::read_to_string(&file_path) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"import_error","message":e.to_string()}),
    };

    let seeds: Vec<Value> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => return json!({"id":id,"type":"error","code":"import_error","message":e.to_string()}),
    };

    let _lock = match acquire_lock(&paths.lock) {
        Ok(l) => l,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let mut imported: u32 = 0;
    let mut skipped: u32 = 0;

    for seed in &seeds {
        let path = seed.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();

        if !upsert {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE path = ?1 AND is_stale = 0",
                    params![path],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if exists {
                skipped += 1;
                continue;
            }
        }

        let entry_id = uuid::Uuid::new_v4().to_string();
        let ts = chrono::Utc::now().to_rfc3339();
        let summary = seed.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let content = seed.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let tags = seed.get("tags").cloned().unwrap_or(Value::Array(vec![]));

        let event = json!({
            "action": "upsert",
            "table": "entries",
            "id": entry_id,
            "path": path,
            "summary": summary,
            "content": content,
            "tags": tags,
            "version_ref": null,
            "ts": ts,
            "session": "mcp-import",
        });

        if let Err(e) = events::append_event(&paths.events, &event) {
            return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
        }
        if let Err(e) = db::apply_event(&conn, emb, &event) {
            return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
        }

        imported += 1;
    }

    json!({"id": id, "type": "ok", "imported": imported, "skipped": skipped})
}

fn handle_rebuild(id: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    use std::fs;

    let _lock = match acquire_lock(&paths.lock) {
        Ok(l) => l,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    if paths.db.exists() {
        let _ = fs::remove_file(&paths.db);
        let db_str = paths.db.to_string_lossy();
        let _ = fs::remove_file(format!("{}-wal", db_str));
        let _ = fs::remove_file(format!("{}-shm", db_str));
    }

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let evts = match events::read_events(&paths.events) {
        Ok(e) => e,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let total = evts.len();
    for (i, event) in evts.iter().enumerate() {
        let progress = json!({"id":id,"type":"progress","processed":i+1,"total":total});
        println!("{progress}");
        let _ = io::stdout().flush();

        if let Err(e) = db::apply_event(&conn, emb, event) {
            return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
        }
    }

    json!({"id": id, "type": "ok", "rebuilt": total})
}
