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
        "expire" => handle_expire(&id, &req, paths, emb),
        "stale_check" => handle_stale_check(&id, &req, paths),
        "compact" => handle_compact(&id, paths),
        "reembed" => handle_reembed(&id, &req, paths, emb),
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
    let path_prefix = req.get("path_prefix").and_then(|v| v.as_str()).map(|s| s.to_string());
    let tag_filter = req.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());

    let opts = db::SearchOptions {
        limit,
        do_fts: mode == "fts" || mode == "hybrid",
        do_semantic: mode == "semantic" || mode == "hybrid",
        path_prefix,
        tag_filter,
    };

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    // Cap content per entry to prevent port line buffer overflow (10MB limit).
    // 8000 chars per entry * 50 entries = 400KB typical, well under the limit.
    const MAX_CONTENT_CHARS: usize = 8000;

    match db::search_entries(&conn, emb, &query, &opts) {
        Ok(results) => {
            let entries: Vec<Value> = results
                .into_iter()
                .map(|e| {
                    let tags: Value =
                        serde_json::from_str(&e.tags).unwrap_or(Value::Array(vec![]));
                    let content = if e.content.len() > MAX_CONTENT_CHARS {
                        format!("{}...(truncated)", &e.content[..MAX_CONTENT_CHARS])
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
            stmt.query_map(params![path], |r| r.get(0))
                .unwrap_or_else(|_| panic!("query failed"))
                .filter_map(|r| r.ok())
                .collect()
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
        "version_ref": null,
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
        let progress = json!({"id":id,"type":"progress","processed":i+1,"total":total});
        println!("{progress}");
        let _ = io::stdout().flush();

        if let Err(e) = db::apply_event(&conn, emb, event) {
            return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()});
        }
    }

    json!({"id": id, "type": "ok", "rebuilt": total})
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

    let candidates: Vec<(i64, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap_or_else(|_| panic!("query failed"))
        .filter_map(|r| r.ok())
        .collect();

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
    let evts_before = match events::read_events(&paths.events) {
        Ok(e) => e.len(),
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let compact_cmd = crate::commands::compact::Compact;
    if let Err(e) = compact_cmd.execute_with_paths(paths) {
        return json!({"id":id,"type":"error","code":"compact_error","message":e.to_string()});
    }

    let evts_after = match events::read_events(&paths.events) {
        Ok(e) => e.len(),
        Err(_) => 0,
    };

    json!({"id": id, "type": "ok", "before": evts_before, "after": evts_after})
}

fn handle_stale_check(id: &Value, req: &Value, paths: &config::Paths) -> Value {
    let files: Vec<String> = match req.get("files") {
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        _ => return json!({"id":id,"type":"error","code":"parse_error","message":"missing files array"}),
    };

    let conn = match db::open_db(&paths.db) {
        Ok(c) => c,
        Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
    };

    let repo_root = config::git_repo_root();
    let mut stale_entries: Vec<Value> = Vec::new();

    for file in &files {
        let rel_path = if let Some(ref root) = repo_root {
            let p = std::path::Path::new(file);
            if p.is_absolute() {
                p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
            } else {
                file.clone()
            }
        } else {
            file.clone()
        };

        let mut stmt = match conn.prepare(
            "SELECT id, summary, version_ref, path FROM entries
             WHERE (path = ?1 OR path LIKE '%' || ?1 OR ?1 LIKE '%' || path)
               AND version_ref IS NOT NULL AND is_stale = 0",
        ) {
            Ok(s) => s,
            Err(e) => return json!({"id":id,"type":"error","code":"db_error","message":e.to_string()}),
        };

        let rows: Vec<(String, String, String, String)> = stmt
            .query_map(params![rel_path], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3).unwrap_or_else(|_| rel_path.clone()),
                ))
            })
            .unwrap_or_else(|_| panic!("query failed"))
            .filter_map(|r| r.ok())
            .collect();

        for (entry_id, summary, version_ref, stored_path) in rows {
            let mut cmd = std::process::Command::new("git");
            if let Some(ref root) = repo_root {
                cmd.arg("-C").arg(root);
            }
            cmd.args(["log", "--oneline", &format!("{}..HEAD", version_ref), "--", &stored_path]);
            if let Ok(out) = cmd.output() {
                if out.status.success() && !out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
                    let commits = std::str::from_utf8(&out.stdout).unwrap_or("").lines().count();
                    stale_entries.push(json!({
                        "id": entry_id,
                        "path": stored_path,
                        "summary": summary,
                        "version_ref": version_ref,
                        "commits_behind": commits,
                    }));
                }
            }
        }
    }

    json!({"id": id, "type": "result", "stale": stale_entries, "checked": files.len()})
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
