//! kb — agent knowledge base CLI
//!
//! Manages agent-kb-events.jsonl (committed event log) and agent-kb.db
//! (local materialized SQLite cache). Modeled on `br` (beads).
//!
//! All writes acquire the agentic lock (.state/.lock) and append to the JSONL
//! event log before materializing to SQLite (write-through). `kb rebuild`
//! replays the full event log to reconstruct the DB from scratch.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use fs2::FileExt;
use rusqlite::{params, Connection};
use uuid::Uuid;

// ── Vector math ───────────────────────────────────────────────────────────────

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "kb", about = "Agent knowledge base CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add or update a knowledge entry
    Add {
        #[arg(long)]
        path: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        content: String,
        /// Comma-separated tags
        #[arg(long)]
        tags: String,
        #[arg(long)]
        version_ref: Option<String>,
        /// Entry ID (auto-generated UUID if omitted)
        #[arg(long)]
        id: Option<String>,
    },
    /// Mark an entry stale
    Expire {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Replay all events and rebuild agent-kb.db from scratch
    Rebuild,
    /// Search knowledge entries (default: hybrid FTS5 + semantic re-rank)
    Search {
        query: String,
        /// FTS5 keyword search only
        #[arg(long)]
        fts: bool,
        /// Semantic similarity search only
        #[arg(long)]
        semantic: bool,
    },
    /// Compact the event log (squash superseded events)
    Compact,
    /// List test cases
    Tests {
        #[arg(long)]
        app: Option<String>,
    },
    /// Add or update a test case
    TestAdd {
        #[arg(long)]
        app: String,
        #[arg(long)]
        name: String,
        /// browser | rust_tool
        #[arg(long)]
        protocol: String,
        /// JSON blob of test config
        #[arg(long)]
        config: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        version_ref: Option<String>,
    },
    /// Record a test run result
    Run {
        test_id: String,
        /// pass | fail
        #[arg(long)]
        result: String,
        #[arg(long)]
        adapter: Option<String>,
        #[arg(long)]
        detail: Option<String>,
    },
}

// ── Repo paths ────────────────────────────────────────────────────────────────

struct Paths {
    lock: PathBuf,
    events: PathBuf,
    db: PathBuf,
    fastembed_cache: PathBuf,
}

fn find_paths() -> Result<Paths> {
    // Walk up from cwd to find the repo root (has a .state/ directory).
    let cwd = std::env::current_dir()?;
    let mut dir: &Path = &cwd;
    loop {
        if dir.join(".state").is_dir() {
            return Ok(Paths {
                lock: dir.join(".state").join(".lock"),
                events: dir.join("agent-kb").join("agent-kb-events.jsonl"),
                db: dir.join("agent-kb").join("agent-kb.db"),
                fastembed_cache: fastembed_cache_dir(),
            });
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => bail!(
                "Could not find repo root (no .state/ directory in {} or any parent)",
                cwd.display()
            ),
        }
    }
}

fn fastembed_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("FASTEMBED_CACHE_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cache").join("fastembed")
}

// ── Agentic lock ──────────────────────────────────────────────────────────────

struct Lock(File);

fn acquire_lock(lock_path: &Path) -> Result<Lock> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .open(lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    f.lock_exclusive()
        .with_context(|| format!("acquire lock {}", lock_path.display()))?;
    Ok(Lock(f))
}

// ── Database ──────────────────────────────────────────────────────────────────

fn open_db(db_path: &Path) -> Result<Connection> {
    if let Some(p) = db_path.parent() {
        fs::create_dir_all(p)?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("open DB {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    ensure_schema(&conn)?;
    Ok(conn)
}

fn ensure_schema(conn: &Connection) -> Result<()> {
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

        -- Standalone FTS5 table (no external content= to avoid trigger complexity).
        -- id UNINDEXED lets us join back to entries without re-storing the text there.
        CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
            id UNINDEXED, path, summary, content, tags
        );

        -- Embedding store: raw float32 LE bytes, rowid-linked to entries.
        -- Cosine similarity computed in Rust (avoids sqlite-vec build issues).
        CREATE TABLE IF NOT EXISTS entries_emb (
            rowid    INTEGER PRIMARY KEY REFERENCES entries(rowid) ON DELETE CASCADE,
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

// ── Event log ─────────────────────────────────────────────────────────────────

fn append_event(events_path: &Path, event: &serde_json::Value) -> Result<()> {
    if let Some(p) = events_path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(events_path)
        .with_context(|| format!("open events {}", events_path.display()))?;
    writeln!(f, "{}", serde_json::to_string(event)?)?;
    Ok(())
}

fn read_events(events_path: &Path) -> Result<Vec<serde_json::Value>> {
    if !events_path.exists() {
        return Ok(vec![]);
    }
    let f = File::open(events_path)?;
    let reader = BufReader::new(f);
    let mut events = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse events line {}", i + 1))?;
        events.push(v);
    }
    Ok(events)
}

// ── Embeddings ────────────────────────────────────────────────────────────────

fn init_model(cache_dir: &Path) -> Result<TextEmbedding> {
    TextEmbedding::try_new(
        TextInitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(true),
    )
    .context("init fastembed BAAI/bge-small-en-v1.5")
}

fn embed(model: &mut TextEmbedding, text: &str) -> Result<Vec<f32>> {
    let mut vecs = model
        .embed(vec![text.to_string()], None)
        .context("generate embedding")?;
    vecs.pop().context("empty embedding result")
}

fn f32s_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

// ── Apply event to DB ─────────────────────────────────────────────────────────

fn apply_event(
    conn: &Connection,
    model: Option<&mut TextEmbedding>,
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

            // Sync FTS5 (delete existing, insert fresh).
            conn.execute("DELETE FROM entries_fts WHERE id=?1", params![id])?;
            conn.execute(
                "INSERT INTO entries_fts(id, path, summary, content, tags)
                 VALUES(?1,?2,?3,?4,?5)",
                params![id, path, summary, content, tags],
            )?;

            // Sync embedding store.
            if let Some(m) = model {
                let text = format!("{} {} {}", path, summary, content);
                let emb = embed(m, &text)?;
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

// ── Command handlers ──────────────────────────────────────────────────────────

fn cmd_add(
    paths: &Paths,
    path: String,
    summary: String,
    content: String,
    tags: String,
    version_ref: Option<String>,
    id: Option<String>,
) -> Result<()> {
    let _lock = acquire_lock(&paths.lock)?;
    let id = id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let tags_json: serde_json::Value =
        serde_json::json!(tags.split(',').map(|t| t.trim()).collect::<Vec<_>>());
    let ts = Utc::now().to_rfc3339();
    let session = std::env::var("OMC_SESSION_ID").unwrap_or_else(|_| "cli".to_string());

    let event = serde_json::json!({
        "action": "upsert",
        "table": "entries",
        "id": id,
        "path": path,
        "summary": summary,
        "content": content,
        "tags": tags_json,
        "version_ref": version_ref,
        "ts": ts,
        "session": session,
    });

    append_event(&paths.events, &event)?;

    let conn = open_db(&paths.db)?;
    let mut model = init_model(&paths.fastembed_cache).ok();
    apply_event(&conn, model.as_mut(), &event)?;

    println!("added  {} ({})", path, id);
    Ok(())
}

fn cmd_expire(paths: &Paths, id: String, reason: Option<String>) -> Result<()> {
    let _lock = acquire_lock(&paths.lock)?;
    let ts = Utc::now().to_rfc3339();
    let session = std::env::var("OMC_SESSION_ID").unwrap_or_else(|_| "cli".to_string());

    let event = serde_json::json!({
        "action": "expire",
        "table": "entries",
        "id": id,
        "reason": reason,
        "ts": ts,
        "session": session,
    });

    append_event(&paths.events, &event)?;
    let conn = open_db(&paths.db)?;
    apply_event(&conn, None, &event)?;

    println!("expired {}", id);
    Ok(())
}

fn cmd_rebuild(paths: &Paths) -> Result<()> {
    let _lock = acquire_lock(&paths.lock)?;

    // Drop the DB so we start from a clean slate.
    if paths.db.exists() {
        fs::remove_file(&paths.db)?;
        let db_str = paths.db.to_string_lossy();
        let _ = fs::remove_file(format!("{}-wal", db_str));
        let _ = fs::remove_file(format!("{}-shm", db_str));
    }

    let conn = open_db(&paths.db)?;
    let mut model = init_model(&paths.fastembed_cache)?;
    let events = read_events(&paths.events)?;

    eprintln!("replaying {} events…", events.len());
    for event in &events {
        apply_event(&conn, Some(&mut model), event)
            .with_context(|| format!("apply event: {}", event))?;
    }

    eprintln!("rebuild complete");
    Ok(())
}

fn cmd_search(paths: &Paths, query: String, fts: bool, semantic: bool) -> Result<()> {
    // If neither flag: hybrid (both).
    let do_fts = fts || (!fts && !semantic);
    let do_semantic = semantic || (!fts && !semantic);

    let conn = open_db(&paths.db)?;

    if do_fts {
        println!("=== FTS results ===");
        let mut stmt = conn.prepare(
            "SELECT e.id, e.path, e.summary, e.tags
             FROM entries_fts f
             JOIN entries e ON e.id = f.id
             WHERE f.entries_fts MATCH ?1 AND e.is_stale=0
             ORDER BY rank
             LIMIT 10",
        )?;
        let mut count = 0;
        let rows = stmt.query_map(params![query], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, path, summary, tags) = row?;
            println!("  [{path}] {summary}  tags={tags}  id={id}");
            count += 1;
        }
        if count == 0 {
            println!("  (no results)");
        }
    }

    if do_semantic {
        println!("=== Semantic results ===");
        let mut model = init_model(&paths.fastembed_cache)?;
        let q_emb = embed(&mut model, &query)?;

        // Load all non-stale entries with embeddings; compute cosine similarity in memory.
        let mut stmt = conn.prepare(
            "SELECT e.id, e.path, e.summary, e.tags, emb.embedding
             FROM entries_emb emb
             JOIN entries e ON e.rowid = emb.rowid
             WHERE e.is_stale = 0",
        )?;
        let mut candidates: Vec<(f32, String, String, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .map(|(id, path, summary, tags, blob)| {
                let emb: Vec<f32> = blob
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                let sim = cosine_similarity(&q_emb, &emb);
                (sim, id, path, summary, tags)
            })
            .collect();

        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<_> = candidates.into_iter().take(10).collect();

        if top.is_empty() {
            println!("  (no results)");
        } else {
            for (sim, id, path, summary, tags) in top {
                println!("  [{path}] {summary}  sim={sim:.4}  tags={tags}  id={id}");
            }
        }
    }

    Ok(())
}

fn cmd_compact(paths: &Paths) -> Result<()> {
    let _lock = acquire_lock(&paths.lock)?;
    let events = read_events(&paths.events)?;
    let original_count = events.len();

    // For entries/test_cases: keep only the last upsert per id.
    // For run_history inserts: keep all (append-only).
    // Expire events: fold into the entry upsert (mark is_stale=true field).
    use std::collections::HashMap;
    let mut entry_last: HashMap<String, usize> = HashMap::new();
    let mut test_last: HashMap<String, usize> = HashMap::new();
    let mut expire_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut run_indices: Vec<usize> = Vec::new();

    for (i, ev) in events.iter().enumerate() {
        let action = ev["action"].as_str().unwrap_or("");
        let table = ev["table"].as_str().unwrap_or("");
        let id = ev["id"].as_str().unwrap_or("").to_string();
        match (action, table) {
            ("upsert", "entries") => {
                entry_last.insert(id, i);
            }
            ("expire", "entries") => {
                expire_ids.insert(id);
            }
            ("upsert", "test_cases") => {
                test_last.insert(id, i);
            }
            ("insert", "run_history") => {
                run_indices.push(i);
            }
            _ => {}
        }
    }

    let mut compacted: Vec<serde_json::Value> = Vec::new();

    // Entry upserts (ordered by original position), with stale flag folded in.
    let mut entry_pairs: Vec<(usize, &str)> = entry_last
        .iter()
        .map(|(id, &i)| (i, id.as_str()))
        .collect();
    entry_pairs.sort_by_key(|(i, _)| *i);
    for (i, id) in entry_pairs {
        let mut ev = events[i].clone();
        if expire_ids.contains(id) {
            ev["is_stale"] = serde_json::json!(true);
        }
        compacted.push(ev);
    }

    // Test case upserts.
    let mut test_pairs: Vec<(usize, String)> = test_last.into_iter().map(|(id, i)| (i, id)).collect();
    test_pairs.sort_by_key(|(i, _)| *i);
    for (i, _) in test_pairs {
        compacted.push(events[i].clone());
    }

    // Run history (all, original order).
    for i in run_indices {
        compacted.push(events[i].clone());
    }

    let tmp = paths.events.with_extension("jsonl.tmp");
    {
        let mut f = File::create(&tmp)?;
        for ev in &compacted {
            writeln!(f, "{}", serde_json::to_string(ev)?)?;
        }
    }
    fs::rename(&tmp, &paths.events)?;

    println!(
        "compacted: {} events → {}",
        original_count,
        compacted.len()
    );
    Ok(())
}

fn cmd_tests(paths: &Paths, app: Option<String>) -> Result<()> {
    let conn = open_db(&paths.db)?;
    let mut count = 0;

    if let Some(ref a) = app {
        let mut stmt = conn.prepare(
            "SELECT id, app, name, protocol FROM test_cases
             WHERE app=?1 AND is_stale=0 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![a], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, app, name, proto) = row?;
            println!("{app}/{name}  [{proto}]  id={id}");
            count += 1;
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, app, name, protocol FROM test_cases
             WHERE is_stale=0 ORDER BY app, name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, app, name, proto) = row?;
            println!("{app}/{name}  [{proto}]  id={id}");
            count += 1;
        }
    }

    if count == 0 {
        println!("(no test cases)");
    }
    Ok(())
}

fn cmd_test_add(
    paths: &Paths,
    app: String,
    name: String,
    protocol: String,
    config: String,
    id: Option<String>,
    version_ref: Option<String>,
) -> Result<()> {
    let _lock = acquire_lock(&paths.lock)?;
    let id = id.unwrap_or_else(|| format!("{}-{}", app, name.replace(' ', "-")));
    let ts = Utc::now().to_rfc3339();
    let session = std::env::var("OMC_SESSION_ID").unwrap_or_else(|_| "cli".to_string());

    let event = serde_json::json!({
        "action": "upsert",
        "table": "test_cases",
        "id": id,
        "app": app,
        "name": name,
        "protocol": protocol,
        "config": config,
        "version_ref": version_ref,
        "ts": ts,
        "session": session,
    });

    append_event(&paths.events, &event)?;
    let conn = open_db(&paths.db)?;
    apply_event(&conn, None, &event)?;

    println!("added test case  {}/{} ({})", app, name, id);
    Ok(())
}

fn cmd_run(
    paths: &Paths,
    test_id: String,
    result: String,
    adapter: Option<String>,
    detail: Option<String>,
) -> Result<()> {
    if result != "pass" && result != "fail" {
        bail!("--result must be 'pass' or 'fail', got: {}", result);
    }
    let _lock = acquire_lock(&paths.lock)?;
    let ts = Utc::now().to_rfc3339();
    let session = std::env::var("OMC_SESSION_ID").unwrap_or_else(|_| "cli".to_string());
    let run_id = Uuid::new_v4().to_string();

    let event = serde_json::json!({
        "action": "insert",
        "table": "run_history",
        "test_id": test_id,
        "result": result,
        "adapter": adapter,
        "detail": detail,
        "ts": ts,
        "run_id": run_id,
        "session": session,
    });

    append_event(&paths.events, &event)?;
    let conn = open_db(&paths.db)?;
    apply_event(&conn, None, &event)?;

    println!("recorded run {}  {test_id} → {result}", &run_id[..8]);
    Ok(())
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = find_paths()?;

    match cli.command {
        Command::Add {
            path,
            summary,
            content,
            tags,
            version_ref,
            id,
        } => cmd_add(&paths, path, summary, content, tags, version_ref, id),

        Command::Expire { id, reason } => cmd_expire(&paths, id, reason),

        Command::Rebuild => cmd_rebuild(&paths),

        Command::Search { query, fts, semantic } => cmd_search(&paths, query, fts, semantic),

        Command::Compact => cmd_compact(&paths),

        Command::Tests { app } => cmd_tests(&paths, app),

        Command::TestAdd {
            app,
            name,
            protocol,
            config,
            id,
            version_ref,
        } => cmd_test_add(&paths, app, name, protocol, config, id, version_ref),

        Command::Run {
            test_id,
            result,
            adapter,
            detail,
        } => cmd_run(&paths, test_id, result, adapter, detail),
    }
}
