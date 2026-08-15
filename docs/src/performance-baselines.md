# Performance Baselines

`agentic-kb` keeps two benchmark lanes for interactive performance.

## Why Two Lanes

Criterion is the attribution lane. It isolates in-process components, keeps stable per-component history, and makes regression continuity possible across optimization work. The relevant benches here are `bench_interactive_surfaces` and `bench_search_vs_size`.

Hyperfine CLI is the authoritative lane for the interactive budget because it pays what the hook pays: process spawn, config load, command dispatch, and candle model load on the hybrid path. The budget is:

`p95 < 500ms` at `10k` entries, cold, on `kb context`, `kb cited-by`, and `kb_search-with-verify`, with `search-hybrid-embed` carried as the real session-hook shape.

Treat the lanes differently:

- Use Criterion to explain where time goes.
- Use Hyperfine CLI to decide whether the user-visible budget passed.

## Re-Running The Lanes

### Hyperfine CLI lane

Cold lane:

```bash
nix develop -c bash scripts/bench-interactive.sh cold
```

Warm lane:

```bash
nix develop -c bash scripts/bench-interactive.sh warm
```

Environment knobs:

```bash
BENCH_RUNS=20 BENCH_LARGE_RUNS=10 BENCH_WORK_DIR=.state/bench-interactive \
  nix develop -c bash scripts/bench-interactive.sh cold
```

What the harness does:

- Builds `kb` and `kb-bench-fixture`.
- Seeds fresh `10k` and `100k` fixture repos with seed `42`.
- Runs Hyperfine on `search-verify-on`, `search-verify-off`, `context`, `cited-by`, and `search-hybrid-embed`.
- Writes raw Hyperfine JSON to `"$BENCH_WORK_DIR"/raw-<date>-<mode>/`.
- Writes the stable consolidated artifact to `.omc/benches/<date>-interactive-cli-<cold|warm>.json`.

Model cache requirement:

- `scripts/bench-interactive.sh` exports `FASTEMBED_CACHE_PATH` before running the hybrid lane.
- The intended cache is the repo-local `.fastembed-cache`; the harness also checks the sibling repository root when invoked from a worktree.
- `src/config.rs:275` honors `FASTEMBED_CACHE_PATH` first, so the benchmark lane uses that cache instead of falling back to `$HOME/.cache/fastembed`.

### Criterion attribution lane

Interactive component attribution:

```bash
KB_NO_EMBED=1 nix develop -c cargo bench --bench bench_interactive_surfaces
```

Search-vs-size attribution:

```bash
KB_NO_EMBED=1 nix develop -c cargo bench --bench bench_search_vs_size
```

What Criterion emits:

- Per-group estimates under `target/criterion/**/new/estimates.json`.
- Stable checked-in summaries only when a follow-up task exports them into `.omc/benches/<date>-*.json`.

The Criterion lane uses `BenchEmbedder`, not the real candle embedder. That is intentional: it preserves reproducibility and isolates search stages, but it is not the authoritative end-to-end budget lane.

## Artifact Schema

The current interactive CLI artifact schema is `.omc/benches/<date>-interactive-cli-{cold,warm}.json`:

```json
{
  "meta": {
    "date": "YYYY-MM-DD",
    "branch": "<git branch>",
    "commit": "<short sha>",
    "embedder": "<embedder description>",
    "seed": 42,
    "sample_size": {
      "10000": 20,
      "100000": 10
    },
    "actual_elapsed_seconds": 5286.0,
    "note": "<artifact note>"
  },
  "results": {
    "10000-context": {
      "p50_seconds": 0.0467,
      "p95_seconds": 0.0661,
      "max_seconds": 0.0665,
      "times_seconds": []
    }
  }
}
```

`meta` carries the run identity and capture context:

- `date`
- `branch`
- `commit`
- `embedder`
- `seed`
- `sample_size`
- `actual_elapsed_seconds`
- `note`

Each per-surface row carries:

- `p50_seconds`
- `p95_seconds`
- `max_seconds`
- `times_seconds`

The checked-in `2026-06-10` baseline artifacts are older reference rows. They are mean/CI-only summaries and do not contain percentile fields.

## Current Baselines

### 10k cold budget

The current citable cold budget rows come from `.omc/benches/2026-08-15-interactive-cli-cold.json`.

| Surface | Cold p95 | Budget | Verdict |
|---|---:|---:|---|
| `cited-by` | 12.299ms | 500ms | PASS |
| `context` | 66.109ms | 500ms | PASS |
| `search-verify` | 7.415ms | 500ms | PASS |
| `hybrid-embed` | 356.757ms | 500ms | PASS |

### 100k graceful bound

The `100k` ratio breach is established from the clean in-process floor:

- semantic brute-force scan: about `647ms`
- cue-row scan: about `689ms`
- floor before process/model/merge/format overhead: `1.336s`
- ratio bound from the clean `10k` cold hybrid p95: `4 x 356.757ms = 1.427s`

That leaves about `91ms` of slack before unavoidable end-to-end overhead, so the ratio breach is established without citing the contaminated `100k` CLI lane.

The `2026-08-15` `100k` CLI p95 rows are contaminated by concurrent builds and are not citable as precise p95 evidence. Final closure on the absolute `<= 2s` target still awaits one clean post-optimization CLI run.

## Attribution And Known Issues

- Cue-row scan dominates and is linear: about `67ms -> 689ms`.
- Semantic brute-force scan dominates and is linear: about `60ms -> 647ms`.
- `context` is also materially linear: about `47ms -> 515ms`.
- `cited-by` is linear despite its path index: about `2.7ms -> 26.9ms`. This remains an open query-shape question.
- The `search_vs_size/10000/hybrid` vs `search_vs_size/10000/hybrid_verify_k10` `12x` inversion is a harness anomaly. Those two `10k` rows are non-citable.
- Embedder first-touch is a one-time outlier: `91.9s` once versus about `304ms` steady-state.

## Interpreting A Run

- `PASS`: the cited budget row is within the target.
- `BREACH`: the cited budget row exceeds the target.
- `INCONCLUSIVE`: the run cannot support a precise verdict, usually because contamination or a harness anomaly invalidates the row as evidence.

Contamination can only inflate latency measurements.

- A `PASS` under contamination is conservative.
- A `BREACH` under contamination is not conclusive.
- An `INCONCLUSIVE` row should not be used as acceptance evidence in either direction.
