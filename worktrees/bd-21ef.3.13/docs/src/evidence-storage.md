# Evidence Storage

The agent KB is an event-sourced system. A JSONL event log is the source of truth, materialized into SQLite on read. The standing invariant is:

```
DB ≡ Materialize(events.jsonl)  [live-state equivalence]
```

This page describes how evidence rows are stored, retained, and recovered across the two major operations: incremental updates and compaction.

## Storage Contract

The contract governing evidence rows is defined in two architecture decision records (ADR-1 and ADR-2).

### ADR-1 — Upsert Preserves Evidence; Status Always Derived

An entry upsert does **not** clear the entry's evidence set. When an entry is re-added (via `kb_add` with the same `id`), any evidence citations already recorded survive the upsert.

The `evidence_status` field (present | missing | n_a) is **always** derived from the evidence row set on replay. The upsert event payload carries an `evidence_status` field for historical compatibility, but it is not authoritative — it is discarded during `Materialize()` and the status is recomputed from rows.

**Legacy entries (entries added before an explicit `kind` field was mandatory) carry a special carve-out:**
- A fresh insert of a legacy entry initializes `evidence_status = 'n_a'` (the "grandfather" status).
- A re-upsert of an existing legacy entry does NOT overwrite its current status; the status is preserved as-is.
- Evidence rows attached to a legacy entry are dropped during compaction if the compaction reorders them to precede the entry's upsert (since they were no-ops on the original replay).

**Stale entries are orphan-equivalent:**
A stale entry (one that has been expired but whose record still exists in the database with `is_stale=1`) is treated the same as an absent entry for the purposes of evidence updates. An `evidence_add` or `evidence_expire` event targeting a stale entry is silently skipped — no row is added, no error is raised.

### ADR-2 — Entry Expire GCs Its Evidence; Live-State Equivalence

Expiring an entry removes its evidence rows from the database. When `kb_core::expire()` processes an entry, the `evidence` table is purged of all rows for that entry.

Because compaction deliberately retains only the last upsert per live entry and drops entries whose last expire follows their last upsert, the invariant is weakened from **full-state equivalence** to **live-state equivalence**. This means:

```
For every live entry E:
  evidence[E] in DB ≡ Materialize(events.jsonl)[E]
```

Stale entries (entries with `is_stale=1` after an expire) are not required to match their materialized state. This is not a repair; it is an intentional design boundary: a stale entry's loss through compaction + rebuild is a known gap, not a violation.

## Compaction Retention and Ordering Rules

Compaction rewrites `events.jsonl` by filtering and re-ordering events. The process is divided into retention (which events to keep) and emission (in what order to write them).

### Retention

An evidence event (`evidence_add`, `citation_healed`, or `evidence_expire`) survives compaction if and only if:

1. Its parent entry `E` is **live at the end of the log** (the entry's last upsert comes after its last expire).
2. The event's original index `i` is **strictly greater than** `expire_last[E]` (the index of `E`'s most recent expire event). This bound prevents resurrecting evidence that was explicitly deleted.
3. The event's parent entry `E` was **live at its own index** `i`. That is, there exists a upsert of `E` at an index strictly before `i`, and no expire of `E` at an index between that upsert and `i`.

**Result:** Evidence orphaned by a fresh-database rebuild (events that precede their parent's first upsert) are not preserved, since they were no-ops during original materialization.

### Emission

Retained events are emitted in a specific order:

1. Walk upserts in their original index order.
2. For each retained upsert of a live entry `E`, immediately emit all of `E`'s retained evidence events that precede the *next* upsert of `E` (or the log end).
3. Preserve the relative order of evidence events for the same entry.

**Result:** Evidence events are never reordered ahead of their parent entry's upsert. The relative order of evidence events for a single entry (e.g., `evidence_add` followed by `citation_healed`) is preserved.

### Fixpoint Property

Compacting an already-compacted log is a no-op: the compacted log's size does not change, and a second compaction produces identical output. This is verified by the assertion `compact.rs:670-674` on every run.

## Torn-Tail Policy

The event-log reader (`events.rs:119-149`) tolerates incomplete writes at the end of the log, but enforces strict correctness for the middle.

### Tolerated: Final Unterminated Line

If the final line in `events.jsonl` is truncated — either a partial JSON object or a split multi-byte UTF-8 sequence — the reader succeeds and returns all **complete** events up to that point. The truncation is surfaced in the return value and can be logged by the caller.

The implementation uses `BufRead::read_until(b'\n', ...)` rather than `lines()` to distinguish a torn final line (no trailing newline) from a complete-but-malformed line (terminated with `\n` but corrupt JSON).

**Soundness assumption:** `append_event()` and `append_events_batch()` (`events.rs:88-116`) write content and then a newline to an unbuffered file. A process crash can only truncate the tail, never produce a terminated-but-truncated line.

### Hard error: Malformed Middle Lines

Any decode error on a line that is not the final line is a hard failure. The read operation returns an error and processing stops. This catches mid-log corruption, which is strictly worse than a truncated tail.

### Append Interaction

The append path (`events.rs`) also repairs a torn tail it finds when opening the log, using the same `parse_event_line` classification the reader uses — so repair and reader agree on what counts as "torn" versus "corrupt". The two tolerated-tail shapes are handled differently:

- **Reader-parseable but unterminated** (complete JSON, missing trailing newline — the crash window is the JSON bytes fully written but the newline not yet flushed): repair appends only the missing newline. No bytes are removed and no sidecar is created, because the reader already accepts this record as valid; sidecaring it would silently diverge the DB from the log and lose the event on the next rebuild.
- **Genuinely unparseable** (partial JSON or a split multi-byte UTF-8 sequence): repair sidecars the torn bytes to `events.jsonl.torn-<timestamp>` and truncates the log to the last complete line, exactly as compaction does below.

**Rule:** append repair never removes an event the reader accepts.

### Compact Interaction

When compaction reads a log with a tolerated torn tail, the torn bytes are preserved to a sidecar file `events.jsonl.torn-<timestamp>` before the new log is written and renamed into place. This is loud — the compaction caller sees the sidecar creation in the trace — so the operator can investigate if needed. The original partial event is not included in the compacted log.

## Evidence Lifecycle

The table below summarizes the state of evidence rows under different operations.

| Operation | Evidence Rows | Evidence Status | Notes |
|-----------|---------------|-----------------|-------|
| Initial `kb_add` (new entry) | inserted | derived from row count | status = missing |
| `evidence_add` | added | recomputed | status may change to present |
| `citation_healed` | updated in-place | not changed | healing does not alter status |
| `evidence_expire` | deleted | recomputed | status may change to missing |
| Entry upsert (re-add) | **preserved** | **preserved** (legacy) or **recomputed** (explicit-kind) | ADR-1: non-destructive |
| Entry expire | deleted (via GC) | set to n_a | ADR-2: GC on expire |
| Stale upsert (revive after expire) | deleted (via GC on the stale branch) | recomputed | same path as entry expire |
| Compaction (all cases above) | retained per index-relative bounds | unchanged on DB (rebuilt on next reload) | per retention rules above |

## Formal Model

The evidence storage contract is formally specified in TLA+ at `.state/agent-kb/tla/AgentKbEvidence.tla` (on the agentic branch). The module encodes ADR-1 and ADR-2 in the `ApplyEventE` action and defines `CompactionEquivalenceE` as live-state equivalence.

### Counterexamples

During implementation, five counterexamples were discovered where the code diverged from the contract. All are now closed.

| Counterexample | Shape | Resolution |
|---|---|---|
| **CE1** — evidence reordered ahead of its parent upsert | Evidence event compacted to precede the entry that created it | T2 emission rule: splice evidence after the parent's upsert |
| **CE2** — `evidence_expire` dropped, evidence resurrected | Compaction dropped `evidence_expire` events, so deletes were lost on rebuild | T1 retention arm: include `evidence_expire` in the match |
| **CE3** — legacy upsert re-grandfathers a de-legacied entry | `legacy_add` on an existing entry unconditionally reset `evidence_status` to `n_a` | ADR-1 amendment: legacy upsert on an existing entry preserves status |
| **CE4** — legacy upsert trailing an explicit-kind upsert | A compacted legacy upsert would set `n_a` after code had derived `missing` | Writer-producible-alphabet guard: `legacy_add` cannot fire if the entry has an explicit `add` in the log |
| **CE5** — post-expire `evidence_add` revived by compaction | An evidence event orphaned during a fresh rebuild could be reapplied by compaction | Closed by contract: live-at-index retention bound in ADR-2 implementation |

For detailed traces and TLC run results, see `.state/agent-kb/tla/T0-counterexample.md`.

### Verification

TLC model checking is run on every modification to `AgentKbEvidence.tla`. The verification uses two key regression gates:

- **Run 1 (no-compact):** Exercises all actions except `DoCompact` and verifies `CompactionEquivalenceE` is vacuously true (both sides produce the same state without compaction). This ensures the ADR-1/ADR-2 rewrite of `ApplyEventE` is internally consistent. **Distinct states: 159,545.**

- **Run 4 (full):** Includes `DoCompact` and verifies the full `CompactionEquivalenceE` (live-state equivalence after compaction). **Distinct states: 159,545** — identical to Run 1, which confirms that compaction does not alter the live-state invariant.

If a future change makes compaction lossy again, Run 4's state count will diverge upward before any invariant failure is reported, serving as an early warning.

## See Also

- [MCP Surfaces](./mcp.md) — evidence fields in search results and `kb_get`
- [Search Tuning](./search-tuning.md) — how evidence influences ranking
