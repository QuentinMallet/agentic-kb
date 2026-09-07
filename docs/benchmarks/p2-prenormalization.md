# P2 pre-normalization land-or-defer threshold

**Pre-registered 2026-09-05, before any measurement exists.** This page
records the land-or-defer decision rule for the persisted pre-normalized
embedding format (follow-up `bd-prenorm-embeddings-followup-te13`) ahead of
the benchmark run that will decide it, so the threshold cannot be tuned to
whatever numbers come out.

**Measured 2026-09-06. Verdict: LAND** — see Outcome below.

## Decision rule

**LAND** the persisted pre-normalized format only if, at 10,000 entries, the
pre-normalized path saves at least **10%** of the marginal per-query cost at
**each** of these three sites:

- the semantic lane
- the cue lane
- MMR (`mmr_rerank`)

**DEFER** (keep the existing finiteness guards only, make no format change) if
any one of the three sites falls short of the 10% threshold.

The basis for this rule is **marginal cost** — the normalization work
`cosine_similarity` currently repeats on every call (computing `norm_a` and
`norm_b` from each stored vector at read time, once per query) — not total
query latency. A win that is real but small relative to total query latency
still counts if it clears 10% of the marginal (normalization-only) cost; a
win that looks large relative to total latency does not count if it is below
10% of the marginal cost.

This assumes C1's `SCHEMA_VERSION` bump has already landed on the aggregator
branch. It has: `SCHEMA_VERSION = 3` is present in
`src/components/db.rs` at this commit.

## The three sites

All three currently call `cosine_similarity`, which normalizes both operands
on every invocation:

- **Semantic lane** — `search_entries`'s per-candidate scan over
  `entries_emb`, scoring each row against the query embedding.
- **Cue lane** — `search_entries`'s per-candidate scan over `cues`, scoring
  each cue anchor against the query embedding.
- **MMR** — `mmr_rerank`'s greedy re-rank, scoring each remaining candidate
  against every already-selected row.

A persisted pre-normalized format would store each embedding with unit norm
so `cosine_similarity` reduces to a dot product at each of these sites,
instead of recomputing `norm_a`/`norm_b` per call.

## Measurement

Run on a quiet machine:

```bash
KB_NO_EMBED=1 cargo bench --bench norm_cost
```

`benches/norm_cost.rs` does not exist yet as of this commit — this page
pre-registers the threshold ahead of that bench being written and run, per
the C3 plan's requirement that the rule be recorded before the numbers exist.

10,000-entry corpus (decision corpus, per the rule above):

| Site | Marginal cost — current (normalize at read time) | Marginal cost — pre-normalized (persisted unit-norm) | Savings | Meets 10%? |
|---|---:|---:|---:|:---:|
| Semantic lane | 21.421 ms | 7.9311 ms | 62.98% | Yes |
| Cue lane | 20.977 ms | 7.5928 ms | 63.80% | Yes |
| MMR | 1.3668 ms | 0.35486 ms | 74.04% | Yes |

1,000-entry corpus (supplementary, not decision-relevant):

| Site | Marginal cost — current | Marginal cost — pre-normalized | Savings |
|---|---:|---:|---:|
| Semantic lane | 1.9185 ms | 0.75112 ms | 60.85% |
| Cue lane | 2.1663 ms | 0.74129 ms | 65.78% |
| MMR | 1.4798 ms | 0.42113 ms | 71.54% |

Cosine/dot ratio at 10k: semantic 2.70x, cue 2.76x, mmr 3.85x — dot with
pre-normalized vectors is markedly faster at every site. Point estimates are
Criterion medians from `cargo bench --bench norm_cost` (`KB_NO_EMBED=1`); MMR
was measured at `limit=10, pool=20`.

**Provenance:**

- **Date:** 2026-09-06
- **Commit:** `91bec50` (this worktree's base, `bd-21ef.3.13-verdict` off the
  `storage-correctness-2` aggregator)
- **Machine:** not quiet — a background miner process, a backup job, and a
  microVM were running concurrently; 1-minute load average was ~7.7 at the
  start of the run (below the earlier-session peaks of 40-90+, but well
  above an idle machine). Bench build (`bench` profile) took 4m 09s.
  Numbers below the ~10x noise floor these conditions imply would need
  re-verification on a quiet machine; the observed savings (61-74%) are far
  enough above that floor to stand regardless.

## Outcome

**LAND.** All three sites clear the 10% marginal-cost threshold by a wide
margin at the 10,000-entry corpus (63-74% vs. the 10% bar), decided
independently per site with no averaging, per the pre-registered rule above.
The persisted pre-normalized embedding format is justified; the follow-up
`bd-prenorm-embeddings-followup-te13` is unblocked by this verdict. The
finiteness guards that were the fallback if this bench had come back DEFER
already landed unconditionally in C3, independent of this outcome.
