# Unreadable or missing log blocks writes

## Decision

When inspection cannot read the log past `cursor.off`, recovery warns and
declines to rebuild, and every write path refuses to proceed. Read-only paths
continue to serve the current DB with the existing staleness warning; reads are
not state transitions in `DurableBatch` and are therefore unmodelled. A missing
log has the same contract when the cursor or DB proves that the absence is not
a genuinely empty store.

`Damage` may occur after any number of applied batches and while a complete,
durable batch is still unapplied. It leaves `deferred` set and the DB/cursor
unchanged. `StartBatch`, `Compact`, `RecoverIdle`, and `RebuildAll` are disabled
while deferred. `Repair` makes the damaged tail readable in place; fair
`Recovery` then folds the durable committed tail from `cursor.off`, advances
the cursor to `DurCommittedLen`, and clears the deferred state.

This preserves `CursorAgreesWithDB` and `DBNotAheadOfDurable` without a
deferred-state exception. `CursorNeverAheadOfDB` checks that replaying the tail
over the cursor-described DB produces the full durable materialization, and
`DeferredConverges` checks that repaired damage eventually becomes caught up.
Only `Recovery` needs fairness: the liveness antecedent begins after `Repair`
has already happened.

## Withdrawn alternative

Round 1 allowed a `DeferredWrite`: a later batch was appended, synced, and
applied while the unreadable earlier tail kept the cursor unchanged. That
alternative is withdrawn. If a durable-but-unapplied batch precedes the damage,
materializing a later batch creates a non-prefix of the durable log and violates
`DBNotAheadOfDurable`; weakening cursor equality merely hid the same ordering
error. `UnsafeWriteWhileDeferred` exists only in the `Fixed = FALSE`
counterexample model so TLC can expose that rejected behavior.

`TailReplayIsIdempotent` was also dropped. With the model's set-valued
`FoldEvents`, applying any fixed upsert/expire sequence twice is structurally
equal to applying it once, so the formula was a tautology rather than useful
evidence. Idempotence of non-set production arms remains a code/test obligation.

## Model boundary: missing log

The module represents an existing log as a sequence and has no file-existence
state. Encoding absence as `<<>>` would conflate `LogMissing` with a genuinely
empty store, so `ASSUME OutOfScope_LogMissing` remains explicit. The required
ninth recovery row is nevertheless normative: if the log is absent while
`cursor.off > 0` or the entries table is non-empty, warn, serve reads, refuse
writes, and decline automatic rebuild until the log is restored.
