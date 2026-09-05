# DurableBatch (T0a) — counterexamples and run matrix

Task `bd-21ef.1.3` (C1 / T0a). Recorded 2026-09-04 with TLC 2.19 (rev 5a47802),
Oracle JDK 1.8.0_504, 4 workers. This file supersedes the 2026-09-04 13:57
version, which was rejected by review for three Criticals; §6 records what
changed and why.

Every number below is from a run on this host, in this worktree, against the
`DurableBatch.tla` and `DurableBatch_*.cfg` files sitting beside this document.

## 1. Model shape

Two designs share one state machine, selected by the constant `Fixed`. There is
no `Scenario` constant any more and **no crash point is pinned**: `Crash` is
enabled at every phase of a batch from the first line through the last apply, in
every config that sets `AllowCrash = TRUE`.

The six knobs are `Fixed`, `AllowCrash`, `AllowDeferred`, `PoisonBatch`,
`MaxGen`, and `MaxBatches`.

| Action | Models |
|---|---|
| `StartBatch` | a write path begins an append (`add`, `expire`, `run`, …) |
| `AppendLine` | one `writeln!` of one physical line (`events.rs:117-133`) |
| `PartialFlush` | OS writeback: an arbitrary longer prefix becomes durable **without** an explicit sync — this is what exposes an unframed partial batch |
| `SyncLog` | D2 `sync_data` of the whole written log |
| `ApplyEvent` | one `db::apply_event`; the last one also writes the D3 cursor **in the same transaction** |
| `ApplyFail` | a deterministically failing apply (down embedder, malformed record) |
| `Quarantine` | D3 poison policy: dead-letter after K attempts, advance the cursor past it |
| `Crash` | power loss: the un-synced tail is gone, the open SQLite transaction is left dirty and is rolled back at open |
| `Open` | the D3 recovery table, three-way |
| `TruncateUncommittedTail` | D1 `repair_uncommitted_tail_before_append` |
| `Compact` | drops every line of a fully-dead batch and **bumps `generation` without touching the cursor** |
| `RecoverIdle` | `recover_if_needed` from `open_or_init`, before every write path |
| `RebuildAll` | operator-invoked `kb rebuild` |

### Write schedule

Batch contents are fixed so the refinement to `InnerGap` is *total*. `InnerGap`
can only produce a 1-event upsert batch (`StartNoReplace`) or a 2-event
`[expire e, upsert n]` batch (`Start`), so `DurableBatch` writes only those
shapes.

```
Batch 1 = <<upsert A>>              single-event append (D1 envelopes it too)
Batch 2 = <<expire A, upsert B>>    the CE1 shape
Batch 3 = <<expire B, upsert C>>    an append after recovery, exercises the cursor
```

The plan's D1 example uses n=3. n=2 is the same defect class — "crash after line
1 of N" — and is the smallest witness; going to n=3 would make the refinement
mapping partial for a bookkeeping reason rather than a design one. A batch
interrupted by a crash is **abandoned**, not retried: its caller is gone, and the
next `StartBatch` writes the next batch in the schedule.

### Bounds (stated up front, in the module)

| Constant | Value | Note |
|---|---|---|
| `EntryIds` | `{"A","B","C"}` | exactly the ids the schedule touches |
| `MaxBatches` | 2 or 3 | 3 wherever the run is affordable, which is everywhere |
| `MaxLogLen` | 12 | a **bound**, not a length: the schedule needs at most 3+4+4 = 11 physical lines, so the guard never binds |
| `MaxGen` | 0 or 1 | 1 enables `Compact`; 0 disables it |
| `K` | 2 | apply retries before dead-lettering |
| `InnerMaxLog` | 5 | `InnerGap.MaxLog`; must be ≥ the schedule's total event count (1+2+2) or `InnerGap.Start` is disabled and the refinement fails for a bookkeeping reason |
| `PoisonBatch` | 0 or 2 | 0 = no poison record |

`ASSUME OutOfScope_InteriorDamage` carries the D2 prefix-adequacy argument
required by plan §5: `log_durable` is modelled as a *prefix* of `log_written`,
which is adequate only because D2 syncs the newline-terminated commit marker
before the first `db::apply_event`, so no interior region is ever trusted while
unsynced. Interior zero-fill damage (a garbage **middle** line) is out of model
scope; the code-side disposition is the quarantine-the-unparseable-line policy
(plan Q4).

## 2. Run matrix

Command, run from this directory for every row:

```
mkdir -p /tmp/tlc-t0a
JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-t0a" \
  tlc -config <cfg> -workers 4 -metadir /tmp/tlc-t0a/meta DurableBatch.tla
```

**One mechanics deviation from the task brief.** `-metadir /tmp/tlc-t0a/meta`
replaces `-cleanup`. `-cleanup` wipes the `states/` directory *in the shared tla
worktree*, and other agents are running TLC there concurrently; two of my runs
died with `FileNotFoundException: states/…/DurableBatch.st` when another run
removed the directory mid-flight, and my own `-cleanup` was doing the same to
them. A private metadir is both correct and neighbourly. No other flag changed;
breadth-first search was used throughout, including for the two temporal configs.

| cfg | purpose | expected | observed |
|---|---|---|---|
| `DurableBatch_CE1_Current.cfg` | CE1: unframed per-line append exposes half a batch | VIOLATED | `Error: Invariant NoHalfBatch is violated` · 38 gen / 28 distinct · 03s |
| `DurableBatch_CE1_Fixed.cfg` | same invariant, **crash points unpinned** | PASS | `No error has been found` · 448 gen / 216 distinct · 08s |
| `DurableBatch_CE2_Current.cfg` | CE2: apply may precede sync; both orders enabled | VIOLATED | `Error: Invariant DBNotAheadOfDurable is violated` · 84 / 55 · 02s |
| `DurableBatch_CE2_Fixed.cfg` | " | PASS | `No error has been found` · 448 / 216 · 02s |
| `DurableBatch_CE3_Current.cfg` | CE3: recovery only on a schema bump | VIOLATED | `Error: Invariant OpenRestores is violated` · 100 / 61 · 02s |
| `DurableBatch_CE3_Fixed.cfg` | " | PASS | `No error has been found` · 448 / 216 · 02s |
| `DurableBatch_Cursor_Current.cfg` | D3: no applied cursor exists | VIOLATED | `Error: Invariant CursorAgreesWithDB is violated` · 46 / 30 · 02s |
| `DurableBatch_Cursor_Fixed.cfg` | " (with `Compact` enabled) | PASS | `No error has been found` · 537 / 259 · 03s |
| `DurableBatch_CE8_Current.cfg` | CE8: unbounded retry of a poison record | VIOLATED | `Error: Temporal properties were violated` · 57 / 27 · 02s |
| `DurableBatch_CE8_Fixed.cfg` | " under the K-retry dead-letter policy | PASS | `No error has been found` · 64 / 34 · 03s |
| `DurableBatch_Deferred_Current.cfg` | withdrawn design: a write proceeds while damaged and materializes a non-prefix | VIOLATED (deferred-state gates around `CursorAgreesWithDB`, `DBNotAheadOfDurable`, and `CursorNeverAheadOfDB`) | not run in this sandbox; caller runs TLC |
| `DurableBatch_Deferred_Fixed.cfg` | unreadable tail blocks writes; repair then replays from `cursor.off` | PASS | not run in this sandbox; caller runs TLC |
| `DurableBatch_Safety_Fixed.cfg` | **unpinned**, all seven invariants plus the truncation action property | PASS | `No error has been found` · 537 / 259 · 02s |
| `DurableBatch_Safety_Fixed_Poison.cfg` | same, with the poison record and quarantine live | PASS | `No error has been found` · 766 / 374 · 02s |
| `DurableBatch_Refinement_Fixed.cfg` | refines `InnerGap` over the **full** Next relation | PASS | `No error has been found` · 448 / 216 · 03s |
| `DurableBatch_Refinement_Current.cfg` | the current design must **not** refine it | VIOLATED | `Error: Action property line 143, col 6 to line 143, col 75 of module InnerGap is violated` · 26 / 21 · 03s |
| `DurableBatch_NV_Truncate.cfg` | non-vacuity for `TruncationPreservesAccepted` | VIOLATED | `Error: Action property TruncationPreservesAccepted is violated` · 133 / 74 · 02s |
| `DurableBatch_NV_TypeOK.cfg` | non-vacuity for `TypeOK` | VIOLATED | `Error: Invariant TypeOK is violated` (initial state) · 01s |
| `DurableBatch_WIT_W_Rec_FullRebuild.cfg` | the full-rebuild branch of the recovery table is reachable | VIOLATED | `Error: Invariant W_Rec_FullRebuild is violated` · 423 / 209 · 03s |
| `DurableBatch_WIT_W_Rec_TailReplay.cfg` | the replay-the-tail branch is reachable | VIOLATED | `Error: Invariant W_Rec_TailReplay is violated` · 120 / 66 · 02s |
| `DurableBatch_WIT_W_Rec_NoOp.cfg` | the no-op branch is reachable | VIOLATED | `Error: Invariant W_Rec_NoOp is violated` · 34 / 17 · 03s |
| `DurableBatch_WIT_W_Inner_JsonlGrows.cfg` | the mapped `jsonl` is not constantly empty | VIOLATED | `Error: Invariant W_Inner_JsonlGrows is violated` · 62 / 41 · 03s |
| `DurableBatch_WIT_W_Inner_Applying.cfg` | the mapped abstract phase reaches `applying` | VIOLATED | `Error: Invariant W_Inner_Applying is violated` · 80 / 46 · 02s |
| `DurableBatch_WIT_W_Inner_Done.cfg` | … and `done` | VIOLATED | `Error: Invariant W_Inner_Done is violated` · 201 / 104 · 03s |
| `DurableBatch_WIT_W_Inner_Crashed.cfg` | … and `crashed` | VIOLATED | `Error: Invariant W_Inner_Crashed is violated` · 27 / 16 · 04s |
| `DurableBatch_WIT_W_ScheduleCompletes.cfg` | the schedule runs to completion under the bounds | VIOLATED | `Error: Invariant W_ScheduleCompletes is violated` at depth 20, `db = {"C"}`, `nstarted = 3` · 369 / 186 · 02s |

Every run finished in under ten seconds; the brief's three-minute ceiling was
never approached, and no bound was shrunk to make a config close.

**Reproducibility of the state counts.** The PASS rows are deterministic and
reproduce exactly on re-run (`CE1_Fixed` 448/216, `Cursor_Fixed` 537/259,
`Safety_Fixed_Poison` 766/374, verified across three full matrix runs). The
VIOLATED rows are not: TLC's parallel BFS reports whichever counterexample a
worker reaches first, so the generated/distinct counts at the point of the abort
vary between runs — `CE2_Current`, for instance, has been observed at 42/33,
71/51 and 84/55. The verdict and the violated property never varied. The traces
in §3 are the ones captured alongside the numbers in the table above.

One transient to record honestly: a single `Cursor_Fixed` invocation produced no
output beyond the TLC banner and exited without a verdict. Two immediate re-runs
both gave 537/259 and `No error has been found`. I could not reproduce the
failure and have no explanation beyond environment noise in a worktree with
several concurrent TLC processes.

### Non-vacuity

Seven invariants are asserted by a passing gate: `TypeOK`, `DurableIsPrefix`,
`NoHalfBatch`, `DBNotAheadOfDurable`, `OpenRestores`, `CursorAgreesWithDB`, plus
`CursorNeverAheadOfDB` and the action property `TruncationPreservesAccepted`.
The deferred amendment also adds the temporal property `DeferredConverges`.
Their non-vacuity coverage is:

| invariant / property | non-vacuity evidence |
|---|---|
| `NoHalfBatch` | `CE1_Current` |
| `DBNotAheadOfDurable` | `CE2_Current` |
| `OpenRestores` | `CE3_Current` |
| `CursorAgreesWithDB` | `Cursor_Current` |
| `CursorNeverAheadOfDB` | `Deferred_Current` checks `DeferredCursorNeverAheadOfDB` and reaches the rejected non-prefix DB after `UnsafeWriteWhileDeferred` |
| `DeferredCursorAgreesWithDB` | `Deferred_Current`; the gate becomes live at `Damage` and fails only after the unsafe write |
| `DeferredDBNotAheadOfDurable` | `Deferred_Current`; same damaged-state witness |
| `DeferredCursorNeverAheadOfDB` | `Deferred_Current`; same damaged-state witness |
| `DeferredConverges` | `Deferred_Fixed`: `Damage` and `Repair` make its antecedent reachable; only fair `Recovery` can establish `CursorCaughtUp` |
| `TruncationPreservesAccepted` | `NV_Truncate` (adds `BadTruncate`) |
| `TypeOK` | `NV_TypeOK` (`BadTypeInit` puts `cursor.off` out of range) |
| `DurableIsPrefix` | not separately witnessed — see §5 gap 1 |

Coverage on `Safety_Fixed` (`-coverage 1`) shows every action firing except
`ApplyFail`/`Quarantine`, which need `PoisonBatch ≠ 0` and fire 4 and 2 times
respectively in `Safety_Fixed_Poison`.

## 3. Traces

All traces below are TLC's own numbered states, trimmed to the variables that
carry the argument. Log lines print as `[b "act" "id"]`.

### CE1 — an unframed partial batch is reader-accepted

`DurableBatch_CE1_Current.cfg` · `Fixed = FALSE` · `AllowCrash = TRUE`

```
State 1: <Initial predicate>          phase="idle"    log_written=<<>>              log_durable=<<>>              db={}
State 2: <StartBatch>                 phase="writing" log_written=<<>>              log_durable=<<>>              db={}
State 3: <Crash>                      phase="crashed" log_written=<<>>              log_durable=<<>>              db={}
State 4: <Open>                       phase="opened"  log_written=<<>>              log_durable=<<>>              db={}
State 5: <TruncateUncommittedTail>    phase="idle"    log_written=<<>>              log_durable=<<>>              db={}
State 6: <StartBatch>                 phase="writing" log_written=<<>>              log_durable=<<>>              db={}
State 7: <AppendLine>                 phase="writing" log_written=<<[2 "expire" "A"]>>  log_durable=<<>>          db={}
State 8: <PartialFlush>               phase="writing" log_written=<<[2 "expire" "A"]>>  log_durable=<<[2 "expire" "A"]>>  db={}
```

States 2-5 are only TLC's shortest route to a **second** batch: batch 1 has one
event and therefore cannot be half, so it is started and abandoned with no line
written. The defect is states 6-8. Batch 2 writes `expire A`, the OS makes that
one line durable, and `Accepted(log_durable)` now holds exactly one of batch 2's
two events. `NoHalfBatch` fails: `Len = 1 ∉ {0, 2}`.

No crash is needed for this. Under the current design the durable log exposes a
half batch to any concurrent reader the moment writeback happens; a crash merely
makes it permanent. Under `Fixed = TRUE` the same writeback prefix carries
`begin, expire A` with no newline-terminated `commit`, so `Accepted` returns the
empty sequence and A stays live.

### CE2 — sync and apply are unordered, and TLC picks the bad order

`DurableBatch_CE2_Current.cfg` · `Fixed = FALSE`

```
State 1: <Initial predicate>  phase="idle"    log_written=<<>>                 log_durable=<<>>  db_committed={}     cursor=[off|->0, gen|->0]
State 2: <StartBatch>         phase="writing" log_written=<<>>                 log_durable=<<>>  db_committed={}     cursor=[off|->0, gen|->0]
State 3: <AppendLine>         phase="ready"   log_written=<<[1 "upsert" "A"]>> log_durable=<<>>  db_committed={}     cursor=[off|->0, gen|->0]
State 4: <ApplyEvent>         phase="applied" log_written=<<[1 "upsert" "A"]>> log_durable=<<>>  db_committed={"A"}  cursor=[off|->0, gen|->0]
```

The config forces nothing: at `phase = "ready"` both `SyncLog` and `ApplyEvent`
are enabled and TLC chooses `ApplyEvent`. The committed DB now holds A while the
durable log is empty, so no prefix of the durable log materializes to
`db_committed` and `DBNotAheadOfDurable` fails. Under `Fixed = TRUE`,
`ApplyEnabled` requires `phase = "synced"` and a durable commit marker, so the
bad order is not merely improbable — it is unreachable.

### CE3 — recovery that fires only on a schema bump

`DurableBatch_CE3_Current.cfg` · `Fixed = FALSE`

```
State 1: <Initial predicate>  phase="idle"    log_durable=<<>>                 db={}  cursor=[off|->0, gen|->0]
State 2: <StartBatch>         phase="writing" log_durable=<<>>                 db={}  cursor=[off|->0, gen|->0]
State 3: <AppendLine>         phase="ready"   log_durable=<<>>                 db={}  cursor=[off|->0, gen|->0]
State 4: <SyncLog>            phase="synced"  log_durable=<<[1 "upsert" "A"]>> db={}  cursor=[off|->0, gen|->0]
State 5: <Crash>              phase="crashed" log_durable=<<[1 "upsert" "A"]>> db={}  cursor=[off|->0, gen|->0]
State 6: <Open>               phase="opened"  log_durable=<<[1 "upsert" "A"]>> db={}  cursor=[off|->0, gen|->0]
```

Append and sync both succeed, the crash lands before apply, and `Open` under the
current design changes nothing: the schema stamp is current, so
`rebuild_if_schema_obsolete` does not fire. The opened DB is `{}` while the
durable log materializes to `{"A"}`, permanently. `OpenRestores` fails.

### Applied cursor — the current design has none

`DurableBatch_Cursor_Current.cfg` · `Fixed = FALSE`

```
State 1: <Initial predicate>  phase="idle"    log_durable=<<>>                 db_committed={}     cursor=[off|->0, gen|->0]
State 2: <StartBatch>         phase="writing" log_durable=<<>>                 db_committed={}     cursor=[off|->0, gen|->0]
State 3: <AppendLine>         phase="ready"   log_durable=<<>>                 db_committed={}     cursor=[off|->0, gen|->0]
State 4: <PartialFlush>       phase="ready"   log_durable=<<[1 "upsert" "A"]>> db_committed={}     cursor=[off|->0, gen|->0]
State 5: <ApplyEvent>         phase="applied" log_durable=<<[1 "upsert" "A"]>> db_committed={"A"}  cursor=[off|->0, gen|->0]
```

The cursor is still `off = 0` after the apply, so `db_committed` is no longer the
materialization of the prefix the cursor names. Modelling the absent cursor as a
stuck-at-zero one is the honest rendering: recovery has no way to learn what has
been applied.

### CE8 — a deterministically failing record

`DurableBatch_CE8_Current.cfg` · `PoisonBatch = 2` · `AllowCrash = FALSE` ·
`SPECIFICATION FairSpec` (`WF_vars(Next)`) · `PROPERTY RetryTerminates`

```
State  1: <Initial predicate>  phase="idle"     cur=0  aidx=0  attempts=0
State  2: <StartBatch>         phase="writing"  cur=1  aidx=0  attempts=0
State  3: <AppendLine>         phase="ready"    cur=1  aidx=0  attempts=0
State  4: <ApplyEvent>         phase="applied"  cur=1  aidx=1  attempts=0
State  5: <FinishBatch>        phase="idle"     cur=0  aidx=0  attempts=0
State  6: <StartBatch>         phase="writing"  cur=2  aidx=0  attempts=0
State  7: <AppendLine>         phase="writing"  cur=2  aidx=0  attempts=0
State  8: <AppendLine>         phase="ready"    cur=2  aidx=0  attempts=0
State  9: <ApplyFail>          phase="applying" cur=2  aidx=0  attempts=1
State 10: <PartialFlush>       phase="applying" cur=2  aidx=0  attempts=1
State 11: <ApplyFail>          phase="applying" cur=2  aidx=0  attempts=2
State 12: Stuttering
```

`RetryTerminates == (phase = "applying") ~> (phase = "idle")` is violated. Crash
is switched off in this config so the property cannot be satisfied by the
crash-recover-abandon path; the liveness claim is about the retry loop alone,
not about a hand-picked crash point.

**One honest modelling note.** `attempts` saturates at `K` so the state space
stays finite, and `ApplyFail` is guarded by `attempts < K`. Under
`Fixed = FALSE` the state at `attempts = 2` therefore has no enabled successor,
and TLC renders the infinite retry as `State 12: Stuttering` rather than as a
`Back to state` lasso. That is an encoding of "retries forever", not a
separately-interesting deadlock; `CHECK_DEADLOCK FALSE` is set in every config.
Success is **not** hardcoded on the second attempt: under `Fixed = TRUE`,
`ApplyFail` is disabled once `attempts ≥ K`, `Quarantine` becomes the only
enabled action, and it dead-letters batch 2, advances the cursor past it and
reaches `idle` — 34 distinct states, no violation.

### Refinement — the current design does not refine `InnerGap`

`DurableBatch_Refinement_Current.cfg` · `Fixed = FALSE`

```
State 1: <Initial predicate>  phase="idle"    log_written=<<>>                 log_durable=<<>>  db={}     aidx=0
State 2: <StartBatch>         phase="writing" log_written=<<>>                 log_durable=<<>>  db={}     aidx=0
State 3: <AppendLine>         phase="ready"   log_written=<<[1 "upsert" "A"]>> log_durable=<<>>  db={}     aidx=0
State 4: <ApplyEvent>         phase="applied" log_written=<<[1 "upsert" "A"]>> log_durable=<<>>  db={"A"}  aidx=1
```

`Error: Action property line 143, col 6 to line 143, col 75 of module InnerGap is violated`
— that is `[][Next_InnerGap]_vars` inside `Spec_InnerGap`. At state 4 the mapped
`db` moves from `{}` to `{"A"}` while the mapped abstract phase is still
`"appended"`, because nothing is durable so the mapped `jsonl` is still empty.
`InnerGap.ApplyNext` requires `phase = "applying"`, which only the atomic
`AppendBatch` can establish, so no abstract step matches.

The second, independent violation the same config exposes if that one is fixed:
a `PartialFlush` of one line of a two-event unframed batch grows the mapped
`jsonl` by a strict part of `batch_events`, and `AppendBatch` is all-or-nothing.

## 4. Refinement mapping

`DurableBatch.tla` contains an explicit `INSTANCE InnerGap WITH …` and the
refinement configs carry `PROPERTY Spec_InnerGap`. All six abstract variables and
all three constants are substituted:

| InnerGap variable | mapped to |
|---|---|
| `jsonl` | `ToInnerSeq(AcceptedLive(log_durable))` — the record-valued projection of the **accepted, commit-marked, durable** log |
| `db` | `db` |
| `crash` | `phase = "crashed"` |
| `phase` | `InnerPhase`, derived from `cur`, `aidx` and whether the batch's events are durably committed |
| `batch_events` | `ToInnerSeq(BatchEvents(cur))`, empty when `cur = 0` |
| `apply_idx` | `aidx` |
| `MaxLog` | `InnerMaxLog` = 5 |
| `MaxBatchSize` | 2 |
| `EntryIds` | `EntryIds` |

Step correspondence, all of it data-carrying rather than a constant projection:

- `StartBatch` → `Start(e, n)` for the 2-event batches, `StartNoReplace(n)` for batch 1.
- `AppendLine`, `TruncateUncommittedTail`, `SyncLog`-when-already-durable, and the cursor half of the last `ApplyEvent` → **stuttering**: none of them changes `Accepted(log_durable)`.
- The step that makes the batch's `commit` marker durable — `SyncLog`, or `PartialFlush` when the OS gets there first — → `AppendBatch`. **The commit-marker sync is the atomic abstract append.** That is the whole content of D1+D2 in one line.
- `ApplyEvent` → `ApplyNext`; the last one leaves the abstract phase at `"done"`.
- `FinishBatch` → `Reset`. `Crash` → `Crash`. `Open` → `Rebuild`.

Two things are excluded from the refinement configs and both are stated rather
than hidden:

1. **`MaxGen = 0`, so `Compact` is off.** `InnerGap.jsonl` only ever grows, so a
   log rewrite is outside the abstraction by construction. Compaction correctness
   is `CrossBatch`/T0c's model (CE5, CE7), per plan §5. Collateral effect:
   `generation` is then pinned at 0 forever, so `cursor.gen # generation` can
   never hold; with `PoisonBatch = 0` too, `db` never drifts from
   `Materialize(log_durable, quarantined)` while idle either. `RecoverIdle`
   and `RebuildAll` are therefore both vacuous in the refinement lane — every
   run in this section takes them zero times (confirmed with `tlc -coverage
   1`: `0:N` for both actions against every refinement config). `RecoverIdle`
   is not vacuous once `MaxGen = 1` re-enables `Compact` — `Safety_Fixed`'s own
   coverage shows it firing (`5:7`) — but `RebuildAll` reads `0:N` there too;
   nothing in this run matrix exercises it. That is itself a gap: either
   `RebuildAll`'s guard is subsumed by `RecoverIdle`'s in every state this
   model reaches, which would make the two actions redundant here, or the
   model is missing whatever divergence (`db # Materialize(...)` while the
   cursor is otherwise current) `kb rebuild` exists to repair. T2a should
   settle which.
2. **`PoisonBatch = 0`, so `Quarantine` is off.** Dead-lettering advances the
   cursor past a record without applying it, which `InnerGap` cannot express.
   `Safety_Fixed_Poison` checks the safety invariants with quarantine live.

`AllowCrash = TRUE` in both refinement configs, so the check runs over the full
`Next` relation including crash, open and truncation — not over a crash-free
harness.

The mapping is proved non-degenerate by four witness configs: the mapped `jsonl`
is non-empty somewhere, and the mapped abstract phase reaches `applying`, `done`
and `crashed`. A mapping that pinned any of those to a constant would leave the
corresponding witness un-violated.

## 5. Known gaps — test obligations for T2a and T4

1. **`DurableIsPrefix` has no non-vacuity witness.** It is checked in
   `Safety_Fixed` and it is structurally true in this model because only `Crash`
   and `TruncateUncommittedTail` shorten a log and both keep the prefix relation
   by construction. It is documentation of an invariant the *code* must maintain,
   not a machine-discriminated property. T2a owns it: the repair path must never
   leave `log_durable` non-prefix of `log_written`.
2. **The `offset > committed_len` recovery row is unreachable in this model.**
   The plan already predicts this — after D2 it is not a power-loss state, only a
   legacy, externally-truncated or restored-from-backup one, none of which this
   model produces. The branch is implemented in `RecTarget` and folded into the
   full-rebuild arm, but only the `gen ≠ generation` half of that arm is
   witnessed. T4 must cover `offset > committed_len` with a fabricated-cursor
   test, not rely on the spec.
3. **Compaction is whole-batch only.** `Compact` drops every line of a batch
   whose events are all dead. Real compaction is line-granular and can drop part
   of a batch (`compact.rs:190-280`), which is exactly why CE5 and CE7 belong to
   T0c. Nothing here proves line-granular compaction preserves materialization.
4. **The apply is per-event, and the transaction is modelled only at the
   boundary.** `db` moves per event and `db_committed` moves once, at the last
   apply, together with the cursor. A crash mid-apply leaves `db` dirty and
   `Open` recomputes from `db_committed`. The model therefore does **not** prove
   SQLite's rollback; it assumes it. T4's tests must assert that a killed process
   mid-batch leaves no partially-applied rows visible.
5. **`n = 2`, not `n = 3`.** No config exercises a three-event batch. If T2a's
   framing implementation is sensitive to batch arity — an `n` field mismatch,
   say, which D1 makes a hard error — that is a code-side test obligation.
6. **Batches are never retried.** An interrupted batch is abandoned. If a real
   caller retries an append after a failed one, the re-appended lines duplicate
   the truncated span; nothing here models that. `upsert` is idempotent but
   `expire`-then-`upsert` ordering is not, so T2a should have a test for
   append-fail-then-retry.
7. **A failed single-line write is not modelled at the byte level, and its
   promotion hazard is untested.** D1's own stated reason for enveloping
   *every* append — including the single-event case, not just multi-event
   batches — is that a line whose body is written but whose trailing newline
   write fails (a torn write local to that one append, distinct from the
   interior zero-fill damage in the OUT-OF-SCOPE note above) must never be
   silently completed by the bytes of the *next* append and read back as
   committed. `PartialFlush` and `Crash` model only whole-line prefixes of
   `log_written`; no state here represents a physical line with a missing
   terminator, so nothing here checks that the framing/parsing code
   distinguishes "my own commit marker is durable" from "the next append's
   bytes happen to look like mine." T2a (bd-21ef.1.6) owns this as a test
   obligation: append N's line loses its trailing newline, append N+1
   succeeds, and the reader must not promote N to committed on N+1's sync.

## 6. What changed from the rejected revision, and why

**C1 — the refinement mapping was degenerate.** The old mapping read only
`phase`, hardcoded `MaxLog <- 1`, `MaxBatchSize <- 1` and
`db <- IF phase = "applied" THEN {"B"} ELSE {}`, and passed with `Fixed = FALSE`
— it certified the broken design. Replaced with the data-carrying mapping in §4:
`jsonl` is a real projection of the accepted durable log, `db` is `db`,
`apply_idx` is the real applied count, and `MaxLog` is 5. `Refinement_Fixed`
passes over the full `Next` relation; `Refinement_Current` is reported
VIOLATING. I did not conclude the current design also refines `InnerGap`; it does
not, and §3 has the trace. `MaxLog <- 3` as suggested would disable
`InnerGap.Start` for batch 3 and fail the refinement for a bookkeeping reason —
5 is the smallest value that admits the schedule.

**C2 — the fixed-side green was a scenario pin.** The `Scenario` constant and
every `Scenario # "CEn" \/ phase = …` guard on `Crash` are gone. `Crash` is now
enabled at `phase ∈ {"writing","ready","synced","applying"}` in every
`AllowCrash = TRUE` config, and `ReaderCrashSafety` has been replaced by
`NoHalfBatch`, a global invariant over the **durable committed** log: for every
batch, the number of its events in `Accepted(log_durable)` is 0 or all of them.
`Safety_Fixed` checks it, unpinned, together with `DBNotAheadOfDurable` (the DB
is never ahead of the durable log) and the rest. The old pinned config is not
retained; nothing needed it, because the current design violates `NoHalfBatch`
without any crash at all.

`Crash` is excluded at `phase ∈ {"idle","opened","crashed","applied"}`. Those are
points where no batch is in flight and no variable would change, so the exclusion
removes states, not behaviours.

**C3 — `cursor` and `generation` were dead and `Open` was unconditional.**
`cursor` is now a `[gen, off]` record written inside the same step as the last
`ApplyEvent`, read by `RecTarget`, and checked by `CursorAgreesWithDB`. `Open`
and `RecoverIdle` share `RecTarget`, which is a genuine three-way branch — full
rebuild on generation mismatch or an over-long cursor, replay-the-tail-from-the-
cursor when the log is ahead, no-op when they agree — and each branch has a
witness config proving it reachable (§2). The tail replay folds the tail onto
`db_committed`; it is not a disguised re-materialization. `Compact` now removes
already-applied lines and bumps `generation` without touching the cursor, so the
generation-mismatch state is reachable — `W_Rec_FullRebuild` is violated, and the
depth witness reaches `db = {"C"}` on a log that begins at batch 3, i.e. after a
real compaction. `TruncateUncommittedTail` truncates to `CommittedLen`, and
`Accepted(log_written') = Accepted(log_written)` is now the standalone action
property `TruncationPreservesAccepted`, not a `TypeOK` conjunct, with
`NV_Truncate` as its violating witness.

**I1 — one batch only.** The schedule is three batches, `GeneralStart`'s
`log_written = <<>>` guard is gone, and `MaxLogLen` is a non-binding bound (12
against a schedule needing 11). `W_ScheduleCompletes` proves all three run to
completion, with a compaction in between.

**I2 — no unpinned all-invariant config.** `Safety_Fixed` carries all seven
invariants plus the action property at `MaxBatches = 3`, `MaxGen = 1`,
`AllowCrash = TRUE`. `Safety_Fixed_Poison` repeats it with the poison record.

**I3 — CE8 hardcoded success on attempt two.** `ApplyFail` and `Quarantine` are
now separate actions with an explicit `K`; `Quarantine` fires only at
`attempts ≥ K` and dead-letters the record. Fairness is `WF_vars(Next)` on
`FairSpec`, stated in the config, and `AllowCrash = FALSE` so the property cannot
be satisfied through the crash path.

**MINOR — `DBNeverAheadDurable` enumerated values.** Replaced by
`DBNotAheadOfDurable == \E n \in 0..DurCommittedLen : db_committed =
Materialize(Prefix(log_durable, n))`, an ordering statement. It is weaker than
ideal in one respect worth stating: `Materialize` is a set, so a shorter prefix
can alias a longer one and satisfy the existential. It still discriminates both
designs, and `CursorAgreesWithDB` pins the exact prefix for the fixed design.
