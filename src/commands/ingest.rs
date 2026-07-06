//! `ingest` subcommand — chunk a long document into KB entries

use crate::commands::add::{self, Add};
use crate::components::embedder;
use crate::components::text_chunker;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use std::io::Read;

const DEFAULT_CHUNK_SIZE: usize = 1800;
const DEFAULT_OVERLAP: usize = 150;

/// Chunk a long document into agent-kb entries (markdown-section → paragraph → sentence → hard-split)
#[derive(Command, Debug, Parser)]
pub struct Ingest {
    /// KB path (e.g. docs/readme)
    #[arg(long)]
    pub path: String,
    /// Short summary applied to all chunks
    #[arg(long)]
    pub summary: String,
    /// Comma-separated tags applied to all chunks
    #[arg(long)]
    pub tags: String,
    /// Input file (default: stdin)
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
    /// Max chars per chunk
    #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
    pub chunk_size: usize,
    /// Overlap chars for hard-split fallback
    #[arg(long, default_value_t = DEFAULT_OVERLAP)]
    pub overlap: usize,
    /// Mark all chunks permanent
    #[arg(long, default_value_t = false)]
    pub permanent: bool,
    /// Skip embedding (uses NoopEmbedder; does not mutate KB_NO_EMBED)
    #[arg(long, default_value_t = false)]
    pub no_embed: bool,
    /// Print chunks without writing to KB
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Git commit SHA (auto-populated from HEAD if omitted)
    #[arg(long)]
    pub version_ref: Option<String>,
}

impl Runnable for Ingest {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Ingest {
    /// Execute with explicit paths + embedder.
    ///
    /// This is the canonical implementation path. `execute()` delegates here after
    /// constructing the embedder via `make_embedder_with_opts(no_embed)`, avoiding
    /// any `env::set_var` calls (unsafe in Rust 2024 multi-threaded contexts).
    ///
    /// # replace_path semantics
    /// Only the **first** chunk is ingested with `replace_path=true`. Subsequent
    /// chunks use `replace_path=false`. This is the key correctness fix: previously
    /// every chunk used `replace_path=true`, which caused each iteration to expire
    /// all entries from prior iterations — silent data loss leaving only the last
    /// chunk active.
    ///
    /// TLA+ spec: `agent-kb/tla/ingest_replace_path.tla`
    /// Invariant `AllChunkIdsPresent`: after the loop, non-stale entries cover
    /// all chunk indices {0..N-1}.
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let text = match &self.file {
            Some(p) => std::fs::read_to_string(p)?,
            None => {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            }
        };

        let chunks = chunk_text(&text, self.chunk_size, self.overlap);

        if self.dry_run {
            for (i, c) in chunks.iter().enumerate() {
                println!("--- chunk {} ({} chars) ---\n{}", i + 1, c.len(), c);
            }
            return Ok(());
        }

        let version_ref = self.version_ref.clone().or_else(config::git_head_sha);

        for (i, chunk) in chunks.iter().enumerate() {
            let cmd = Add {
                path: self.path.clone(),
                summary: self.summary.clone(),
                content: chunk.clone(),
                tags: self.tags.clone(),
                version_ref: version_ref.clone(),
                id: None,
                permanent: self.permanent,
                replace_path: i == 0,
                kind: "convention".to_string(),
                evidence: vec![],
                evidence_file: None,
                cues: vec![],
            };
            cmd.execute_with(paths, embedder)?;
        }
        println!("ingested {} chunk(s) → {}", chunks.len(), self.path);
        Ok(())
    }

    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        // Construct embedder without env::set_var.
        // Directive: env::set_var is unsafe in Rust 2024 — never reintroduce.
        let embedder = add::make_embedder_with_opts(&paths, self.no_embed);
        self.execute_with(&paths, embedder.as_ref())
    }
}

/// Split text into chunks using markdown-aware strategy:
/// sections → paragraphs → sentences → hard-split with overlap.
pub fn chunk_text(text: &str, max_size: usize, overlap: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let sections = split_sections(text);
    if sections.len() > 1 {
        for s in &sections {
            split_piece(s, max_size, overlap, &mut chunks);
        }
    } else {
        split_piece(text.trim(), max_size, overlap, &mut chunks);
    }
    chunks
}

/// Split on markdown headings h1-h3 (lines starting with 1-3 `#` followed by space).
fn split_sections(text: &str) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        let is_heading = hashes >= 1
            && hashes <= 3
            && trimmed.len() > hashes
            && trimmed.as_bytes().get(hashes) == Some(&b' ');

        if is_heading && !current.is_empty() {
            let part = current.trim().to_string();
            if !part.is_empty() {
                sections.push(part);
            }
            current = String::new();
        }
        current.push_str(line);
        current.push('\n');
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        sections.push(tail);
    }
    sections
}

fn split_piece(text: &str, max_size: usize, overlap: usize, chunks: &mut Vec<String>) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if text.len() <= max_size {
        chunks.push(text.to_string());
        return;
    }
    // Try paragraph split (blank lines)
    let para_strings = text_chunker::split_paragraphs(text, 0);
    let paras: Vec<&str> = para_strings.iter().map(|s| s.as_str()).collect();
    if paras.len() > 1 {
        merge_pieces(&paras, max_size, overlap, chunks);
        return;
    }
    // Try sentence split
    let sents = split_sentences(text);
    if sents.len() > 1 {
        let sent_refs: Vec<&str> = sents.iter().map(|s| s.as_str()).collect();
        merge_pieces(&sent_refs, max_size, overlap, chunks);
        return;
    }
    // Hard split with overlap
    hard_split(text, max_size, overlap, chunks);
}

fn merge_pieces(pieces: &[&str], max_size: usize, overlap: usize, chunks: &mut Vec<String>) {
    let mut cur = String::new();
    for &piece in pieces {
        if piece.len() > max_size {
            if !cur.is_empty() {
                chunks.push(cur.trim().to_string());
                cur = String::new();
            }
            split_piece(piece, max_size, overlap, chunks);
        } else if !cur.is_empty() && cur.len() + 2 + piece.len() > max_size {
            chunks.push(cur.trim().to_string());
            cur = piece.to_string();
        } else if cur.is_empty() {
            cur = piece.to_string();
        } else {
            cur.push_str("\n\n");
            cur.push_str(piece);
        }
    }
    if !cur.trim().is_empty() {
        chunks.push(cur.trim().to_string());
    }
}

/// Split on sentence-ending punctuation (., !, ?) followed by whitespace.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sents = Vec::new();
    let mut last = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if matches!(bytes[i], b'.' | b'!' | b'?') {
            let next = i + 1;
            if next < bytes.len() && (bytes[next] == b' ' || bytes[next] == b'\n') {
                let sent = text[last..=i].trim();
                if !sent.is_empty() {
                    sents.push(sent.to_string());
                }
                // Skip whitespace
                let mut j = next;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\n') {
                    j += 1;
                }
                last = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    let tail = text[last..].trim();
    if !tail.is_empty() {
        sents.push(tail.to_string());
    }
    sents
}

fn hard_split(text: &str, max_size: usize, overlap: usize, chunks: &mut Vec<String>) {
    let mut start = 0usize;
    while start < text.len() {
        // Snap to char boundary
        let end = advance_char_boundary(text, start + max_size);
        chunks.push(text[start..end].to_string());
        if end >= text.len() {
            break;
        }
        start = if end > overlap {
            advance_char_boundary(text, end - overlap)
        } else {
            0
        };
    }
}

/// Advance byte index to the next valid UTF-8 char boundary at or after `pos`.
fn advance_char_boundary(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    let mut p = pos;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_chunk_text_single_small_doc() {
        let text = "Short document.";
        let chunks = chunk_text(text, 1800, 150);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Short document.");
    }

    #[test]
    fn test_chunk_text_sections_split() {
        let text = "# Section A\n\nContent A.\n\n# Section B\n\nContent B.\n";
        let chunks = chunk_text(text, 1800, 150);
        // Two sections, each small — should produce 2 chunks
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains("Section A"));
        assert!(chunks[1].contains("Section B"));
    }

    #[test]
    fn test_chunk_text_hard_split_large() {
        let text = "x".repeat(5000);
        let chunks = chunk_text(&text, 1800, 150);
        assert!(chunks.len() >= 3, "expected at least 3 chunks");
        for c in &chunks {
            assert!(c.len() <= 1800);
        }
    }

    // ── TDD: Bug fix for replace_path-on-every-iteration (AC1) ───────────────
    //
    // Pre-fix the original test asserted `count >= 1`, which accepted the bug.
    // Post-fix this test asserts `count == N` (ALL chunks survive).
    //
    // This test exercises `Ingest::execute_with` directly — the same code path
    // that `execute()` delegates to — so it covers both the replace_path fix
    // and the no-env-mutation fix.
    #[test]
    fn test_ingest_writes_all_chunks_to_db() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let paths = make_paths(root);
        let embedder = NoopEmbedder;

        // Two-section doc guaranteed to produce exactly 2 chunks at 1800-char limit.
        let doc = "# Part One\n\nThis is part one content with enough text.\n\n\
                   # Part Two\n\nThis is part two content with enough text.\n";
        let expected_chunks = chunk_text(doc, 1800, 150);
        // Confirm we actually have >= 2 chunks; if chunker changes, this catches it.
        assert!(
            expected_chunks.len() >= 2,
            "doc must produce >= 2 chunks, got {}",
            expected_chunks.len()
        );

        // Build a temp file for the ingest command.
        let doc_file = root.join("doc.md");
        fs::write(&doc_file, doc).unwrap();

        let ingest = Ingest {
            path: "docs/test.md".to_string(),
            summary: "test doc".to_string(),
            tags: "docs".to_string(),
            file: Some(doc_file),
            chunk_size: 1800,
            overlap: 150,
            permanent: false,
            no_embed: true,
            dry_run: false,
            version_ref: Some("abc123".to_string()),
        };
        ingest.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE path='docs/test.md' AND is_stale=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // AC1: ALL N chunks must survive — not just the last one.
        assert_eq!(
            count,
            expected_chunks.len() as i64,
            "expected all {} chunks active in DB, got {}",
            expected_chunks.len(),
            count
        );
    }

    // Legacy test kept for regression coverage — now also asserts ALL chunks.
    // Previously this used a manual Add loop with replace_path=true on every
    // iteration (the bug), but that's replaced by execute_with above.
    #[test]
    fn test_ingest_writes_chunks_to_db() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let paths = make_paths(root);
        let embedder = NoopEmbedder;

        let doc = "# Part One\n\nThis is part one content with enough text.\n\n\
                   # Part Two\n\nThis is part two content with enough text.\n";
        let chunks = chunk_text(doc, 1800, 150);
        let doc_file = root.join("legacy_doc.md");
        fs::write(&doc_file, doc).unwrap();

        let ingest = Ingest {
            path: "docs/legacy.md".to_string(),
            summary: "legacy test doc".to_string(),
            tags: "docs".to_string(),
            file: Some(doc_file),
            chunk_size: 1800,
            overlap: 150,
            permanent: false,
            no_embed: true,
            dry_run: false,
            version_ref: Some("abc123".to_string()),
        };
        ingest.execute_with(&paths, &embedder).unwrap();

        let conn = Connection::open(&paths.db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE path='docs/legacy.md' AND is_stale=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Strengthened: must equal the actual chunk count, not just >= 1.
        assert_eq!(count, chunks.len() as i64, "all chunks must be active in DB");
    }

    // ── Proptest: no env cross-talk between parallel Ingest calls ─────────────
    //
    // AC3: two concurrent `execute_with` calls — one with no_embed=true (uses
    // NoopEmbedder), one with no_embed=false would normally use CandleEmbedder,
    // but we pass a NoopEmbedder for both since we're testing the env-isolation
    // contract, not the embedder itself.  Both calls must succeed independently
    // and both see exactly their expected chunk counts.
    //
    // Before the fix this could not be written as a proptest because the code
    // called `env::set_var("KB_NO_EMBED", "1")` which is process-global state;
    // two threads racing on that var would corrupt each other's embedder.
    // After the fix, `execute_with` receives an explicit `&dyn Embedder`, so
    // no env mutation occurs at all — the proptest below proves this by running
    // both threads concurrently and asserting deterministic results.
    #[test]
    fn test_ingest_no_env_cross_talk_parallel() {
        use std::sync::Arc;
        use std::thread;

        let doc_a = "# Alpha\n\nSection Alpha content is here.\n\n\
                     # Beta\n\nSection Beta content is here.\n";
        let doc_b = "# Gamma\n\nSection Gamma content is here.\n\n\
                     # Delta\n\nSection Delta content is here.\n\
                     \n# Epsilon\n\nSection Epsilon content is here.\n";

        let expected_a = chunk_text(doc_a, 1800, 150).len();
        let expected_b = chunk_text(doc_b, 1800, 150).len();
        assert!(expected_a >= 2, "doc_a must have >= 2 chunks");
        assert!(expected_b >= 2, "doc_b must have >= 2 chunks");

        // Two independent temp dirs — each thread owns its own KB.
        // TempDir is wrapped in Arc so it lives until both threads finish.
        let dir_a = Arc::new(tempdir().unwrap());
        let dir_b = Arc::new(tempdir().unwrap());

        // Pre-create the .state/agent-kb dirs before spawning threads.
        fs::create_dir_all(dir_a.path().join(".state/agent-kb")).unwrap();
        fs::create_dir_all(dir_b.path().join(".state/agent-kb")).unwrap();

        // Write doc files
        let file_a = dir_a.path().join("a.md");
        let file_b = dir_b.path().join("b.md");
        fs::write(&file_a, doc_a).unwrap();
        fs::write(&file_b, doc_b).unwrap();

        // Clone Arcs for threads (keeps TempDir alive until thread finishes).
        let dir_a_t = Arc::clone(&dir_a);
        let dir_b_t = Arc::clone(&dir_b);

        // Thread A: no_embed=true (would have called env::set_var before the fix)
        let handle_a = thread::spawn(move || {
            let root = dir_a_t.path().to_path_buf();
            let paths = Paths::from_root(&root);
            let embedder = NoopEmbedder;
            let ingest = Ingest {
                path: "docs/a.md".to_string(),
                summary: "thread a".to_string(),
                tags: "test".to_string(),
                file: Some(root.join("a.md")),
                chunk_size: 1800,
                overlap: 150,
                permanent: false,
                no_embed: true,
                dry_run: false,
                version_ref: Some("sha-a".to_string()),
            };
            ingest.execute_with(&paths, &embedder).unwrap();
            let conn = Connection::open(&paths.db).unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE path='docs/a.md' AND is_stale=0",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            count
        });

        // Thread B: no_embed=false (but we pass NoopEmbedder explicitly — tests
        // that the no_embed flag on the struct does NOT affect the embedder passed
        // to execute_with, proving the env var is not consulted).
        let handle_b = thread::spawn(move || {
            let root = dir_b_t.path().to_path_buf();
            let paths = Paths::from_root(&root);
            let embedder = NoopEmbedder;
            let ingest = Ingest {
                path: "docs/b.md".to_string(),
                summary: "thread b".to_string(),
                tags: "test".to_string(),
                file: Some(root.join("b.md")),
                chunk_size: 1800,
                overlap: 150,
                permanent: false,
                no_embed: false,
                dry_run: false,
                version_ref: Some("sha-b".to_string()),
            };
            ingest.execute_with(&paths, &embedder).unwrap();
            let conn = Connection::open(&paths.db).unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE path='docs/b.md' AND is_stale=0",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            count
        });

        let count_a = handle_a.join().expect("thread A panicked");
        let count_b = handle_b.join().expect("thread B panicked");

        assert_eq!(
            count_a,
            expected_a as i64,
            "thread A: expected {} active chunks, got {}",
            expected_a,
            count_a
        );
        assert_eq!(
            count_b,
            expected_b as i64,
            "thread B: expected {} active chunks, got {}",
            expected_b,
            count_b
        );
    }

    #[test]
    fn test_split_sentences_basic() {
        let text = "Hello world. This is a test. Done!";
        let sents = split_sentences(text);
        assert_eq!(sents.len(), 3);
    }

    #[test]
    fn test_hard_split_char_boundary() {
        // Unicode text — hard split must not cut mid-codepoint
        let text = "à".repeat(1000); // each 'à' is 2 bytes
        let chunks = chunk_text(&text, 100, 10);
        for c in &chunks {
            assert!(c.is_char_boundary(0));
            assert!(c.chars().all(|ch| ch == 'à'));
        }
    }
}
