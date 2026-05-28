//! `mcp` subcommand — line-delimited JSON port protocol server
//!
//! Spawned by the Elixir PortManager GenServer. Speaks the agentic-kb port
//! protocol: line-delimited JSON over stdin/stdout (one JSON object per line).
//!
//! Protocol: see .omc/specs/agentic-kb-port-protocol.md

use crate::commands::add::{acquire_lock, make_embedder};
use crate::components::{db, embedder, events};
use crate::config;
use abscissa_core::{Command, Runnable};
use anyhow::Result;
use clap::Parser;
use rusqlite::{params, Connection};
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

        // Open a persistent connection for read-only handlers; write handlers
        // open their own connection under the file lock.
        let conn = db::open_db(&paths.db)?;

        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let response = handle_request(&line, &paths, &conn, emb.as_ref());
            println!("{response}");
            io::stdout().flush()?;
        }
        Ok(())
    }
}

fn handle_request(line: &str, paths: &config::Paths, conn: &Connection, emb: &dyn embedder::Embedder) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return json!({"type":"error","code":"parse_error","message":e.to_string()});
        }
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "search" => handle_search(&id, &req, conn, emb),
        "add" => handle_add(&id, &req, paths, emb),
        "import" => handle_import(&id, &req, paths, emb),
        "expire" => handle_expire(&id, &req, paths, emb),
        "stale_check" => handle_stale_check(&id, &req, conn, paths),
        "compact" => handle_compact(&id, paths),
        "reembed" => handle_reembed(&id, &req, paths, emb),
        "run" => handle_run(&id, &req, paths, emb),
        "test_add" => handle_test_add(&id, &req, paths, emb),
        "tests" => handle_tests(&id, &req, conn),
        "rebuild" => handle_rebuild(&id, paths, emb),
        _ => json!({
            "id": id,
            "type": "error",
            "code": "unknown_method",
            "message": format!("unknown method: {method}")
        }),
    }
}

fn handle_search(id: &Value, req: &Value, conn: &Connection, emb: &dyn embedder::Embedder) -> Value {
    let query = match req.get("query").and_then(|q| q.as_str()) {
        Some(q) => q.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing query"}),
    };
    let limit = req.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
    let mode = req.get("mode").and_then(|m| m.as_str()).unwrap_or("hybrid");
    let path_prefix = req.get("path_prefix").and_then(|v| v.as_str()).map(|s| s.to_string());
    let tag_filter = req.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());

    let opts = db::SearchOptions {
        limit,
        do_fts: mode == "fts" || mode == "hybrid",
        do_semantic: mode == "semantic" || mode == "hybrid",
        path_prefix,
        tag_filter,
    };

    // Cap content per entry to prevent port line buffer overflow (10MB limit).
    // 8000 chars per entry * 50 entries = 400KB typical, well under the limit.
    const MAX_CONTENT_CHARS: usize = 8000;

    match db::search_entries(conn, emb, &query, &opts) {
        Ok(results) => {
            let entries: Vec<Value> = results
                .into_iter()
                .map(|e| {
                    let tags: Value =
                        serde_json::from_str(&e.tags).unwrap_or(Value::Array(vec![]));
                    let content = if e.content.chars().count() > MAX_CONTENT_CHARS {
                        let truncated: String = e.content.chars().take(MAX_CONTENT_CHARS).collect();
                        format!("{}...(truncated)", truncated)
                    } else {
                        e.content
                    };
                    json!({
                        "path": e.path,
                        "summary": e.summary,
                        "content": content,
                        "tags": tags,
                        "score": e.score,
                        "id": e.id,
                        "source": e.source,
                    })
                })
                .collect();
            json!({"id": id, "type": "result", "entries": entries})
        }
        Err(e) => json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    }
}

fn handle_add(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let path = match req.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing path"}),
    };
    let summary = req.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let content = req.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let tags = req.get("tags").cloned().unwrap_or(Value::Array(vec![]));
    let permanent = req.get("permanent").and_then(|v| v.as_bool()).unwrap_or(false);
    let replace_path = req.get("replace_path").and_then(|v| v.as_bool()).unwrap_or(false);

    let entry_id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();

    let _lock = match acquire_lock(&paths.lock) {
        Ok(l) => l,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    // --replace-path: expire existing non-stale entries at this path before inserting
    if replace_path {
        let existing_ids: Vec<String> = {
            let mut stmt = match conn.prepare(
                "SELECT id FROM entries WHERE path=?1 AND is_stale=0",
            ) {
                Ok(s) => s,
                Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
            };
            let ids: Vec<String> = match stmt.query_map(params![path], |r| r.get(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
            };
            ids
        };
        for old_id in existing_ids {
            let expire_ev = json!({
                "action": "expire", "table": "entries",
                "id": old_id, "reason": "replaced by MCP kb_add replace_path",
                "ts": ts, "session": "mcp",
            });
            if let Err(e) = events::append_event(&paths.events, &expire_ev) {
                return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
            }
            if let Err(e) = db::apply_event(&conn, emb, &expire_ev) {
                return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
            }
        }
    }

    let event = json!({
        "action": "upsert",
        "table": "entries",
        "id": entry_id,
        "path": path,
        "summary": summary,
        "content": content,
        "tags": tags,
        "version_ref": config::git_head_sha(),
        "permanent": permanent,
        "ts": ts,
        "session": "mcp",
    });

    if let Err(e) = events::append_event(&paths.events, &event) {
        return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
    }

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
        // Progress goes to stderr — stdout is the MCP response channel
        eprintln!("kb: rebuild {}/{}", i + 1, total);
        let _ = io::stderr().flush();

        if let Err(e) = db::apply_event(&conn, emb, event) {
            return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
        }
    }

    json!({"id": id, "type": "ok", "rebuilt": total})
}

fn handle_run(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let test_id = match req.get("test_id").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing test_id"}),
    };
    let result = match req.get("result").and_then(|v| v.as_str()) {
        Some(r) if r == "pass" || r == "fail" => r.to_string(),
        _ => return json!({"id":id,"type":"error","code":"parse_error","message":"result must be 'pass' or 'fail'"}),
    };
    let adapter = req.get("adapter").and_then(|v| v.as_str()).map(|s| s.to_string());
    let detail = req.get("detail").and_then(|v| v.as_str()).map(|s| s.to_string());

    let _lock = match acquire_lock(&paths.lock) {
        Ok(l) => l,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let ts = chrono::Utc::now().to_rfc3339();
    let run_id = uuid::Uuid::new_v4().to_string();
    let event = json!({
        "action": "insert", "table": "run_history",
        "test_id": test_id, "result": result,
        "adapter": adapter, "detail": detail,
        "ts": ts, "run_id": run_id, "session": "mcp",
    });

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

    json!({"id": id, "type": "ok", "run_id": run_id, "test_id": test_id, "result": result})
}

fn handle_test_add(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let app = match req.get("app").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing app"}),
    };
    let name = match req.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing name"}),
    };
    let protocol = match req.get("protocol").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing protocol"}),
    };
    let config_str = match req.get("config").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing config"}),
    };

    let test_id = req.get("test_id").and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}-{}", app, name.replace(' ', "-")));

    let _lock = match acquire_lock(&paths.lock) {
        Ok(l) => l,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let ts = chrono::Utc::now().to_rfc3339();
    let event = json!({
        "action": "upsert", "table": "test_cases",
        "id": test_id, "app": app, "name": name,
        "protocol": protocol, "config": config_str,
        "version_ref": null, "ts": ts, "session": "mcp",
    });

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

    json!({"id": id, "type": "ok", "test_id": test_id})
}

fn handle_tests(id: &Value, req: &Value, conn: &Connection) -> Value {
    let app_filter = req.get("app").and_then(|v| v.as_str()).map(|s| s.to_string());

    let results: Vec<Value> = if let Some(ref app) = app_filter {
        let mut stmt = conn.prepare(
            "SELECT id, app, name, protocol FROM test_cases WHERE app=?1 AND is_stale=0 ORDER BY name"
        ).unwrap();
        stmt.query_map(params![app], |r| {
            Ok(json!({"id": r.get::<_,String>(0)?, "app": r.get::<_,String>(1)?,
                       "name": r.get::<_,String>(2)?, "protocol": r.get::<_,String>(3)?}))
        }).unwrap().filter_map(|r| r.ok()).collect()
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, app, name, protocol FROM test_cases WHERE is_stale=0 ORDER BY app, name"
        ).unwrap();
        stmt.query_map([], |r| {
            Ok(json!({"id": r.get::<_,String>(0)?, "app": r.get::<_,String>(1)?,
                       "name": r.get::<_,String>(2)?, "protocol": r.get::<_,String>(3)?}))
        }).unwrap().filter_map(|r| r.ok()).collect()
    };

    json!({"id": id, "type": "result", "test_cases": results, "count": results.len()})
}

fn handle_reembed(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let dry_run = req.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
    let max_chars = req.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(1800) as usize;

    if emb.is_noop() {
        return json!({"id":id,"type":"ok","embedded":0,"skipped":0,"missing":0,
                       "message":"KB_NO_EMBED is set — no embedder available"});
    }

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let mut stmt = match conn.prepare(
        "SELECT e.rowid, e.id, e.path, e.summary, e.content
         FROM entries e
         WHERE e.is_stale = 0
           AND e.rowid NOT IN (SELECT rowid FROM entries_emb)",
    ) {
        Ok(s) => s,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let candidates: Vec<(i64, String, String, String, String)> = match stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let total_missing = candidates.len();
    let to_embed: Vec<_> = candidates.iter()
        .filter(|(_, _, path, summary, content)| path.len() + summary.len() + content.len() + 2 <= max_chars)
        .collect();
    let skipped = total_missing - to_embed.len();

    if dry_run {
        return json!({"id":id,"type":"ok","embedded":0,"skipped":skipped,"missing":to_embed.len(),
                       "dry_run":true});
    }

    let mut done = 0u32;
    let mut failed = 0u32;
    for (rowid, _id, path, summary, content) in &to_embed {
        let text = format!("{} {} {}", path, summary, content);
        match emb.embed(&text) {
            Ok(emb_vec) => {
                let blob = crate::models::f32s_to_blob(&emb_vec);
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO entries_emb(rowid, embedding) VALUES(?1, ?2)",
                    params![rowid, blob],
                );
                done += 1;
            }
            Err(_) => { failed += 1; }
        }
    }

    json!({"id":id,"type":"ok","embedded":done,"failed":failed,"skipped":skipped})
}

fn handle_compact(id: &Value, paths: &config::Paths) -> Value {
    let compact_cmd = crate::commands::compact::Compact;
    match compact_cmd.execute_with_paths(paths) {
        Ok((before, after)) => json!({"id": id, "type": "ok", "before": before, "after": after}),
        Err(e) => json!({"id":id,"type":"error","code":"compact_error","message":e.to_string()}),
    }
}

fn handle_stale_check(id: &Value, req: &Value, conn: &Connection, _paths: &config::Paths) -> Value {
    use crate::commands::stale_check::run_stale_check;

    let files: Vec<String> = req
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let explicit_commits: Vec<String> = req
        .get("commits")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let blame = req.get("blame").and_then(|v| v.as_bool()).unwrap_or(false);

    if files.is_empty() && explicit_commits.is_empty() {
        return json!({"id":id,"type":"error","code":"parse_error","message":"provide files or commits"});
    }

    let repo_root = config::git_repo_root();
    let report = match run_stale_check(conn, &files, &explicit_commits, blame, repo_root.as_deref()) {
        Ok(r) => r,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let stale: Vec<Value> = report
        .stale
        .iter()
        .map(|e| json!({
            "id": e.id,
            "path": e.path,
            "summary": e.summary,
            "version_ref": e.version_ref,
            "commits_behind": e.commits_behind,
        }))
        .collect();
    let review: Vec<Value> = report
        .review
        .iter()
        .map(|e| json!({
            "id": e.id,
            "path": e.path,
            "summary": e.summary,
            "version_ref": e.version_ref,
        }))
        .collect();
    let unreachable: Vec<Value> = report
        .unreachable
        .iter()
        .map(|e| json!({
            "id": e.id,
            "path": e.path,
            "summary": e.summary,
            "version_ref": e.version_ref,
        }))
        .collect();

    json!({
        "id": id,
        "type": "result",
        "stale": stale,
        "review": review,
        "unreachable": unreachable,
        "checked": report.checked,
    })
}

fn handle_expire(id: &Value, req: &Value, paths: &config::Paths, emb: &dyn embedder::Embedder) -> Value {
    let entry_id = match req.get("entry_id").and_then(|v| v.as_str()) {
        Some(i) => i.to_string(),
        None => return json!({"id":id,"type":"error","code":"parse_error","message":"missing entry_id"}),
    };
    let reason = req.get("reason").and_then(|v| v.as_str()).map(|s| s.to_string());
    let force = req.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    let _lock = match acquire_lock(&paths.lock) {
        Ok(l) => l,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    // Guard: refuse to expire permanent entries unless force=true
    if !force {
        let permanent: Option<i64> = conn
            .query_row(
                "SELECT permanent FROM entries WHERE id=?1",
                params![entry_id],
                |r| r.get(0),
            )
            .ok();
        if permanent == Some(1) {
            return json!({
                "id": id, "type": "error", "code": "permanent_guard",
                "message": format!("entry '{}' is permanent; set force=true to expire it", entry_id)
            });
        }
    }

    let ts = chrono::Utc::now().to_rfc3339();
    let event = json!({
        "action": "expire",
        "table": "entries",
        "id": entry_id,
        "reason": reason,
        "ts": ts,
        "session": "mcp",
    });

    if let Err(e) = events::append_event(&paths.events, &event) {
        return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
    }
    if let Err(e) = db::apply_event(&conn, emb, &event) {
        return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
    }

    json!({"id": id, "type": "ok", "expired": entry_id})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::embedder::NoopEmbedder;
    use std::fs;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, config::Paths, NoopEmbedder) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = config::Paths::from_root(root);
        (dir, paths, NoopEmbedder)
    }

    #[test]
    fn test_handle_add_basic() {
        let (_dir, paths, emb) = setup();
        let id = json!("t1");
        let req = json!({"method":"add","id":"t1","path":"test/a","summary":"sum","content":"body","tags":["t"]});
        let resp = handle_add(&id, &req, &paths, &emb);
        assert_eq!(resp["type"], "ok");
        assert!(resp["entry_id"].as_str().is_some());
    }

    #[test]
    fn test_handle_add_permanent() {
        let (_dir, paths, emb) = setup();
        let id = json!("t2");
        let req = json!({"method":"add","id":"t2","path":"test/b","summary":"s","content":"c","tags":[],"permanent":true});
        let resp = handle_add(&id, &req, &paths, &emb);
        assert_eq!(resp["type"], "ok");

        let conn = db::open_db(&paths.db).unwrap();
        let entry_id = resp["entry_id"].as_str().unwrap();
        let perm: i64 = conn.query_row(
            &format!("SELECT permanent FROM entries WHERE id='{}'", entry_id), [], |r| r.get(0)
        ).unwrap();
        assert_eq!(perm, 1);
    }

    #[test]
    fn test_handle_add_replace_path() {
        let (_dir, paths, emb) = setup();
        let id = json!("t3");
        // Add first entry
        let req1 = json!({"method":"add","id":"t3","path":"test/c","summary":"old","content":"old","tags":[]});
        let r1 = handle_add(&id, &req1, &paths, &emb);
        let old_id = r1["entry_id"].as_str().unwrap().to_string();

        // Replace
        let req2 = json!({"method":"add","id":"t3b","path":"test/c","summary":"new","content":"new","tags":[],"replace_path":true});
        handle_add(&id, &req2, &paths, &emb);

        let conn = db::open_db(&paths.db).unwrap();
        let stale: i64 = conn.query_row(
            &format!("SELECT is_stale FROM entries WHERE id='{}'", old_id), [], |r| r.get(0)
        ).unwrap();
        assert_eq!(stale, 1, "old entry must be stale after replace_path");
    }

    #[test]
    fn test_handle_search_with_filters() {
        let (_dir, paths, emb) = setup();
        let id = json!("s1");
        // Add entries
        let req_add = json!({"method":"add","id":"s1","path":"src/auth","summary":"auth mod","content":"jwt","tags":["auth"]});
        handle_add(&id, &req_add, &paths, &emb);
        let req_add2 = json!({"method":"add","id":"s2","path":"docs/readme","summary":"docs","content":"readme","tags":["docs"]});
        handle_add(&id, &req_add2, &paths, &emb);

        // Search with path_prefix filter
        let conn = db::open_db(&paths.db).unwrap();
        let req = json!({"method":"search","id":"s3","query":"auth","path_prefix":"src/","mode":"fts"});
        let resp = handle_search(&id, &req, &conn, &emb);
        assert_eq!(resp["type"], "result");
        let entries = resp["entries"].as_array().unwrap();
        assert!(entries.iter().all(|e| e["path"].as_str().unwrap().starts_with("src/")));
    }

    #[test]
    fn test_handle_expire_basic() {
        let (_dir, paths, emb) = setup();
        let id = json!("e1");
        let req_add = json!({"method":"add","id":"e1","path":"test/x","summary":"s","content":"c","tags":[]});
        let r = handle_add(&id, &req_add, &paths, &emb);
        let entry_id = r["entry_id"].as_str().unwrap();

        let req = json!({"method":"expire","id":"e2","entry_id":entry_id});
        let resp = handle_expire(&id, &req, &paths, &emb);
        assert_eq!(resp["type"], "ok");
        assert_eq!(resp["expired"].as_str().unwrap(), entry_id);
    }

    #[test]
    fn test_handle_expire_permanent_guard() {
        let (_dir, paths, emb) = setup();
        let id = json!("pg1");
        let req_add = json!({"method":"add","id":"pg1","path":"test/perm","summary":"s","content":"c","tags":[],"permanent":true});
        let r = handle_add(&id, &req_add, &paths, &emb);
        let entry_id = r["entry_id"].as_str().unwrap();

        // Without force → error
        let req = json!({"method":"expire","id":"pg2","entry_id":entry_id});
        let resp = handle_expire(&id, &req, &paths, &emb);
        assert_eq!(resp["type"], "error");
        assert_eq!(resp["code"], "permanent_guard");

        // With force → ok
        let req2 = json!({"method":"expire","id":"pg3","entry_id":entry_id,"force":true});
        let resp2 = handle_expire(&id, &req2, &paths, &emb);
        assert_eq!(resp2["type"], "ok");
    }

    #[test]
    fn test_handle_compact() {
        let (_dir, paths, emb) = setup();
        let id = json!("c1");
        // Add 3 entries with same id → compact should squash
        for i in 0..3 {
            let ev = json!({"action":"upsert","table":"entries","id":"dup","path":"a","summary":format!("v{i}"),"content":"c","tags":[],"ts":"2024-01-01T00:00:00Z"});
            events::append_event(&paths.events, &ev).unwrap();
            let conn = db::open_db(&paths.db).unwrap();
            db::apply_event(&conn, &emb, &ev).unwrap();
        }

        let resp = handle_compact(&id, &paths);
        assert_eq!(resp["type"], "ok");
        assert_eq!(resp["before"], 3);
        assert_eq!(resp["after"], 1);
    }

    #[test]
    fn test_handle_test_add_and_tests() {
        let (_dir, paths, emb) = setup();
        let id = json!("ta1");
        let req = json!({"method":"test_add","id":"ta1","app":"myapp","name":"login test","protocol":"browser","config":"{}"});
        let resp = handle_test_add(&id, &req, &paths, &emb);
        assert_eq!(resp["type"], "ok");
        assert!(resp["test_id"].as_str().is_some());

        // List tests
        let req2 = json!({"method":"tests","id":"ta2","app":"myapp"});
        let conn = db::open_db(&paths.db).unwrap();
        let resp2 = handle_tests(&id, &req2, &conn);
        assert_eq!(resp2["type"], "result");
        assert_eq!(resp2["count"], 1);
    }

    #[test]
    fn test_handle_run() {
        let (_dir, paths, emb) = setup();
        let id = json!("r1");
        // Add test case first
        let req_tc = json!({"method":"test_add","id":"r1","app":"myapp","name":"t1","protocol":"browser","config":"{}"});
        let tc = handle_test_add(&id, &req_tc, &paths, &emb);
        let test_id = tc["test_id"].as_str().unwrap();

        let req = json!({"method":"run","id":"r2","test_id":test_id,"result":"pass","detail":"all green"});
        let resp = handle_run(&id, &req, &paths, &emb);
        assert_eq!(resp["type"], "ok");
        assert_eq!(resp["result"], "pass");
    }

    #[test]
    fn test_handle_import_upsert() {
        let (dir, paths, emb) = setup();
        let id = json!("imp1");

        // Add initial entry at path "test/imp"
        let req_add = json!({"method":"add","id":"imp1","path":"test/imp","summary":"v1","content":"c1","tags":["a"]});
        handle_add(&id, &req_add, &paths, &emb);

        // Write a seeds JSON file with a new entry at same path
        let seeds_path = dir.path().join("seeds.json");
        let seeds = json!([{"path":"test/imp","summary":"v2","content":"c2","tags":["b"]}]);
        fs::write(&seeds_path, serde_json::to_string(&seeds).unwrap()).unwrap();

        // Import with upsert=false → should skip (path already exists)
        let req = json!({"method":"import","id":"imp2","path":seeds_path.to_str().unwrap()});
        let resp = handle_import(&id, &req, &paths, &emb);
        assert_eq!(resp["type"], "ok");
        assert_eq!(resp["skipped"], 1);
        assert_eq!(resp["imported"], 0);

        // Import with upsert=true → should import
        let req2 = json!({"method":"import","id":"imp3","path":seeds_path.to_str().unwrap(),"upsert":true});
        let resp2 = handle_import(&id, &req2, &paths, &emb);
        assert_eq!(resp2["type"], "ok");
        assert_eq!(resp2["imported"], 1);
    }

    #[test]
    fn test_handle_rebuild() {
        let (_dir, paths, emb) = setup();
        let id = json!("rb1");

        // Add some entries via events
        let req = json!({"method":"add","id":"rb1","path":"test/rb","summary":"s","content":"c","tags":[]});
        handle_add(&id, &req, &paths, &emb);

        // Rebuild should recreate DB from events
        let resp = handle_rebuild(&id, &paths, &emb);
        assert_eq!(resp["type"], "ok");
        assert!(resp["rebuilt"].as_u64().unwrap() >= 1);

        // Verify entry still exists after rebuild
        let conn = db::open_db(&paths.db).unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE path='test/rb'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_handle_stale_check() {
        let (_dir, paths, emb) = setup();
        let id = json!("sc1");

        // Insert entry with a specific version_ref directly via events (bypasses git HEAD auto-capture)
        let ev = json!({
            "action":"upsert","table":"entries","id":"sc-entry",
            "path":"src/old.rs","summary":"old fn","content":"c","tags":[],
            "version_ref":"abc123","ts":"2024-01-01T00:00:00Z","session":"test"
        });
        events::append_event(&paths.events, &ev).unwrap();
        let conn = db::open_db(&paths.db).unwrap();
        db::apply_event(&conn, &emb, &ev).unwrap();

        // stale_check returns "result" type and "stale" + "review" arrays
        // In a tempdir (no git repo), git log fails gracefully → no entries flagged stale
        let req_sc = json!({"method":"stale_check","id":"sc2","files":["src/old.rs"]});
        let resp = handle_stale_check(&id, &req_sc, &conn, &paths);
        assert_eq!(resp["type"], "result");
        assert!(resp["stale"].as_array().is_some());
        assert!(resp["review"].as_array().is_some());
        assert_eq!(resp["checked"], 1);

        // stale_check with no files and no commits → error
        let req_bad = json!({"method":"stale_check","id":"sc3"});
        let resp_bad = handle_stale_check(&id, &req_bad, &conn, &paths);
        assert_eq!(resp_bad["type"], "error");
        assert_eq!(resp_bad["code"], "parse_error");
    }

    #[test]
    fn test_handle_stale_check_by_commit() {
        let (_dir, paths, emb) = setup();
        let id = json!("sc4");

        // Insert entry with a specific version_ref (commit SHA)
        let sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let ev = json!({
            "action":"upsert","table":"entries","id":"sc-commit-entry",
            "path":"architecture/normatix","summary":"normatix arch","content":"details","tags":[],
            "version_ref":sha,"ts":"2024-01-01T00:00:00Z","session":"test"
        });
        events::append_event(&paths.events, &ev).unwrap();
        let conn = db::open_db(&paths.db).unwrap();
        db::apply_event(&conn, &emb, &ev).unwrap();

        // Query by exact commit SHA → entry appears in review list
        let req = json!({"method":"stale_check","id":"sc5","commits":[sha]});
        let resp = handle_stale_check(&id, &req, &conn, &paths);
        assert_eq!(resp["type"], "result");
        let review = resp["review"].as_array().unwrap();
        assert_eq!(review.len(), 1);
        assert_eq!(review[0]["id"], "sc-commit-entry");
        assert_eq!(review[0]["version_ref"], sha);

        // Unknown SHA → empty review
        let req2 = json!({"method":"stale_check","id":"sc6","commits":["0000000000000000000000000000000000000000"]});
        let resp2 = handle_stale_check(&id, &req2, &conn, &paths);
        assert_eq!(resp2["type"], "result");
        assert_eq!(resp2["review"].as_array().unwrap().len(), 0);
    }
}
