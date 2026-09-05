# Event Log Format

`events.jsonl` is a JSONL log. Current writers wrap every append, including a
single event, in one commit envelope. `write_span`

```jsonl
{"action":"batch_begin","batch_id":"9f1c…","n":3}
{"action":"upsert","table":"entries","id":"e1"}
{"action":"evidence_add","table":"evidence","entry_id":"e1"}
{"action":"expire","table":"entries","id":"e0"}
{"action":"batch_commit","batch_id":"9f1c…","n":3}
```

The markers must carry the same `batch_id` and declared event count `n`, with
exactly `n` non-marker lines between them. A batch becomes committed only when
the matching `batch_commit` and its final newline are present. Markers are
framing, not events returned to the materializer. `scan_events`, `marker_n`,
`marker_batch_id`, `test_single_append_is_wrapped_in_a_commit_envelope`

## Reader rules

| Log shape | Reader result |
|---|---|
| Marker-free line outside a span | Standalone committed event. The pre-framing corpus therefore needs no migration. `test_legacy_marker_less_log_reads_as_standalone_committed_events`, `test_vendored_116_event_legacy_corpus_replays_byte_identically` |
| Span ending in a matching, newline-terminated commit with the declared count | All enclosed events are committed. `test_batch_append_of_three_events_is_one_span` |
| Trailing begin without a complete commit | The whole span is uncommitted; its events are omitted and `committed_len` remains before the begin. `test_dangling_begin_at_eof_is_dropped_by_the_reader`, `test_span_without_a_newline_terminated_commit_is_uncommitted` |
| A second begin while a span is open | Hard error, never a silent drop. `test_mid_log_dangling_begin_is_a_hard_error` |
| More event lines than declared | Hard error as soon as the extra line is read. `test_more_event_lines_than_declared_n_is_a_hard_error` |
| Begin count, commit count, or observed count disagree | Hard error. `test_commit_n_disagreeing_with_the_span_is_a_hard_error` |
| Commit has no begin, or its batch ID differs | Hard error. `test_commit_marker_without_a_begin_is_a_hard_error`, `test_commit_with_a_mismatched_batch_id_is_a_hard_error` |

`ReadEvents::committed_len` is the byte offset immediately after the last
committed record, including the commit newline but excluding an open tail. It
is a span boundary, never a position inside a batch. `ReadEvents`,
`test_committed_len_is_a_span_boundary_not_a_line_boundary`

Under the event-log lock, append repair truncates a trailing open span back to
`committed_len` before appending. Copying discarded bytes to an
`events.jsonl.torn-<timestamp>` sidecar is best effort and cannot prevent the
truncation. Mid-log framing violations remain hard errors.
`repair_uncommitted_tail_before_append`,
`test_append_truncates_a_complete_dangling_span`,
`test_span_truncation_proceeds_when_the_sidecar_write_fails`

## Byte-level example

This file is 108 bytes; each displayed line ends in one `0a` byte:

```text
bytes   0..45   {"action":"batch_begin","batch_id":"b","n":1}\n
bytes  46..60   {"action":"x"}\n
bytes  61..107  {"action":"batch_commit","batch_id":"b","n":1}\n
```

The line lengths are 46, 15, and 47 bytes, so `committed_len` is 108. Without
the final newline the span is uncommitted and `committed_len` is 0.
`scan_events`, `test_span_without_a_newline_terminated_commit_is_uncommitted`

## Durability ordering

The writer emits the commit marker, calls `File::sync_data`, completes required
directory-entry syncs, and only then returns the committed length.
`append_events_batch_with_sync`

`append_and_apply_with` completes that durable append before any SQLite apply.
A sync failure prevents the database write, so the order is commit marker, log
durability, database apply, then cursor update. `append_events_batch_with_sync`,
`append_and_apply_with`, `test_append_sync_precedes_caller_apply`,
`test_sync_failure_returns_once_and_caller_applies_nothing`

## Measured cost

TODO: fill from T2b on a quiet machine: **<T2b p50/p95, quiet machine>**.

```sh
BENCH_LANES=write bash scripts/bench-interactive.sh cold
```
