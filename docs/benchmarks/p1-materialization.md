# P1 semantic and cue materialization

P1 retains the complete semantic `(score, id)` rank vector, but fetches full entry metadata only
for the first `2 * limit` ids. The cue scan similarly retains one `(score, cue)` winner per entry,
truncates the ranked winners to `2 * limit`, and only then fetches entry metadata. The counters below
measure the UTF-8 bytes held in the fetched metadata fields (including the id); they are a stable
allocation-pressure proxy, not a process RSS measurement.

Run the ignored 10k test on the host (it seeds the same deterministic `BenchEmbedder` corpus as
`kb-bench-fixture`, with seed 42):

```bash
cargo test -p kb p1_materialization_measurement_10k -- --ignored --nocapture
```

Copy the emitted counters into both cells rather than inferring cue behavior from the hybrid total.

| lens3 item / lane | corpus | before rows / bytes | after rows / bytes | command output |
|---|---:|---:|---:|---|
| #9 semantic entry scan | 10,000 entries | not measured (no pre-P1 baseline run; this test exercises only the current, already-bounded code path) | 200 rows / 58,991 bytes | `semantic: semantic_rows=200 semantic_bytes=58991 cue_rows=0 cue_bytes=0` |
| #10 cue scan | 10,000 entries / 10,000 cues | not measured (no pre-P1 baseline run; this test exercises only the current, already-bounded code path) | 200 rows / 59,381 bytes | `cue: semantic_rows=200 semantic_bytes=58991 cue_rows=200 cue_bytes=59381` |

The semantic-only iteration reports the semantic lane independently. The hybrid iteration reports
both counters; use its cue columns for #10. With the default measurement limit of 100, each
materialized row count must be at most 200.

## Measurement conditions (2026-09-05)

- Worktree: `.state/worktrees/bd-21ef.3` (evidence for bd-21ef.3.14).
- Command: `cargo test p1_materialization_measurement_10k -- --ignored --nocapture`.
- Host was not quiet: 1-minute load average was 6.38 at the start of the run (no `cargo`/`rustc`/
  `nextest` processes were running by name, but this is well above an idle machine).
- Test build (debug/test profile): 1m 36s. The test itself ran 72.81s.
- Full verbatim output (both iterations of the same test, plus an unrelated FTS5 log line the
  test also emits):

  ```
  semantic: semantic_rows=200 semantic_bytes=58991 cue_rows=0 cue_bytes=0
  kb: fts5_content_entries_search date=2026-09-05 result_count=0
  cue: semantic_rows=200 semantic_bytes=58991 cue_rows=200 cue_bytes=59381
  test components::db::tests::p1_materialization_measurement_10k ... ok
  ```

  Result: `ok. 1 passed; 0 failed`.

## What the matrix shows

Both iterations report exactly 200 materialized semantic rows / 58,991 bytes at the 10,000-entry,
limit=100 corpus — the `2 * limit` bound the methodology above describes, not the full 10,000-row
corpus. The semantic-only iteration reports `cue_rows=0 cue_bytes=0`, confirming the cue lane is
not touched when it isn't exercised. The hybrid iteration additionally materializes 200 cue rows /
59,381 bytes on top of the same 200 semantic rows / 58,991 bytes. No timing figures for the
materialization step itself are printed by this test (only the overall build time and total test
wall-clock, both above); the test's own pass/fail is solely on row/byte counts, not latency.
