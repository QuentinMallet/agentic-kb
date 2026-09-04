# P2 pre-normalization threshold (pre-registered)

Status: threshold recorded before running `norm_cost`; no measurements are present in this file.

## Cost basis

C1 T3 has already landed a `SCHEMA_VERSION` 2 to 3 bump on branch `bd-21ef.1` for keyed
`run_history`. On the combined program branch, every existing database therefore already pays for
the mandatory backup and full event-log replay with re-embedding. A pre-normalization change can
ride that rebuild: it needs no separate marker, creates no opt-in coverage gap, and requires no
permanent dual read path. This decision therefore uses the **marginal cost on that combined
program branch**, not the standalone C3 migration cost.

## Pre-registered decision rule

> Land pre-normalized persisted embeddings in a follow-up epic if recomputing `norm_b` accounts
> for at least 10% of per-query similarity-pass time at the 10,000-entry corpus at any one of the
> semantic, cue, or MMR sites; otherwise defer it.

For each cell, `norm_b cost % = (cosine time - pre-normalized dot time) / cosine time * 100`, using
Criterion's point estimates. Ten percent is large enough to exceed ordinary microbenchmark noise
and to be an actionable hot-loop saving, while the already-paid C1 rebuild makes the marginal
migration cost low enough that one materially improved production site is sufficient. The C3 plan
identifies the `bd-tx0` `verify_matrix` precedent: use explicit independently measured cells and
copy command output rather than inferring one lane from an aggregate. No `bd-tx0` methodology
document is present under this checkout's `docs/` or `.omc/`.

## Measurement matrix

Each result cell records `cosine recomputing norm_b / dot with vectors pre-normalized in memory`
and the derived percentage. Pre-normalization occurs outside the timed region.

| similarity site | 1k cosine | 1k pre-normalized dot | 1k norm_b cost | 10k cosine | 10k pre-normalized dot | 10k norm_b cost |
|---|---:|---:|---:|---:|---:|---:|
| semantic `db.rs` cosine loop | TODO | TODO | TODO | TODO | TODO | TODO |
| cue best-per-entry loop | TODO | TODO | TODO | TODO | TODO | TODO |
| MMR pairwise loop (`limit * 2` pool) | TODO | TODO | TODO | TODO | TODO | TODO |

## Verdict

Run from the repository devShell:

```bash
KB_NO_EMBED=1 cargo bench --bench norm_cost
```

Verdict:
