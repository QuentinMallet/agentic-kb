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
| #9 semantic entry scan | 10,000 entries | TODO | TODO | TODO |
| #10 cue scan | 10,000 entries / 10,000 cues | TODO | TODO | TODO |

The semantic-only iteration reports the semantic lane independently. The hybrid iteration reports
both counters; use its cue columns for #10. With the default measurement limit of 100, each
materialized row count must be at most 200.
