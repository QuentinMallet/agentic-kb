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

## Model boundary: missing log (superseded 2026-09-05)

**This section is stale.** It described a state where `LogMissing` had no
file-existence state and `ASSUME OutOfScope_LogMissing` stood in for the
missing row. That ASSUME is gone; row 9 is now modelled directly. See
"2026-09-05 — LogMissing (row 9) modelled directly" below for the current
state of the module. The paragraph is left here, struck through in spirit but
not in fact, so the history of what changed and why is traceable:

> The module represents an existing log as a sequence and has no
> file-existence state. Encoding absence as `<<>>` would conflate
> `LogMissing` with a genuinely empty store, so `ASSUME OutOfScope_LogMissing`
> remains explicit. The required ninth recovery row is nevertheless normative:
> if the log is absent while `cursor.off > 0` or the entries table is
> non-empty, warn, serve reads, refuse writes, and decline automatic rebuild
> until the log is restored.

## 2026-09-05 — LogMissing (row 9) modelled directly

`bd-21ef.1.17`. `ASSUME OutOfScope_LogMissing` is retired. The module gained a
`log_present` variable distinguishing a fresh, present-but-empty log from an
absent one, and `LogMissing == ~log_present /\ (cursor.off > 0 \/ db_committed
/= EmptyDB)` — an exact transcription of `cursor.rs:inspect`'s missing-log
guard, including the choice of the committed DB for "non-trivial prior state".

**Actions.** `LogVanishes` (external loss: `log_present' = FALSE`, both log
sequences emptied, `db`/`db_committed`/`cursor` retained for reads) is gated
`AllowLogMissing`, `phase = "idle"`, non-trivial prior state, and — added in
the spec-gate fix round below — `~deferred`. `UnsafeWriteWhileLogMissing`
(`Fixed = FALSE` only) is the withdrawn alternative: it resurrects the file by
writing the next batch to a new empty log, which the fixed design refuses by
guarding `StartBatch`, `Compact`, `Recovery`, `RecoverIdle`, and `RebuildAll`
with `~LogMissing`.

**The `~deferred` guard on `LogVanishes` is a modelling choice, not a physical
claim** — a real log file can vanish while a deferral is outstanding. The
guard exists to mirror `cursor.rs:inspect`'s dispatch order (a call made after
both conditions hold reports `LogMissing`, never `Defer`) and to keep
`DeferredConverges` (the D3 liveness claim) meaningful without needing to be
scoped around an untested composition: with the guard, `LogMissing` forces
`phase = "idle"` and blocks every action that could start a new deferral, and
`LogVanishes` cannot fire while one is outstanding, so the two absorbing
conditions cannot compose in this model. That is a property of the guard, not
an independent fact about the implementation.

**Counterexample isolation.** The first committed `Current` cfg's
counterexample did not actually traverse `LogVanishes`/`UnsafeWriteWhileLogMissing`
— an unrelated, shallower CE2-class defect (`ApplyEvent` sets `db_committed'`
unconditionally under `Fixed = FALSE`) pre-empted it. Fixed by adding a
monotone ghost `log_lost` (set once by `LogVanishes`, never cleared) and a
gated invariant triple `LostDBNotAheadOfDurable` / `LostCursorAgreesWithDB` /
`LostCursorNeverAheadOfDB`, mirroring the module's `Deferred*` idiom:
`log_lost` stays TRUE across a resurrection even though `LogMissing` itself
goes false again, so the shallow pre-emption (which fires while
`log_lost = FALSE`) no longer satisfies the antecedent. (A cheaper
action-property-only alternative, checking only `PROPERTY
LogMissingDoesNotStart`, was also verified to isolate the defect; the ghost
route was chosen because it produces an end-to-end counterexample from the
real `Init` and generalizes to any future post-resurrection obligation.)

**Tightening.** `LogMissingFreezesState == [][LogMissing => UNCHANGED <<db,
db_committed, cursor, generation>>]_vars` was added alongside the bare
`\/ LogMissing` invariant disjuncts: the disjunct is sound only because
nothing under `Fixed` can move those variables while `LogMissing` holds, and
the freeze property states that directly rather than relying on it being
true today. `W_NotLogMissing == ~LogMissing` was added as a reachability
witness, matching the module's `W_Rec_*` / `W_Inner_*` convention.

**`RebuildAll`'s guard.** `~LogMissing` on `RebuildAll` models only the
*automatic* decline (`recover_if_needed`, the same code path `RecoverIdle`
models) — not the operator-invoked `kb rebuild` command, which the
implementation deliberately does not gate on `LogMissing`
(`rebuild.rs:393`'s `execute_with` never calls `cursor::inspect`) and which
can destructively empty a row-9 database. The comment and module header were
corrected to say this; the deliberate override itself is not modelled.

**Recorded boundaries (none machine-checked):**
1. Log restoration has no transition; row 9 is absorbing under `Fixed`.
2. The operator-invoked `kb rebuild` override of `LogMissing` (above).
3. A cursorless populated database whose log has vanished is NOT row 9 in the
   code — `cursor::inspect` classifies it `FullRebuild(CursorMissing)` before
   the missing-log check runs (`cursor.rs:583`), separately fail-closed by
   `rebuild.rs`'s `full_rebuild_for`. The module always carries a cursor and
   cannot express this state.

**Cfg verdicts** (full detail and state counts in
`DurableBatch-counterexamples.md`, "LogMissing — row 9 refuses resurrection"):

| cfg | verdict |
|---|---|
| `DurableBatch_LogMissing_Current.cfg` | VIOLATED `LostDBNotAheadOfDurable` (trace: `LogVanishes` → `UnsafeWriteWhileLogMissing`) |
| `DurableBatch_LogMissing_Fixed.cfg` | PASS (incl. `LogMissingBlocksWrites`, `LogMissingDoesNotStart`, `LogMissingFreezesState`) |
| `DurableBatch_LogMissing_Compose.cfg` | PASS — `Fixed`, `AllowCrash`, `AllowDeferred`, `AllowLogMissing` all TRUE, safety only |
| `DurableBatch_WIT_W_NotLogMissing.cfg` | VIOLATED (witness — proves `LogMissing` reachable) |
| refinement (`DurableBatch_Refinement_Fixed.cfg` with `AllowLogMissing` flipped, scratch only) | `InnerSafety` violated — row 9 is out of the `InnerGap` refinement's scope, documented next to the pre-existing compaction exclusion |
| all 26 pre-existing `DurableBatch_*.cfg` | unchanged verdicts and exact distinct-state counts (`log_lost` is constant and the `~deferred` guard on `LogVanishes` is non-restrictive whenever `AllowLogMissing = FALSE`) |
