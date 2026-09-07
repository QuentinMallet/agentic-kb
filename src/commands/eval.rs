//! `eval` subcommand — run the retrieval golden-set benchmark.

use crate::components::db;
use crate::components::embedder;
use crate::components::retrieval_eval::{
    compare_reports, evaluate_split, parse_golden_jsonl, validate_sealed_manifest, EvalReport,
    Split, SplitManifest, Verdict,
};
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;

const EXIT_COMPARE_REGRESSION: i32 = 3;

/// Evaluate retrieval quality against a golden set (recall@k, MRR).
///
/// Golden-set JSONL now includes a `split` field (`dev` or `sealed`; legacy
/// lines default to `dev`). `--sealed` hard-fails unless the adjacent
/// `split-manifest.json` validates. `--compare` prints one McNemar outcome:
/// `SIGNIFICANT`, `INCONCLUSIVE`, or `REGRESSION`; only `REGRESSION` exits 3.
#[derive(Command, Debug, Parser)]
pub struct Eval {
    /// Path to golden set JSONL (`query`, `expected_ids`, optional `split`)
    #[arg(required_unless_present = "compare")]
    pub golden: Option<std::path::PathBuf>,
    /// Run the frozen sealed partition. Hard-fails unless
    /// `split-manifest.json` beside the golden file validates.
    #[arg(long)]
    pub sealed: bool,
    /// Compare two JSON EvalReport files from paired same-corpus, same-time
    /// arms. Outputs `SIGNIFICANT`, `INCONCLUSIVE`, or `REGRESSION`;
    /// `REGRESSION` exits 3. `INCONCLUSIVE` means "no information, NOT no
    /// regression".
    #[arg(long, num_args = 2, value_names = ["BEFORE", "AFTER"])]
    pub compare: Option<Vec<std::path::PathBuf>>,
    /// FTS5 keyword search only
    #[arg(long)]
    pub fts: bool,
    /// Semantic similarity search only
    #[arg(long)]
    pub semantic: bool,
    /// Result cutoff k (default: 10)
    #[arg(long, default_value_t = 10)]
    pub k: usize,
    /// Emit the full report as JSON instead of a table
    #[arg(long)]
    pub json: bool,
    /// Exit non-zero if recall@k falls below this threshold (CI gate)
    #[arg(long)]
    pub min_recall: Option<f64>,
    /// Exit non-zero if MRR falls below this threshold (CI gate)
    #[arg(long)]
    pub min_mrr: Option<f64>,
}

impl Runnable for Eval {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Eval {
    pub fn execute(&self) -> anyhow::Result<()> {
        if let Some(files) = &self.compare {
            return self.execute_compare(files);
        }
        let paths = config::Paths::discover()?;
        let emb = crate::commands::add::make_embedder(&paths);
        self.execute_with(&paths, emb.as_ref())
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let kb_config = config::KbConfig::from_paths(paths);
        let golden = self
            .golden
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("golden set is required outside --compare mode"))?;
        let text = std::fs::read_to_string(golden)?;
        let all_cases = parse_golden_jsonl(&text)?;
        let requested = if self.sealed {
            Split::Sealed
        } else {
            Split::Dev
        };
        let cases: Vec<_> = all_cases
            .iter()
            .filter(|c| c.split == requested)
            .cloned()
            .collect();

        if self.sealed {
            let manifest_path = golden
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("split-manifest.json");
            let manifest: SplitManifest =
                serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
            validate_sealed_manifest(&all_cases, &paths.events, &manifest)?;
        }

        let opts = db::SearchOptions {
            limit: self.k,
            do_fts: self.fts || !self.semantic,
            do_semantic: self.semantic || !self.fts,
            path_prefix: None,
            tag_filter: None,
            inline_verify_k: 0, // verification is orthogonal to ranking quality
            repo_root: None,
            verify_pool_size: None,
            recency_lambda: kb_config.recency_lambda,
            mmr_lambda: kb_config.mmr_lambda,
        };

        // A read: an uninitialized repository behaves exactly like an
        // initialized-but-empty one (zero entries either way), so evaluate
        // against a throwaway empty in-memory schema instead of erroring —
        // never create the repository's own database from a read. The
        // cursor/log staleness check below only makes sense against the
        // repository's own database, so it is skipped for the substitute.
        let conn = match db::open_ro(&paths.db) {
            Ok(conn) => {
                // A read: detect and warn, never recover (C2/ADR-7).
                crate::components::cursor::warn_if_behind(&conn, paths);
                conn
            }
            Err(e) if db::is_db_uninitialized(&e) => {
                db::note_uninitialized(&paths.db);
                db::open_db_memory()?
            }
            Err(e) => return Err(e),
        };
        let report = evaluate_split(&conn, embedder, &cases, &opts, requested)?;
        let recall = report.recall_at_k();
        let mrr = report.mrr();

        if self.json {
            #[derive(serde::Serialize)]
            struct JsonOut<'a> {
                recall_at_k: f64,
                mrr: f64,
                #[serde(flatten)]
                report: &'a EvalReport,
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&JsonOut {
                    recall_at_k: recall,
                    mrr,
                    report: &report
                })?
            );
        } else {
            println!("=== Retrieval eval (k={}) ===", report.k);
            for c in &report.per_case {
                let rank = c.first_rank.map_or("-".to_string(), |r| r.to_string());
                println!(
                    "  recall={:.2}  first_rank={rank:>3}  hits={}/{}  {}",
                    c.recall(),
                    c.hits,
                    c.expected,
                    c.query
                );
            }
            println!(
                "cases={}  recall@{}={recall:.4}  mrr={mrr:.4}",
                report.per_case.len(),
                report.k
            );
        }

        let mut failed = Vec::new();
        if let Some(min) = self.min_recall {
            if recall < min {
                failed.push(format!("recall@{} {recall:.4} < min {min:.4}", report.k));
            }
        }
        if let Some(min) = self.min_mrr {
            if mrr < min {
                failed.push(format!("mrr {mrr:.4} < min {min:.4}"));
            }
        }
        if !failed.is_empty() {
            anyhow::bail!("eval gate failed: {}", failed.join("; "));
        }
        Ok(())
    }

    fn execute_compare(&self, files: &[std::path::PathBuf]) -> anyhow::Result<()> {
        let before: EvalReport = serde_json::from_str(&std::fs::read_to_string(&files[0])?)?;
        let after: EvalReport = serde_json::from_str(&std::fs::read_to_string(&files[1])?)?;
        let comparison = compare_reports(&before, &after)?;
        if comparison.verdict == Verdict::Inconclusive {
            println!("INCONCLUSIVE: no information — NOT evidence of no regression (discordant_pairs={})", comparison.discordant_pairs);
        } else {
            println!(
                "{}: discordant_pairs={}",
                comparison.verdict.name(),
                comparison.discordant_pairs
            );
        }
        if let Some(code) = compare_exit_code(comparison.verdict) {
            std::process::exit(code);
        }
        Ok(())
    }
}

fn compare_exit_code(verdict: Verdict) -> Option<i32> {
    match verdict {
        // `kb eval --compare` is a CI gate only for a demonstrated regression.
        Verdict::Regression => Some(EXIT_COMPARE_REGRESSION),
        Verdict::Significant | Verdict::Inconclusive => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::add::Add;
    use crate::components::embedder::NoopEmbedder;
    use crate::config::Paths;
    use std::fs;
    use tempfile::tempdir;

    fn setup_kb_with_entry(root: &std::path::Path) -> Paths {
        fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
        let paths = Paths::from_root(root);
        let add_cmd = Add {
            path: "src/auth.rs".to_string(),
            summary: "authentication jwt tokens".to_string(),
            content: "verifies bearer jwt".to_string(),
            tags: "auth".to_string(),
            version_ref: Some("abc".to_string()),
            id: Some("eval-cli-1".to_string()),
            permanent: false,
            replace_path: false,
            kind: "convention".to_string(),
            evidence: vec![],
            evidence_file: None,
            cues: vec![],
        };
        add_cmd.execute_with(&paths, &NoopEmbedder).unwrap();
        paths
    }

    fn eval_cmd(golden: std::path::PathBuf, min_recall: Option<f64>) -> Eval {
        Eval {
            golden: Some(golden),
            sealed: false,
            compare: None,
            fts: true,
            semantic: false,
            k: 10,
            json: false,
            min_recall,
            min_mrr: None,
        }
    }

    /// End-to-end CLI path: golden file → report, gate passes at recall 1.0.
    #[test]
    fn test_cmd_eval_runs_and_gate_passes() {
        let dir = tempdir().unwrap();
        let paths = setup_kb_with_entry(dir.path());

        let golden = dir.path().join("golden.jsonl");
        fs::write(&golden, "{\"query\": \"authentication jwt\", \"expected_ids\": [\"eval-cli-1\"], \"split\": \"dev\"}\n").unwrap();

        eval_cmd(golden, Some(0.9))
            .execute_with(&paths, &NoopEmbedder)
            .unwrap();
    }

    /// --min-recall above the achievable score must fail the gate (non-Ok).
    #[test]
    fn test_cmd_eval_gate_fails_below_threshold() {
        let dir = tempdir().unwrap();
        let paths = setup_kb_with_entry(dir.path());

        let golden = dir.path().join("golden.jsonl");
        fs::write(&golden, "{\"query\": \"zzz unfindable\", \"expected_ids\": [\"eval-cli-1\"], \"split\": \"dev\"}\n").unwrap();

        let err = eval_cmd(golden, Some(0.5)).execute_with(&paths, &NoopEmbedder);
        assert!(err.is_err(), "gate must fail when recall < min_recall");
        assert!(err.unwrap_err().to_string().contains("eval gate failed"));
    }

    #[test]
    fn test_compare_exit_code_only_for_regressions() {
        assert_eq!(
            compare_exit_code(crate::components::retrieval_eval::Verdict::Regression),
            Some(EXIT_COMPARE_REGRESSION)
        );
        assert_eq!(
            compare_exit_code(crate::components::retrieval_eval::Verdict::Significant),
            None
        );
        assert_eq!(
            compare_exit_code(crate::components::retrieval_eval::Verdict::Inconclusive),
            None
        );
    }
}
