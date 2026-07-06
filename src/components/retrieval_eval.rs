//! Retrieval eval harness: golden-set benchmark for search quality.
//!
//! Measures recall@k and MRR over a golden set of (query → expected entry ids)
//! cases so retrieval changes (RRF constants, recency λ, embedding text, new
//! fusion lanes) can be validated instead of tuned blind.
//!
//! Metric decomposition follows Memora's trajectory scorer (groundedness /
//! redundancy / cost): recall@k is the groundedness proxy; redundancy and
//! latency reporting can be layered on later without changing the golden format.

use crate::components::db::{self, SearchOptions};
use crate::components::embedder::Embedder;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// One golden case: a query and the entry ids a good retriever must return.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoldenCase {
    pub query: String,
    pub expected_ids: Vec<String>,
}

/// Per-case outcome.
#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub query: String,
    /// Number of expected ids for this case.
    pub expected: usize,
    /// How many expected ids appeared in the top-k results.
    pub hits: usize,
    /// 1-based rank of the first expected id found, if any.
    pub first_rank: Option<usize>,
}

impl CaseResult {
    /// recall@k for this case: hits / expected.
    pub fn recall(&self) -> f64 {
        self.hits as f64 / self.expected as f64
    }

    /// Reciprocal rank contribution: 1/first_rank, or 0 when nothing was found.
    pub fn reciprocal_rank(&self) -> f64 {
        self.first_rank.map_or(0.0, |r| 1.0 / r as f64)
    }
}

/// Aggregate report over all golden cases.
#[derive(Debug, Serialize)]
pub struct EvalReport {
    /// k used for the run (search result cutoff).
    pub k: usize,
    pub per_case: Vec<CaseResult>,
}

impl EvalReport {
    /// Mean per-case recall@k over all cases.
    pub fn recall_at_k(&self) -> f64 {
        mean(self.per_case.iter().map(CaseResult::recall))
    }

    /// Mean reciprocal rank over all cases.
    pub fn mrr(&self) -> f64 {
        mean(self.per_case.iter().map(CaseResult::reciprocal_rank))
    }
}

fn mean(it: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = it.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

/// Parse a golden set from JSONL text. Blank lines and `#` comments are
/// skipped. Rejects cases with empty `expected_ids` and files with zero cases —
/// both would silently inflate the aggregate.
pub fn parse_golden_jsonl(text: &str) -> Result<Vec<GoldenCase>> {
    let mut cases = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let case: GoldenCase = serde_json::from_str(line)
            .with_context(|| format!("golden set line {}: invalid JSON", lineno + 1))?;
        if case.expected_ids.is_empty() {
            bail!("golden set line {}: expected_ids must not be empty", lineno + 1);
        }
        cases.push(case);
    }
    if cases.is_empty() {
        bail!("golden set contains no cases");
    }
    Ok(cases)
}

/// Run every golden case through `search_entries` and score the results.
/// `opts.limit` is k. Evidence verification is a waste here; callers should
/// pass `inline_verify_k: 0`.
pub fn evaluate(
    conn: &rusqlite::Connection,
    embedder: &dyn Embedder,
    cases: &[GoldenCase],
    opts: &SearchOptions,
) -> Result<EvalReport> {
    let mut per_case = Vec::with_capacity(cases.len());
    for case in cases {
        let results = db::search_entries(conn, embedder, &case.query, opts)?;
        let mut hits = 0usize;
        let mut first_rank: Option<usize> = None;
        for (i, r) in results.iter().enumerate() {
            if case.expected_ids.iter().any(|e| e == &r.id) {
                hits += 1;
                if first_rank.is_none() {
                    first_rank = Some(i + 1);
                }
            }
        }
        per_case.push(CaseResult {
            query: case.query.clone(),
            expected: case.expected_ids.len(),
            hits,
            first_rank,
        });
    }
    Ok(EvalReport { k: opts.limit, per_case })
}
