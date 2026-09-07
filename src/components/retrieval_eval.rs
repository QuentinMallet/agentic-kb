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
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    #[default]
    Dev,
    Sealed,
}

/// One golden case: a query and the entry ids a good retriever must return.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoldenCase {
    pub query: String,
    pub expected_ids: Vec<String>,
    #[serde(default)]
    pub split: Split,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SplitManifest {
    pub corpus_hash: String,
    pub sealed_ids: Vec<String>,
    pub dev_ids: Vec<String>,
    pub frozen_at: String,
    /// `ids-only` is the documented fallback when the foreign event log was
    /// unavailable at freeze time; otherwise this is `id-content-hash`.
    pub corpus_hash_domain: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Significant,
    Inconclusive,
    Regression,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Self::Significant => "SIGNIFICANT",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::Regression => "REGRESSION",
        }
    }
}

pub struct Comparison {
    pub verdict: Verdict,
    pub discordant_pairs: usize,
}

/// Pre-registered exact McNemar decision table. A case is a success when it has
/// at least one hit. Mixed-direction discordance is inconclusive by design.
pub fn compare_reports(before: &EvalReport, after: &EvalReport) -> Result<Comparison> {
    if before.per_case.len() != after.per_case.len() {
        bail!("compare requires paired result sets of equal length");
    }
    let mut favorable = 0;
    let mut against = 0;
    for (a, b) in before.per_case.iter().zip(&after.per_case) {
        if a.query != b.query {
            bail!("compare requires paired same-corpus results in identical query order");
        }
        match (a.hits > 0, b.hits > 0) {
            (false, true) => favorable += 1,
            (true, false) => against += 1,
            _ => {}
        }
    }
    let discordant_pairs = favorable + against;
    let verdict = if discordant_pairs >= 6 && against == 0 {
        Verdict::Significant
    } else if discordant_pairs >= 6 && favorable == 0 {
        Verdict::Regression
    } else {
        Verdict::Inconclusive
    };
    Ok(Comparison {
        verdict,
        discordant_pairs,
    })
}

/// Per-case outcome.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Deserialize, Serialize)]
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
            bail!(
                "golden set line {}: expected_ids must not be empty",
                lineno + 1
            );
        }
        cases.push(case);
    }
    if cases.is_empty() {
        bail!("golden set contains no cases");
    }
    Ok(cases)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Materialize only the live entry state needed by the freeze from JSONL.
/// This intentionally never consults SQLite.
pub fn corpus_hash_from_event_log(
    events_path: &Path,
    expected_ids: &BTreeSet<String>,
) -> Result<(String, BTreeSet<String>)> {
    let events = crate::components::events::read_events(events_path)?;
    if let Some(torn_tail) = &events.torn_tail {
        eprintln!(
            "retrieval-eval: WARNING event log at {} has a torn final line {} ({} bytes) — hashing only the complete prefix",
            events_path.display(),
            torn_tail.line,
            torn_tail.bytes.len()
        );
    }
    let mut live: BTreeMap<String, String> = BTreeMap::new();
    for event in events.events {
        if event.get("table").and_then(|v| v.as_str()) != Some("entries") {
            continue;
        }
        let Some(id) = event.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        match event.get("action").and_then(|v| v.as_str()) {
            Some("upsert") => {
                if event
                    .get("is_stale")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    live.remove(id);
                } else {
                    let content = event.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    live.insert(id.to_owned(), sha256_hex(content.as_bytes()));
                }
            }
            Some("expire") => {
                live.remove(id);
            }
            _ => {}
        }
    }
    let present = expected_ids
        .iter()
        .filter(|id| live.contains_key(*id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut domain = Vec::new();
    for id in expected_ids {
        if let Some(content_hash) = live.get(id) {
            domain.extend_from_slice(id.as_bytes());
            domain.push(0);
            domain.extend_from_slice(content_hash.as_bytes());
            domain.push(b'\n');
        }
    }
    Ok((sha256_hex(&domain), present))
}

pub fn ids_only_corpus_hash(expected_ids: &BTreeSet<String>) -> String {
    let mut domain = Vec::new();
    for id in expected_ids {
        domain.extend_from_slice(id.as_bytes());
        domain.push(b'\n');
    }
    sha256_hex(&domain)
}

pub fn expected_ids(cases: &[GoldenCase]) -> BTreeSet<String> {
    cases
        .iter()
        .flat_map(|c| c.expected_ids.iter().cloned())
        .collect()
}

pub fn validate_sealed_manifest(
    cases: &[GoldenCase],
    events_path: &Path,
    manifest: &SplitManifest,
) -> Result<()> {
    let dev_ids = expected_ids(
        &cases
            .iter()
            .filter(|c| c.split == Split::Dev)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let sealed_ids = expected_ids(
        &cases
            .iter()
            .filter(|c| c.split == Split::Sealed)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let frozen_dev = manifest.dev_ids.iter().cloned().collect::<BTreeSet<_>>();
    let frozen_sealed = manifest.sealed_ids.iter().cloned().collect::<BTreeSet<_>>();
    if dev_ids != frozen_dev || sealed_ids != frozen_sealed || !dev_ids.is_disjoint(&sealed_ids) {
        bail!("SEALED_SPLIT_MANIFEST_MISMATCH: golden membership differs from frozen manifest");
    }
    let ids = expected_ids(cases);
    let (event_hash, present) = corpus_hash_from_event_log(events_path, &ids)?;
    let absent: Vec<_> = ids.difference(&present).cloned().collect();
    if !absent.is_empty() {
        bail!("SEALED_CORPUS_STALE_OR_ABSENT: {}", absent.join(","));
    }
    let actual_hash = match manifest.corpus_hash_domain.as_str() {
        "id-content-hash" => event_hash,
        "ids-only" => ids_only_corpus_hash(&ids),
        other => bail!("SEALED_MANIFEST_UNKNOWN_HASH_DOMAIN: {other}"),
    };
    if actual_hash != manifest.corpus_hash {
        bail!(
            "SEALED_MANIFEST_HASH_MISMATCH: expected {}, got {}",
            manifest.corpus_hash,
            actual_hash
        );
    }
    Ok(())
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
    Ok(EvalReport {
        k: opts.limit,
        per_case,
    })
}

pub fn evaluate_split(
    conn: &rusqlite::Connection,
    embedder: &dyn Embedder,
    cases: &[GoldenCase],
    opts: &SearchOptions,
    requested: Split,
) -> Result<EvalReport> {
    if let Some(case) = cases.iter().find(|c| c.split != requested) {
        bail!(
            "EVAL_SPLIT_REFUSAL: {:?} case reached {:?} scorer: {}",
            case.split,
            requested,
            case.query
        );
    }
    evaluate(conn, embedder, cases, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(hits: &[bool]) -> EvalReport {
        EvalReport {
            k: 10,
            per_case: hits
                .iter()
                .enumerate()
                .map(|(i, hit)| CaseResult {
                    query: format!("q{i}"),
                    expected: 1,
                    hits: usize::from(*hit),
                    first_rank: hit.then_some(1),
                })
                .collect(),
        }
    }

    #[test]
    fn exact_mcnemar_boundary_is_six_unanimous_pairs() {
        assert_eq!(
            compare_reports(&report(&[false; 6]), &report(&[true; 6]))
                .unwrap()
                .verdict,
            Verdict::Significant
        );
        assert_eq!(
            compare_reports(&report(&[false; 5]), &report(&[true; 5]))
                .unwrap()
                .verdict,
            Verdict::Inconclusive
        );
    }
}
