# Bench Baseline: recall-ideas

- Base commit: 820fbd1 (post-FTS5-migration: a7f8e47 in history)
- Date: 2026-06-22
- BENCH_LARGE_SIZE: not set (1M skipped)
- Notes: 100k/hybrid_verify_k10 bench timed out during seeding (100k entries × 20 evidence rows = 2M rows). All other scenarios completed. Criterion reports p50/p95/p99 as [low mid high] interval — "mid" used as p50 estimate, "high" as p99 proxy.

## Results

Criterion reports `time: [low mid high]` (95% CI). Columns: p50≈mid, p99≈high.

| Scenario | Size | p50 (mid) | p99 (high) |
|---|---|---|---|
| fts_only | 1k | 1.2204 ms | 1.2675 ms |
| semantic_only | 1k | 2.2071 ms | 2.3374 ms |
| hybrid | 1k | 3.3259 ms | 3.3730 ms |
| hybrid_verify_k10 | 1k | 6.5319 ms | 6.6014 ms |
| fts_only | 10k | 4.1825 ms | 4.2306 ms |
| semantic_only | 10k | 21.860 ms | 22.128 ms |
| hybrid | 10k | 34.623 ms | 35.051 ms |
| hybrid_verify_k10 | 10k | 9.7200 ms | 9.8192 ms |
| fts_only | 100k | 35.029 ms | 35.947 ms |
| semantic_only | 100k | 203.02 ms | 204.13 ms |
| hybrid | 100k | 371.88 ms | 381.66 ms |
| hybrid_verify_k10 | 100k | n/a (timed out seeding) | n/a |

## Acceptance gate for T6 (recency bias)

Recency-bias λ multiplier must add < 5% p95 latency at 100k entries vs these baseline numbers.

**Key gate values (100k hybrid p99 = 381.66 ms):**
- 5% budget: +19.1 ms → post-recency-bias 100k/hybrid p99 must be < 400.8 ms
- Recency pass adds a single batch SELECT on `limit` IDs (default 10) + O(limit) multiply + O(limit log limit) re-sort. Expected overhead at limit=10: < 0.5 ms.

**Command used:**
```
cargo bench --bench bench_search_vs_size -- --warm-up-time 1 --measurement-time 3
```
(via `nix develop --command`)
