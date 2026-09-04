//! Budgeted, working-set-aware context selection.
//!
//! Token counts use the deliberately simple heuristic `ceil(UTF-8 bytes / 4)`.
//! Entries are indivisible: the selector never truncates content to meet a budget.

use crate::components::{db, embedder::Embedder, query_hits};
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command as ProcessCommand;

const RRF_K: f32 = 60.0;

/// Select working-set-relevant KB entries without exceeding a token budget.
///
/// Scoring blends working-set evidence overlap with branch-token FTS matches,
/// then greedily packs whole entries until the budget is exhausted. Text mode
/// prints summary + first paragraph + a `[kb#<id>]` handle for later expansion.
/// With no qualifying entries the command is silent. When `KB_INJECTION_SOURCE`
/// is set, selected ids are recorded to the best-effort query-hits telemetry DB.
#[derive(Command, Debug, Parser)]
pub struct Context {
    /// Approximate token budget; tokens are estimated as ceil(UTF-8 bytes / 4)
    #[arg(long)]
    pub budget: usize,
    /// Minimum blended relevance score. Unset means "relevance or silence":
    /// keep any entry with non-zero signal, otherwise emit nothing.
    #[arg(long)]
    pub floor: Option<f32>,
    /// Emit a JSON array instead of context text with `[kb#<id>]` handles
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Clone, Debug)]
struct Candidate {
    id: String,
    path: String,
    summary: String,
    rendered: String,
    tokens: usize,
    score: f32,
    has_signal: bool,
    cited_file: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonRow<'a> {
    id: &'a str,
    path: &'a str,
    summary: &'a str,
    tokens: usize,
    score: f32,
}

impl Runnable for Context {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Context {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        if !paths.db.exists() {
            return Ok(());
        }
        let repo_root = config::git_repo_root();
        let working_set = repo_root
            .as_deref()
            .map(enumerate_working_set)
            .unwrap_or_default();
        let branch_tokens = repo_root
            .as_deref()
            .and_then(current_branch)
            .map(|b| branch_tokens(&b))
            .unwrap_or_default();
        let conn = db::open_db(&paths.db)?;
        // This command deliberately uses search_entries' FTS-only lane; a
        // no-op embedder keeps selection read-only and avoids model setup.
        let emb = crate::components::embedder::NoopEmbedder;
        let candidates = build_candidates(&conn, &emb, &working_set, &branch_tokens)?;
        let (selected, spent) = greedy_select(candidates, self.budget, self.floor);

        let stdout = io::stdout();
        let mut out = stdout.lock();
        render(&selected, self.json, &mut out)?;
        if let Ok(surface) = std::env::var("KB_INJECTION_SOURCE") {
            let session_id =
                std::env::var("CLAUDE_SESSION_ID").unwrap_or_else(|_| "unknown".into());
            let injected: Vec<_> = selected
                .entries
                .iter()
                .map(|entry| (entry.id.clone(), entry.cited_file.clone()))
                .collect();
            query_hits::record_injection(&paths.query_hits, &session_id, &injected, &surface);
        }
        eprintln!(
            "context: entries considered/selected: {}/{}; tokens emitted/budget: {}/{}",
            selected.considered,
            selected.entries.len(),
            spent,
            self.budget
        );
        Ok(())
    }
}

struct Selection {
    considered: usize,
    entries: Vec<Candidate>,
}

fn approx_tokens(text: &str) -> usize {
    text.len().saturating_add(3) / 4
}

fn first_paragraph(content: &str) -> &str {
    let trimmed = content.trim();
    trimmed
        .split_once("\n\n")
        .map_or(trimmed, |(paragraph, _)| paragraph)
}

fn entry_text(id: &str, summary: &str, content: &str) -> String {
    let paragraph = first_paragraph(content);
    if paragraph.is_empty() {
        format!("{}\n[kb#{}]\n", summary.trim(), id)
    } else {
        format!("{}\n\n{}\n[kb#{}]\n", summary.trim(), paragraph, id)
    }
}

fn citation_file(path: &str) -> &str {
    let Some((file, range)) = path.rsplit_once(':') else {
        return path;
    };
    if range.split_once('-').is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && b.bytes().all(|c| c.is_ascii_digit())
    }) {
        file
    } else {
        path
    }
}

fn build_candidates(
    conn: &Connection,
    embedder: &dyn Embedder,
    working_set: &BTreeSet<String>,
    branch_tokens: &[String],
) -> anyhow::Result<Vec<Candidate>> {
    let query = branch_tokens.join(" ");
    let fts_rank: HashMap<String, usize> = if query.is_empty() {
        HashMap::new()
    } else {
        let opts = db::SearchOptions {
            limit: db::MAX_LIMIT,
            do_fts: true,
            do_semantic: false,
            inline_verify_k: 0,
            ..db::SearchOptions::default()
        };
        let mut results = db::search_entries(conn, embedder, &query, &opts)?;
        // FTS5 does not define the order of equal-rank rows. The public FTS-only
        // search result intentionally normalizes scores to 1.0, so establish a
        // fixed total order before converting the result list to RRF ranks.
        results.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        results
            .into_iter()
            .enumerate()
            .map(|(rank, e)| (e.id, rank + 1))
            .collect()
    };

    let mut evidence: HashMap<String, (usize, usize, Option<String>)> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT ev.entry_id, ev.citation_path FROM evidence ev \
         JOIN entries e ON e.id=ev.entry_id WHERE e.is_stale=0 AND ev.citation_path IS NOT NULL",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (id, path) = row?;
        let counts = evidence.entry(id).or_default();
        counts.1 += 1;
        if counts.2.is_none() {
            counts.2 = Some(citation_file(&path).to_owned());
        }
        if working_set.contains(citation_file(&path)) {
            counts.0 += 1;
        }
    }

    let mut stmt =
        conn.prepare("SELECT id, path, summary, content FROM entries WHERE is_stale=0")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, path, summary, content) = row?;
        let (matching, total, cited_file) = evidence.get(&id).cloned().unwrap_or_default();
        let overlap = if total == 0 {
            0.0
        } else {
            matching as f32 / total as f32
        };
        let fts = fts_rank
            .get(&id)
            .map_or(0.0, |rank| 1.0 / (RRF_K + *rank as f32));
        let rendered = entry_text(&id, &summary, &content);
        out.push(Candidate {
            id,
            path,
            summary,
            tokens: approx_tokens(&rendered),
            rendered,
            score: overlap + fts,
            has_signal: overlap > 0.0 || fts > 0.0,
            cited_file,
        });
    }
    Ok(out)
}

fn greedy_select(
    mut candidates: Vec<Candidate>,
    budget: usize,
    floor: Option<f32>,
) -> (Selection, usize) {
    let considered = candidates.len();
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    let mut spent = 0usize;
    let mut entries = Vec::new();
    for candidate in candidates {
        let clears_floor = floor.map_or(candidate.has_signal, |f| candidate.score >= f);
        if clears_floor && spent.saturating_add(candidate.tokens) <= budget {
            spent += candidate.tokens;
            entries.push(candidate);
        }
    }
    (
        Selection {
            considered,
            entries,
        },
        spent,
    )
}

/// Narrow benchmark API for the allocation/sort/greedy-pack portion of context.
#[doc(hidden)]
pub fn benchmark_greedy_select(
    candidates: &[(String, usize, f32, bool)],
    budget: usize,
) -> (usize, usize) {
    let candidates = candidates
        .iter()
        .map(|(id, tokens, score, has_signal)| Candidate {
            id: id.clone(),
            path: id.clone(),
            summary: id.clone(),
            rendered: id.clone(),
            tokens: *tokens,
            score: *score,
            has_signal: *has_signal,
            cited_file: None,
        })
        .collect();
    let (selection, spent) = greedy_select(candidates, budget, None);
    (selection.entries.len(), spent)
}

/// Narrow benchmark API for DB-backed FTS/evidence scoring and selection.
#[doc(hidden)]
pub fn benchmark_db_selection(
    conn: &Connection,
    working_set: &BTreeSet<String>,
    branch_tokens: &[String],
    budget: usize,
) -> anyhow::Result<(usize, usize)> {
    let embedder = crate::components::embedder::NoopEmbedder;
    let candidates = build_candidates(conn, &embedder, working_set, branch_tokens)?;
    let (selection, spent) = greedy_select(candidates, budget, None);
    Ok((selection.entries.len(), spent))
}

/// Benchmark the complete git-enumeration plus DB-scoring context path.
#[doc(hidden)]
pub fn benchmark_context_path(
    conn: &Connection,
    repo_root: &Path,
    budget: usize,
) -> anyhow::Result<(usize, usize)> {
    let working_set = enumerate_working_set(repo_root);
    let tokens = current_branch(repo_root)
        .map(|b| branch_tokens(&b))
        .unwrap_or_default();
    benchmark_db_selection(conn, &working_set, &tokens, budget)
}

fn render<W: Write>(selection: &Selection, json: bool, writer: &mut W) -> anyhow::Result<()> {
    if selection.entries.is_empty() {
        if json {
            writer.write_all(b"[]\n")?;
        }
        return Ok(());
    }
    if json {
        let rows: Vec<_> = selection
            .entries
            .iter()
            .map(|e| JsonRow {
                id: &e.id,
                path: &e.path,
                summary: &e.summary,
                tokens: e.tokens,
                score: e.score,
            })
            .collect();
        serde_json::to_writer(&mut *writer, &rows)?;
        writeln!(writer)?;
    } else {
        for entry in &selection.entries {
            writer.write_all(entry.rendered.as_bytes())?;
        }
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn current_branch(root: &Path) -> Option<String> {
    let bytes = git_output(root, &["branch", "--show-current"])?;
    let branch = String::from_utf8(bytes).ok()?.trim().to_owned();
    (!branch.is_empty()).then_some(branch)
}

fn branch_tokens(branch: &str) -> Vec<String> {
    let stripped = regex::Regex::new(r"^[a-z]+-[a-z0-9]+-")
        .expect("static regex")
        .replace(branch, "");
    stripped
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

fn enumerate_working_set(root: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    let Some(status) = git_output(root, &["status", "--porcelain=v1", "-z"]) else {
        return files;
    };
    let mut records = status.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let renamed_or_copied =
            matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C');
        files.insert(String::from_utf8_lossy(&record[3..]).into_owned());
        // In -z format a rename/copy has a second NUL-delimited record for
        // the source name. The first record above is the destination name.
        if renamed_or_copied {
            let _ = records.next();
        }
    }
    let base = ["main", "master"].into_iter().find(|name| {
        git_output(
            root,
            &["rev-parse", "--verify", &format!("refs/heads/{name}")],
        )
        .is_some()
    });
    if let Some(base) = base {
        if let Some(diff) = git_output(root, &["diff", "--name-only", &format!("{base}...HEAD")]) {
            files.extend(
                String::from_utf8_lossy(&diff)
                    .lines()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{db, embedder::NoopEmbedder};
    use proptest::prelude::*;
    use rusqlite::{params, Connection};
    use serde_json::json;
    use std::collections::HashSet;
    use tempfile::tempdir;

    fn candidate(id: &str, bytes: usize, score: f32, signal: bool) -> Candidate {
        let rendered = "x".repeat(bytes);
        Candidate {
            id: id.into(),
            path: id.into(),
            summary: id.into(),
            tokens: approx_tokens(&rendered),
            rendered,
            score,
            has_signal: signal,
            cited_file: None,
        }
    }

    fn git(root: &Path, args: &[&str]) {
        assert!(ProcessCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    fn insert_entry(conn: &Connection, id: &str, path: &str, summary: &str, content: &str) {
        let event = json!({
            "action": "upsert",
            "table": "entries",
            "id": id,
            "path": path,
            "summary": summary,
            "content": content,
            "tags": [],
            "ts": "2024-01-01T00:00:00Z",
        });
        db::apply_event(conn, &NoopEmbedder, &event).unwrap();
    }

    fn insert_evidence(conn: &Connection, id: &str, entry_id: &str, citation_path: &str) {
        conn.execute(
            "INSERT INTO evidence(
                id, entry_id, kind, citation_path, citation_hash, recorded_at
             ) VALUES (?1, ?2, 'code', ?3, 'sha256:test', '2024-01-01T00:00:00Z')",
            params![id, entry_id, citation_path],
        )
        .unwrap();
    }

    #[test]
    fn branch_ticket_prefix_is_stripped_and_tokens_normalized() {
        assert_eq!(
            branch_tokens("bd-rhx-context_budget/RRF"),
            ["context", "budget", "rrf"]
        );
        assert_eq!(
            branch_tokens("bd-rhx.12-context"),
            ["bd", "rhx", "12", "context"]
        );
        assert_eq!(
            branch_tokens("feature/context-budget"),
            ["feature", "context", "budget"]
        );
    }

    #[test]
    fn working_set_unions_dirty_staged_and_branch_diff() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        for name in ["dirty.txt", "staged.txt"] {
            std::fs::write(root.join(name), "base").unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "base"]);
        git(root, &["switch", "-c", "bd-rhx.12-context"]);
        std::fs::write(root.join("branch.txt"), "branch").unwrap();
        git(root, &["add", "branch.txt"]);
        git(root, &["commit", "-m", "branch"]);
        std::fs::write(root.join("dirty.txt"), "dirty").unwrap();
        std::fs::write(root.join("staged.txt"), "staged").unwrap();
        git(root, &["add", "staged.txt"]);
        let got = enumerate_working_set(root);
        let expected: BTreeSet<String> = ["dirty.txt", "staged.txt", "branch.txt"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert!(got.is_superset(&expected));
    }

    #[test]
    fn oversized_middle_entry_is_skipped_not_truncated() {
        let input = vec![
            candidate("a", 8, 3.0, true),
            candidate("b", 100, 2.0, true),
            candidate("c", 8, 1.0, true),
        ];
        let (selected, spent) = greedy_select(input, 4, None);
        assert_eq!(
            selected
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(spent, 4);
    }

    #[test]
    fn empty_json_selection_renders_empty_array() {
        let selection = Selection {
            considered: 0,
            entries: Vec::new(),
        };
        let mut out = Vec::new();
        render(&selection, true, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[]\n");
    }

    proptest! {
        #[test]
        fn budget_never_exceeded(costs in prop::collection::vec(1usize..100, 0..30), budget in 0usize..300) {
            let input = costs.into_iter().enumerate().map(|(i, n)| candidate(&format!("{i:03}"), n * 4, 1.0, true)).collect();
            let (_, spent) = greedy_select(input, budget, None);
            prop_assert!(spent <= budget);
        }

        #[test]
        fn emitted_entry_ids_are_unique(
            candidates in prop::collection::vec((1usize..100, 0f32..5.0, any::<bool>()), 0..30),
            budget in 0usize..300,
            floor in prop::option::of(0f32..5.0),
        ) {
            let input: Vec<_> = candidates.into_iter().enumerate().map(|(i, (bytes, score, signal))| {
                candidate(&format!("{i:03}"), bytes, score, signal)
            }).collect();
            let (selected, _) = greedy_select(input, budget, floor);
            let ids: Vec<_> = selected.entries.iter().map(|e| e.id.as_str()).collect();
            let unique: HashSet<_> = ids.iter().copied().collect();
            prop_assert_eq!(unique.len(), ids.len());
        }

        #[test]
        fn below_floor_is_silent(scores in prop::collection::vec(0f32..1.0, 0..30)) {
            let input = scores.into_iter().enumerate().map(|(i, score)| candidate(&i.to_string(), 4, score, true)).collect();
            let (selected, _) = greedy_select(input, 2, Some(2.0));
            let mut bytes = Vec::new(); render(&selected, false, &mut bytes).unwrap();
            prop_assert!(bytes.is_empty());
        }

        #[test]
        fn selection_is_byte_deterministic(costs in prop::collection::vec(1usize..30, 0..20), budget in 0usize..100) {
            let input: Vec<_> = costs.into_iter().enumerate().map(|(i, n)| candidate(&format!("{i:03}"), n, (i % 3) as f32, true)).collect();
            let (a, _) = greedy_select(input.clone(), budget, None);
            let (b, _) = greedy_select(input, budget, None);
            let (mut x, mut y) = (Vec::new(), Vec::new()); render(&a, false, &mut x).unwrap(); render(&b, false, &mut y).unwrap();
            prop_assert_eq!(x, y);
        }

        #[test]
        fn emitted_multibyte_chunks_are_utf8_valid(chars in prop::collection::vec(prop_oneof![Just('é'), Just('界'), Just('🦀')], 1..30)) {
            let content: String = chars.into_iter().collect();
            let rendered = entry_text("utf8", "résumé", &content);
            let input = vec![Candidate { id: "utf8".into(), path: "p".into(), summary: "résumé".into(), tokens: approx_tokens(&rendered), rendered, score: 1.0, has_signal: true, cited_file: None }];
            let (selected, _) = greedy_select(input, usize::MAX, None);
            for entry in selected.entries { prop_assert!(String::from_utf8(entry.rendered.into_bytes()).is_ok()); }
        }

        #[test]
        fn every_skipped_relevant_entry_was_unaffordable_at_its_rank(costs in prop::collection::vec(1usize..60, 0..25), budget in 0usize..100) {
            let mut ranked: Vec<_> = costs.into_iter().enumerate().map(|(i, n)| candidate(&format!("{i:03}"), n * 4, (100-i) as f32, true)).collect();
            ranked.sort_by(|a,b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
            let (selected, _) = greedy_select(ranked.clone(), budget, None);
            let ids: HashSet<_> = selected.entries.iter().map(|e| e.id.as_str()).collect();
            let mut spent_before = 0;
            for entry in ranked {
                if ids.contains(entry.id.as_str()) { spent_before += entry.tokens; }
                else { prop_assert!(spent_before.saturating_add(entry.tokens) > budget); }
            }
        }
    }

    #[test]
    fn floor_filters_individually_in_mixed_candidates() {
        let conn = db::open_db_memory().unwrap();
        insert_entry(
            &conn,
            "010-low",
            "docs/low.md",
            "cold summary",
            "No working-set evidence and no branch-token hit.",
        );
        insert_entry(
            &conn,
            "020-high",
            "docs/high.md",
            "working set summary",
            "Has evidence on the active file.",
        );
        insert_entry(
            &conn,
            "030-top",
            "docs/top.md",
            "signal branch token summary",
            "Has working-set evidence and an FTS branch-token hit.",
        );
        insert_evidence(&conn, "ev-020", "020-high", "src/hot.rs:0-10");
        insert_evidence(&conn, "ev-030", "030-top", "src/hot.rs:11-20");

        let working_set = BTreeSet::from([String::from("src/hot.rs")]);
        let branch_tokens = vec![String::from("signal")];
        let input = build_candidates(&conn, &NoopEmbedder, &working_set, &branch_tokens).unwrap();
        let (selected, spent) = greedy_select(input, usize::MAX, None);
        let ids: Vec<_> = selected.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["030-top", "020-high"]);
        assert_eq!(
            spent,
            selected.entries.iter().map(|e| e.tokens).sum::<usize>()
        );

        let mut bytes = Vec::new();
        render(&selected, false, &mut bytes).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("[kb#020-high]"));
        assert!(rendered.contains("[kb#030-top]"));
        assert!(!rendered.contains("[kb#010-low]"));
    }

    #[test]
    fn tied_scores_render_byte_identically_with_id_ascending_tiebreak() {
        let input = vec![
            candidate("020-zeta", 4, 1.0, true),
            candidate("010-alpha", 4, 1.0, true),
            candidate("030-omega", 4, 1.0, true),
        ];
        let (first, _) = greedy_select(input.clone(), 3, None);
        let (second, _) = greedy_select(input, 3, None);
        let first_ids: Vec<_> = first.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(first_ids, ["010-alpha", "020-zeta", "030-omega"]);

        let (mut a, mut b) = (Vec::new(), Vec::new());
        render(&first, false, &mut a).unwrap();
        render(&second, false, &mut b).unwrap();
        assert_eq!(a, b);
    }
}
