//! `cited-by` subcommand

use crate::components::db;
use crate::components::verification::{verify_evidence, RelocationPolicy};
use crate::config;
use crate::models::{Evidence, VerificationStatus};
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::Connection;
use serde::Serialize;
use std::io::{self, Write};
use std::path::Path;

/// List live KB entries whose evidence cites a given file.
///
/// Text mode emits one `GOVERNED <STATUS> [<path>] <summary> id=<entry_id>`
/// line per live entry. Status is one of `VERIFIED`, `RELOCATED`,
/// `UNVERIFIED`, or `DEFERRED`. `--json` emits the same information as a JSON
/// array instead of line-oriented output.
#[derive(Command, Debug, Parser)]
pub struct CitedBy {
    /// Repo-relative file path to match against evidence.citation_path
    pub file: String,
    /// Emit a JSON array instead of `GOVERNED ...` text lines
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CitedByStatus {
    Unverified,
    Relocated,
    Verified,
    Deferred,
}

impl CitedByStatus {
    fn as_str(self) -> &'static str {
        match self {
            CitedByStatus::Verified => "VERIFIED",
            CitedByStatus::Relocated => "RELOCATED",
            CitedByStatus::Unverified => "UNVERIFIED",
            CitedByStatus::Deferred => "DEFERRED",
        }
    }

    fn severity_rank(self) -> u8 {
        match self {
            CitedByStatus::Unverified => 0,
            CitedByStatus::Relocated => 1,
            CitedByStatus::Verified => 2,
            CitedByStatus::Deferred => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct CitedByRow {
    id: String,
    path: String,
    summary: String,
    status: &'static str,
    citation_path: String,
}

impl Runnable for CitedBy {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl CitedBy {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        let repo_root = config::git_repo_root();
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        self.execute_with(&paths, repo_root.as_deref(), &mut handle)
    }

    /// Execute against explicit paths, rendering into `writer`.
    ///
    /// A pure read: it opens with [`db::open_ro`] and never takes the write
    /// lock (ADR-7). An uninitialized repository yields an empty result plus a
    /// one-line stderr note rather than an error, preserving the first-run
    /// behaviour of the pre-split read path.
    pub fn execute_with<W: Write>(
        &self,
        paths: &config::Paths,
        repo_root: Option<&Path>,
        writer: &mut W,
    ) -> anyhow::Result<()> {
        let conn = match db::open_ro(&paths.db) {
            Ok(conn) => conn,
            Err(e) if db::is_db_uninitialized(&e) => {
                db::note_uninitialized(&paths.db);
                return render_rows(&[], self.json, writer);
            }
            Err(e) => return Err(e),
        };
        self.execute_with_conn(&conn, repo_root, writer)
    }

    fn execute_with_conn<W: Write>(
        &self,
        conn: &Connection,
        repo_root: Option<&Path>,
        writer: &mut W,
    ) -> anyhow::Result<()> {
        let rows = build_rows(conn, &self.file, repo_root)?;
        render_rows(&rows, self.json, writer)
    }
}

fn build_rows(
    conn: &Connection,
    file: &str,
    repo_root: Option<&Path>,
) -> anyhow::Result<Vec<CitedByRow>> {
    let entries = db::entries_citing(conn, file)?;
    let mut rows = Vec::with_capacity(entries.len());

    for entry in entries {
        let mut best: Option<(CitedByStatus, String)> = None;
        for evidence in &entry.evidence {
            let citation_path = evidence.citation_path.clone().unwrap_or_default();
            let status = classify_evidence(evidence, file, repo_root);
            let replace = best
                .as_ref()
                .map(|(current, _)| status.severity_rank() < current.severity_rank())
                .unwrap_or(true);
            if replace {
                best = Some((status, citation_path));
            }
        }

        if let Some((status, citation_path)) = best {
            rows.push(CitedByRow {
                id: entry.id,
                path: entry.path,
                summary: entry.summary,
                status: status.as_str(),
                citation_path,
            });
        }
    }

    Ok(rows)
}

/// Run the cited-by query and FileOnly verification path without rendering.
#[doc(hidden)]
pub fn benchmark_cited_by(
    conn: &Connection,
    file: &str,
    repo_root: &Path,
) -> anyhow::Result<usize> {
    Ok(build_rows(conn, file, Some(repo_root))?.len())
}

fn classify_evidence(ev: &Evidence, file: &str, repo_root: Option<&Path>) -> CitedByStatus {
    let Some(citation_path) = ev.citation_path.as_deref() else {
        return CitedByStatus::Deferred;
    };
    let Some(root) = repo_root else {
        return CitedByStatus::Deferred;
    };
    if ev.kind != "code" || !is_citation_for_file(citation_path, file) {
        return CitedByStatus::Deferred;
    }

    match verify_evidence(ev, root, RelocationPolicy::FileOnly).status {
        VerificationStatus::Verified => CitedByStatus::Verified,
        VerificationStatus::Relocated => CitedByStatus::Relocated,
        VerificationStatus::Unverified => CitedByStatus::Unverified,
    }
}

fn is_citation_for_file(citation_path: &str, file: &str) -> bool {
    citation_path == file
        || citation_path
            .strip_prefix(file)
            .is_some_and(|suffix| suffix.starts_with(':'))
}

fn render_rows<W: Write>(rows: &[CitedByRow], json: bool, writer: &mut W) -> anyhow::Result<()> {
    if rows.is_empty() {
        if json {
            writer.write_all(b"[]\n")?;
        }
        return Ok(());
    }

    if json {
        serde_json::to_writer(&mut *writer, rows)?;
        writeln!(writer)?;
        return Ok(());
    }

    for row in rows {
        writeln!(
            writer,
            "GOVERNED {} [{}] {} id={}",
            row.status,
            row.path.replace('\n', " "),
            row.summary.replace('\n', " "),
            row.id
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::db::open_db_memory;
    use rusqlite::params;
    use serde_json::Value;
    use sha2::{Digest, Sha256};

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("sha256:{:x}", hasher.finalize())
    }

    #[test]
    fn test_is_citation_for_file_accepts_whole_file_equality() {
        assert!(is_citation_for_file("src/foo.rs", "src/foo.rs"));
        assert!(is_citation_for_file("src/foo.rs:0-4", "src/foo.rs"));
        assert!(!is_citation_for_file("src/foo.rs.bak", "src/foo.rs"));
    }

    fn insert_entry(conn: &Connection, id: &str, path: &str, summary: &str, is_stale: i64) {
        conn.execute(
            "INSERT INTO entries (id, path, summary, content, tags, is_stale, updated_at)
             VALUES (?1, ?2, ?3, '', '[]', ?4, '2024-01-01T00:00:00Z')",
            params![id, path, summary, is_stale],
        )
        .unwrap();
    }

    fn insert_evidence(
        conn: &Connection,
        id: &str,
        entry_id: &str,
        kind: &str,
        citation_path: &str,
        citation_hash: &str,
        citation_excerpt: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO evidence(
                id, entry_id, kind, citation_path, citation_hash, citation_excerpt, recorded_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '2024-01-01T00:00:00Z')",
            params![
                id,
                entry_id,
                kind,
                citation_path,
                citation_hash,
                citation_excerpt
            ],
        )
        .unwrap();
    }

    fn write_repo_file(root: &Path, rel: &str, content: &str) {
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, content).unwrap();
    }

    fn byte_range(haystack: &str, needle: &str) -> (usize, usize) {
        let start = haystack.find(needle).unwrap();
        let end = start + needle.len();
        (start, end)
    }

    fn setup_fixture(conn: &Connection, root: &Path) {
        let verified_excerpt = "fn verified_target() {\n    println!(\"verified\");\n}\n";
        let relocated_excerpt = concat!(
            "fn relocated_target() {\n",
            "    let message = \"moved to a different byte offset\";\n",
            "    println!(\"{message}\");\n",
            "}\n"
        );
        let content =
            format!("// stale bytes here\n{verified_excerpt}\n// spacer\n{relocated_excerpt}");
        write_repo_file(root, "src/foo.rs", &content);

        let (verified_start, verified_end) = byte_range(&content, verified_excerpt);
        let verified_hash = sha256_hex(&content.as_bytes()[verified_start..verified_end]);

        insert_entry(
            conn,
            "deferred-entry",
            "docs/deferred.md",
            "Deferred summary",
            0,
        );
        insert_evidence(
            conn,
            "ev-deferred",
            "deferred-entry",
            "code",
            "src/foo.rs",
            "sha256:unused",
            None,
        );

        insert_entry(
            conn,
            "relocated-entry",
            "docs/relocated.md",
            "Relocated summary",
            0,
        );
        insert_evidence(
            conn,
            "ev-relocated",
            "relocated-entry",
            "code",
            &format!("src/foo.rs:0-{}", relocated_excerpt.len()),
            "sha256:deadbeef",
            Some(relocated_excerpt),
        );

        insert_entry(
            conn,
            "unverified-entry",
            "docs/unverified.md",
            "Unverified summary",
            0,
        );
        insert_evidence(
            conn,
            "ev-unverified",
            "unverified-entry",
            "code",
            &format!("src/foo.rs:{}-{}", verified_start, verified_end),
            "sha256:deadbeef",
            Some("fn missing_target() {\n    println!(\"nope\");\n}\n"),
        );

        insert_entry(
            conn,
            "verified-entry",
            "docs/verified.md",
            "Verified summary",
            0,
        );
        insert_evidence(
            conn,
            "ev-verified",
            "verified-entry",
            "code",
            &format!("src/foo.rs:{verified_start}-{verified_end}"),
            &verified_hash,
            Some(verified_excerpt),
        );

        insert_entry(conn, "stale-entry", "docs/stale.md", "Stale summary", 1);
        insert_evidence(
            conn,
            "ev-stale",
            "stale-entry",
            "code",
            &format!("src/foo.rs:{verified_start}-{verified_end}"),
            &verified_hash,
            Some(verified_excerpt),
        );
    }

    #[test]
    fn test_cited_by_text_output_reports_statuses_and_never_emits_stale_prefix() {
        let conn = open_db_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        setup_fixture(&conn, dir.path());

        let cmd = CitedBy {
            file: "src/foo.rs".to_string(),
            json: false,
        };
        let mut out = Vec::new();
        cmd.execute_with_conn(&conn, Some(dir.path()), &mut out)
            .unwrap();

        let stdout = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| line.starts_with("GOVERNED ")));
        assert!(lines.iter().all(|line| !line.starts_with("STALE")));
        assert_eq!(
            lines,
            vec![
                "GOVERNED UNVERIFIED [docs/deferred.md] Deferred summary id=deferred-entry",
                "GOVERNED RELOCATED [docs/relocated.md] Relocated summary id=relocated-entry",
                "GOVERNED UNVERIFIED [docs/unverified.md] Unverified summary id=unverified-entry",
                "GOVERNED VERIFIED [docs/verified.md] Verified summary id=verified-entry",
            ]
        );
    }

    #[test]
    fn test_cited_by_empty_result_is_silent() {
        let conn = open_db_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cmd = CitedBy {
            file: "src/missing.rs".to_string(),
            json: false,
        };
        let mut out = Vec::new();
        cmd.execute_with_conn(&conn, Some(dir.path()), &mut out)
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_cited_by_empty_json_result_is_array() {
        let conn = open_db_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cmd = CitedBy {
            file: "src/missing.rs".to_string(),
            json: true,
        };
        let mut out = Vec::new();
        cmd.execute_with_conn(&conn, Some(dir.path()), &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[]\n");
    }

    #[test]
    fn test_cited_by_json_shape() {
        let conn = open_db_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        setup_fixture(&conn, dir.path());

        let cmd = CitedBy {
            file: "src/foo.rs".to_string(),
            json: true,
        };
        let mut out = Vec::new();
        cmd.execute_with_conn(&conn, Some(dir.path()), &mut out)
            .unwrap();

        let rows: Value = serde_json::from_slice(&out).unwrap();
        let arr = rows.as_array().unwrap();
        assert_eq!(arr.len(), 4);

        let first = &arr[0];
        assert_eq!(first["id"], "deferred-entry");
        assert_eq!(first["path"], "docs/deferred.md");
        assert_eq!(first["summary"], "Deferred summary");
        assert_eq!(first["status"], "UNVERIFIED");
        assert_eq!(first["citation_path"], "src/foo.rs");
    }
}
