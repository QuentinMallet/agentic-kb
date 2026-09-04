# Write-Path Baseline

Pre-change baseline for the `kb add` write path, recorded for `T1b`
(`bd-21ef.1.2`) so that `T2b` (fsync ordering, `bd-21ef.1.7`) has a number to
compare against before it changes any write-path code. This is the baseline
only — no fsync-ordering changes are present at this commit.

## Provenance

- **Date:** 2026-09-04
- **Commit:** `2e2051d7fd805ed3737747decc0128dc946ed46f` (`2e2051d`), branch
  `bd-21ef.1.2`
- **Machine:** `x86_64`, AMD Ryzen 5 130 with Radeon Graphics, 12 logical
  cores, 62 GiB RAM
- **Embedder:** `KB_NO_EMBED=1` (NoopEmbedder) in both lanes — the honest
  denominator excluding model-load/embedding cost from the measured write path

## Hyperfine CLI lane (authoritative, end-to-end)

Command:

```bash
export CARGO_TARGET_DIR=/home/urist/Documents/perso/agentic-kb/target
BENCH_LANES=write nix develop /home/urist/Documents/perso/agentic-kb -c \
  bash scripts/bench-interactive.sh cold
```

Each Hyperfine sample runs one `kb add` against a fresh temporary copy of a
`kb-bench-fixture`-seeded 10,000-entry corpus (seed `42`), so p50/p95 are
per-add latencies from a fixed starting state (200 runs, cold mode):

| Metric | Value |
|---|---|
| p50 | 57.0 ms |
| p95 | 90.5 ms |
| max | 119.7 ms |

Mean ± σ (Hyperfine's own summary): 61.9 ms ± 14.7 ms
(User: 7.4 ms, System: 6.9 ms), range 43.3 ms – 119.7 ms.

Consolidated artifact:
`.omc/benches/2026-09-04-interactive-cli-write-cold.json`.

## Criterion attribution lane (in-process cost)

Command:

```bash
export CARGO_TARGET_DIR=/home/urist/Documents/perso/agentic-kb/target
KB_NO_EMBED=1 nix develop /home/urist/Documents/perso/agentic-kb -c \
  cargo bench --bench write_path
```

The `write_path` criterion target (`benches/write_path.rs`) attributes the
in-process cost of `kb_core::add` against a template `.state/agent-kb`
directory pre-seeded once with the 10,000-entry `kb-bench-fixture` corpus
(seed `42`) and cheaply file-copied per iteration, isolated from two of its
constituent steps (raw JSONL append, and DB apply of the resulting event
batch):

```
write_path/10000/kb_core_add_no_embed
                        time:   [110.27 ms 117.49 ms 126.24 ms]
Found 1 outliers among 30 measurements (3.33%)
  1 (3.33%) high severe

write_path/10000/append_events_batch_only
                        time:   [4.7891 ms 5.0078 ms 5.2578 ms]
Found 1 outliers among 30 measurements (3.33%)
  1 (3.33%) high mild

write_path/10000/db_apply_batch_only
                        time:   [54.321 ms 72.675 ms 101.73 ms]
Found 2 outliers among 30 measurements (6.67%)
  1 (3.33%) high mild
  1 (3.33%) high severe
```

(Criterion reports `[lower-bound point-estimate upper-bound]` of the mean per
iteration.) `append_events_batch_only` and `db_apply_batch_only` are the same
call shapes `kb_core::add` performs internally, run in isolation so the
in-process cost is split by step; `kb_core_add_no_embed` is the full call and
is not merely their sum since it also does dedup/near-duplicate lookups and
other bookkeeping around those two steps.

## Test suite

`cargo nextest run` (fast tier, default `PROPTEST_CASES` unset →16 heavy-test
case count): **547 tests run: 547 passed (2 slow), 0 skipped** in 120.18s.
