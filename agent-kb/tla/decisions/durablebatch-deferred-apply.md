# Deferred apply after unreadable log tail

## Decision

`DurableBatch` now distinguishes an unreadable durable tail from a repaired
tail whose recovery is still outstanding. `Damage` makes a complete written
batch durable and marks a line beyond `cursor.off` unreadable. While that
condition remains, `DeferredWrite` fsync-appends and applies later batches but
does not change the cursor. `Repair` makes the damaged line readable again;
fair `Recovery` then replays from the old cursor through EOF and stamps the
cursor there. Repeated event arms are set insertion/removal, so replay is
idempotent.

## Why weakening the cursor invariant is sound

The old equality between the DB and the cursor prefix describes a settled
cursor, not a DB that intentionally contains durable writes beyond that
cursor. It is therefore retained as `CursorAgreesWithDB` for the
counterexample and required by `CursorAgreesWhenNotDeferred` only when no
deferred recovery is outstanding. `CursorNeverAheadOfDB` separately requires
that replaying the durable tail over the current DB reaches the log's
materialization, and `DBNotAheadOfDurable` still prevents DB effects without
a preceding durable append. `TailReplayIsIdempotent` witnesses that a second
replay has no effect. `DeferredConverges`, under the fairness in
`DeferredFairSpec`, requires repaired damage eventually to reach EOF equality.

## Not modelled

The module has no log-existence state: both `log_written` and `log_durable`
are sequences representing an existing file. Consequently `LogMissing` and
its read/write/rebuild decision row are outside this amendment rather than
encoded ambiguously as an empty log. The implementation requirement remains:
when the file is absent and either cursor offset is positive or the entries
table is non-empty, reads proceed but writes and rebuild recovery are refused.
Tail fingerprints and the byte-level in-place rewrite are also abstracted to
the two booleans `deferred` and `damage_repaired`; physical offsets remain
line counts, as in the existing model.
