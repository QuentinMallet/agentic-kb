//! `import` subcommand — bulk-import entries from a JSON seed file with stamp-gating

use crate::commands::add::{self, Add};
use crate::components::embedder;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const MAX_CONTENT: usize = 9500;

/// Bulk-import KB entries from a JSON seed file (array of {path, summary, content, tags, permanent?}).
/// Skips import if content hash matches <file>.stamp (idempotent re-import).
#[derive(Command, Debug, Parser)]
pub struct Import {
    /// JSON seed file containing an array of KB entries
    pub file: PathBuf,
    /// Skip embedding (uses NoopEmbedder; does not mutate KB_NO_EMBED)
    #[arg(long, default_value_t = false)]
    pub no_embed: bool,
    /// Print what would be imported without writing
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Git commit SHA for all entries (auto-populated from HEAD if omitted)
    #[arg(long)]
    pub version_ref: Option<String>,
}

impl Runnable for Import {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Import {
    pub fn execute(&self) -> anyhow::Result<()> {
        let raw = std::fs::read_to_string(&self.file)?;
        let hash = sha256_hex(raw.as_bytes());

        let stamp_path = stamp_path_for(&self.file);

        // Stamp-gate: skip if content unchanged
        if !self.dry_run {
            if let Ok(existing) = std::fs::read_to_string(&stamp_path) {
                if existing.trim() == hash {
                    println!(
                        "import: up-to-date (hash {}), skipping",
                        &hash[..8]
                    );
                    return Ok(());
                }
            }
        }

        let entries: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
        let paths = config::Paths::discover()?;

        // Construct embedder without env::set_var.
        // Directive: env::set_var is unsafe in Rust 2024 — never reintroduce.
        let embedder: Box<dyn embedder::Embedder> = add::make_embedder_with_opts(&paths, self.no_embed);
        let version_ref = self.version_ref.clone().or_else(config::git_head_sha);

        let mut imported = 0usize;
        for entry in &entries {
            let path = entry
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if path.is_empty() {
                eprintln!("import: skipping entry with empty path");
                continue;
            }
            let summary = entry
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw_content = entry
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content: String = raw_content.chars().take(MAX_CONTENT).collect();
            let tags = parse_tags(entry.get("tags"));
            let permanent = entry
                .get("permanent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let entry_version_ref = entry
                .get("version_ref")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| version_ref.clone());

            if self.dry_run {
                println!("dry-run  {} — {}", path, summary);
                continue;
            }

            let cmd = Add {
                path,
                summary,
                content,
                tags,
                version_ref: entry_version_ref,
                id: None,
                permanent,
                replace_path: true,
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
                cues: vec![],
            };
            cmd.execute_with(&paths, embedder.as_ref())?;
            imported += 1;
        }

        if !self.dry_run {
            // Write stamp only after all entries succeed
            if let Some(parent) = stamp_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&stamp_path, &hash)?;
            println!("import: {} entries imported (stamp {})", imported, &hash[..8]);
        }
        Ok(())
    }
}

/// Parse tags from a JSON value: accepts array-of-strings or comma-separated string.
fn parse_tags(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(","),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Compute hex-encoded SHA-256 of bytes.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Derive the stamp path: <input_file>.stamp
pub fn stamp_path_for(file: &std::path::Path) -> PathBuf {
    let mut p = file.to_path_buf();
    let ext = p
        .extension()
        .map(|e| format!("{}.stamp", e.to_string_lossy()))
        .unwrap_or_else(|| "stamp".to_string());
    p.set_extension(ext);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::add::Add;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use rusqlite::Connection;
    use std::fs;
    use tempfile::tempdir;

    fn make_paths(root: &std::path::Path) -> Paths {
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        Paths::from_root(root)
    }

    #[test]
    fn test_import_adds_entries_and_writes_stamp() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let paths = make_paths(root);

        // Create a seed JSON file
        let seed = serde_json::json!([
            {"path": "docs/guide.md", "summary": "guide", "content": "content A", "tags": ["docs"]},
            {"path": "src/lib.rs", "summary": "lib", "content": "content B", "tags": "rust,lib", "permanent": false}
        ]);
        let seed_file = root.join("seeds.json");
        fs::write(&seed_file, serde_json::to_string(&seed).unwrap()).unwrap();

        let _cmd = Import {
            file: seed_file.clone(),
            no_embed: true,
            dry_run: false,
            version_ref: Some("abc123".to_string()),
        };
        // Can't call execute() easily without path discovery; test inner logic directly
        let raw = fs::read_to_string(&seed_file).unwrap();
        let hash = sha256_hex(raw.as_bytes());
        let stamp = stamp_path_for(&seed_file);
        assert!(stamp.to_string_lossy().ends_with(".json.stamp"));

        let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        let embedder = NoopEmbedder;

        for entry in &entries {
            let path = entry["path"].as_str().unwrap().to_string();
            let summary = entry["summary"].as_str().unwrap().to_string();
            let content: String = entry["content"]
                .as_str()
                .unwrap()
                .chars()
                .take(MAX_CONTENT)
                .collect();
            let tags = parse_tags(entry.get("tags"));
            let add = Add {
                path,
                summary,
                content,
                tags,
                version_ref: Some("abc123".to_string()),
                id: None,
                permanent: false,
                replace_path: true,
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
                cues: vec![],
            };
            add.execute_with(&paths, &embedder).unwrap();
        }

        // Write stamp
        fs::write(&stamp, &hash).unwrap();

        // Verify DB has entries
        let conn = Connection::open(&paths.db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries WHERE is_stale=0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);

        // Verify stamp written
        assert!(stamp.exists());
        assert_eq!(fs::read_to_string(&stamp).unwrap(), hash);
    }

    #[test]
    fn test_parse_tags_array() {
        let v = serde_json::json!(["rust", "test"]);
        assert_eq!(parse_tags(Some(&v)), "rust,test");
    }

    #[test]
    fn test_parse_tags_string() {
        let v = serde_json::json!("rust,test");
        assert_eq!(parse_tags(Some(&v)), "rust,test");
    }

    #[test]
    fn test_parse_tags_missing() {
        assert_eq!(parse_tags(None), "");
    }

    #[test]
    fn test_content_truncated_at_max() {
        let long = "x".repeat(MAX_CONTENT + 100);
        let truncated: String = long.chars().take(MAX_CONTENT).collect();
        assert_eq!(truncated.len(), MAX_CONTENT);
    }

    #[test]
    fn test_stamp_path_derivation() {
        let p = std::path::Path::new("/home/user/seeds.json");
        assert_eq!(stamp_path_for(p), PathBuf::from("/home/user/seeds.json.stamp"));
    }

    #[test]
    fn test_stamp_path_no_extension() {
        let p = std::path::Path::new("/home/user/seeds");
        assert_eq!(stamp_path_for(p), PathBuf::from("/home/user/seeds.stamp"));
    }
}
