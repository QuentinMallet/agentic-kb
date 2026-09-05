# P2 pre-normalization land-or-defer threshold

**Pre-registered 2026-09-05, before any measurement exists.** This page
records the land-or-defer decision rule for the persisted pre-normalized
embedding format (follow-up `bd-prenorm-embeddings-followup-te13`) ahead of
the benchmark run that will decide it, so the threshold cannot be tuned to
whatever numbers come out. Every result cell below is `TODO`; do not fill
this page in without also filling in the measurement's date, commit, and
machine.

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

| Site | Marginal cost — current (normalize at read time) | Marginal cost — pre-normalized (persisted unit-norm) | Savings | Meets 10%? |
|---|---:|---:|---:|:---:|
| Semantic lane | TODO | TODO | TODO | TODO |
| Cue lane | TODO | TODO | TODO | TODO |
| MMR | TODO | TODO | TODO | TODO |

**Provenance (fill in when measured):**

- **Date:** TODO
- **Commit:** TODO
- **Machine:** TODO

## Outcome

TODO — LAND or DEFER, decided by applying the rule above to the filled-in
table. Do not decide from a subset of the three sites, and do not average
across sites: each site must independently clear 10%.
