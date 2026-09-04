# Retrieval Eval (`kb eval`)

Golden-set benchmark for search quality. Measures recall@k and MRR over a set
of (query → expected entry ids) cases so retrieval changes — RRF constants,
`recency_lambda`, `mmr_lambda`, `KB_EMBED_TEXT`, cue anchors — are validated
against numbers instead of tuned blind.

## Golden Set Format

JSONL, one case per line. Blank lines and `#` comments are skipped. Cases with
empty `expected_ids` are rejected.

Each case may set `split` to `dev` or `sealed`. Legacy lines without `split`
default to `dev`.

```jsonl
# retrieval golden set — agentic-kb
{"query": "recency decay half-life", "expected_ids": ["<entry-uuid>"], "split": "dev"}
{"query": "how does rebuild avoid blocking writers", "expected_ids": ["<uuid-1>", "<uuid-2>"], "split": "sealed"}
```

Build cases from real queries you expect the KB to answer: take an entry,
write the question a future session would actually ask, and record the entry
id. 20–50 cases is enough to catch regressions.

## Running

```bash
kb eval golden.jsonl                 # hybrid (default), k=10
kb eval golden.jsonl --fts --k 5     # FTS lane only
kb eval golden.jsonl --semantic      # semantic lane only
kb eval golden.jsonl --sealed        # sealed split only; manifest must validate
kb eval golden.jsonl --json          # machine-readable report
```

Output:

```
=== Retrieval eval (k=10) ===
  recall=1.00  first_rank=  1  hits=1/1  recency decay half-life
  recall=0.50  first_rank=  3  hits=1/2  how does rebuild avoid blocking writers
cases=2  recall@10=0.7500  mrr=0.6667
```

## Metrics

- **recall@k** — per case: `|expected ∩ top-k| / |expected|`, averaged over cases.
- **MRR** — per case: `1 / rank of first expected id` (0 if none found), averaged.

## CI Gate

`--min-recall` / `--min-mrr` exit non-zero when the aggregate falls below the
threshold, so the eval can gate merges:

```bash
kb eval golden.jsonl --min-recall 0.8 --min-mrr 0.6
```

## Workflow for Retrieval Changes

1. Record baseline: `kb eval golden.jsonl --json > before.json`
2. Apply the change (e.g. set `KB_EMBED_TEXT=abstraction` + `kb reembed`).
3. Compare: `kb eval golden.jsonl --json > after.json`
4. Keep the change only if recall/MRR hold or improve.

## Sealed Split

`kb eval --sealed` runs only the `split = "sealed"` cases and hard-fails unless
`split-manifest.json` beside the golden file validates against the current
event log. This is the "frozen holdout" path: if the manifest no longer matches
the corpus, the command stops instead of silently reinterpreting the benchmark.

## Comparing Two Runs

Use `--compare` with two JSON reports from paired same-corpus, same-time arms:

```bash
kb eval --compare before.json after.json
```

The output is one of:

- `SIGNIFICANT` — the paired comparison shows a significant improvement.
- `INCONCLUSIVE` — no information, NOT evidence of no regression.
- `REGRESSION` — significant negative movement; exits with code `3`.

`SIGNIFICANT` and `INCONCLUSIVE` exit `0`. Only `REGRESSION` is a hard CI gate.
