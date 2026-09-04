//! `compress` subcommand — semantic paragraph deduplication for bloated KB entries

#![allow(deprecated)] // db::open_db (ADR-1) — remaining call sites migrate in C2/L1b, L2, L3, L1c
use crate::commands::add::make_embedder;
use crate::commands::add_validation::compute_evidence_status_write;
use crate::components::{kb_core, redactor, text_chunker};
use crate::config;
use crate::models::cosine_similarity;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::{params, OptionalExtension};

/// Compress a KB entry by deduplicating semantically similar paragraphs
#[derive(Command, Debug, Parser)]
pub struct Compress {
    /// KB path to compress (e.g. "architecture/search/recency-bias")
    pub path: String,

    /// Compress entries whose body exceeds this many chars (default: from config)
    #[arg(long)]
    pub threshold_chars: Option<usize>,

    /// Show what would change without writing
    #[arg(long)]
    pub dry_run: bool,
}

impl Runnable for Compress {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Compress {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let kb_config = config::KbConfig::from_paths(&paths);
        let emb = make_embedder(&paths);
        run(self, &kb_config, &paths, emb.as_ref())
    }
}

pub fn run(
    compress: &Compress,
    config: &config::KbConfig,
    paths: &config::Paths,
    embedder: &dyn crate::components::embedder::Embedder,
) -> anyhow::Result<()> {
    use crate::components::db;

    let threshold = compress
        .threshold_chars
        .unwrap_or(config.compress_threshold);

    // Step 1: load the most recent non-stale entry at the given path.
    let conn = db::open_db(&paths.db)?;
    let row: Option<(String, String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, summary, content, kind FROM entries WHERE path = ?1 AND is_stale = 0 ORDER BY rowid DESC LIMIT 1",
        )?;
        stmt.query_row(params![compress.path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .optional()?
    };

    let (entry_id, summary, content, kind) = match row {
        None => {
            println!("nothing to compress: no entry found at '{}'", compress.path);
            return Ok(());
        }
        Some(r) => r,
    };

    // If already below threshold, nothing to do (idempotence).
    if content.len() <= threshold {
        println!(
            "nothing to compress: entry at '{}' is {} chars (threshold {})",
            compress.path,
            content.len(),
            threshold
        );
        return Ok(());
    }

    // Step 3: fail loudly when embeddings are unavailable.
    if embedder.is_noop() {
        anyhow::bail!("kb compress requires embeddings; KB_NO_EMBED=1 is set");
    }

    // Step 2: split into paragraphs. Cap at 512 to keep O(n²) cosine dedup
    // bounded even if content is unusually dense with short paragraphs.
    const MAX_PARAGRAPHS: usize = 512;
    let paragraphs: Vec<String> = text_chunker::split_paragraphs(&content, 100)
        .into_iter()
        .take(MAX_PARAGRAPHS)
        .collect();
    let original_count = paragraphs.len();

    // Step 4: embed each paragraph.
    let embeddings: Vec<Vec<f32>> = paragraphs
        .iter()
        .map(|p| embedder.embed(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        embeddings.iter().flatten().all(|x| x.is_finite()),
        "embedder returned a non-finite component"
    );

    // Step 5: greedy cosine deduplication — keep first, drop subsequent near-duplicates.
    let cutoff = config.compress_cosine_cutoff;
    let mut keep: Vec<bool> = vec![true; paragraphs.len()];
    for i in 0..paragraphs.len() {
        if !keep[i] {
            continue;
        }
        for j in (i + 1)..paragraphs.len() {
            if !keep[j] {
                continue;
            }
            if is_near_duplicate(&embeddings[i], &embeddings[j], cutoff) {
                keep[j] = false;
            }
        }
    }

    let surviving: Vec<&str> = paragraphs
        .iter()
        .zip(keep.iter())
        .filter_map(|(p, &k)| if k { Some(p.as_str()) } else { None })
        .collect();
    let kept_count = surviving.len();

    // Step 6: assemble compressed body.
    let compressed_body = format!(
        "(compressed {}→{} paragraphs)\n\n{}",
        original_count,
        kept_count,
        surviving.join("\n\n")
    );

    // Step 7: redact defensively.
    let compressed_body = redactor::redact_str(&compressed_body).into_owned();

    let original_chars = content.len();
    let new_chars = compressed_body.len();

    // Step 8: dry-run — print and return without writing.
    if compress.dry_run {
        println!("dry-run: would compress '{}':", compress.path);
        println!(
            "  {}→{} chars, {}→{} paragraphs",
            original_chars, new_chars, original_count, kept_count
        );
        println!("---\n{compressed_body}\n---");
        return Ok(());
    }

    // Step 9: write back via kb_core::add with replace_path=true.
    // Drop the conn before acquiring the flock inside kb_core::add.
    drop(conn);

    // Fetch tags + evidence for the existing entry so the compressed version
    // preserves the original's evidential standing (required for mandated kinds).
    let conn2 = db::open_db(&paths.db)?;
    let tags_json: serde_json::Value = {
        let mut stmt = conn2.prepare("SELECT tags FROM entries WHERE id = ?1")?;
        let tags_str: String = stmt.query_row(params![entry_id], |r| r.get(0))?;
        serde_json::from_str(&tags_str).unwrap_or(serde_json::json!([]))
    };
    let evidence_rows: Vec<serde_json::Value> = {
        let mut stmt = conn2.prepare(
            "SELECT kind, citation_path, citation_sha, citation_hash, citation_excerpt, derived_from
             FROM evidence WHERE entry_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![entry_id], |r| {
                Ok(serde_json::json!({
                    "kind":             r.get::<_, String>(0)?,
                    "citation_path":    r.get::<_, Option<String>>(1)?,
                    "citation_sha":     r.get::<_, Option<String>>(2)?,
                    "citation_hash":    r.get::<_, String>(3)?,
                    "citation_excerpt": r.get::<_, Option<String>>(4)?,
                    "derived_from":     r.get::<_, Option<String>>(5)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<serde_json::Value>>>()?;
        rows
    };
    drop(conn2);

    let ts = chrono::Utc::now().to_rfc3339();
    let new_id = uuid::Uuid::new_v4().to_string();
    let evidence_status = compute_evidence_status_write(&kind, &evidence_rows);

    let add_args = kb_core::AddArgs {
        id: new_id,
        path: compress.path.clone(),
        summary: summary.clone(),
        content: compressed_body,
        tags: tags_json,
        version_ref: None,
        permanent: false,
        replace_path: true,
        kind,
        evidence_status: evidence_status.to_string(),
        evidence_rows,
        ts,
        session: "cli".to_string(),
        session_id: None,
        expire_reason: "compressed by kb compress".to_string(),
        dedup_cutoff: None,
        cues: vec![],
    };

    kb_core::add(paths, embedder, add_args)?;

    // Step 10: print summary.
    println!(
        "Compressed {}: {}→{} chars, {}→{} paragraphs",
        compress.path, original_chars, new_chars, original_count, kept_count
    );

    Ok(())
}

fn is_near_duplicate(a: &[f32], b: &[f32], cutoff: f32) -> bool {
    cosine_similarity(a, b) > cutoff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::add::Add;
    use crate::components::db;
    use crate::components::embedder::Embedder;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::{KbConfig, Paths};
    use std::fs;
    use tempfile::tempdir;

    struct TestEmbedder;

    impl Embedder for TestEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![text.len() as f32, 1.0])
        }
    }

    fn make_paths(root: &std::path::Path) -> Paths {
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        Paths::from_root(root)
    }

    fn add_entry(paths: &Paths, path: &str, content: &str) {
        let emb = NoopEmbedder;
        let cmd = Add {
            path: path.to_string(),
            summary: "test entry".to_string(),
            content: content.to_string(),
            tags: "test".to_string(),
            version_ref: None,
            id: None,
            permanent: false,
            replace_path: false,
            kind: "memory".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        cmd.execute_with(paths, &emb).unwrap();
    }

    #[test]
    fn test_compress_nothing_to_compress_below_threshold() {
        let dir = tempdir().unwrap();
        let paths = make_paths(dir.path());
        add_entry(&paths, "docs/small", "short body");
        let config = KbConfig::default();
        let emb = NoopEmbedder;
        let cmd = Compress {
            path: "docs/small".to_string(),
            threshold_chars: Some(100),
            dry_run: false,
        };
        // Should succeed without error (body is below threshold).
        run(&cmd, &config, &paths, &emb).unwrap();
    }

    #[test]
    fn test_compress_no_entry_found() {
        let dir = tempdir().unwrap();
        let paths = make_paths(dir.path());
        let config = KbConfig::default();
        let emb = NoopEmbedder;
        let cmd = Compress {
            path: "does/not/exist".to_string(),
            threshold_chars: None,
            dry_run: false,
        };
        run(&cmd, &config, &paths, &emb).unwrap();
    }

    #[test]
    fn test_compress_noop_embedder_errors_when_above_threshold() {
        let dir = tempdir().unwrap();
        let paths = make_paths(dir.path());
        let large = "x".repeat(5000);
        add_entry(&paths, "docs/large", &large);
        let config = KbConfig::default();
        let emb = NoopEmbedder;
        let cmd = Compress {
            path: "docs/large".to_string(),
            threshold_chars: Some(100),
            dry_run: false,
        };
        let err = run(&cmd, &config, &paths, &emb).unwrap_err();
        assert!(
            err.to_string().contains("KB_NO_EMBED"),
            "expected KB_NO_EMBED error, got: {err}"
        );
    }

    #[test]
    fn test_compress_dry_run_no_writes() {
        let dir = tempdir().unwrap();
        let paths = make_paths(dir.path());
        let large = "x".repeat(5000);
        add_entry(&paths, "docs/dry", &large);
        let config = KbConfig::default();
        let emb = NoopEmbedder;
        let cmd = Compress {
            path: "docs/dry".to_string(),
            threshold_chars: Some(100),
            dry_run: true,
        };
        // With NoopEmbedder above threshold → should hit the is_noop() guard before dry_run output.
        // dry_run check is AFTER the noop guard, so this returns Err.
        let result = run(&cmd, &config, &paths, &emb);
        assert!(result.is_err());
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn nan_embedding_is_not_promoted_by_compress_cutoff() {
        assert!(!is_near_duplicate(&[f32::NAN, 1.0], &[1.0, 1.0], 0.9));
    }

    #[test]
    fn test_compress_propagates_evidence_row_decode_failure() {
        let dir = tempdir().unwrap();
        let (paths, conn) = db::test_db(dir.path());
        let emb = NoopEmbedder;
        let content = "paragraph one\n\nparagraph two\n\nparagraph three".repeat(80);
        let upsert = serde_json::json!({
            "action": "upsert",
            "table": "entries",
            "id": "compress-corrupt",
            "path": "docs/compress-corrupt",
            "summary": "entry",
            "content": content,
            "tags": [],
            "kind": "belief",
            "evidence_status": "missing",
            "ts": "2024-01-01T00:00:00Z"
        });
        db::apply_event(&conn, &emb, &upsert).unwrap();
        let evidence = serde_json::json!({
            "action": "evidence_add",
            "table": "evidence",
            "entry_id": "compress-corrupt",
            "evidence": {
                "id": "compress-ev",
                "entry_id": "compress-corrupt",
                "kind": "code",
                "citation_path": "src/lib.rs:1-2",
                "citation_sha": null,
                "citation_hash": "sha256:ok",
                "citation_excerpt": null,
                "derived_from": null,
                "recorded_at": "2024-01-01T00:00:00Z"
            },
            "ts": "2024-01-01T00:00:00Z"
        });
        db::apply_event(&conn, &emb, &evidence).unwrap();
        conn.execute(
            "UPDATE evidence SET citation_hash = CAST(X'00' AS BLOB) WHERE id = ?1",
            rusqlite::params!["compress-ev"],
        )
        .unwrap();
        drop(conn);

        let config = KbConfig {
            compress_threshold: 100,
            ..KbConfig::default()
        };
        let cmd = Compress {
            path: "docs/compress-corrupt".to_string(),
            threshold_chars: Some(100),
            dry_run: false,
        };
        let err = run(&cmd, &config, &paths, &TestEmbedder).unwrap_err();
        assert!(
            err.to_string().contains("Invalid column type")
                || err.to_string().contains("invalid column type"),
            "expected decode failure, got: {err}"
        );
    }

    #[test]
    fn test_compress_strands_no_evidence_on_expired_entry_id() {
        let dir = tempdir().unwrap();
        // add_locked resolves + re-verifies citation_path against a real repo
        // file under the flock when carrying evidence into the new entry.
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), b"12345\n").unwrap();
        let paths = make_paths(dir.path());
        let conn = db::open_db(&paths.db).unwrap();
        let emb = NoopEmbedder;
        let content = "paragraph one\n\nparagraph two\n\nparagraph three".repeat(80);
        let upsert = serde_json::json!({
            "action": "upsert", "table": "entries", "id": "compress-old",
            "path": "docs/compress-gc", "summary": "entry", "content": content,
            "tags": [], "kind": "belief", "ts": "2024-01-01T00:00:00Z"
        });
        db::apply_event(&conn, &emb, &upsert).unwrap();
        // add_locked re-verifies the caller-supplied citation_hash against the
        // real file when compress carries this evidence into the new entry,
        // so the seeded hash must actually match "src/lib.rs" bytes 1-2.
        let real_hash = crate::components::verification::compute_citation_hash(
            dir.path(),
            "src/lib.rs",
            Some((1, 2)),
        )
        .unwrap();
        let evidence = serde_json::json!({
            "action": "evidence_add", "table": "evidence", "entry_id": "compress-old",
            "evidence": {
                "id": "compress-old-ev", "kind": "code",
                "citation_path": "src/lib.rs:1-2", "citation_hash": real_hash
            }
        });
        db::apply_event(&conn, &emb, &evidence).unwrap();
        drop(conn);

        run(
            &Compress {
                path: "docs/compress-gc".to_string(),
                threshold_chars: Some(100),
                dry_run: false,
            },
            &KbConfig::default(),
            &paths,
            &TestEmbedder,
        )
        .unwrap();

        let conn = db::open_db(&paths.db).unwrap();
        let stranded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM evidence WHERE entry_id='compress-old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stranded, 0);
    }
}
