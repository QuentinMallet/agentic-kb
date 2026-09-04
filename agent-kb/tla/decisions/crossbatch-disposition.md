# `CrossBatch.tla` disposition (C1 / T0c, `bd-21ef.1.5`)

**Verdict: retained unchanged as the coarse-grained boundary regression gate;
superseded by `DurableBatch.tla` for every durability claim.**

Revision 1 of the C1 plan proposed amending `CrossBatch.tla` "so the invariant is
over the durable committed log." Read against the module, that instruction is
either a no-op or a full rewrite, because the distinction it asks to restate does
not exist in the module.

## Line facts

- `CrossBatch.tla:20` declares exactly three variables: `jsonl`, `db`,
  `phase_cb`. There is no `log_written`/`log_durable` split, so "the durable
  committed log" has no referent to restate the invariant over.
- All four commit actions update the log and the DB in a single atomic step:
  `:53-54` and `:62-63` (`CommitCallA`, `CommitCallA_NoReplace`), `:73-74` and
  `:82-83` (`CommitCallB`, `CommitCallB_NoReplace`). Each writes `jsonl'` and
  `db'` in the same conjunct list, so no state exists in which the log has moved
  and the DB has not.
- `Next_CrossBatch` (`:87-91`) offers only those four actions. There is no crash
  action, no sync action and no recovery action anywhere in the module.
- `Boundary_Invariant` (`:94-96`) is consequently true by construction of the
  commit actions rather than by any durability argument.

A module with one atomic write action per call cannot express a torn append, an
unsynced tail, or an apply that outruns the log. Bolting a `log_durable` variable
onto it would not be an amendment; it would be `DurableBatch.tla` with a
different file name and a weaker action set.

## What each module now owns

| Concern | Owner |
|---|---|
| Per-line append, commit markers, fsync ordering, crash, torn-tail truncation | `DurableBatch.tla` (T0a) |
| Three-phase rebuild, snapshot boundary, swap kill points | `RebuildProtocol.tla` (T0b) |
| Compaction, materialization equality, the applied cursor and its generation | `CompactMaterialize.tla` (T0c) |
| DB = Materialize(JSONL) at every *call boundary*, order-sensitively | `CrossBatch.tla` (unchanged) |

`CrossBatch.tla` keeps real value in the last row. Its `FoldLog_CB` (`:38-49`)
encodes last-writer-wins materialization, and `Boundary_Invariant` pins the
cross-invocation boundary: a future change that makes `kb_core::add` leave the DB
disagreeing with the log *between* calls fails this gate immediately and cheaply.
That is a different question from the one the durability modules ask, and it is
worth keeping a fast regression gate for.

## Consequences

1. No edit lands on `CrossBatch.tla` or `CrossBatch.cfg` under C1. The file's
   green status is not evidence about durability and must not be cited as such.
2. Durability claims cite `DurableBatch.tla`; compaction and cursor claims cite
   `CompactMaterialize.tla`. Citing `CrossBatch.tla` for either is the
   false-coverage failure recorded in `gotchas/tla/compact-spec-fidelity-gap`.
3. `CrossBatch.cfg` already carries `CHECK_DEADLOCK FALSE` (added by the `bd-w45u`
   audit); nothing in C1 changes its bounds (`EntryIds = {e1, e2}`,
   `MaxBatchSize = 2`).
