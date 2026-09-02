//! Database operations

use crate::components::embedder::Embedder;
use crate::components::verification::{RelocationPolicy, VerificationOutcome};
use crate::models::{
    blob_to_f32s, cosine_similarity, decode_emb_blob, decode_f16_blob_into, f32s_to_blob,
    f32s_to_f16_blob, Evidence, VerificationStatus, EMB_DIMS,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Resource caps (br-h9g, security I2)
// ---------------------------------------------------------------------------
//
// The MCP entry points are reachable by any local agent that speaks the port
// protocol, so request inputs must be treated as untrusted. Two amplification
// vectors motivated these caps:
//
//   1. `limit` and `inline_verify_k` together gate how many entries get
//      verified inline. Each verified entry spawns one OS thread per evidence
//      row via std::thread::scope, so the worst-case thread fan-out is
//      `limit * evidence_rows_per_entry`. Without caps, a single malicious
//      request could exhaust the host's thread budget.
//
//   2. `evidence` rows per entry are operator-authored today but could be
//      bulk-imported tomorrow; bounding the per-entry fetch prevents one
//      pathological entry from dominating a search response (and its
//      verification fan-out).
//
// Values are deliberately conservative — they sit well above any observed
// agent workflow (typical limit=10, inline_verify_k=10, evidence rows ~5)
// while keeping worst-case fan-out at 100 * 200 = 20k cited rows / 20 * 200
// = 4k verification threads, which the test host tolerates.
pub const MAX_LIMIT: usize = 100;

/// Relocation policy on the interactive search path — pinned to
/// [`RelocationPolicy::Never`].
///
/// Pre-mortem S2 (`.omc/plans/kb-delivery.md` §6): the verification pool is
/// bounded and `tx_work.send` blocks the producer once `pool_size * 2` tasks are
/// in flight. A relocation walking a tree occupies one worker for its whole
/// duration, so with `pool_size` such units the entire drain stalls behind the
/// slowest one — mean and p95 stay flat while the tail explodes. Relocation
/// belongs on the stale-check/audit lane, never here.
///
/// Asserted by `tests/citation_relocation.rs` and
/// `tests/relocation_tail_latency.rs`.
pub const SEARCH_PATH_RELOCATION_POLICY: RelocationPolicy = RelocationPolicy::Never;

/// Entry field caps — MUST match the schema CHECK constraints on `entries`
/// (SQLite LENGTH counts characters on TEXT, hence char-based clamping).
pub const MAX_SUMMARY_CHARS: usize = 200;
pub const MAX_ENTRY_CONTENT_CHARS: usize = 10_000;

/// Clamp a field to `max` chars, warning loudly when data is dropped.
/// Deterministic — replaying the same event always yields the same row.
fn clamp_chars<'a>(s: &'a str, max: usize, field: &str, id: &str) -> std::borrow::Cow<'a, str> {
    if s.chars().count() <= max {
        std::borrow::Cow::Borrowed(s)
    } else {
        eprintln!(
            "kb: WARNING entry {id}: legacy {field} exceeds {max} chars — clamped on replay \
             (pre-cap event; the JSONL retains the full value)"
        );
        std::borrow::Cow::Owned(s.chars().take(max).collect())
    }
}
pub const MAX_INLINE_VERIFY_K: usize = 20;
pub const MAX_EVIDENCE_ROWS_PER_ENTRY: usize = 200;
pub const MAX_PER_ENTRY_BYTES: usize = 8 * 1024 * 1024; // br-und: 8 MiB per entry

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

/// Delete expired peer edges and orphaned graphs.
pub fn sweep_expired_peers(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM peers WHERE expires_at IS NOT NULL AND expires_at < datetime('now');
         DELETE FROM graphs WHERE id NOT IN (SELECT DISTINCT graph_id FROM peers WHERE graph_id IS NOT NULL);",
    )?;
    Ok(())
}

/// Schema generation of THIS binary. Bump when derived-state shape changes
/// in a way that requires replaying the event log (new tables/lanes whose
/// rows only materialize through apply_event, changed embedding semantics).
///
/// v1: implicit — every DB created before the stamp existed.
/// v2: cues + kb_meta tables, cue rows materialized from upsert events.
pub const SCHEMA_VERSION: i64 = 2;

/// True when the DB carries the current schema_version stamp.
///
/// Only fresh DBs (created by `open_db`/`open_db_memory` from nothing) and
/// rebuild outputs are stamped — a pre-existing DB opened by a newer binary
/// gets missing TABLES from `ensure_schema` but keeps reading as obsolete
/// until a rebuild replays the log into them.
pub fn schema_is_current(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT value FROM kb_meta WHERE key='schema_version'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse::<i64>().ok())
    .is_some_and(|v| v >= SCHEMA_VERSION)
}

fn stamp_schema_version(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO kb_meta(key, value) VALUES('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// Open (or create) the SQLite database at the given path.
pub fn open_db(db_path: &Path) -> Result<Connection> {
    if let Some(p) = db_path.parent() {
        fs::create_dir_all(p)?;
    }
    let conn =
        Connection::open(db_path).with_context(|| format!("open DB {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    // Fresh-DB detection BEFORE ensure_schema: a DB with no entries table was
    // just created and gets stamped current; a pre-existing DB keeps whatever
    // stamp it has (none = legacy = obsolete) so callers can force a rebuild.
    let is_fresh: bool = !table_exists(&conn, "entries");
    ensure_schema(&conn)?;
    if is_fresh {
        stamp_schema_version(&conn)?;
    }
    sweep_expired_peers(&conn)?;
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Open an in-memory database with the full schema.
pub fn open_db_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    ensure_schema(&conn)?;
    stamp_schema_version(&conn)?;
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

        -- FTS5 content table (migration target). Triggers keep it in sync with entries.
        -- Old contentless entries_fts remains active until deprecation (br-fts5-content-migration-31t.8).
        -- Minimum SQLite version: 3.20 (FTS5 content= + trigger semantics).
        CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts_v2 USING fts5(
            id UNINDEXED, path, summary, content, tags,
            content='entries', content_rowid='rowid'
        );

        -- AFTER INSERT: populate FTS v2 for non-stale entries only.
        -- SQLite >= 3.20 required for FTS5 content='entries' + trigger semantics.
        CREATE TRIGGER IF NOT EXISTS entries_ai_fts_v2 AFTER INSERT ON entries
        WHEN new.is_stale = 0
        BEGIN
            INSERT INTO entries_fts_v2(rowid, id, path, summary, content, tags)
            VALUES (new.rowid, new.id, new.path, new.summary, new.content, new.tags);
        END;

        -- AFTER UPDATE: remove old FTS v2 entry only if it was indexed (is_stale=0);
        -- re-insert only if new row is not stale.
        -- Guard with SELECT...WHERE to avoid double-delete corruption (SQLITE_CORRUPT_VTAB)
        -- when an expired (stale) entry is re-upserted.
        CREATE TRIGGER IF NOT EXISTS entries_au_fts_v2 AFTER UPDATE ON entries
        BEGIN
            INSERT INTO entries_fts_v2(entries_fts_v2, rowid, id, path, summary, content, tags)
            SELECT 'delete', old.rowid, old.id, old.path, old.summary, old.content, old.tags
            WHERE old.is_stale = 0;
            INSERT INTO entries_fts_v2(rowid, id, path, summary, content, tags)
            SELECT new.rowid, new.id, new.path, new.summary, new.content, new.tags
            WHERE new.is_stale = 0;
        END;

        -- AFTER DELETE: remove from FTS v2 only if the row was indexed.
        CREATE TRIGGER IF NOT EXISTS entries_ad_fts_v2 AFTER DELETE ON entries
        BEGIN
            INSERT INTO entries_fts_v2(entries_fts_v2, rowid, id, path, summary, content, tags)
            SELECT 'delete', old.rowid, old.id, old.path, old.summary, old.content, old.tags
            WHERE old.is_stale = 0;
        END;

        CREATE TABLE IF NOT EXISTS entries_emb (
            rowid    INTEGER PRIMARY KEY,
            embedding BLOB NOT NULL
        );

        -- Cue anchors (Memora pickup .4): agent-supplied semantic entry points,
        -- embedded per row. Rows are replaced wholesale on entry upsert and
        -- removed on expire (agent-kb/tla/CueBatch.tla S2/S3).
        CREATE TABLE IF NOT EXISTS cues (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id  TEXT NOT NULL,
            cue       TEXT NOT NULL,
            embedding BLOB
        );
        CREATE INDEX IF NOT EXISTS idx_cues_entry ON cues(entry_id);

        -- Derived-state metadata (NOT event-sourced; rebuilt DBs start empty).
        -- embed_text_mode: which KB_EMBED_TEXT vintage entries_emb was built
        -- with — used to warn on mixed-vintage writes.
        CREATE TABLE IF NOT EXISTS kb_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
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
    let _ =
        conn.execute_batch("ALTER TABLE entries ADD COLUMN evidence_status TEXT DEFAULT 'n/a';");
    // Migration: add session_id column for Phase 5 audit confidence per-session weighting.
    let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN session_id TEXT;");
    // Migration: add run_id to audit_runs for Phase 5 idempotency (INSERT OR IGNORE on unique index).
    let _ = conn.execute_batch("ALTER TABLE audit_runs ADD COLUMN run_id TEXT;");
    // Traffic-weighted audit sampling: arm metadata lives on the sampled
    // candidate and is joined by audit_report after a verdict is recorded.
    let _ = conn.execute_batch(
        "ALTER TABLE audit_run_candidates ADD COLUMN arm TEXT NOT NULL DEFAULT 'uniform';",
    );
    let _ = conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_audit_runs_run_entry ON audit_runs(run_id, entry_id);"
    );
    // Migration: add updated_at to source_weights for Phase 5 weight tracking.
    let _ = conn.execute_batch(
        "ALTER TABLE source_weights ADD COLUMN updated_at TEXT DEFAULT (datetime('now'));",
    );
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
        CREATE INDEX IF NOT EXISTS idx_evidence_citation_path ON evidence(citation_path);

        CREATE TABLE IF NOT EXISTS audit_runs (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id       TEXT,
            entry_id     TEXT NOT NULL,
            audited_at   TEXT DEFAULT (datetime('now')),
            verdict      TEXT NOT NULL CHECK(verdict IN ('true','false')),
            evidence_ref TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_audit_runs_run_entry
            ON audit_runs(run_id, entry_id);
        CREATE TABLE IF NOT EXISTS source_weights (
            kind        TEXT NOT NULL,
            session_id  TEXT NOT NULL DEFAULT '__GLOBAL__',
            successes   INTEGER NOT NULL DEFAULT 0,
            failures    INTEGER NOT NULL DEFAULT 0,
            updated_at  TEXT DEFAULT (datetime('now')),
            PRIMARY KEY (kind, session_id)
        );
        CREATE TABLE IF NOT EXISTS audit_run_candidates (
            run_id     TEXT NOT NULL,
            entry_id   TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            arm        TEXT NOT NULL DEFAULT 'uniform',
            PRIMARY KEY (run_id, entry_id)
        );
        "#,
    )?;
    // AC-P6: peer graph tables (additive; no-op on already-migrated DBs).
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS graphs (
            id          TEXT PRIMARY KEY,
            graph_type  TEXT NOT NULL CHECK(graph_type IN ('epic','dep')),
            epic_slug   TEXT,
            source_repo TEXT NOT NULL,
            created_at  TEXT DEFAULT (datetime('now')),
            expires_at  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_graphs_source_repo ON graphs(source_repo);

        CREATE TABLE IF NOT EXISTS peers (
            id          TEXT PRIMARY KEY,
            graph_id    TEXT REFERENCES graphs(id),
            source_repo TEXT NOT NULL,
            target_repo TEXT NOT NULL,
            edge_type   TEXT NOT NULL DEFAULT 'member',
            epic_slug   TEXT,
            created_at  TEXT DEFAULT (datetime('now')),
            expires_at  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_peers_source_repo ON peers(source_repo);
        CREATE INDEX IF NOT EXISTS idx_peers_target_repo ON peers(target_repo);
        CREATE INDEX IF NOT EXISTS idx_peers_epic_slug   ON peers(epic_slug);
        "#,
    )?;
    // AC-P6 migrations: add cross-repo provenance columns to entries.
    let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN origin_repo TEXT;");
    let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN cross_repo_epic TEXT;");
    // Deprecation gate: tracks the four event-gated signals required before dropping entries_fts.
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS fts5_deprecation_gate (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT OR IGNORE INTO fts5_deprecation_gate(key, value) VALUES
            ('post_cutover_writes', '0'),
            ('rollback_invocations', '0'),
            ('parity_rerun_divergence', '-1'),
            ('rollback_drill_passed', '0');
        "#,
    )?;
    maybe_drop_contentless_fts(conn)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct CitingEntry {
    pub id: String,
    pub path: String,
    pub summary: String,
    pub evidence: Vec<Evidence>,
}

fn entries_citing_sql() -> &'static str {
    "SELECT e.id, e.path, e.summary,
            ev.id, ev.kind, ev.citation_path, ev.citation_sha, ev.citation_hash,
            ev.citation_excerpt, ev.derived_from, ev.recorded_at
     FROM (
         SELECT id, entry_id, kind, citation_path, citation_sha, citation_hash,
                citation_excerpt, derived_from, recorded_at
         FROM evidence
         WHERE citation_path = ?1
         UNION ALL
         SELECT id, entry_id, kind, citation_path, citation_sha, citation_hash,
                citation_excerpt, derived_from, recorded_at
         FROM evidence
         WHERE citation_path >= ?2 AND citation_path < ?3
     ) ev
     JOIN entries e ON e.id = ev.entry_id
     WHERE e.is_stale = 0
     ORDER BY e.path, e.id, ev.citation_path, ev.id"
}

pub fn entries_citing(conn: &Connection, path_query: &str) -> Result<Vec<CitingEntry>> {
    let lower = format!("{path_query}:");
    let upper = format!("{path_query};");
    let mut stmt = conn.prepare(entries_citing_sql())?;
    let rows: Vec<(String, String, String, Evidence)> = stmt
        .query_map(params![path_query, lower, upper], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                Evidence {
                    id: r.get(3)?,
                    entry_id: r.get(0)?,
                    kind: r.get(4)?,
                    citation_path: r.get(5)?,
                    citation_sha: r.get(6)?,
                    citation_hash: r.get(7).unwrap_or_default(),
                    citation_excerpt: r.get(8)?,
                    derived_from: r.get(9)?,
                    recorded_at: r.get(10)?,
                },
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out: Vec<CitingEntry> = Vec::new();
    for (id, path, summary, evidence) in rows {
        if let Some(last) = out.last_mut() {
            if last.id == id {
                last.evidence.push(evidence);
                continue;
            }
        }
        out.push(CitingEntry {
            id,
            path,
            summary,
            evidence: vec![evidence],
        });
    }
    Ok(out)
}

/// Set a deprecation gate signal (key → value as string).
pub fn set_deprecation_gate(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO fts5_deprecation_gate(key, value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}

/// Drop entries_fts (contentless) and its write triggers when all four
/// deprecation gate signals are met:
///   1. post_cutover_writes >= 1000
///   2. rollback_invocations == 0
///   3. parity_rerun_divergence == 0  (must have been set by a parity rerun)
///   4. rollback_drill_passed == 1
///
/// Idempotent: if entries_fts no longer exists the function is a no-op.
pub fn maybe_drop_contentless_fts(conn: &Connection) -> Result<()> {
    // Fast path: table already dropped.
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entries_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        return Ok(());
    }

    // Read gate signals.
    let gate_val = |key: &str| -> i64 {
        conn.query_row(
            "SELECT value FROM fts5_deprecation_gate WHERE key=?1",
            [key],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(-1)
    };

    let post_cutover_writes = gate_val("post_cutover_writes");
    let rollback_invocations = gate_val("rollback_invocations");
    let parity_rerun_divergence = gate_val("parity_rerun_divergence");
    let rollback_drill_passed = gate_val("rollback_drill_passed");

    let gate_open = post_cutover_writes >= 1000
        && rollback_invocations == 0
        && parity_rerun_divergence == 0
        && rollback_drill_passed == 1;

    if !gate_open {
        return Ok(());
    }

    // All gates satisfied — drop contentless FTS table and its write triggers.
    // The FTS5 'delete' trigger (entries_ai_fts) was created by earlier schema
    // versions; drop it too if it exists.
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS entries_fts;
        DROP TRIGGER IF EXISTS entries_ai_fts;
        DROP TRIGGER IF EXISTS entries_au_fts;
        DROP TRIGGER IF EXISTS entries_ad_fts;
        "#,
    )?;
    conn.execute_batch("VACUUM;")?;
    eprintln!(
        "kb: fts5_v1_table_dropped post_cutover_writes={post_cutover_writes} \
         parity_rerun_divergence={parity_rerun_divergence} \
         rollback_drill_passed={rollback_drill_passed}"
    );
    Ok(())
}

fn increment_post_cutover_writes(conn: &Connection) {
    let _ = conn.execute(
        "UPDATE fts5_deprecation_gate SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='post_cutover_writes'",
        [],
    );
}

/// Which text is embedded per entry. Controlled by `KB_EMBED_TEXT`:
/// - unset / `"full"`: path + summary + content (legacy default)
/// - `"abstraction"`: path + summary + flattened tags — the Memora principle
///   of indexing abstractions, not content. Content stays FTS-indexed.
///
/// Switching modes requires a full `kb reembed --all` (or rebuild) — mixed
/// vintages in entries_emb make cosine scores incomparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedTextMode {
    Full,
    Abstraction,
}

impl EmbedTextMode {
    /// Parse a mode string; anything unrecognized falls back to Full so a
    /// typo can never silently drop content from every new embedding.
    pub fn parse(v: Option<&str>) -> Self {
        match v {
            Some("abstraction") => EmbedTextMode::Abstraction,
            _ => EmbedTextMode::Full,
        }
    }

    pub fn from_env() -> Self {
        Self::parse(std::env::var("KB_EMBED_TEXT").ok().as_deref())
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EmbedTextMode::Full => "full",
            EmbedTextMode::Abstraction => "abstraction",
        }
    }
}

/// Stamp/verify the embed-text mode vintage of `entries_emb`.
///
/// The first embed write stamps the active mode into `kb_meta`; later writes
/// under a DIFFERENT mode warn loudly — mixed-vintage embeddings make cosine
/// scores incomparable and degrade ranking with no error (review finding).
/// The stamp resets only when derived state is rebuilt from scratch (rebuild
/// replays into a fresh DB), which is exactly when a mode switch is safe.
pub fn check_embed_mode_vintage(conn: &Connection, mode: EmbedTextMode) {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM kb_meta WHERE key='embed_text_mode'",
            [],
            |r| r.get(0),
        )
        .ok();
    match stored {
        None => {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO kb_meta(key, value) VALUES('embed_text_mode', ?1)",
                params![mode.as_str()],
            );
        }
        Some(v) if v != mode.as_str() => {
            eprintln!(
                "kb: WARNING embed_text_mode mismatch — entries_emb built with '{v}', \
                 KB_EMBED_TEXT now '{}'; cosine scores are incomparable across vintages. \
                 Run `kb rebuild` (or clear entries_emb + `kb reembed`) to converge.",
                mode.as_str()
            );
        }
        _ => {}
    }
}

/// Flatten a JSON-array tags string (`["a","b"]`) to plain words for
/// embedding. Non-JSON tag strings pass through unchanged.
fn tags_for_embedding(tags: &str) -> String {
    match serde_json::from_str::<Vec<String>>(tags) {
        Ok(v) => v.join(" "),
        Err(_) => tags.to_string(),
    }
}

/// Build the text embedded for an entry. Single source of truth shared by
/// apply_event, `kb reembed`, and the MCP reembed handler — the three sites
/// must never diverge or cosine scores become vintage-dependent.
pub fn entry_embed_text(
    mode: EmbedTextMode,
    path: &str,
    summary: &str,
    content: &str,
    tags: &str,
) -> String {
    match mode {
        EmbedTextMode::Full => format!("{} {} {}", path, summary, content),
        EmbedTextMode::Abstraction => {
            format!("{} {} {}", path, summary, tags_for_embedding(tags))
        }
    }
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
            // Legacy-log tolerance (br-23b-handoff-tomorrow-y0a): events
            // written before the length caps existed can exceed the schema
            // CHECKs. Clamp deterministically instead of aborting — replay
            // (rebuild is the RECOVERY path) must never fail on data the log
            // actually contains. New writes are rejected with a clear error
            // in kb_core::add before any event is appended.
            let summary = clamp_chars(summary, MAX_SUMMARY_CHARS, "summary", id);
            let content = clamp_chars(content, MAX_ENTRY_CONTENT_CHARS, "content", id);
            let (summary, content) = (summary.as_ref(), content.as_ref());
            let tags = event["tags"].to_string();
            let version_ref = event["version_ref"].as_str();
            let ts = event["ts"].as_str().unwrap_or("");
            let permanent = event["permanent"].as_bool().unwrap_or(false) as i32;
            let is_stale = event["is_stale"].as_bool().unwrap_or(false) as i32;
            // Legacy events without kind/evidence_status fields default to
            // 'belief' / 'n/a' — matching the column DEFAULT for pre-migration rows.
            let has_explicit_kind = event.get("kind").is_some();
            let kind = event["kind"].as_str().unwrap_or("belief");
            let evidence_status = event["evidence_status"].as_str().unwrap_or("n/a");
            let session_id = event["session_id"].as_str();

            // Single transaction: entry INSERT + FTS sync + embedding + cue
            // replace. One upsert event applies atomically, matching
            // CueBatch.tla's ApplyNext — a reader can never observe the entry
            // with a stale cue set or missing embedding mid-apply.
            //
            // Manual BEGIN/COMMIT because `apply_event` takes `&Connection`,
            // not `&mut Connection` (same pattern as the expire branch).
            conn.execute_batch("BEGIN")?;
            let result = (|| -> Result<bool> {
                // A legacy upsert carries no kind, so its 'n/a' is the AC2
                // grandfather rather than a derived value. The grandfather may
                // INITIALIZE a fresh row, but re-applying it to an existing row
                // would re-grandfather an entry the evidence lifecycle has
                // already de-legacied — payload-style authority of exactly the
                // kind ADR-1 abolishes, and order-sensitive under compact
                // (agent-kb/tla/AgentKbEvidence.tla, CE3). So the ON CONFLICT
                // path leaves evidence_status alone for legacy events; the
                // explicit-kind path recomputes it below regardless.
                let upsert_sql = if has_explicit_kind {
                    "INSERT INTO entries(id, path, summary, content, tags, version_ref, permanent, is_stale, kind, evidence_status, session_id, created_at, updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)
                     ON CONFLICT(id) DO UPDATE SET
                       path=excluded.path, summary=excluded.summary,
                       content=excluded.content, tags=excluded.tags,
                       version_ref=excluded.version_ref,
                       permanent=excluded.permanent,
                       is_stale=excluded.is_stale,
                       kind=excluded.kind,
                       evidence_status=excluded.evidence_status,
                       session_id=excluded.session_id,
                       updated_at=excluded.updated_at"
                } else {
                    "INSERT INTO entries(id, path, summary, content, tags, version_ref, permanent, is_stale, kind, evidence_status, session_id, created_at, updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)
                     ON CONFLICT(id) DO UPDATE SET
                       path=excluded.path, summary=excluded.summary,
                       content=excluded.content, tags=excluded.tags,
                       version_ref=excluded.version_ref,
                       permanent=excluded.permanent,
                       is_stale=excluded.is_stale,
                       kind=excluded.kind,
                       session_id=excluded.session_id,
                       updated_at=excluded.updated_at"
                };
                conn.execute(
                    upsert_sql,
                    params![id, path, summary, content, tags, version_ref, permanent, is_stale, kind, evidence_status, session_id, ts],
                )?;

                // Stale entries: clean up FTS/embeddings/cues so they don't
                // appear in search (br-improvement-catalog-23b.6 GC,
                // CueBatch.tla S2). Returns false: stale upserts don't count
                // toward the FTS deprecation gate.
                if is_stale == 1 {
                    // entries_fts may be gone after the deprecation gate fires; treat as no-op.
                    let _ = conn.execute("DELETE FROM entries_fts WHERE id=?1", params![id]);
                    conn.execute(
                        "DELETE FROM entries_emb WHERE rowid = \
                         (SELECT rowid FROM entries WHERE id=?1)",
                        params![id],
                    )?;
                    conn.execute("DELETE FROM cues WHERE entry_id=?1", params![id])?;
                    return Ok(false);
                }

                if has_explicit_kind {
                    let derived_status = compute_evidence_status(conn, id)?;
                    conn.execute(
                        "UPDATE entries SET evidence_status=?1 WHERE id=?2",
                        params![derived_status, id],
                    )?;
                }

                let rowid: i64 =
                    conn.query_row("SELECT rowid FROM entries WHERE id=?1", params![id], |r| {
                        r.get(0)
                    })?;

                // Sync FTS5 — v1 writes are no-ops after the deprecation gate drops entries_fts.
                let _ = conn.execute("DELETE FROM entries_fts WHERE id=?1", params![id]);
                let _ = conn.execute(
                    "INSERT INTO entries_fts(id, path, summary, content, tags)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![id, path, summary, content, tags],
                );

                // Sync embedding store (f16 wire format — 768 bytes per entry)
                if !embedder.is_noop() {
                    let mode = EmbedTextMode::from_env();
                    check_embed_mode_vintage(conn, mode);
                    let text = entry_embed_text(mode, path, summary, content, &tags);
                    let emb = embedder.embed(&text)?;
                    let blob = f32s_to_f16_blob(&emb);
                    conn.execute(
                        "INSERT OR REPLACE INTO entries_emb(rowid, embedding) VALUES(?1,?2)",
                        params![rowid, blob],
                    )?;
                }

                // Replace cue rows wholesale (CueBatch.tla S3). Legacy events
                // without a "cues" field still clear rows from any prior
                // upsert that had cues.
                conn.execute("DELETE FROM cues WHERE entry_id=?1", params![id])?;
                if let Some(cues) = event["cues"].as_array() {
                    for cue in cues.iter().filter_map(|c| c.as_str()) {
                        let blob: Option<Vec<u8>> = if embedder.is_noop() {
                            None
                        } else {
                            Some(f32s_to_f16_blob(&embedder.embed(cue)?))
                        };
                        conn.execute(
                            "INSERT INTO cues(entry_id, cue, embedding) VALUES(?1,?2,?3)",
                            params![id, cue, blob],
                        )?;
                    }
                }
                Ok(true)
            })();
            let counts_toward_gate = match result {
                Ok(counts) => counts,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            };
            conn.execute_batch("COMMIT")?;
            if counts_toward_gate {
                increment_post_cutover_writes(conn);
            }
        }

        ("expire", "entries") => {
            let id = event["id"].as_str().context("missing id")?;
            // Single transaction: spec-conformant expire reset + FTS/emb/cue GC.
            // Resetting evidence_status to 'n/a' closes the TLC-CE-class order
            // sensitivity repro [legacy_up, ev_expire, expire, legacy_up];
            // evidence-row GC lands later with the ADR-2 task.
            //
            // We can't use `Connection::transaction()` here because `apply_event`
            // takes `&Connection`, not `&mut Connection`. Manual BEGIN/COMMIT it is.
            conn.execute_batch("BEGIN")?;
            let result = (|| -> Result<()> {
                conn.execute(
                    "UPDATE entries SET is_stale=1, evidence_status='n/a', updated_at=datetime('now') WHERE id=?1",
                    params![id],
                )?;
                // Remove from FTS so expired entries don't appear in search.
                // entries_fts may be gone after the deprecation gate fires; treat as no-op.
                let _ = conn.execute("DELETE FROM entries_fts WHERE id=?1", params![id]);
                // GC: remove embedding row so entries_emb stays in sync with live entries.
                conn.execute(
                    "DELETE FROM entries_emb WHERE rowid = \
                     (SELECT rowid FROM entries WHERE id=?1)",
                    params![id],
                )?;
                // Cue rows die with their entry (CueBatch.tla S2 — no orphans).
                conn.execute("DELETE FROM cues WHERE entry_id=?1", params![id])?;
                Ok(())
            })();
            if let Err(e) = result {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            conn.execute_batch("COMMIT")?;
            increment_post_cutover_writes(conn);
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
            let ev_id = ev["id"]
                .as_str()
                .context("evidence_add: missing evidence.id")?;
            let entry_id = event["entry_id"]
                .as_str()
                .context("evidence_add: missing entry_id")?;

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

            let kind = ev["kind"]
                .as_str()
                .context("evidence_add: missing evidence.kind")?;
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

            // Recompute evidence_status unconditionally. The prior "preserve n/a for
            // legacy entries" branch caused incremental-vs-replay divergence: a legacy
            // upsert (no explicit kind/evidence_status) followed by evidence_add kept
            // evidence_status='n/a' on the incremental path, while a full rebuild from
            // the same events would converge to 'present' via compute_evidence_status.
            // The fix is to always call the soft-mandate helper after an evidence row
            // change so both paths agree (br-f7y).
            let new_status = compute_evidence_status(conn, entry_id)?;
            conn.execute(
                "UPDATE entries SET evidence_status=?1, updated_at=datetime('now') WHERE id=?2",
                params![new_status, entry_id],
            )?;
        }

        ("citation_healed", "evidence") => {
            let ev_id = event["evidence_id"]
                .as_str()
                .context("citation_healed: missing evidence_id")?;
            let entry_id = event["entry_id"]
                .as_str()
                .context("citation_healed: missing entry_id")?;
            let new_path = event["new_path"]
                .as_str()
                .context("citation_healed: missing new_path")?;

            // Heal writes citation_path and NOTHING else. citation_hash stays
            // as recorded (spec CitationRelocation.tla StoredHashImmutable):
            // an excerpt match locates the code, it does not re-attest it.
            // A healed row therefore reads back as `relocated`, and only a
            // later re-verification pass that re-hashes the content at the new
            // path can promote it to `verified`.
            conn.execute(
                "UPDATE evidence SET citation_path=?1 WHERE id=?2 AND entry_id=?3",
                params![new_path, ev_id, entry_id],
            )?;
        }

        ("evidence_expire", "evidence") => {
            let ev_id = event["evidence_id"]
                .as_str()
                .context("evidence_expire: missing evidence_id")?;
            let entry_id = event["entry_id"]
                .as_str()
                .context("evidence_expire: missing entry_id")?;

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
#[derive(Clone)]
pub struct SearchOptions {
    pub limit: usize,
    pub do_fts: bool,
    pub do_semantic: bool,
    /// Only return entries whose path starts with this prefix.
    pub path_prefix: Option<String>,
    /// Only return entries that have this exact tag.
    pub tag_filter: Option<String>,
    /// Maximum number of results to verify inline (AC18 narrow-K fallback).
    /// Results beyond this count get `verified: null`. Default: 10.
    pub inline_verify_k: usize,
    /// Repository root used for inline evidence verification.
    ///
    /// Preferred for MCP and other long-running contexts where the process CWD
    /// is not the repo (e.g. the MCP port is typically spawned with CWD `/`,
    /// causing the CWD-based `find_repo_root()` walk to fail). MCP callers
    /// derive this via `root_from_db()` (see `src/commands/mcp.rs:40-45`).
    ///
    /// When `None`, `search_entries` falls back to walking up from CWD via
    /// `find_repo_root()`. CLI invocations may leave this `None` because the
    /// user runs the binary from inside the repo tree.
    pub repo_root: Option<PathBuf>,
    /// Pool size for the bounded verify thread pool (br-23b.13).
    /// Currently unused; reserved for forward-compatibility with the
    /// 23b.13 task that replaces the per-request `thread::scope` path.
    pub verify_pool_size: Option<usize>,
    /// Recency-bias decay factor (λ in exp(-λ·days)) applied after RRF scoring.
    /// 0.0 disables the pass entirely (byte-identical behavior). Only applied
    /// in hybrid mode. Forced to 0.0 for peer federation queries (clock skew).
    pub recency_lambda: f32,
    /// MMR diversification strength for hybrid search. 0.0 disables (default,
    /// byte-identical to pre-MMR behavior). In (0,1]: greedy re-rank over a
    /// 2×limit pool maximizing λ·relevance − (1−λ)·max_cosine_to_selected.
    ///
    /// Note: the cue-anchor lane (and MMR) run only in HYBRID mode
    /// (do_fts && do_semantic). Semantic-only mode is a raw
    /// entry-embedding debugging lane and intentionally skips both.
    pub mmr_lambda: f32,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: true,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 10,
            repo_root: None,
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        }
    }
}

/// An evidence row attached to a search result, with inline verification flag.
pub struct SearchEvidence {
    pub id: String,
    pub kind: String,
    pub citation_path: Option<String>,
    pub citation_sha: Option<String>,
    pub citation_hash: String,
    pub citation_excerpt: Option<String>,
    /// `Some(true/false)` if verified inline; `None` if skipped by narrow-K fallback.
    pub verified: Option<bool>,
    /// Full inline verification status when verification ran; `None` when
    /// verification was deferred by inline_verify_k or the per-entry byte cap.
    pub verification_status: Option<VerificationStatus>,
}

impl SearchEvidence {
    pub fn status_str(&self) -> &'static str {
        match self.verification_status {
            Some(status) => status.as_str(),
            None => "deferred",
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
    /// Relevance score. For RRF hybrid: sum of 1/(k+rank) across sources.
    /// For FTS-only: 1.0. For semantic-only: raw cosine similarity.
    pub score: f32,
    /// `"fts"`, `"semantic"`, or `"rrf"` (hybrid Reciprocal Rank Fusion).
    pub source: &'static str,
    /// Scoring kind: `"fts"` | `"semantic"` | `"rrf"`.
    /// Equals `source` for single-lane results; `"rrf"` for hybrid-merged results.
    pub score_kind: &'static str,
    /// Evidence rows with inline verification results.
    pub evidence: Vec<SearchEvidence>,
    /// Beta(1,1) posterior confidence: (s+1)/(s+f+2). Bootstrap value 0.5 when no audits.
    pub confidence: f32,
    /// Total audit verdicts recorded for this entry's (kind, session_id) pair.
    pub audit_n: u32,
    /// Originating repo path. `None` means local DB; `Some(path)` means fetched from a peer.
    pub origin_repo: Option<String>,
    /// DB `updated_at` for stale-warning checks in presentation layers.
    pub updated_at: String,
}

pub struct FetchEntryByIdResult {
    pub id: String,
    pub path: String,
    pub summary: String,
    pub content: String,
    pub tags: String,
    pub version_ref: Option<String>,
    pub is_stale: bool,
    pub permanent: bool,
    pub created_at: String,
    pub updated_at: String,
    pub kind: String,
    pub evidence_status: String,
    pub evidence: Vec<Evidence>,
}

pub fn fetch_entry_by_id(
    conn: &Connection,
    entry_id: &str,
) -> Result<Option<FetchEntryByIdResult>> {
    let row = conn
        .query_row(
            "SELECT id, path, summary, content, tags, version_ref, is_stale,
                    permanent, created_at, updated_at, COALESCE(kind, 'belief'),
                    COALESCE(evidence_status, 'n/a')
             FROM entries
             WHERE id = ?1",
            params![entry_id],
            |r| {
                Ok(FetchEntryByIdResult {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    summary: r.get(2)?,
                    content: r.get(3)?,
                    tags: r.get(4)?,
                    version_ref: r.get(5)?,
                    is_stale: r.get::<_, i64>(6)? != 0,
                    permanent: r.get::<_, i64>(7)? != 0,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                    kind: r.get(10)?,
                    evidence_status: r.get(11)?,
                    evidence: vec![],
                })
            },
        )
        .optional()?;

    let Some(mut entry) = row else {
        return Ok(None);
    };

    let mut evidence_map = fetch_evidence_for_entries(conn, &[entry_id.to_string()])?;
    entry.evidence = evidence_map.remove(entry_id).unwrap_or_default();
    Ok(Some(entry))
}

/// Fetch evidence rows for the given entry IDs, capped at
/// `MAX_EVIDENCE_ROWS_PER_ENTRY` per entry (br-h9g, security I2).
///
/// Issues one `WHERE entry_id IN (…)` query per chunk of ≤999 IDs
/// (SQLite's default SQLITE_MAX_VARIABLE_NUMBER limit).  Per-entry capping
/// uses `ROW_NUMBER() OVER (PARTITION BY entry_id ORDER BY recorded_at, id)`
/// so the DB filters excess rows before they cross the Rust boundary.
/// When an entry exceeds the cap a warning is emitted to stderr.
///
/// Result ordering: the returned Vec per entry follows `recorded_at, id`
/// order, identical to the previous per-entry loop.  The HashMap key order
/// is unspecified (HashMap), matching prior behaviour.
pub fn fetch_evidence_for_entries(
    conn: &Connection,
    entry_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<Evidence>>> {
    let mut map: std::collections::HashMap<String, Vec<Evidence>> =
        std::collections::HashMap::new();
    if entry_ids.is_empty() {
        return Ok(map);
    }

    // SQLite's default limit is 999 host parameters per statement.
    // Use 998 so the row-number cap value occupies the last slot.
    const CHUNK: usize = 998;
    // Fetch one extra row per entry so we can detect truncation.
    let probe_limit = (MAX_EVIDENCE_ROWS_PER_ENTRY + 1) as i64;

    for chunk in entry_ids.chunks(CHUNK) {
        // Build "?, ?, …" placeholder list for this chunk.
        let placeholders: String = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        // The row_number cap is the next parameter index after the chunk.
        let rn_param_idx = chunk.len() + 1;

        // Window function caps per-entry rows at probe_limit so we receive
        // at most MAX_EVIDENCE_ROWS_PER_ENTRY+1 rows per entry, letting us
        // detect (and warn about) truncation without a separate COUNT query.
        let sql = format!(
            "SELECT id, entry_id, kind, citation_path, citation_sha, citation_hash,
                    citation_excerpt, derived_from, recorded_at
             FROM (
               SELECT id, entry_id, kind, citation_path, citation_sha, citation_hash,
                      citation_excerpt, derived_from, recorded_at,
                      ROW_NUMBER() OVER (PARTITION BY entry_id ORDER BY recorded_at, id) AS rn
               FROM evidence
               WHERE entry_id IN ({placeholders})
             )
             WHERE rn <= ?{rn_param_idx}
             ORDER BY entry_id, recorded_at, id"
        );

        let mut stmt = conn.prepare(&sql)?;

        // Bind entry_id strings first, then the probe_limit.
        let rows_raw: Vec<Evidence> = {
            use rusqlite::types::ToSql;
            let mut params_vec: Vec<&dyn ToSql> = chunk.iter().map(|s| s as &dyn ToSql).collect();
            params_vec.push(&probe_limit);

            stmt.query_map(params_vec.as_slice(), |r| {
                Ok(Evidence {
                    id: r.get(0)?,
                    entry_id: r.get(1)?,
                    kind: r.get(2)?,
                    citation_path: r.get(3)?,
                    citation_sha: r.get(4)?,
                    citation_hash: r.get(5).unwrap_or_default(),
                    citation_excerpt: r.get(6)?,
                    derived_from: r.get(7)?,
                    recorded_at: r.get(8)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect()
        };

        // Group rows by entry_id (rows are already sorted by entry_id via ORDER BY).
        // Emit truncation warning when an entry hits the probe limit.
        for ev in rows_raw {
            map.entry(ev.entry_id.clone()).or_default().push(ev);
        }
    }

    for (entry_id, rows) in map.iter_mut() {
        if rows.len() > MAX_EVIDENCE_ROWS_PER_ENTRY {
            eprintln!(
                "kb: entry {entry_id} evidence rows truncated to \
                 MAX_EVIDENCE_ROWS_PER_ENTRY={MAX_EVIDENCE_ROWS_PER_ENTRY} (had >= {})",
                rows.len()
            );
            rows.truncate(MAX_EVIDENCE_ROWS_PER_ENTRY);
        }
    }

    Ok(map)
}

/// Which FTS table search queries hit. Controlled by `KB_FTS_READ_PATH`:
///   "contentless"     — entries_fts (explicit rollback override)
///   "content_entries" — entries_fts_v2 (content='entries' table; default)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtsReadPath {
    Contentless,
    ContentEntries,
}

impl FtsReadPath {
    pub fn from_env() -> Self {
        // Unset or "content_entries" → content_entries (default post-cutover).
        // Explicit "contentless" preserves the rollback affordance.
        match std::env::var("KB_FTS_READ_PATH").as_deref() {
            Ok("contentless") => FtsReadPath::Contentless,
            _ => FtsReadPath::ContentEntries,
        }
    }
}

pub type FtsRow = (String, String, String, String, String, String);

pub fn fts_query_contentless(
    conn: &Connection,
    safe_query: &str,
    opts: &SearchOptions,
) -> Result<Vec<FtsRow>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.path, e.summary, e.content, e.tags, e.updated_at
         FROM entries_fts f
         JOIN entries e ON e.id = f.id
         WHERE f.entries_fts MATCH ?1
           AND e.is_stale = 0
           AND (?2 IS NULL OR e.path LIKE (?2 || '%'))
           AND (?3 IS NULL OR EXISTS (SELECT 1 FROM json_each(e.tags) WHERE value = ?3))
         ORDER BY rank
         LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(
            params![
                safe_query,
                opts.path_prefix,
                opts.tag_filter,
                opts.limit as i64
            ],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn fts_query_content_entries(
    conn: &Connection,
    safe_query: &str,
    opts: &SearchOptions,
) -> Result<Vec<FtsRow>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.path, e.summary, e.content, e.tags, e.updated_at
         FROM entries_fts_v2 f
         JOIN entries e ON e.rowid = f.rowid
         WHERE f.entries_fts_v2 MATCH ?1
           AND e.is_stale = 0
           AND (?2 IS NULL OR e.path LIKE (?2 || '%'))
           AND (?3 IS NULL OR EXISTS (SELECT 1 FROM json_each(e.tags) WHERE value = ?3))
         ORDER BY rank
         LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(
            params![
                safe_query,
                opts.path_prefix,
                opts.tag_filter,
                opts.limit as i64
            ],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Shared hybrid search used by both the CLI and MCP handler.
///
/// FTS queries are wrapped in double-quotes to enable phrase search and prevent
/// FTS5 operator injection (e.g. unexpected `AND`/`OR` parsing).
/// Active FTS table is selected by `KB_FTS_READ_PATH` (default: "content_entries").
/// Frontier expand primitive (Memora pickup .7): live entries adjacent to the
/// seed ids. Adjacency facets, one point each:
///   * same path directory (all components up to the last `/`)
///   * at least one shared tag
///   * at least one shared cue text
///   * at least one shared evidence citation file
///
/// The calling agent is the retrieval policy loop (Memora's EXPAND / RE_QUERY /
/// STOP); this function only materializes the frontier. Seeds are excluded,
/// stale entries are excluded, results sorted by facet count descending.
/// `score` = facet count, `score_kind` = "expand".
pub fn expand_entries(conn: &Connection, ids: &[String], limit: usize) -> Result<Vec<SearchEntry>> {
    use std::collections::{HashMap, HashSet};

    if ids.is_empty() {
        return Ok(vec![]);
    }
    // Request-amplification cap: seeds come from untrusted MCP input and feed
    // SQL placeholder construction + facet scans. Excess seeds are dropped.
    const MAX_EXPAND_SEEDS: usize = 32;
    let ids = &ids[..ids.len().min(MAX_EXPAND_SEEDS)];
    let limit = limit.min(MAX_LIMIT);
    let seed_set: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();

    fn dirname(path: &str) -> Option<&str> {
        path.rfind('/').map(|i| &path[..i])
    }
    fn parse_tags(tags: &str) -> HashSet<String> {
        serde_json::from_str::<Vec<String>>(tags)
            .map(|v| v.into_iter().collect())
            .unwrap_or_default()
    }
    fn citation_file(citation_path: &str) -> &str {
        citation_path
            .rsplit_once(':')
            .map_or(citation_path, |(f, _)| f)
    }

    // Seed facets.
    let mut seed_dirs: HashSet<String> = HashSet::new();
    let mut seed_tags: HashSet<String> = HashSet::new();
    {
        let placeholders: String = (1..=ids.len())
            .map(|i| format!("?{}", i))
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT path, tags FROM entries WHERE is_stale = 0 AND id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(String, String)> = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        if rows.is_empty() {
            return Ok(vec![]); // unknown or stale seeds
        }
        for (path, tags) in rows {
            if let Some(d) = dirname(&path) {
                seed_dirs.insert(d.to_string());
            }
            seed_tags.extend(parse_tags(&tags));
        }
    }

    // Cue texts and evidence citation files, keyed by entry — one query each
    // over live entries; overlap computed in Rust (KB scale is small; same
    // O(n) tradeoff as the semantic brute-force scan).
    let mut cues_by_entry: HashMap<String, HashSet<String>> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT c.entry_id, c.cue FROM cues c
         JOIN entries e ON e.id = c.entry_id WHERE e.is_stale = 0",
    ) {
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        for (entry_id, cue) in rows {
            cues_by_entry.entry(entry_id).or_default().insert(cue);
        }
    }
    let mut files_by_entry: HashMap<String, HashSet<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT ev.entry_id, ev.citation_path FROM evidence ev
             JOIN entries e ON e.id = ev.entry_id
             WHERE e.is_stale = 0 AND ev.citation_path IS NOT NULL",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        for (entry_id, cp) in rows {
            files_by_entry
                .entry(entry_id)
                .or_default()
                .insert(citation_file(&cp).to_string());
        }
    }
    let empty: HashSet<String> = HashSet::new();
    let seed_cues: HashSet<&String> = ids
        .iter()
        .flat_map(|id| cues_by_entry.get(id).unwrap_or(&empty))
        .collect();
    let seed_files: HashSet<&String> = ids
        .iter()
        .flat_map(|id| files_by_entry.get(id).unwrap_or(&empty))
        .collect();

    // Score all live candidates by facet overlap.
    let mut stmt = conn.prepare(
        "SELECT id, path, summary, content, tags, updated_at FROM entries WHERE is_stale = 0",
    )?;
    let candidates: Vec<(String, String, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut scored: Vec<SearchEntry> = Vec::new();
    for (id, path, summary, content, tags, updated_at) in candidates {
        if seed_set.contains(id.as_str()) {
            continue;
        }
        let mut facets = 0u8;
        if dirname(&path).is_some_and(|d| seed_dirs.contains(d)) {
            facets += 1;
        }
        if !seed_tags.is_empty() && !parse_tags(&tags).is_disjoint(&seed_tags) {
            facets += 1;
        }
        if cues_by_entry
            .get(&id)
            .is_some_and(|cs| cs.iter().any(|c| seed_cues.contains(c)))
        {
            facets += 1;
        }
        if files_by_entry
            .get(&id)
            .is_some_and(|fs| fs.iter().any(|f| seed_files.contains(f)))
        {
            facets += 1;
        }
        if facets > 0 {
            scored.push(SearchEntry {
                id,
                path,
                summary,
                content,
                tags,
                score: facets as f32,
                source: "expand",
                score_kind: "expand",
                evidence: vec![],
                confidence: 0.5,
                audit_n: 0,
                origin_repo: None,
                updated_at,
            });
        }
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    scored.truncate(limit);
    Ok(scored)
}

/// Greedy MMR re-rank of `entries` in place (Memora pickup .6).
///
/// Selection: seed with the top-scored entry (top-1 is never displaced), then
/// repeatedly pick argmax of `λ·rel_norm − (1−λ)·max_cosine_to_selected`.
/// `rel_norm` is the entry score divided by the pool max, mapping RRF scores
/// onto [0,1] so both terms share a scale.
///
/// Entries without an embedding row (NoopEmbedder vintage) get similarity 0 —
/// they look maximally diverse, which errs toward inclusion, never exclusion.
fn mmr_rerank(conn: &Connection, entries: &mut Vec<SearchEntry>, lambda: f32) {
    // Fetch embeddings for the pool in one query.
    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    let placeholders: String = (1..=ids.len())
        .map(|i| format!("?{}", i))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT e.id, emb.embedding FROM entries e
         JOIN entries_emb emb ON emb.rowid = e.rowid
         WHERE e.id IN ({})",
        placeholders
    );
    let emb_map: std::collections::HashMap<String, Vec<f32>> = match conn.prepare(&sql) {
        Ok(mut stmt) => stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map(|rows| {
                rows.filter_map(|r| r.ok())
                    .map(|(id, blob)| (id, decode_emb_blob(&blob)))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => return, // best-effort: no embeddings, keep RRF order
    };

    let max_score = entries.iter().map(|e| e.score).fold(f32::MIN, f32::max);
    if max_score <= 0.0 {
        return;
    }

    let mut remaining: Vec<SearchEntry> = std::mem::take(entries);
    // Seed: highest-scored entry (list is sorted descending).
    let mut selected: Vec<SearchEntry> = vec![remaining.remove(0)];

    while !remaining.is_empty() {
        let (best_idx, _) = remaining
            .iter()
            .enumerate()
            .map(|(i, cand)| {
                let rel = cand.score / max_score;
                let max_sim = emb_map
                    .get(&cand.id)
                    .map(|cv| {
                        selected
                            .iter()
                            .filter_map(|s| emb_map.get(&s.id))
                            .map(|sv| cosine_similarity(cv, sv).max(0.0))
                            .fold(0.0f32, f32::max)
                    })
                    .unwrap_or(0.0);
                (i, lambda * rel - (1.0 - lambda) * max_sim)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or((0, 0.0));
        selected.push(remaining.remove(best_idx));
    }

    *entries = selected;
}

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
        let safe_query: String = query
            .split_whitespace()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");

        let read_path = FtsReadPath::from_env();
        let rows = match read_path {
            FtsReadPath::Contentless => fts_query_contentless(conn, &safe_query, opts)?,
            FtsReadPath::ContentEntries => fts_query_content_entries(conn, &safe_query, opts)?,
        };

        // Divergence detection: compare both read paths and emit a warning when
        // they disagree. In debug builds, assert equality to catch regressions
        // early. In release builds, log only so production is never disrupted.
        #[cfg(debug_assertions)]
        {
            let alt_rows = match read_path {
                FtsReadPath::Contentless => fts_query_content_entries(conn, &safe_query, opts),
                FtsReadPath::ContentEntries => fts_query_contentless(conn, &safe_query, opts),
            };
            if let Ok(alt) = alt_rows {
                let primary_ids: std::collections::BTreeSet<&str> =
                    rows.iter().map(|(id, ..)| id.as_str()).collect();
                let alt_ids: std::collections::BTreeSet<&str> =
                    alt.iter().map(|(id, ..)| id.as_str()).collect();
                debug_assert_eq!(
                    primary_ids, alt_ids,
                    "fts5_dual_write_divergence: primary={:?} alt={:?}",
                    primary_ids, alt_ids
                );
            }
        }
        #[cfg(not(debug_assertions))]
        {
            let alt_rows = match read_path {
                FtsReadPath::Contentless => fts_query_content_entries(conn, &safe_query, opts),
                FtsReadPath::ContentEntries => fts_query_contentless(conn, &safe_query, opts),
            };
            if let Ok(alt) = alt_rows {
                let primary_ids: std::collections::BTreeSet<&str> =
                    rows.iter().map(|(id, ..)| id.as_str()).collect();
                let alt_ids: std::collections::BTreeSet<&str> =
                    alt.iter().map(|(id, ..)| id.as_str()).collect();
                if primary_ids != alt_ids {
                    eprintln!(
                        "kb: fts5_dual_write_divergence read_path={:?} \
                         primary_count={} alt_count={}",
                        read_path,
                        primary_ids.len(),
                        alt_ids.len()
                    );
                }
            }
        }

        // T5a opt-in counter: log each content_entries search so the cutover
        // gate (T5b) can verify ≥50 searches and ≥7 distinct days before flipping.
        if read_path == FtsReadPath::ContentEntries {
            let date = chrono::Utc::now().format("%Y-%m-%d");
            eprintln!(
                "kb: fts5_content_entries_search date={date} result_count={}",
                rows.len()
            );
        }

        for (id, path, summary, content, tags, updated_at) in rows {
            seen_ids.insert(id.clone());
            entries.push(SearchEntry {
                id,
                path,
                summary,
                content,
                tags,
                score: 1.0,
                source: "fts",
                score_kind: "fts",
                evidence: vec![],
                confidence: 0.5,
                audit_n: 0,
                origin_repo: None,
                updated_at,
            });
        }
    }

    if opts.do_semantic && !embedder.is_noop() {
        let q_emb = embedder.embed(query)?;
        let mut stmt = conn.prepare(
            "SELECT e.id, e.path, e.summary, e.content, e.tags, e.updated_at, emb.embedding
             FROM entries_emb emb
             JOIN entries e ON e.rowid = emb.rowid
             WHERE e.is_stale = 0
               AND (?1 IS NULL OR e.path LIKE (?1 || '%'))
               AND (?2 IS NULL OR EXISTS (SELECT 1 FROM json_each(e.tags) WHERE value = ?2))",
        )?;
        // TODO: O(n) brute-force scan — replace with ANN index (e.g. sqlite-vss) when entry count exceeds ~10k
        //
        // Scratch buffer allocated ONCE outside the loop — no per-row Vec allocation.
        // decode_f16_blob_into clears and fills scratch in-place; cosine_similarity
        // reads from it. Mismatch (corrupt/legacy blob) results in sim=0.0 via
        // decode_emb_blob fallback via length dispatch.
        let rows: Vec<(String, String, String, String, String, String, Vec<u8>)> = stmt
            .query_map(params![opts.path_prefix, opts.tag_filter], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut scratch: Vec<f32> = Vec::with_capacity(EMB_DIMS);
        let mut candidates: Vec<(f32, String, String, String, String, String, String)> =
            Vec::with_capacity(rows.len());
        for (id, path, summary, content, tags, updated_at, blob) in rows {
            decode_f16_blob_into(&blob, &mut scratch);
            let sim = if scratch.is_empty() {
                // blob was not canonical f16 — fall back to graceful decode
                let fallback = decode_emb_blob(&blob);
                cosine_similarity(&q_emb, &fallback)
            } else {
                cosine_similarity(&q_emb, &scratch)
            };
            candidates.push((sim, id, path, summary, content, tags, updated_at));
        }

        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Cue lane (Memora pickup .4): score each live entry by its best-cosine
        // cue anchor. Ranked separately so RRF fuses it as a third source.
        // Best-effort: absence of the cues table (pre-migration DB) is not an
        // error, just an empty lane.
        let mut cue_ranked: Vec<(f32, String, String, String, String, String, String)> = Vec::new();
        if opts.do_fts {
            if let Ok(mut stmt) = conn.prepare(
            "SELECT c.entry_id, c.cue, c.embedding, e.path, e.summary, e.content, e.tags, e.updated_at
             FROM cues c
             JOIN entries e ON e.id = c.entry_id
             WHERE e.is_stale = 0
               AND c.embedding IS NOT NULL
               AND (?1 IS NULL OR e.path LIKE (?1 || '%'))
               AND (?2 IS NULL OR EXISTS (SELECT 1 FROM json_each(e.tags) WHERE value = ?2))",
        ) {
            let cue_rows: Vec<(String, Vec<u8>, String, String, String, String, String)> = stmt
                .query_map(params![opts.path_prefix, opts.tag_filter], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();

            // Best cue score per entry.
            let mut best: std::collections::HashMap<String, (f32, String, String, String, String, String)> =
                std::collections::HashMap::new();
            for (entry_id, blob, path, summary, content, tags, updated_at) in cue_rows {
                decode_f16_blob_into(&blob, &mut scratch);
                let sim = if scratch.is_empty() {
                    let fallback = decode_emb_blob(&blob);
                    cosine_similarity(&q_emb, &fallback)
                } else {
                    cosine_similarity(&q_emb, &scratch)
                };
                match best.get(&entry_id) {
                    Some((prev, ..)) if *prev >= sim => {}
                    _ => {
                        best.insert(entry_id, (sim, path, summary, content, tags, updated_at));
                    }
                }
            }
            cue_ranked = best
                .into_iter()
                .map(|(id, (sim, path, summary, content, tags, updated_at))| {
                    (sim, id, path, summary, content, tags, updated_at)
                })
                .collect();
            cue_ranked
                .sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            cue_ranked.truncate(opts.limit.saturating_mul(2));
        }
        }

        if opts.do_fts {
            // Hybrid mode: apply Reciprocal Rank Fusion (RRF, k=60) to combine
            // FTS and semantic rankings. Each entry's RRF score is the sum of
            // 1/(k+rank) across all sources it appears in, where rank is 1-based.
            //
            // This replaces the naive "FTS gets 1.0, semantic gets cosine" approach
            // which caused FTS to dominate every tie. RRF is order-aware: an entry
            // appearing in both sources earns two rank contributions and will
            // outrank an entry appearing in only one, regardless of raw score.
            const RRF_K: f32 = 60.0;

            // Build a map of id → RRF score, seeding with FTS ranks.
            let mut rrf_scores: std::collections::HashMap<String, f32> =
                std::collections::HashMap::new();
            for (fts_rank, entry) in entries.iter().enumerate() {
                let contrib = 1.0 / (RRF_K + (fts_rank + 1) as f32);
                rrf_scores.insert(entry.id.clone(), contrib);
            }

            // Second RRF source: semantic candidate ranks.
            for (sem_rank, (_, id, ..)) in candidates.iter().enumerate() {
                let contrib = 1.0 / (RRF_K + (sem_rank + 1) as f32);
                let entry = rrf_scores.entry(id.clone()).or_insert(0.0);
                *entry += contrib;
            }

            // Third RRF source: cue-anchor lane (best cue cosine per entry).
            for (cue_rank, (_, id, ..)) in cue_ranked.iter().enumerate() {
                let contrib = 1.0 / (RRF_K + (cue_rank + 1) as f32);
                let entry = rrf_scores.entry(id.clone()).or_insert(0.0);
                *entry += contrib;
            }

            // Re-build entries from rrf_scores, merging FTS entries and new semantic-only entries.
            // Existing FTS entries already have their metadata; semantic-only ones need it added.
            let mut fts_meta: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for (i, entry) in entries.iter().enumerate() {
                fts_meta.insert(entry.id.clone(), i);
            }

            // For semantic-only entries (not in FTS), create new SearchEntry values.
            // We cap to opts.limit * 2 candidates to avoid iterating all of them.
            for (_, id, path, summary, content, tags, updated_at) in
                candidates.into_iter().take(opts.limit * 2)
            {
                if !fts_meta.contains_key(&id) {
                    fts_meta.insert(id.clone(), entries.len());
                    entries.push(SearchEntry {
                        id: id.clone(),
                        path,
                        summary,
                        content,
                        tags,
                        score: 0.0, // will be overwritten below
                        source: "semantic",
                        score_kind: "rrf",
                        evidence: vec![],
                        confidence: 0.5,
                        audit_n: 0,
                        origin_repo: None,
                        updated_at,
                    });
                }
            }

            // Cue-only entries (reached via a cue anchor, absent from both the
            // FTS and entry-embedding lanes) still need materializing.
            for (_, id, path, summary, content, tags, updated_at) in cue_ranked.into_iter() {
                if !fts_meta.contains_key(&id) {
                    fts_meta.insert(id.clone(), entries.len());
                    entries.push(SearchEntry {
                        id: id.clone(),
                        path,
                        summary,
                        content,
                        tags,
                        score: 0.0, // will be overwritten below
                        source: "cue",
                        score_kind: "rrf",
                        evidence: vec![],
                        confidence: 0.5,
                        audit_n: 0,
                        origin_repo: None,
                        updated_at,
                    });
                }
            }

            // Apply RRF scores to all entries and mark score_kind as "rrf".
            for entry in &mut entries {
                let rrf = rrf_scores.get(&entry.id).copied().unwrap_or(0.0);
                entry.score = rrf;
                entry.score_kind = "rrf";
            }

            // Sort by RRF score descending and cap at limit.
            // With MMR enabled, keep a 2×limit pool so diversification has
            // candidates to swap in; the final truncate happens after MMR.
            entries.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let pool = if opts.mmr_lambda > 0.0 {
                opts.limit.saturating_mul(2)
            } else {
                opts.limit
            };
            entries.truncate(pool);

            // Recency-bias post-RRF pass: multiply each entry's score by
            // exp(-λ·days_since_updated_at). Skip entirely when λ=0.0 to
            // preserve byte-identical behavior with pre-recency-bias code.
            if opts.recency_lambda != 0.0 && !entries.is_empty() {
                let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
                let placeholders: String = (1..=ids.len())
                    .map(|i| format!("?{}", i))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT id, updated_at FROM entries WHERE id IN ({})",
                    placeholders
                );
                let now = chrono::Utc::now().naive_utc();
                let decay_map: std::collections::HashMap<String, f32> = match conn.prepare(&sql) {
                    Ok(mut stmt) => stmt
                        .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                        })
                        .map(|rows| {
                            rows.filter_map(|r| r.ok())
                                .map(|(id, updated_at)| {
                                    let days = chrono::NaiveDateTime::parse_from_str(
                                        &updated_at,
                                        "%Y-%m-%d %H:%M:%S",
                                    )
                                    .map(|dt| {
                                        let secs = (now - dt).num_seconds() as f32;
                                        (secs / 86400.0).max(0.0)
                                    })
                                    .unwrap_or(0.0);
                                    let decay = (-opts.recency_lambda * days).exp();
                                    (id, decay)
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    Err(_) => std::collections::HashMap::new(),
                };
                for entry in &mut entries {
                    let decay = decay_map.get(&entry.id).copied().unwrap_or(1.0);
                    entry.score *= decay;
                }
                // Re-sort: multiplication may change relative order.
                entries.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }

            // MMR diversification pass (Memora pickup .6): greedy re-rank of
            // the pool penalizing similarity to already-selected results, then
            // cut to limit. Scores and score_kind are left untouched — MMR
            // changes ORDER and MEMBERSHIP, not the relevance signal.
            if opts.mmr_lambda > 0.0 && entries.len() > 1 {
                // Clamp to [0,1]: λ>1 would flip the diversity penalty into a
                // similarity REWARD, actively clustering duplicates.
                mmr_rerank(conn, &mut entries, opts.mmr_lambda.clamp(0.0, 1.0));
                entries.truncate(opts.limit);
            }
        } else {
            // Semantic-only mode: no RRF, raw cosine scores, score_kind="semantic".
            for (sim, id, path, summary, content, tags, updated_at) in
                candidates.into_iter().take(opts.limit)
            {
                entries.push(SearchEntry {
                    id,
                    path,
                    summary,
                    content,
                    tags,
                    score: sim,
                    source: "semantic",
                    score_kind: "semantic",
                    evidence: vec![],
                    confidence: 0.5,
                    audit_n: 0,
                    origin_repo: None,
                    updated_at,
                });
            }
        }
    }

    // Fetch evidence rows for all result entries and attach with inline verification.
    let entry_ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();

    // Prefetch source_weights in ONE query (br-ei2.8 AC5: not per-row subquery).
    // Uses COALESCE(entries.session_id, '__GLOBAL__') to match the write path.
    let weights_map: std::collections::HashMap<String, (i64, i64)> = if entry_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        let placeholders: String = (1..=entry_ids.len())
            .map(|i| format!("?{}", i))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT e.id, COALESCE(sw.successes,0), COALESCE(sw.failures,0)
             FROM entries e
             LEFT JOIN source_weights sw
               ON sw.kind = e.kind
               AND sw.session_id = COALESCE(e.session_id,'__GLOBAL__')
             WHERE e.id IN ({})",
            placeholders
        );
        match conn.prepare(&sql) {
            Ok(mut stmt) => stmt
                .query_map(rusqlite::params_from_iter(entry_ids.iter()), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map(|rows| {
                    rows.filter_map(|r| r.ok())
                        .map(|(id, s, f)| (id, (s, f)))
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => std::collections::HashMap::new(),
        }
    };

    // Attach confidence from prefetched weights (Beta(1,1) posterior).
    for entry in &mut entries {
        let (s, f) = weights_map.get(&entry.id).copied().unwrap_or((0, 0));
        entry.confidence = (s + 1) as f32 / (s + f + 2) as f32;
        entry.audit_n = (s + f) as u32;
    }

    let mut evidence_map = fetch_evidence_for_entries(conn, &entry_ids)?;

    // Resolve repo root: prefer explicit `opts.repo_root` (MCP path — CWD is
    // typically `/`, so CWD-based discovery fails). Fall back to walking up
    // from CWD via `find_repo_root()` (CLI path — user runs from inside repo).
    let repo_root: Option<PathBuf> = opts.repo_root.clone().or_else(find_repo_root);

    let verify_count = opts.inline_verify_k.min(entries.len());

    // br-improvement-catalog-23b.13: bounded scoped pool.
    // ADR-C: explicit std::thread, not rayon.
    //
    // Pool size: opts.verify_pool_size → num_cpus::get_physical() fallback.
    // min(1) guards against systems returning 0 physical CPUs.
    let pool_size = opts
        .verify_pool_size
        .unwrap_or_else(num_cpus::get_physical)
        .max(1);

    // --- Phase 1: pre-collect per-entry evidence and byte-budget state ---
    // We need to move ev_rows out of evidence_map before the thread::scope so
    // the scope can borrow repo_root without fighting the mutable evidence_map.

    struct EntryWork {
        entry_idx: usize,
        ev_rows: Vec<crate::models::Evidence>,
        do_verify: bool,
        budget_exceeded: bool,
    }

    let mut work_items: Vec<EntryWork> = Vec::with_capacity(entries.len());
    for (idx, entry) in entries.iter().enumerate() {
        let ev_rows = evidence_map.remove(&entry.id).unwrap_or_default();
        let do_verify = idx < verify_count;

        // br-und: compute total bytes for this entry's evidence rows (br-und security I3)
        let mut total_bytes: usize = 0;
        for ev in &ev_rows {
            if let Some(ref citation_path) = ev.citation_path {
                // Parse citation_path format "path:start-end" to extract byte range
                if let Some(colon_idx) = citation_path.rfind(':') {
                    let range_part = &citation_path[colon_idx + 1..];
                    if let Some(dash_idx) = range_part.find('-') {
                        if let (Ok(start), Ok(end)) = (
                            range_part[..dash_idx].parse::<usize>(),
                            range_part[dash_idx + 1..].parse::<usize>(),
                        ) {
                            if start <= end {
                                total_bytes = total_bytes.saturating_add(end - start);
                            }
                        }
                    }
                }
            }
        }

        let budget_exceeded = total_bytes > MAX_PER_ENTRY_BYTES;
        if budget_exceeded {
            eprintln!(
                "kb: entry {} evidence bytes capped at MAX_PER_ENTRY_BYTES={} (had {}); skipping verification",
                entry.id, MAX_PER_ENTRY_BYTES, total_bytes
            );
        }

        work_items.push(EntryWork {
            entry_idx: idx,
            ev_rows,
            do_verify,
            budget_exceeded,
        });
    }

    // --- Phase 2: flatten all verification tasks across entries ---
    // task_ranges[entry_idx] = Some(start..end) within outcomes_flat, or None.
    let mut flat_tasks: Vec<crate::models::Evidence> = Vec::new();
    let mut task_ranges: Vec<Option<std::ops::Range<usize>>> = vec![None; entries.len()];
    for item in &work_items {
        if item.do_verify && !item.budget_exceeded && !item.ev_rows.is_empty() {
            let start = flat_tasks.len();
            flat_tasks.extend(item.ev_rows.iter().cloned());
            task_ranges[item.entry_idx] = Some(start..flat_tasks.len());
        }
    }

    // --- Phase 3: run all verification tasks through the bounded pool ---
    let total_tasks = flat_tasks.len();
    let mut outcomes_flat: Vec<VerificationOutcome> = vec![
        VerificationOutcome {
            status: VerificationStatus::Unverified,
            relocated_to: None,
            reason: None,
        };
        total_tasks
    ];

    if total_tasks > 0 {
        // Work channel: bounded at pool_size * 2 to bound in-flight tasks and
        // provide backpressure to the sender. This is the queue from main → workers.
        // Result channel: unbounded so workers never block emitting results.
        // Main drains tx_result only AFTER dropping tx_work (sequential drain),
        // so it cannot interleave sending and receiving. An unbounded result
        // channel avoids the sender↔result-channel deadlock that arises when
        // both channels are bounded and the main thread sends work while workers
        // try to enqueue results.
        //
        // Total result count is bounded externally: at most
        // MAX_INLINE_VERIFY_K * MAX_EVIDENCE_ROWS_PER_ENTRY (br-h9g security I2).
        let work_chan_cap = (pool_size * 2).max(1);
        std::thread::scope(|scope| {
            let (tx_work, rx_work) =
                crossbeam_channel::bounded::<(usize, crate::models::Evidence)>(work_chan_cap);
            let (tx_result, rx_result) =
                crossbeam_channel::unbounded::<(usize, VerificationOutcome)>();

            // Spawn pool_size worker threads. Each consumes (task_idx, Evidence)
            // from rx_work and sends (task_idx, VerificationOutcome) to tx_result.
            for _ in 0..pool_size {
                let rx = rx_work.clone();
                let tx = tx_result.clone();
                let root_ref = repo_root.as_ref();
                scope.spawn(move || {
                    for (task_idx, ev) in rx {
                        let outcome = if let Some(root) = root_ref {
                            crate::components::verification::verify_evidence(
                                &ev,
                                root,
                                SEARCH_PATH_RELOCATION_POLICY,
                            )
                            .unwrap_or(VerificationOutcome {
                                status: VerificationStatus::Unverified,
                                relocated_to: None,
                                reason: None,
                            })
                        } else {
                            VerificationOutcome {
                                status: VerificationStatus::Unverified,
                                relocated_to: None,
                                reason: None,
                            }
                        };
                        let _ = tx.send((task_idx, outcome));
                    }
                });
            }
            // Drop the unused sender clone so rx_result closes after all
            // worker sender clones are dropped. Drop rx_work so the channel
            // closes once tx_work is dropped (workers drain then exit).
            drop(tx_result);
            drop(rx_work);

            // Send all tasks; bounded work channel provides backpressure.
            for (task_idx, ev) in flat_tasks.into_iter().enumerate() {
                // send() only blocks if workers are slower than the sender;
                // they cannot deadlock here because tx_result is unbounded.
                let _ = tx_work.send((task_idx, ev));
            }
            // Drop tx_work → workers drain remaining work, then exit,
            // dropping their tx_result clones → rx_result closes.
            drop(tx_work);

            // Sequential drain: workers have all exited by the time scope
            // joins; rx_result is fully populated before we iterate.
            for (task_idx, outcome) in rx_result {
                outcomes_flat[task_idx] = outcome;
            }
        });
    }

    // --- Phase 4: assign results back to entries in original order ---
    for item in work_items {
        let entry = &mut entries[item.entry_idx];
        if item.ev_rows.is_empty() {
            entry.evidence = vec![];
        } else if item.do_verify && !item.budget_exceeded {
            let range = task_ranges[item.entry_idx].as_ref().unwrap();
            entry.evidence = item
                .ev_rows
                .into_iter()
                .zip(range.clone())
                .map(|(ev, task_idx)| SearchEvidence {
                    id: ev.id,
                    kind: ev.kind,
                    citation_path: ev.citation_path,
                    citation_sha: ev.citation_sha,
                    citation_hash: ev.citation_hash,
                    citation_excerpt: ev.citation_excerpt,
                    verified: Some(outcomes_flat[task_idx].is_verified()),
                    verification_status: Some(outcomes_flat[task_idx].status),
                })
                .collect();
        } else {
            // Beyond inline_verify_k or budget exceeded: verified=null.
            entry.evidence = item
                .ev_rows
                .into_iter()
                .map(|ev| SearchEvidence {
                    id: ev.id,
                    kind: ev.kind,
                    citation_path: ev.citation_path,
                    citation_sha: ev.citation_sha,
                    citation_hash: ev.citation_hash,
                    citation_excerpt: ev.citation_excerpt,
                    verified: None,
                    verification_status: None,
                })
                .collect();
        }
    }

    Ok(entries)
}

/// Walk up from CWD to find a directory containing `.git`.
/// Returns None if not found (e.g. in tempdir tests).
fn find_repo_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir: &Path = &cwd;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::embedder::NoopEmbedder;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static UPDATED_AT_FETCH_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn count_updated_at_fetches(sql: &str) {
        if sql.starts_with("SELECT id, updated_at FROM entries WHERE id IN (") {
            UPDATED_AT_FETCH_COUNT.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    struct SearchTestEmbedder;

    impl crate::components::embedder::Embedder for SearchTestEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            let first = if text.contains("rank-c") {
                0.7
            } else if text.contains("rank-b") {
                0.8
            } else {
                0.9
            };
            Ok(vec![first, (1.0_f32 - first * first).sqrt()])
        }

        fn is_noop(&self) -> bool {
            false
        }
    }

    fn seed_single_fetch_search_corpus(conn: &Connection) {
        let embedder = SearchTestEmbedder;
        for (id, summary) in [
            ("rank-a", "sealedwaiver sealedwaiver rank-a"),
            ("rank-b", "sealedwaiver rank-b with deliberately longer filler text"),
            ("rank-c", "semantic-only rank-c"),
        ] {
            let event = serde_json::json!({
                "action": "upsert", "table": "entries", "id": id,
                "path": format!("tests/{id}.md"), "summary": summary,
                "content": "fixed corpus", "tags": [], "kind": "observation",
                "evidence_status": "missing", "is_stale": false,
                "ts": "2999-01-01 00:00:00"
            });
            apply_event(conn, &embedder, &event).unwrap();
            conn.execute(
                "UPDATE entries SET updated_at = '2999-01-01 00:00:00' WHERE id = ?1",
                [id],
            )
            .unwrap();
        }
    }

    fn single_fetch_opts(do_fts: bool, do_semantic: bool, recency_lambda: f32) -> SearchOptions {
        SearchOptions {
            limit: 10,
            do_fts,
            do_semantic,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 0,
            repo_root: None,
            verify_pool_size: None,
            recency_lambda,
            mmr_lambda: 0.0,
        }
    }

    #[test]
    fn test_search_updated_at_fetch_count_is_lambda_gated() {
        let mut conn = open_db_memory().unwrap();
        seed_single_fetch_search_corpus(&conn);
        conn.trace(Some(count_updated_at_fetches));

        for (lambda, expected) in [(0.0, 0), (0.1, 1)] {
            UPDATED_AT_FETCH_COUNT.store(0, AtomicOrdering::SeqCst);
            let results = search_entries(
                &conn,
                &SearchTestEmbedder,
                "sealedwaiver",
                &single_fetch_opts(true, true, lambda),
            )
            .unwrap();
            assert!(!results.is_empty());
            assert_eq!(
                UPDATED_AT_FETCH_COUNT.load(AtomicOrdering::SeqCst),
                expected,
                "recency_lambda={lambda} must execute exactly {expected} updated_at fetches"
            );
        }
    }

    #[test]
    fn test_single_fetch_preserves_sealed_recency_ranking_bytes() {
        let conn = open_db_memory().unwrap();
        seed_single_fetch_search_corpus(&conn);
        let results = search_entries(
            &conn,
            &SearchTestEmbedder,
            "sealedwaiver",
            &single_fetch_opts(true, true, 0.1),
        )
        .unwrap();
        let actual = results
            .iter()
            .map(|entry| format!("{}:{:08x}", entry.id, entry.score.to_bits()))
            .collect::<Vec<_>>()
            .join("\n");

        // This byte-identical ranking fixture constitutes the stated sealed-split
        // waiver in the kb-profiling epic plan; it seals the pre-single-fetch path.
        let expected = "rank-a:3d064b8a\nrank-b:3d042108\nrank-c:3c820821";
        assert_eq!(actual.as_bytes(), expected.as_bytes());
    }

    #[test]
    fn test_search_returns_non_empty_updated_at_in_every_lane_mode() {
        let conn = open_db_memory().unwrap();
        seed_single_fetch_search_corpus(&conn);

        for (mode, do_fts, do_semantic, query) in [
            ("fts-only", true, false, "sealedwaiver"),
            ("semantic-only", false, true, "semantic query"),
            ("hybrid", true, true, "sealedwaiver"),
        ] {
            let results = search_entries(
                &conn,
                &SearchTestEmbedder,
                query,
                &single_fetch_opts(do_fts, do_semantic, 0.0),
            )
            .unwrap();
            assert!(!results.is_empty(), "{mode} must return seeded entries");
            assert!(
                results.iter().all(|entry| !entry.updated_at.is_empty()),
                "every {mode} result must carry updated_at from its retrieval lane"
            );
        }
    }

    fn seed_entry_row(conn: &Connection, id: &str, path: &str, summary: &str, is_stale: i64) {
        conn.execute(
            "INSERT INTO entries (id, path, summary, content, tags, is_stale, updated_at)
             VALUES (?1, ?2, ?3, '', '[]', ?4, '2024-01-01T00:00:00Z')",
            rusqlite::params![id, path, summary, is_stale],
        )
        .unwrap();
    }

    fn seed_evidence_row(conn: &Connection, id: &str, entry_id: &str, citation_path: &str) {
        conn.execute(
            "INSERT INTO evidence(
                id, entry_id, kind, citation_path, citation_hash, citation_excerpt, recorded_at
             ) VALUES (?1, ?2, 'code', ?3, 'sha256:test', 'excerpt long enough', '2024-01-01T00:00:00Z')",
            rusqlite::params![id, entry_id, citation_path],
        )
        .unwrap();
    }

    fn entry_evidence_status(conn: &Connection, id: &str) -> String {
        conn.query_row(
            "SELECT COALESCE(evidence_status, 'n/a') FROM entries WHERE id=?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn entry_evidence_row_count(conn: &Connection, id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM evidence WHERE entry_id=?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

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
        assert!(tables.contains(&"source_weights".to_string()));
    }

    #[test]
    fn test_init_creates_source_weights_table() {
        let conn = open_db_memory().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(source_weights)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(cols.contains(&"kind".to_string()));
        assert!(cols.contains(&"session_id".to_string()));
        assert!(cols.contains(&"successes".to_string()));
        assert!(cols.contains(&"failures".to_string()));
    }

    #[test]
    fn test_init_adds_session_id_column_on_legacy_db() {
        // Simulate a pre-Phase-5 DB: create entries table without session_id,
        // then run ensure_schema and confirm the column was added.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE entries (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                summary TEXT NOT NULL,
                content TEXT NOT NULL,
                tags TEXT NOT NULL
            );",
        )
        .unwrap();
        // Running ensure_schema on this legacy DB must not error.
        ensure_schema(&conn).unwrap();
        // Confirm session_id column now exists.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(entries)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"session_id".to_string()),
            "session_id must be added to legacy entries table"
        );
    }

    #[test]
    fn test_entries_citing_matches_bare_and_ranged_paths_and_excludes_stale_entries() {
        let conn = open_db_memory().unwrap();

        seed_entry_row(&conn, "live-bare", "docs/live-bare.md", "live bare", 0);
        seed_entry_row(&conn, "live-range", "docs/live-range.md", "live range", 0);
        seed_entry_row(&conn, "stale-range", "docs/stale.md", "stale range", 1);
        seed_entry_row(&conn, "other-file", "docs/other.md", "other file", 0);

        seed_evidence_row(&conn, "ev-bare", "live-bare", "src/foo.rs");
        seed_evidence_row(&conn, "ev-range", "live-range", "src/foo.rs:42-58");
        seed_evidence_row(&conn, "ev-stale", "stale-range", "src/foo.rs:10-20");
        seed_evidence_row(&conn, "ev-other", "other-file", "src/bar.rs:1-5");

        let rows = entries_citing(&conn, "src/foo.rs").unwrap();
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();

        assert_eq!(ids, vec!["live-bare", "live-range"]);
        assert_eq!(
            rows[0].evidence[0].citation_path.as_deref(),
            Some("src/foo.rs")
        );
        assert_eq!(
            rows[1].evidence[0].citation_path.as_deref(),
            Some("src/foo.rs:42-58")
        );
    }

    #[test]
    fn test_entries_citing_query_plan_uses_citation_path_index() {
        let conn = open_db_memory().unwrap();
        let sql = format!("EXPLAIN QUERY PLAN {}", entries_citing_sql());
        let lower = "src/foo.rs:".to_string();
        let upper = "src/foo.rs;".to_string();
        let mut stmt = conn.prepare(&sql).unwrap();
        let details: Vec<String> = stmt
            .query_map(rusqlite::params!["src/foo.rs", lower, upper], |r| r.get(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("idx_evidence_citation_path")),
            "query plan should mention idx_evidence_citation_path, got {details:?}"
        );
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
        assert_eq!(
            evidence_status, "n/a",
            "legacy entry must default to evidence_status='n/a'"
        );

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
        assert_eq!(
            audit_count, 0,
            "audit_runs must be untouched after legacy event replay (L4 boundary)"
        );
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
            .query_row("SELECT path, summary FROM entries WHERE id='e1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(path, "src/lib.rs");
        assert_eq!(summary, "test summary");

        // Check FTS entry exists
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries_fts WHERE id='e1'", [], |r| {
                r.get(0)
            })
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
            .query_row(
                "SELECT COUNT(*) FROM entries_fts WHERE id='fts1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 1);

        let expire = serde_json::json!({
            "action": "expire", "table": "entries", "id": "fts1"
        });
        apply_event(&conn, &embedder, &expire).unwrap();

        // FTS entry must be gone after expire
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries_fts WHERE id='fts1'",
                [],
                |r| r.get(0),
            )
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
            .query_row("SELECT is_stale FROM entries WHERE id='stale1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            is_stale, 1,
            "upsert with is_stale=true must persist staleness"
        );

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
            .query_row("SELECT is_stale FROM entries WHERE id='old1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            is_stale, 0,
            "events without is_stale must default to active"
        );
    }

    #[test]
    fn test_apply_event_upsert_derives_missing_without_evidence_rows() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "derived-missing",
            "path": "src/lib.rs",
            "summary": "entry without evidence",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "present",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &event).unwrap();

        assert_eq!(entry_evidence_row_count(&conn, "derived-missing"), 0);
        assert_eq!(entry_evidence_status(&conn, "derived-missing"), "missing");
    }

    #[test]
    fn test_apply_event_upsert_derives_present_from_existing_evidence_rows() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let initial = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "derived-present",
            "path": "src/lib.rs",
            "summary": "entry with evidence",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "missing",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &initial).unwrap();

        let evidence_add = serde_json::json!({
            "action": "evidence_add",
            "table": "evidence",
            "entry_id": "derived-present",
            "evidence": {
                "id": "ev-derived-present",
                "entry_id": "derived-present",
                "kind": "code",
                "citation_path": "src/lib.rs:1-1",
                "citation_sha": null,
                "citation_hash": "sha256:derived-present",
                "citation_excerpt": null,
                "derived_from": null,
                "recorded_at": "2024-01-01T00:00:00Z"
            },
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &evidence_add).unwrap();

        let replayed_upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "derived-present",
            "path": "src/lib.rs",
            "summary": "entry with evidence replayed",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "missing",
            "ts": "2024-01-02T00:00:00Z"
        });
        apply_event(&conn, &embedder, &replayed_upsert).unwrap();

        assert_eq!(entry_evidence_row_count(&conn, "derived-present"), 1);
        assert_eq!(entry_evidence_status(&conn, "derived-present"), "present");
    }

    #[test]
    fn test_apply_event_legacy_upsert_preserves_na_status() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let event = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "legacy-na",
            "path": "src/lib.rs",
            "summary": "legacy entry",
            "content": "content",
            "tags": [],
            "ts": "2023-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &event).unwrap();

        assert_eq!(entry_evidence_row_count(&conn, "legacy-na"), 0);
        assert_eq!(entry_evidence_status(&conn, "legacy-na"), "n/a");
    }

    /// A legacy upsert event: no `kind`, no `evidence_status`. Replaying one is
    /// the AC2 grandfather path.
    fn legacy_upsert_event(id: &str) -> serde_json::Value {
        serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": id,
            "path": "src/lib.rs",
            "summary": format!("legacy {id}"),
            "content": "content",
            "tags": [],
            "ts": "2023-01-01T00:00:00Z"
        })
    }

    fn evidence_add_event(id: &str) -> serde_json::Value {
        serde_json::json!({
            "action": "evidence_add",
            "table": "evidence",
            "entry_id": id,
            "evidence": {
                "id": format!("ev-{id}"),
                "entry_id": id,
                "kind": "code",
                "citation_path": "src/lib.rs:1-1",
                "citation_sha": null,
                "citation_hash": "sha256:abc",
                "citation_excerpt": null,
                "derived_from": null,
                "recorded_at": "2024-01-01T00:00:00Z"
            },
            "ts": "2024-01-01T00:00:00Z"
        })
    }

    fn evidence_expire_event(id: &str) -> serde_json::Value {
        serde_json::json!({
            "action": "evidence_expire",
            "table": "evidence",
            "entry_id": id,
            "evidence_id": format!("ev-{id}"),
            "reason": "test",
            "ts": "2024-01-01T00:00:00Z"
        })
    }

    #[test]
    fn test_apply_event_expire_resets_evidence_status_to_na() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "expire-reset",
            "path": "src/lib.rs",
            "summary": "entry without evidence",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "present",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();
        assert_eq!(entry_evidence_status(&conn, "expire-reset"), "missing");

        let expire = serde_json::json!({
            "action": "expire",
            "table": "entries",
            "id": "expire-reset"
        });
        apply_event(&conn, &embedder, &expire).unwrap();

        let (is_stale, evidence_status): (i64, String) = conn
            .query_row(
                "SELECT is_stale, COALESCE(evidence_status, 'n/a') FROM entries WHERE id='expire-reset'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(is_stale, 1);
        assert_eq!(evidence_status, "n/a");
    }

    /// ADR-1 corollary (AgentKbEvidence.tla CE3): a legacy upsert may only
    /// INITIALIZE the 'n/a' grandfather on a fresh row. Once an evidence event
    /// has de-legacied the entry, a later legacy upsert must preserve the
    /// derived status instead of re-grandfathering it.
    #[test]
    fn test_legacy_reupsert_does_not_regrandfather_delegacied_entry() {
        let embedder = NoopEmbedder;

        // De-legacied to 'missing' by an evidence_expire naming no live row.
        let conn = open_db_memory().unwrap();
        apply_event(&conn, &embedder, &legacy_upsert_event("c")).unwrap();
        assert_eq!(entry_evidence_status(&conn, "c"), "n/a");
        apply_event(&conn, &embedder, &evidence_expire_event("c")).unwrap();
        assert_eq!(entry_evidence_status(&conn, "c"), "missing");
        apply_event(&conn, &embedder, &legacy_upsert_event("c")).unwrap();
        assert_eq!(
            entry_evidence_status(&conn, "c"),
            "missing",
            "legacy re-upsert must not re-grandfather a de-legacied entry"
        );

        // De-legacied to 'present' by an evidence_add.
        let conn = open_db_memory().unwrap();
        apply_event(&conn, &embedder, &legacy_upsert_event("p")).unwrap();
        apply_event(&conn, &embedder, &evidence_add_event("p")).unwrap();
        assert_eq!(entry_evidence_status(&conn, "p"), "present");
        apply_event(&conn, &embedder, &legacy_upsert_event("p")).unwrap();
        assert_eq!(
            entry_evidence_status(&conn, "p"), "present",
            "legacy re-upsert must not drop an entry with evidence back to n/a"
        );
        assert_eq!(entry_evidence_row_count(&conn, "p"), 1);
    }

    /// The other half of the corollary: preserving the current status on an
    /// existing row keeps a STILL-legacy entry at 'n/a', and a fresh legacy
    /// insert still initializes to 'n/a'.
    #[test]
    fn test_legacy_reupsert_of_still_legacy_entry_keeps_na() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        apply_event(&conn, &embedder, &legacy_upsert_event("l")).unwrap();
        assert_eq!(
            entry_evidence_status(&conn, "l"),
            "n/a",
            "fresh legacy insert initializes the grandfather"
        );

        apply_event(&conn, &embedder, &legacy_upsert_event("l")).unwrap();
        assert_eq!(
            entry_evidence_status(&conn, "l"),
            "n/a",
            "legacy re-upsert of a still-legacy entry keeps n/a"
        );
    }

    /// Order insensitivity — the property CompactionEquivalenceE needs. Compact
    /// keeps only the LAST upsert per entry, so replaying the trailing legacy
    /// upsert with its preceding evidence event must land on the same status as
    /// replaying the full log.
    #[test]
    fn test_legacy_reupsert_after_expire_matches_compacted_replay_status() {
        let embedder = NoopEmbedder;

        let conn_full = open_db_memory().unwrap();
        for event in [
            legacy_upsert_event("d"),
            evidence_expire_event("d"),
            serde_json::json!({
                "action": "expire",
                "table": "entries",
                "id": "d"
            }),
            legacy_upsert_event("d"),
        ] {
            apply_event(&conn_full, &embedder, &event).unwrap();
        }

        let conn_compacted = open_db_memory().unwrap();
        apply_event(&conn_compacted, &embedder, &legacy_upsert_event("d")).unwrap();

        let (full_stale, compacted_stale): (i64, i64) = (
            conn_full
                .query_row("SELECT is_stale FROM entries WHERE id='d'", [], |r| r.get(0))
                .unwrap(),
            conn_compacted
                .query_row("SELECT is_stale FROM entries WHERE id='d'", [], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!(full_stale, 0, "re-upsert must revive the expired entry");
        assert_eq!(compacted_stale, 0, "compacted replay must keep the entry live");
        assert_eq!(
            entry_evidence_status(&conn_full, "d"),
            entry_evidence_status(&conn_compacted, "d"),
            "expire must reset status so replay order matches compacted replay"
        );
        assert_eq!(entry_evidence_status(&conn_full, "d"), "n/a");
    }

    #[test]
    fn test_legacy_reupsert_status_is_compaction_order_insensitive() {
        let embedder = NoopEmbedder;

        let conn_full = open_db_memory().unwrap();
        for event in [
            legacy_upsert_event("c"),
            evidence_expire_event("c"),
            legacy_upsert_event("c"),
        ] {
            apply_event(&conn_full, &embedder, &event).unwrap();
        }

        let conn_compacted = open_db_memory().unwrap();
        for event in [legacy_upsert_event("c"), evidence_expire_event("c")] {
            apply_event(&conn_compacted, &embedder, &event).unwrap();
        }

        assert_eq!(
            entry_evidence_status(&conn_full, "c"),
            entry_evidence_status(&conn_compacted, "c"),
            "trailing legacy re-upsert must not change the derived status"
        );
        assert_eq!(entry_evidence_status(&conn_full, "c"), "missing");
    }

    #[test]
    fn test_apply_event_reupsert_after_expire_recomputes_missing_for_explicit_kind() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "revive-explicit-kind",
            "path": "src/lib.rs",
            "summary": "entry without evidence",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "present",
            "ts": "2024-01-01T00:00:00Z"
        });
        let expire = serde_json::json!({
            "action": "expire",
            "table": "entries",
            "id": "revive-explicit-kind"
        });

        apply_event(&conn, &embedder, &upsert).unwrap();
        apply_event(&conn, &embedder, &expire).unwrap();
        apply_event(&conn, &embedder, &upsert).unwrap();

        assert_eq!(entry_evidence_status(&conn, "revive-explicit-kind"), "missing");
    }

    #[test]
    fn test_apply_event_upsert_status_converges_to_local_rowset_across_reordered_compaction_repro() {
        let embedder = NoopEmbedder;

        let conn_full = open_db_memory().unwrap();
        let conn_reordered = open_db_memory().unwrap();

        let upsert_a = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "cmp-a",
            "path": "src/lib.rs",
            "summary": "cmp a",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "present",
            "ts": "2024-01-01T00:00:00Z"
        });
        let evidence_add = serde_json::json!({
            "action": "evidence_add",
            "table": "evidence",
            "entry_id": "cmp-a",
            "evidence": {
                "id": "ev-cmp-a",
                "entry_id": "cmp-a",
                "kind": "code",
                "citation_path": "src/lib.rs:1-1",
                "citation_sha": null,
                "citation_hash": "sha256:cmp-a",
                "citation_excerpt": null,
                "derived_from": null,
                "recorded_at": "2024-01-01T00:00:00Z"
            },
            "ts": "2024-01-01T00:00:00Z"
        });
        let upsert_b = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "cmp-a",
            "path": "src/lib.rs",
            "summary": "cmp a replayed",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "present",
            "ts": "2024-01-02T00:00:00Z"
        });

        for event in [&upsert_a, &evidence_add, &upsert_b] {
            apply_event(&conn_full, &embedder, event).unwrap();
        }
        for event in [&evidence_add, &upsert_b] {
            apply_event(&conn_reordered, &embedder, event).unwrap();
        }

        assert_eq!(entry_evidence_row_count(&conn_full, "cmp-a"), 1);
        assert_eq!(entry_evidence_status(&conn_full, "cmp-a"), "present");

        assert_eq!(entry_evidence_row_count(&conn_reordered, "cmp-a"), 0);
        assert_eq!(entry_evidence_status(&conn_reordered, "cmp-a"), "missing");
    }

    /// br-h9g (security I2): fetch_evidence_for_entries must cap rows at
    /// MAX_EVIDENCE_ROWS_PER_ENTRY even when the underlying table holds more.
    /// Bounds the thread::scope fan-out in search_entries.
    #[test]
    fn test_fetch_evidence_caps_rows_per_entry() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // Seed one entry the evidence rows can attach to.
        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "cap-host", "path": "src/cap.rs", "summary": "cap host",
            "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        // Insert MAX_EVIDENCE_ROWS_PER_ENTRY + 50 evidence rows for that entry.
        let extra = 50;
        for i in 0..(MAX_EVIDENCE_ROWS_PER_ENTRY + extra) {
            conn.execute(
                "INSERT INTO evidence(id, entry_id, kind, citation_hash, recorded_at)
                 VALUES(?1, ?2, 'code', 'sha256:abc', ?3)",
                params![
                    format!("ev-{i:04}"),
                    "cap-host",
                    format!("2024-01-01T00:00:{:02}Z", i % 60)
                ],
            )
            .unwrap();
        }

        let map = fetch_evidence_for_entries(&conn, &["cap-host".to_string()]).unwrap();
        let rows = map.get("cap-host").expect("entry must be present");
        assert_eq!(
            rows.len(),
            MAX_EVIDENCE_ROWS_PER_ENTRY,
            "fetch_evidence_for_entries must truncate to MAX_EVIDENCE_ROWS_PER_ENTRY"
        );
    }

    // -----------------------------------------------------------------------
    // br-23b.12: batch evidence fetch order-equivalence.
    //
    // Reference implementation kept here as a test helper only — matches the
    // pre-batch per-id loop.  The production fn now issues one query per chunk.
    // -----------------------------------------------------------------------

    /// Reference loop-based implementation kept for order-equivalence testing.
    /// Mirrors the pre-batch logic exactly so the test can compare maps.
    fn fetch_evidence_loop_reference(
        conn: &Connection,
        entry_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<Evidence>>> {
        let mut map: std::collections::HashMap<String, Vec<Evidence>> =
            std::collections::HashMap::new();
        if entry_ids.is_empty() {
            return Ok(map);
        }
        let probe_limit = (MAX_EVIDENCE_ROWS_PER_ENTRY + 1) as i64;
        let mut stmt = conn.prepare(
            "SELECT id, entry_id, kind, citation_path, citation_sha, citation_hash, citation_excerpt, derived_from, recorded_at
             FROM evidence
             WHERE entry_id = ?1
             ORDER BY recorded_at, id
             LIMIT ?2",
        )?;
        for entry_id in entry_ids {
            let rows: Vec<Evidence> = stmt
                .query_map(params![entry_id, probe_limit], |r| {
                    Ok(Evidence {
                        id: r.get(0)?,
                        entry_id: r.get(1)?,
                        kind: r.get(2)?,
                        citation_path: r.get(3)?,
                        citation_sha: r.get(4)?,
                        citation_hash: r.get(5).unwrap_or_default(),
                        citation_excerpt: r.get(6)?,
                        derived_from: r.get(7)?,
                        recorded_at: r.get(8)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            if rows.len() > MAX_EVIDENCE_ROWS_PER_ENTRY {
                let capped: Vec<Evidence> =
                    rows.into_iter().take(MAX_EVIDENCE_ROWS_PER_ENTRY).collect();
                map.insert(entry_id.clone(), capped);
            } else if !rows.is_empty() {
                map.insert(entry_id.clone(), rows);
            }
        }
        Ok(map)
    }

    /// br-23b.12: batched fetch must return a map identical to the reference
    /// per-id loop for 100 entries × 200 evidence rows each.
    ///
    /// Verifies:
    ///   - same key set
    ///   - per-entry Vec identical (same order, same fields)
    ///   - per-entry cap (MAX_EVIDENCE_ROWS_PER_ENTRY) still held
    #[test]
    fn test_batch_evidence_order_equivalence() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        const N_ENTRIES: usize = 100;
        const ROWS_PER_ENTRY: usize = 200; // exactly at the cap

        let entry_ids: Vec<String> = (0..N_ENTRIES)
            .map(|i| format!("batch-eq-entry-{i:03}"))
            .collect();

        // Seed N_ENTRIES entries so foreign-key constraints are satisfied.
        for id in &entry_ids {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": id, "path": format!("batch/{id}"), "summary": id,
                "content": "c", "tags": [], "ts": "2024-01-01T00:00:00Z"
            });
            apply_event(&conn, &embedder, &ev).unwrap();
        }

        // Insert exactly ROWS_PER_ENTRY evidence rows per entry, with
        // deterministic recorded_at so ORDER BY recorded_at, id is stable.
        for (ei, entry_id) in entry_ids.iter().enumerate() {
            for ri in 0..ROWS_PER_ENTRY {
                // Pad seconds/minutes to keep recorded_at unique per row.
                let mins = ri / 60;
                let secs = ri % 60;
                conn.execute(
                    "INSERT INTO evidence(id, entry_id, kind, citation_hash, recorded_at)
                     VALUES(?1, ?2, 'code', 'sha256:test', ?3)",
                    params![
                        format!("ev-{ei:03}-{ri:03}"),
                        entry_id,
                        format!("2024-01-01T00:{mins:02}:{secs:02}Z"),
                    ],
                )
                .unwrap();
            }
        }

        let ref_map = fetch_evidence_loop_reference(&conn, &entry_ids).unwrap();
        let batch_map = fetch_evidence_for_entries(&conn, &entry_ids).unwrap();

        assert_eq!(
            ref_map.len(),
            batch_map.len(),
            "key count must match: ref={} batch={}",
            ref_map.len(),
            batch_map.len()
        );

        for entry_id in &entry_ids {
            let ref_rows = ref_map
                .get(entry_id)
                .expect("reference map must contain entry");
            let batch_rows = batch_map
                .get(entry_id)
                .expect("batch map must contain entry");

            assert_eq!(
                ref_rows.len(),
                batch_rows.len(),
                "row count mismatch for {entry_id}: ref={} batch={}",
                ref_rows.len(),
                batch_rows.len()
            );
            assert_eq!(
                ref_rows.len(),
                MAX_EVIDENCE_ROWS_PER_ENTRY,
                "per-entry cap must hold for {entry_id}"
            );

            for (i, (r, b)) in ref_rows.iter().zip(batch_rows.iter()).enumerate() {
                assert_eq!(
                    r.id, b.id,
                    "row {i} id mismatch for {entry_id}: ref={:?} batch={:?}",
                    r.id, b.id
                );
                assert_eq!(
                    r.recorded_at, b.recorded_at,
                    "row {i} recorded_at mismatch for {entry_id}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // br-bhg: explicit SearchOptions.repo_root threads through to verification.
    // Regression for MCP cwd=/ case where find_repo_root() walks from CWD and
    // returns None (or the wrong root), causing verified=false on every row.
    // -----------------------------------------------------------------------

    /// When `opts.repo_root` is `Some(path)`, inline evidence verification must
    /// resolve citation_path relative to that path — not relative to whatever
    /// repo `find_repo_root()` discovers from the current working directory.
    ///
    /// Construction: write a cited file under a tempdir at a unique relative
    /// path that does NOT exist under the test runner's CWD-discovered repo.
    /// If `search_entries` honors `opts.repo_root`, verification reads bytes
    /// from `<tempdir>/<rel>` and succeeds. If it falls back to CWD discovery,
    /// the file is missing under the wrong root and verification returns
    /// `Some(false)`.
    #[test]
    fn test_search_uses_explicit_repo_root_when_cwd_is_unrelated() {
        use sha2::{Digest, Sha256};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Unique relative path so it cannot collide with any file in the real
        // worktree (the CWD-discovered repo root). If find_repo_root() were
        // used instead of opts.repo_root, the verifier would look here:
        //   <real-worktree>/src/__br_bhg_regression_explicit_root__.rs
        // ...which does not exist, and verified would be Some(false).
        let rel = "src/__br_bhg_regression_explicit_root__.rs";
        let cited_content = b"// br-bhg regression: explicit repo_root\n";
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(rel), cited_content).unwrap();

        let mut h = Sha256::new();
        h.update(cited_content);
        let hash = format!("sha256:{:x}", h.finalize());
        let end = cited_content.len();

        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "br-bhg-regression-1",
            "path": "regression/br-bhg",
            "summary": "br-bhg explicit repo_root regression",
            "content": "regression body for br-bhg explicit repo_root",
            "tags": ["regression", "br-bhg"],
            "kind": "observation",
            "evidence_status": "present",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        let evidence_event = serde_json::json!({
            "action": "evidence_add",
            "table": "evidence",
            "entry_id": "br-bhg-regression-1",
            "evidence": {
                "id": "ev-br-bhg-1",
                "entry_id": "br-bhg-regression-1",
                "kind": "code",
                "citation_path": format!("{rel}:0-{end}"),
                "citation_sha": null,
                "citation_hash": hash,
                "citation_excerpt": null,
                "derived_from": null,
                "recorded_at": "2024-01-01T00:00:00Z"
            },
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &evidence_event).unwrap();

        let opts = SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: false,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 10,
            repo_root: Some(root.to_path_buf()),
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };
        let results = search_entries(&conn, &embedder, "br-bhg regression", &opts).unwrap();

        let entry = results
            .iter()
            .find(|r| r.id == "br-bhg-regression-1")
            .expect("entry must be returned by FTS");
        assert_eq!(
            entry.evidence.len(),
            1,
            "entry must have exactly 1 evidence row"
        );
        assert_eq!(
            entry.evidence[0].verified,
            Some(true),
            "explicit opts.repo_root must be used for verification — CWD-based \
             find_repo_root() would not find the cited file under the tempdir, \
             so a Some(true) here proves repo_root threading works (MCP cwd=/ fix)"
        );
    }

    // -----------------------------------------------------------------------
    // T-S6a: event replay convergence proptest (br-jwe.14, AC13)
    // -----------------------------------------------------------------------

    /// Snapshot of the materialized DB state used for convergence comparison.
    /// Sorted so order of insertion does not affect equality.
    #[derive(Debug, PartialEq, Eq)]
    struct DbSnapshot {
        /// (entry_id, kind, evidence_status, is_stale)
        entries: Vec<(String, String, String, i64)>,
        /// (evidence_id, entry_id, kind)
        evidence: Vec<(String, String, String)>,
    }

    fn snapshot_db_state(conn: &Connection) -> anyhow::Result<DbSnapshot> {
        let mut entries: Vec<(String, String, String, i64)> = conn
            .prepare(
                "SELECT id, COALESCE(kind,'belief'), COALESCE(evidence_status,'n/a'), is_stale \
                 FROM entries ORDER BY id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .filter_map(|r| r.ok())
            .collect();
        entries.sort();

        let mut evidence: Vec<(String, String, String)> = conn
            .prepare("SELECT id, entry_id, kind FROM evidence ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        evidence.sort();

        Ok(DbSnapshot { entries, evidence })
    }

    /// Small fixed alphabet keeps generated sequences tractable and maximises
    /// interesting interactions (evidence on same entry, expire then re-upsert, etc.).
    const ENTRY_IDS: &[&str] = &["e1", "e2", "e3"];
    const EVIDENCE_IDS: &[&str] = &["ev1", "ev2", "ev3", "ev4"];
    const KINDS: &[&str] = &["observation", "belief", "procedure", "convention"];
    const EV_KINDS: &[&str] = &["code", "test", "command", "user"];

    fn arb_entry_id() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;
        (0..ENTRY_IDS.len()).prop_map(|i| ENTRY_IDS[i].to_string())
    }

    fn arb_evidence_id() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;
        (0..EVIDENCE_IDS.len()).prop_map(|i| EVIDENCE_IDS[i].to_string())
    }

    fn arb_event() -> impl proptest::strategy::Strategy<Value = serde_json::Value> {
        use proptest::prelude::*;
        prop_oneof![
            // upsert entry
            (arb_entry_id(), 0..KINDS.len()).prop_map(|(id, ki)| {
                serde_json::json!({
                    "action": "upsert",
                    "table": "entries",
                    "id": id,
                    "path": format!("src/{id}.rs"),
                    "summary": format!("summary for {id}"),
                    "content": format!("content for {id}"),
                    "tags": [],
                    "kind": KINDS[ki],
                    "evidence_status": "missing",
                    "ts": "2024-01-01T00:00:00Z"
                })
            }),
            // legacy upsert (no kind/evidence_status fields — pre-Phase-0 event shape).
            // Without this variant, the convergence proptest never exercises the
            // incremental-vs-replay desync the column-default fallback used to hide
            // (br-f7y).
            arb_entry_id().prop_map(|id| {
                serde_json::json!({
                    "action": "upsert",
                    "table": "entries",
                    "id": id,
                    "path": format!("src/{id}.rs"),
                    "summary": format!("legacy summary for {id}"),
                    "content": format!("legacy content for {id}"),
                    "tags": [],
                    "ts": "2023-01-01T00:00:00Z"
                })
            }),
            // expire entry
            arb_entry_id().prop_map(|id| {
                serde_json::json!({
                    "action": "expire",
                    "table": "entries",
                    "id": id
                })
            }),
            // evidence_add
            (arb_entry_id(), arb_evidence_id(), 0..EV_KINDS.len()).prop_map(|(eid, evid, ki)| {
                serde_json::json!({
                    "action": "evidence_add",
                    "table": "evidence",
                    "entry_id": eid,
                    "evidence": {
                        "id": evid,
                        "entry_id": eid,
                        "kind": EV_KINDS[ki],
                        "citation_path": null,
                        "citation_sha": null,
                        "citation_hash": "abc123",
                        "citation_excerpt": null,
                        "derived_from": null,
                        "recorded_at": "2024-01-01T00:00:00Z"
                    },
                    "ts": "2024-01-01T00:00:00Z"
                })
            }),
            // evidence_expire
            (arb_entry_id(), arb_evidence_id()).prop_map(|(eid, evid)| {
                serde_json::json!({
                    "action": "evidence_expire",
                    "table": "evidence",
                    "entry_id": eid,
                    "evidence_id": evid,
                    "reason": "test",
                    "ts": "2024-01-01T00:00:00Z"
                })
            }),
        ]
    }

    /// br-und (security I3): search_entries must skip verification for entries
    /// whose evidence rows sum to >MAX_PER_ENTRY_BYTES, emitting a warning.
    /// Capping per-entry bytes prevents one pathological entry from exhausting
    /// memory in search result processing.
    #[test]
    fn test_search_caps_evidence_bytes_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create a file to cite
        let cited_content = b"test file for byte capping";
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/big.rs"), cited_content).unwrap();

        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(cited_content);
        let hash = format!("sha256:{:x}", h.finalize());

        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // Upsert one entry
        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "byte-cap-test",
            "path": "src/cap.rs",
            "summary": "byte cap test",
            "content": "c",
            "tags": [],
            "kind": "observation",
            "evidence_status": "present",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        // Insert evidence rows that collectively exceed MAX_PER_ENTRY_BYTES.
        // Each row cites a range; we'll add multiple rows to exceed 8MiB.
        let huge_range_bytes = 5 * 1024 * 1024; // 5 MiB per row
        for i in 0..2 {
            let ev_id = format!("ev-huge-{i}");
            let citation_path = format!("src/big.rs:0-{}", huge_range_bytes);
            conn.execute(
                "INSERT INTO evidence(id, entry_id, kind, citation_path, citation_hash, recorded_at)
                 VALUES(?1, ?2, 'code', ?3, ?4, ?5)",
                params![ev_id, "byte-cap-test", citation_path, hash, "2024-01-01T00:00:00Z"],
            ).unwrap();
        }

        // Search with inline_verify_k=10 so we'd normally verify
        let opts = SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: false,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 10,
            repo_root: Some(root.to_path_buf()),
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };

        let results = search_entries(&conn, &embedder, "byte cap test", &opts).unwrap();
        assert!(!results.is_empty(), "entry must be in results");

        let entry = results.iter().find(|r| r.id == "byte-cap-test").unwrap();
        assert_eq!(entry.evidence.len(), 2, "entry must have 2 evidence rows");

        // Both evidence rows should have verified=None because total exceeds cap
        for ev in &entry.evidence {
            assert_eq!(
                ev.verified, None,
                "evidence must not be verified when entry bytes exceed MAX_PER_ENTRY_BYTES"
            );
        }
    }

    /// RRF fusion correctness (br-improvement-catalog-23b.5):
    ///
    /// Scenario: two entries — entry A appears in BOTH FTS and semantic results,
    /// entry B appears in ONLY the semantic result with a numerically higher raw
    /// cosine score. RRF fused scoring must rank entry A above entry B because A
    /// earns rank-contribution from two sources.
    ///
    /// With NoopEmbedder the semantic lane is skipped entirely, so we test RRF via
    /// direct calls to `search_entries` with a hand-crafted in-memory DB where
    /// embeddings are injected directly. However since NoopEmbedder prevents the
    /// semantic path, we test the following invariants that *are* observable with
    /// NoopEmbedder:
    ///
    /// 1. Hybrid mode with only FTS active assigns score_kind="fts" to results.
    /// 2. FTS-only mode assigns score_kind="fts".
    /// 3. Semantic-only mode with NoopEmbedder returns empty (noop skips it) — but
    ///    if it weren't noop, score_kind="semantic" would be assigned.
    /// 4. score_kind field exists on SearchEntry (compilation check).
    ///
    /// The RRF-beats-raw-score property is tested via unit logic in the rrf test
    /// below that bypasses the embedder by inserting embeddings directly and using
    /// a FakeEmbedder that returns a fixed vector.
    #[test]
    fn test_rrf_score_kind_fts_only_mode() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "rrf-fts-1",
            "path": "src/rrf_test.rs",
            "summary": "rrf fts score_kind test entry",
            "content": "body",
            "tags": ["rrf"],
            "kind": "convention",
            "evidence_status": "n/a",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        // FTS-only mode
        let opts_fts = SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: false,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 0,
            repo_root: None,
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };
        let results =
            search_entries(&conn, &embedder, "rrf fts score_kind test entry", &opts_fts).unwrap();
        assert!(!results.is_empty(), "FTS must return the entry");
        for r in &results {
            assert_eq!(r.score_kind, "fts", "FTS-only mode must set score_kind=fts");
        }

        // Hybrid mode (semantic skipped because NoopEmbedder) — FTS results get score_kind=fts
        let opts_hybrid = SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: true,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 0,
            repo_root: None,
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };
        let hybrid_results = search_entries(
            &conn,
            &embedder,
            "rrf fts score_kind test entry",
            &opts_hybrid,
        )
        .unwrap();
        assert!(
            !hybrid_results.is_empty(),
            "Hybrid must return the entry via FTS lane"
        );
        for r in &hybrid_results {
            assert_eq!(
                r.score_kind, "fts",
                "Hybrid FTS-only path must set score_kind=fts"
            );
        }
    }

    /// RRF ranking: when a real embedder is present, an entry appearing in both
    /// FTS and semantic results must outscore an entry that appears in only one
    /// source, even if the single-source entry has a higher raw semantic score.
    ///
    /// We simulate this with a FakeEmbedder that returns fixed vectors, injecting
    /// embeddings directly via the entries_emb table so we control both lanes.
    #[test]
    fn test_rrf_fusion_dual_source_beats_high_raw_semantic_score() {
        use crate::models::{blob_to_f32s, f32s_to_blob};

        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // Entry A: appears in FTS (summary contains query token) and we'll
        // inject a moderate semantic embedding.
        let upsert_a = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "rrf-dual-A",
            "path": "src/dual_a.rs",
            "summary": "rrf dual source alpha needle",
            "content": "body a",
            "tags": [],
            "kind": "convention",
            "evidence_status": "n/a",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert_a).unwrap();

        // Entry B: does NOT appear in FTS (different summary), but we'll inject
        // a very high-similarity semantic embedding so it would win raw-score sort.
        let upsert_b = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "rrf-dual-B",
            "path": "src/dual_b.rs",
            "summary": "completely unrelated summary zzzz",
            "content": "body b",
            "tags": [],
            "kind": "convention",
            "evidence_status": "n/a",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert_b).unwrap();

        // Get the rowids for A and B
        let rowid_a: i64 = conn
            .query_row("SELECT rowid FROM entries WHERE id='rrf-dual-A'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let rowid_b: i64 = conn
            .query_row("SELECT rowid FROM entries WHERE id='rrf-dual-B'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // Query vector: [1.0, 0.0] (unit vector along dim-0)
        let q_vec: Vec<f32> = vec![1.0, 0.0];

        // Entry A embedding: moderate similarity = [0.8, 0.6] → sim ≈ 0.8
        let emb_a: Vec<f32> = vec![0.8, 0.6];
        // Entry B embedding: very high similarity = [1.0, 0.0] → sim = 1.0
        let emb_b: Vec<f32> = vec![1.0, 0.0];

        // Insert embeddings
        conn.execute(
            "INSERT OR REPLACE INTO entries_emb(rowid, embedding) VALUES(?1, ?2)",
            rusqlite::params![rowid_a, f32s_to_blob(&emb_a)],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO entries_emb(rowid, embedding) VALUES(?1, ?2)",
            rusqlite::params![rowid_b, f32s_to_blob(&emb_b)],
        )
        .unwrap();

        // Use a FakeEmbedder that returns q_vec = [1.0, 0.0]
        struct FixedEmbedder(Vec<f32>);
        impl crate::components::embedder::Embedder for FixedEmbedder {
            fn embed(&self, _: &str) -> anyhow::Result<Vec<f32>> {
                Ok(self.0.clone())
            }
            fn is_noop(&self) -> bool {
                false
            }
        }
        let fixed_emb = FixedEmbedder(q_vec);

        // Hybrid search: FTS will match A (has "needle"), semantic matches both.
        // Raw sort: B(sim=1.0) > A(sim=0.8) → B would win.
        // RRF: A gets rank contribution from FTS(rank=1) + semantic(rank=2),
        //      B gets contribution from semantic only(rank=1).
        // RRF scores (k=60):
        //   A: 1/(60+1) + 1/(60+2) = 0.016393 + 0.016129 = 0.032522
        //   B: 1/(60+1)             = 0.016393
        // A must outrank B.
        let opts = SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: true,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 0,
            repo_root: None,
            verify_pool_size: None,
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };
        let results =
            search_entries(&conn, &fixed_emb, "rrf dual source alpha needle", &opts).unwrap();

        assert!(
            results.len() >= 2,
            "both entries must be returned, got {}",
            results.len()
        );

        let pos_a = results
            .iter()
            .position(|r| r.id == "rrf-dual-A")
            .expect("A must be in results");
        let pos_b = results
            .iter()
            .position(|r| r.id == "rrf-dual-B")
            .expect("B must be in results");
        assert!(
            pos_a < pos_b,
            "RRF: dual-source A (rank={pos_a}) must beat single-source B (rank={pos_b}) even though B has higher raw semantic score"
        );

        // score_kind for hybrid results must be "rrf"
        for r in &results {
            assert_eq!(
                r.score_kind, "rrf",
                "hybrid RRF results must have score_kind=rrf, got {}",
                r.score_kind
            );
        }
    }

    // -----------------------------------------------------------------------
    // br-improvement-catalog-23b.6: entries_emb GC on expire and is_stale
    // -----------------------------------------------------------------------

    /// Regression: insert an entry with a real embedder, expire it, confirm the
    /// corresponding entries_emb row is deleted in the same transaction.
    ///
    /// Uses a FakeEmbedder (returns a fixed non-empty vector) to produce an
    /// actual entries_emb row on upsert. After expire the row must be gone.
    #[test]
    fn test_expire_deletes_entries_emb_row() {
        use crate::models::f32s_to_blob;

        struct FakeEmbedder;
        impl crate::components::embedder::Embedder for FakeEmbedder {
            fn embed(&self, _: &str) -> anyhow::Result<Vec<f32>> {
                Ok(vec![0.1_f32, 0.2_f32, 0.3_f32])
            }
            fn is_noop(&self) -> bool {
                false
            }
        }

        let conn = open_db_memory().unwrap();
        let embedder = FakeEmbedder;

        // Upsert — should write entries_emb row
        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "emb-gc-e1",
            "path": "src/gc_test.rs",
            "summary": "gc test entry",
            "content": "some content",
            "tags": [],
            "kind": "observation",
            "evidence_status": "missing",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        // entries_emb row must exist before expire
        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries_emb WHERE rowid = \
             (SELECT rowid FROM entries WHERE id='emb-gc-e1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 1, "entries_emb row must exist after upsert");

        // Expire
        let expire = serde_json::json!({
            "action": "expire",
            "table": "entries",
            "id": "emb-gc-e1"
        });
        apply_event(&conn, &embedder, &expire).unwrap();

        // entries_emb row must be gone after expire
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries_emb WHERE rowid = \
             (SELECT rowid FROM entries WHERE id='emb-gc-e1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            after, 0,
            "expire must delete the entries_emb row (GC regression)"
        );
    }

    /// Regression: upsert with is_stale=true must delete any existing entries_emb row.
    ///
    /// Simulates the compact path: an entry exists with an embedding, then compact
    /// replays a stale upsert — the embedding orphan must be cleaned up.
    #[test]
    fn test_stale_upsert_deletes_entries_emb_row() {
        use crate::models::f32s_to_blob;

        struct FakeEmbedder;
        impl crate::components::embedder::Embedder for FakeEmbedder {
            fn embed(&self, _: &str) -> anyhow::Result<Vec<f32>> {
                Ok(vec![0.4_f32, 0.5_f32])
            }
            fn is_noop(&self) -> bool {
                false
            }
        }

        let conn = open_db_memory().unwrap();
        let embedder = FakeEmbedder;

        // Upsert live entry — writes entries_emb row
        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "emb-gc-stale1",
            "path": "src/stale_gc.rs",
            "summary": "stale gc test",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "missing",
            "ts": "2024-01-01T00:00:00Z"
        });
        apply_event(&conn, &embedder, &upsert).unwrap();

        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries_emb WHERE rowid = \
             (SELECT rowid FROM entries WHERE id='emb-gc-stale1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 1, "entries_emb row must exist after live upsert");

        // Stale upsert (as produced by compact for an expired entry)
        let stale_upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "emb-gc-stale1",
            "path": "src/stale_gc.rs",
            "summary": "stale gc test",
            "content": "content",
            "tags": [],
            "kind": "belief",
            "evidence_status": "missing",
            "is_stale": true,
            "ts": "2024-01-02T00:00:00Z"
        });
        apply_event(&conn, &embedder, &stale_upsert).unwrap();

        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries_emb WHERE rowid = \
             (SELECT rowid FROM entries WHERE id='emb-gc-stale1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            after, 0,
            "is_stale=true upsert must delete entries_emb row (GC regression)"
        );
    }

    /// Steady-state invariant: after any sequence of upserts and expires,
    /// COUNT(entries_emb) == COUNT(entries WHERE is_stale=0).
    ///
    /// Uses a FakeEmbedder so all live entries get embedding rows written.
    #[test]
    fn test_entries_emb_count_equals_live_entries_invariant() {
        struct FakeEmbedder;
        impl crate::components::embedder::Embedder for FakeEmbedder {
            fn embed(&self, _: &str) -> anyhow::Result<Vec<f32>> {
                Ok(vec![1.0_f32, 0.0_f32])
            }
            fn is_noop(&self) -> bool {
                false
            }
        }

        let conn = open_db_memory().unwrap();
        let embedder = FakeEmbedder;

        // Insert N=6 entries
        for i in 0..6_u32 {
            let ev = serde_json::json!({
                "action": "upsert",
                "table": "entries",
                "id": format!("inv-{i}"),
                "path": format!("src/inv_{i}.rs"),
                "summary": format!("invariant entry {i}"),
                "content": "c",
                "tags": [],
                "kind": "belief",
                "evidence_status": "missing",
                "ts": "2024-01-01T00:00:00Z"
            });
            apply_event(&conn, &embedder, &ev).unwrap();
        }

        // Expire half (entries 0, 2, 4)
        for i in (0..6_u32).step_by(2) {
            let ev = serde_json::json!({
                "action": "expire",
                "table": "entries",
                "id": format!("inv-{i}")
            });
            apply_event(&conn, &embedder, &ev).unwrap();
        }

        let emb_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries_emb", [], |r| r.get(0))
            .unwrap();
        let live_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| {
                r.get(0)
            })
            .unwrap();

        assert_eq!(
            emb_count, live_count,
            "steady-state invariant: COUNT(entries_emb)={emb_count} must equal \
             COUNT(entries WHERE is_stale=0)={live_count}"
        );
        // Sanity: N/2 = 3 live entries
        assert_eq!(live_count, 3, "half the entries should be live");
    }

    // -----------------------------------------------------------------------
    // br-improvement-catalog-23b.13: bounded verify pool — thread fan-out must
    // not exceed pool size.
    //
    // Scenario: 10 entries × 50 evidence rows each. The old unbounded
    // thread::scope spawned 50 OS threads per scope call; with the bounded
    // pool concurrent thread count is limited to `verify_pool_size` (2 here).
    //
    // Samples /proc/self/task at 50µs intervals; asserts peak threads ≤
    // baseline + pool_size + 2 (slack: sampler thread + spare).
    // -----------------------------------------------------------------------
    #[test]
    fn test_verify_pool_thread_fan_out_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // 10 entries × 50 evidence rows each.
        let empty_hash = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        for i in 0..10usize {
            let upsert = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": format!("pool-test-{i}"),
                "path": format!("src/pool_test_{i}.rs"),
                "summary": format!("bounded pool fan-out test entry {i}"),
                "content": format!("content {i}"),
                "tags": ["pool-test"],
                "kind": "observation",
                "evidence_status": "present",
                "ts": "2024-01-01T00:00:00Z"
            });
            apply_event(&conn, &embedder, &upsert).unwrap();
            for j in 0..50usize {
                conn.execute(
                    "INSERT INTO evidence(id, entry_id, kind, citation_path, citation_hash, recorded_at)
                     VALUES(?1, ?2, 'code', ?3, ?4, ?5)",
                    rusqlite::params![
                        format!("pool-ev-{i}-{j:03}"),
                        format!("pool-test-{i}"),
                        format!("src/nonexistent_{i}_{j}.rs:0-1"),
                        empty_hash,
                        format!("2024-01-01T00:00:{:02}Z", j % 60),
                    ],
                ).unwrap();
            }
        }

        let pool_size: usize = 2;

        #[cfg(target_os = "linux")]
        let baseline = std::fs::read_dir("/proc/self/task")
            .map(|d| d.count())
            .unwrap_or(1);
        #[cfg(not(target_os = "linux"))]
        let baseline = 1usize;

        let peak_threads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(baseline));
        let stop_sampler = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peak_clone = std::sync::Arc::clone(&peak_threads);
        let stop_clone = std::sync::Arc::clone(&stop_sampler);
        let sampler = std::thread::spawn(move || {
            while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                #[cfg(target_os = "linux")]
                if let Ok(d) = std::fs::read_dir("/proc/self/task") {
                    let count = d.count();
                    let prev = peak_clone.load(std::sync::atomic::Ordering::Relaxed);
                    if count > prev {
                        peak_clone.store(count, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        });

        let opts = SearchOptions {
            limit: 10,
            do_fts: true,
            do_semantic: false,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 10,
            repo_root: Some(root.to_path_buf()),
            verify_pool_size: Some(pool_size),
            recency_lambda: 0.0,
            mmr_lambda: 0.0,
        };
        let _results = search_entries(&conn, &embedder, "bounded pool fan-out test entry", &opts)
            .expect("search must succeed");

        stop_sampler.store(true, std::sync::atomic::Ordering::Relaxed);
        sampler.join().unwrap();

        let peak = peak_threads.load(std::sync::atomic::Ordering::Relaxed);
        let allowed = baseline + pool_size + 2;
        assert!(
            peak <= allowed,
            "thread fan-out bounded: peak={peak}, baseline={baseline}, \
             pool_size={pool_size}, allowed={allowed}. \
             Old unbounded code peaks at 50+ threads (one per evidence row)."
        );
    }

    #[test]
    fn test_entries_fts_v2_trigger_idempotency() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "e1",
            "path": "src/lib.rs",
            "summary": "fts v2 idempotency test",
            "content": "some content",
            "tags": ["test"],
            "ts": "2024-01-01T00:00:00Z"
        });

        // First upsert
        apply_event(&conn, &embedder, &upsert).unwrap();
        // Second upsert with identical content (ON CONFLICT DO UPDATE path)
        apply_event(&conn, &embedder, &upsert).unwrap();

        // Exactly one FTS v2 row — search by summary term (id is UNINDEXED).
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM entries_fts_v2 WHERE entries_fts_v2 MATCH 'idempotency'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "duplicate upsert must not create two FTS v2 rows");

        // Expire: trigger must remove the FTS v2 row
        let expire = serde_json::json!({
            "action": "expire",
            "table": "entries",
            "id": "e1",
            "ts": "2024-01-01T00:00:00Z",
            "session": "test"
        });
        apply_event(&conn, &embedder, &expire).unwrap();

        let count_after: i64 = conn
            .query_row(
                "SELECT count(*) FROM entries_fts_v2 WHERE entries_fts_v2 MATCH 'idempotency'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count_after, 0, "expired entry must be absent from FTS v2");
    }

    proptest::proptest! {
        /// Replaying an arbitrary sequence of Add/EvidenceAdd/EvidenceExpire/Expire
        /// events into an in-memory DB produces the same materialized state as
        /// direct reduction via apply_event. Models TLA+ PartitionEquivalent.
        ///
        /// Two paths must converge:
        /// - Path A: apply all events one at a time sequentially.
        /// - Path B: apply the same sequence split into two batches (simulating
        ///   a snapshot + catchup replay as in the 3-phase rebuild).
        #[test]
        fn proptest_event_replay_convergence(
            events in proptest::collection::vec(arb_event(), 0..32),
        ) {
            let conn1 = open_db_memory().unwrap();
            let conn2 = open_db_memory().unwrap();
            let embedder = NoopEmbedder;

            // Path A: apply events one at a time
            for ev in &events {
                apply_event(&conn1, &embedder, ev).unwrap();
            }

            // Path B: apply events in two batches (snapshot + catchup)
            let split = events.len() / 2;
            for ev in &events[..split] {
                apply_event(&conn2, &embedder, ev).unwrap();
            }
            for ev in &events[split..] {
                apply_event(&conn2, &embedder, ev).unwrap();
            }

            // Both must produce identical final state on entries + evidence + status
            let state1 = snapshot_db_state(&conn1).unwrap();
            let state2 = snapshot_db_state(&conn2).unwrap();
            proptest::prop_assert_eq!(state1, state2);
        }
    }

    // US-3: dual-write correctness — both FTS tables return the same IDs
    // after applying a batch of upsert/expire events.
    #[test]
    fn test_dual_write_both_fts_tables_agree() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        let events = [
            serde_json::json!({"action":"upsert","table":"entries","id":"dw1","path":"src/main.rs","summary":"dual write alpha","content":"alpha content","tags":["rust"],"ts":"2024-01-01T00:00:00Z"}),
            serde_json::json!({"action":"upsert","table":"entries","id":"dw2","path":"src/lib.rs","summary":"dual write beta","content":"beta content","tags":["rust"],"ts":"2024-01-01T00:00:01Z"}),
            serde_json::json!({"action":"upsert","table":"entries","id":"dw3","path":"docs/foo.md","summary":"dual write gamma","content":"gamma content","tags":["docs"],"ts":"2024-01-01T00:00:02Z"}),
            // Expire one to verify both tables remove it
            serde_json::json!({"action":"expire","table":"entries","id":"dw2","ts":"2024-01-01T01:00:00Z","session":"test"}),
        ];

        for ev in &events {
            apply_event(&conn, &embedder, ev).unwrap();
        }

        let opts = SearchOptions {
            do_fts: true,
            do_semantic: false,
            limit: 20,
            ..Default::default()
        };
        let safe_q = "\"dual\"";

        let v1_ids: Vec<String> = fts_query_contentless(&conn, safe_q, &opts)
            .unwrap()
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        let v2_ids: Vec<String> = fts_query_content_entries(&conn, safe_q, &opts)
            .unwrap()
            .into_iter()
            .map(|(id, ..)| id)
            .collect();

        assert_eq!(
            v1_ids.iter().collect::<std::collections::BTreeSet<_>>(),
            v2_ids.iter().collect::<std::collections::BTreeSet<_>>(),
            "both FTS tables must return the same entry IDs"
        );
        assert!(
            !v1_ids.contains(&"dw2".to_string()),
            "expired entry dw2 must be absent"
        );
        assert!(v1_ids.contains(&"dw1".to_string()), "dw1 must be present");
        assert!(v1_ids.contains(&"dw3".to_string()), "dw3 must be present");
    }

    #[test]
    fn test_deprecation_gate_drops_contentless_fts_when_all_signals_met() {
        let conn = open_db_memory().unwrap();
        let embedder = NoopEmbedder;

        // Apply 1000 upsert/expire cycles to satisfy post_cutover_writes >= 1000.
        for i in 0..500 {
            let ev = serde_json::json!({
                "action": "upsert", "table": "entries",
                "id": format!("gate-{i}"), "path": "src/gate.rs",
                "summary": format!("gate test {i}"),
                "content": "gate content", "tags": ["gate"],
                "ts": "2024-01-01T00:00:00Z"
            });
            apply_event(&conn, &embedder, &ev).unwrap();
            let exp = serde_json::json!({
                "action": "expire", "table": "entries",
                "id": format!("gate-{i}"), "ts": "2024-01-02T00:00:00Z", "session": "test"
            });
            apply_event(&conn, &embedder, &exp).unwrap();
        }

        // Verify counter accumulated
        let writes: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM fts5_deprecation_gate WHERE key='post_cutover_writes'",
                [], |r| r.get(0),
            )
            .unwrap();
        assert!(
            writes >= 1000,
            "expected ≥1000 post_cutover_writes, got {writes}"
        );

        // entries_fts still present — gate not yet open (other signals unset)
        let fts_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entries_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_exists, 1,
            "entries_fts must still exist before all gate signals are met"
        );

        // Satisfy remaining gate signals
        set_deprecation_gate(&conn, "rollback_invocations", "0").unwrap();
        set_deprecation_gate(&conn, "parity_rerun_divergence", "0").unwrap();
        set_deprecation_gate(&conn, "rollback_drill_passed", "1").unwrap();

        // Now trigger the check
        maybe_drop_contentless_fts(&conn).unwrap();

        // entries_fts must be gone
        let fts_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entries_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_after, 0,
            "entries_fts must be dropped after all gate signals are met"
        );

        // Idempotency: calling again must not error
        maybe_drop_contentless_fts(&conn).unwrap();

        // entries_fts_v2 (content='entries') must still be intact
        let v2_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entries_fts_v2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            v2_exists, 1,
            "entries_fts_v2 must remain after contentless drop"
        );

        // Post-drop writes must succeed: apply_event must not fail because entries_fts is gone.
        let post_drop_upsert = serde_json::json!({
            "action": "upsert", "table": "entries",
            "id": "post-drop-1", "path": "src/post_drop.rs",
            "summary": "post-drop write", "content": "content after drop",
            "tags": ["post-drop"], "ts": "2024-01-03T00:00:00Z"
        });
        apply_event(&conn, &embedder, &post_drop_upsert)
            .expect("upsert after entries_fts drop must succeed");
        let post_drop_expire = serde_json::json!({
            "action": "expire", "table": "entries",
            "id": "post-drop-1", "ts": "2024-01-04T00:00:00Z", "session": "test"
        });
        apply_event(&conn, &embedder, &post_drop_expire)
            .expect("expire after entries_fts drop must succeed");
    }
}
