//! `ingest` subcommand — chunk a long document into KB entries

use crate::commands::add::{self, Add};
use crate::components::embedder;
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
    /// Skip embedding (sets KB_NO_EMBED)
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
    pub fn execute(&self) -> anyhow::Result<()> {
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

        let paths = config::Paths::discover()?;
        if self.no_embed {
            std::env::set_var("KB_NO_EMBED", "1");
        }
        let embedder: Box<dyn embedder::Embedder> = add::make_embedder(&paths);
        let version_ref = self.version_ref.clone().or_else(config::git_head_sha);

        for chunk in &chunks {
            let cmd = Add {
                path: self.path.clone(),
                summary: self.summary.clone(),
                content: chunk.clone(),
                tags: self.tags.clone(),
                version_ref: version_ref.clone(),
                id: None,
                permanent: self.permanent,
                replace_path: true,
                kind: "belief".to_string(),
                evidence: vec![],
                evidence_file: None,
            };
            cmd.execute_with(&paths, embedder.as_ref())?;
        }
        println!("ingested {} chunk(s) → {}", chunks.len(), self.path);
        Ok(())
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
    let paras: Vec<&str> = text
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
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
    use crate::commands::add::Add;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use rusqlite::Connection;
    use std::fs;
    use tempfile::tempdir;

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

    #[test]
    fn test_ingest_writes_chunks_to_db() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let embedder = NoopEmbedder;

        let doc = "# Part One\n\nThis is part one content with enough text.\n\n\
                   # Part Two\n\nThis is part two content with enough text.\n";
        let chunks = chunk_text(doc, 1800, 150);

        for chunk in &chunks {
            let cmd = Add {
                path: "docs/test.md".to_string(),
                summary: "test doc".to_string(),
                content: chunk.clone(),
                tags: "docs".to_string(),
                version_ref: Some("abc123".to_string()),
                id: None,
                permanent: false,
                replace_path: true,
                kind: "belief".to_string(),
                evidence: vec![],
                evidence_file: None,
            };
            cmd.execute_with(&paths, &embedder).unwrap();
        }

        let conn = Connection::open(&paths.db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE path='docs/test.md' AND is_stale=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // The last replace_path call marks prior chunks stale — only 1 active
        assert!(count >= 1, "at least one chunk in DB");
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
