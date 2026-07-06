//! `eval` subcommand — run the retrieval golden-set benchmark.

use crate::components::db;
use crate::components::embedder;
use crate::components::retrieval_eval::{evaluate, parse_golden_jsonl};
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;

/// Evaluate retrieval quality against a golden set (recall@k, MRR)
#[derive(Command, Debug, Parser)]
pub struct Eval {
    /// Path to golden set JSONL ({"query": ..., "expected_ids": [...]})
    pub golden: std::path::PathBuf,
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
        let paths = config::Paths::discover()?;
        let emb = crate::commands::add::make_embedder(&paths);
        crate::commands::rebuild::rebuild_if_schema_obsolete(&paths, emb.as_ref())?;
        self.execute_with(&paths, emb.as_ref())
    }

    /// Execute with explicit paths and embedder (for testing).
    pub fn execute_with(
        &self,
        paths: &config::Paths,
        embedder: &dyn embedder::Embedder,
    ) -> anyhow::Result<()> {
        let kb_config = config::KbConfig::from_paths(paths);
        let text = std::fs::read_to_string(&self.golden)?;
        let cases = parse_golden_jsonl(&text)?;

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

        let conn = db::open_db(&paths.db)?;
        let report = evaluate(&conn, embedder, &cases, &opts)?;
        let recall = report.recall_at_k();
        let mrr = report.mrr();

        if self.json {
            #[derive(serde::Serialize)]
            struct JsonOut<'a> {
                recall_at_k: f64,
                mrr: f64,
                #[serde(flatten)]
                report: &'a crate::components::retrieval_eval::EvalReport,
            }
            println!("{}", serde_json::to_string_pretty(&JsonOut { recall_at_k: recall, mrr, report: &report })?);
        } else {
            println!("=== Retrieval eval (k={}) ===", report.k);
            for c in &report.per_case {
                let rank = c.first_rank.map_or("-".to_string(), |r| r.to_string());
                println!(
                    "  recall={:.2}  first_rank={rank:>3}  hits={}/{}  {}",
                    c.recall(), c.hits, c.expected, c.query
                );
            }
            println!("cases={}  recall@{}={recall:.4}  mrr={mrr:.4}", report.per_case.len(), report.k);
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
            golden,
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
        fs::write(&golden, "{\"query\": \"authentication jwt\", \"expected_ids\": [\"eval-cli-1\"]}\n").unwrap();

        eval_cmd(golden, Some(0.9)).execute_with(&paths, &NoopEmbedder).unwrap();
    }

    /// --min-recall above the achievable score must fail the gate (non-Ok).
    #[test]
    fn test_cmd_eval_gate_fails_below_threshold() {
        let dir = tempdir().unwrap();
        let paths = setup_kb_with_entry(dir.path());

        let golden = dir.path().join("golden.jsonl");
        fs::write(&golden, "{\"query\": \"zzz unfindable\", \"expected_ids\": [\"eval-cli-1\"]}\n").unwrap();

        let err = eval_cmd(golden, Some(0.5)).execute_with(&paths, &NoopEmbedder);
        assert!(err.is_err(), "gate must fail when recall < min_recall");
        assert!(err.unwrap_err().to_string().contains("eval gate failed"));
    }
}
