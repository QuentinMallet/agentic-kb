# Event Log Format

`events.jsonl` is the log of record. It is one JSON object per line, and it is
read by every surface that materializes state.

## Commit envelope

Every append — a multi-event batch and a single event alike — is wrapped in an
in-band commit envelope:

```jsonl
{"action":"batch_begin","batch_id":"9f1c…","n":3}
{"action":"upsert","table":"entries","id":"e1", …}
{"action":"evidence_add","table":"evidence","entry_id":"e1", …}
{"action":"expire","table":"entries","id":"e0", …}
{"action":"batch_commit","batch_id":"9f1c…","n":3}
```

A span counts as committed only when its `batch_commit` line is present **and**
newline-terminated. An append interrupted anywhere before that contributes zero
events, instead of leaving a readable prefix of a batch the writer never
finished.

Marker lines are not events. The reader consumes them and never returns them,
so they never reach the materializer, and they are never counted by `kb rebuild`
or `kb compact`.

## Reader rules

| Log shape | Meaning |
|---|---|
| a line outside any span | committed standalone event |
| a span with a newline-terminated `batch_commit` | all of its events are committed |
| a dangling `batch_begin` at end of file | uncommitted; dropped by every reader |
| a dangling `batch_begin` in the middle of the log | hard error, never a silent drop |
| `n` disagreeing with the observed event-line count | hard error |

`ReadEvents::committed_len` is the byte offset one past the last committed
record, excluding any span left open at the tail. It is the only offset that may
cross a process or phase boundary: no span straddles it, so bytes before it can
never be reinterpreted by bytes that arrive later.

## Reading older logs

Logs written before the envelope contain no marker lines, so every one of their
lines is a committed standalone event. They replay unchanged and there is no
migration step.

## Downgrading

A binary that predates the envelope treats marker lines as unknown events and
would apply an uncommitted span as if it were committed. Before rolling back to
one, run `kb compact` under the current binary. Compact rewrites the log from
the reader's output, which is marker-free by construction, so what it emits is
readable by both.

Repair of an uncommitted tail happens on the next append, under the event-log
lock. A dangling span is truncated; the discarded bytes are copied to an
`events.jsonl.torn-<timestamp>` sidecar on a best-effort basis, because an
uncommitted span was never reader-accepted and preserving it must never block
reclaiming the space.
