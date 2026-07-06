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
