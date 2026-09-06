# RebuildProtocol counterexamples — C1/T0b (`bd-21ef.1.4`)

Recorded 2026-09-04, revised same day after the review fixup pass below.
`RebuildProtocol.tla` models the complete rebuild: a Phase-1 boundary,
unlocked Phase-2 replay with concurrent writer appends, Phase-3 positional
catch-up, the WAL-mode transition before the swap, the two swap orders across
all six named kill points (`KP_PRE_CHECKPOINT`, `KP_POST_CHECKPOINT`,
`KP_POST_TMP_SYNC`, `KP_POST_RENAME`, `KP_POST_UNLINK`, `KP_POST_DIR_SYNC`),
and a post-restart `Reopen` action covering both a killed restart and a
clean-finish restart.

## Bounds and tooling

All configurations use `MaxLogLen = 4` and `MaxConcurrentAppends = 2`. This
remains the minimum complete CE6 envelope: `begin, A, B, commit`, with `B` and
`commit` appended while Phase 2 is unlocked. Removing the CE6 replay-order pin
(finding 3 below) only adds interleavings of those same four lines — it never
introduces a new line or a new append — so this bound did not change. `NV_TypeOK`
additionally overrides `MaxLogLen = 0` specifically to force a deliberate
violation at the initial state; that override is local to that one config.
Every config says `CHECK_DEADLOCK FALSE`.

Tool: TLC 2.19 (rev 5a47802), Oracle JDK 1.8.0_504, run from this session as

```text
cd .state/agent-kb/tla
JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-t0b" tlc -config <cfg> -workers 4 -cleanup RebuildProtocol.tla
```

Plain breadth-first TLC ran cleanly in this session (no RMI-bind failure was
observed this time), so no `-dfid` iterative-deepening fallback was needed for
any of the runs below. All seven runs completed in one second each.

## What changed, per review finding

1. **`BatchAtomic` dropped the `killed` conjunct** (`RebuildProtocol.tla:247-251`).
   `Kill` (`:203-208`) is only enabled while `phase \in KillPoints`, and
   `"done"` is not a `KillPoint`, so the invariant's old `\union {"done"}` arm
   required `killed` at a phase where `killed` can never become `TRUE` — dead
   code. The invariant is now `(Scenario = "CE6" /\ (phase \in KillPoints \/
   phase = "done")) => (("A" \in tmp_db) = ("B" \in tmp_db))`, checked at every
   phase from `KP_PRE_CHECKPOINT` through `done`, crash or not. CE6-current
   still violates (now caught two states earlier — see below); CE6-fixed still
   passes.
2. **`TypeOK` (`:58-71`) is now shipped in every Fixed config**
   (`CE4_Fixed`, `CE6_Fixed`, `WAL_Fixed`), and a new
   `RebuildProtocol_NV_TypeOK.cfg` proves it non-vacuous: `MaxLogLen = 0`
   against the CE4 `Init` (`log = <<"W">>`, `committed_len = 1`) violates
   `TypeOK` at the initial state itself, since neither `Len(log) <= MaxLogLen`
   nor `committed_len \in 0..MaxLogLen` can hold.
3. **The CE6 replay-order pin is removed** (`Phase2Replay`, `:113-119`). The
   guard used to read `(Scenario # "CE6" \/ Len(log) = MaxLogLen)`, forcing
   replay to wait for the full four-line log under CE6. That excluded every
   schedule where the rebuild's Phase-2 replay finishes while the writer's
   span is still open. The guard is now just `phase = "p2" /\ ~killed`.
   Removing a restriction only adds reachable states, so CE6-current keeps
   its original violating trace (it is in fact found sooner: state 6 instead
   of state 7) and no pinned locator config was needed for it. CE6-fixed now
   explores the previously-excluded replay-before-append schedules; it grew
   from 18 to **62** distinct states and still reports no violation. Bounds
   are unchanged (see above) — only the interleavings enumerated grew.
4. **The Phase-3 cursor write is now read.** `Phase3CatchUp` (`:120-131`) sets
   `cursor` but until this fixup no action ever consulted it. `Reopen`
   (`:221-228`) models the next process start — from a killed restart or a
   clean `"done"` finish — and computes `replayed'` from whether
   `cursor.offset = committed_len /\ cursor.generation = 0` (this module has
   no compaction/generation-bump action — that lives in `DurableBatch.tla` —
   so `generation` is instantiated as the constant `0` cursor already carries
   throughout). Two new invariants: `CursorMatchesAtDone` (`:267-268`, "the
   cursor written before the swap still describes the finished state") and
   `NoReplayOnMatchedCursor` (`:270`, "reopening after a matched cursor never
   replays" — T5b's acceptance criterion). Both hold on every config, Current
   and Fixed alike, because `Kill` can only fire from a `KillPoint`, and every
   `KillPoint` is reachable only after `Phase3CatchUp` has already set the
   cursor — so by the time `Reopen` can run, `cursor.offset` always equals the
   (by-then-frozen) `committed_len`. Reachability of both `"done"` and
   `"reopened"`, and of a killed-then-reopened path specifically, was checked
   with temporary probe invariants (not shipped) before trusting this as a
   real, non-vacuous check rather than an unreachable one.
5. **`SwappedInWalMode` now checks a transition, not a restated constant**
   (`SetTmpWalMode`, `:159-165`; `FirstNameOperation`, `:166-177`). `tmp_mode`
   used to be set once in `Init` to `"WAL"` when `Fixed` and never touched
   again — the invariant `live_db.db = "tmp" => tmp_mode = "WAL"` was
   trivially true by construction under Fixed and never explored a
   transition. `tmp_mode` now starts at `"DELETE"` unconditionally (matching
   what `rebuild.rs:379,425` actually forces today, Fixed or not), and a new
   action `SetTmpWalMode`, gated on `Fixed`, is the only way it ever becomes
   `"WAL"`. `FirstNameOperation`'s Fixed branch additionally requires
   `tmp_mode = "WAL"` before it will rename the tmp into place, so the fixed
   swap order structurally cannot name the tmp live before the transition
   fires. NV-WAL-current still violates (see below, now via
   `SecondNameOperation` since the unlink-then-rename order is unaffected);
   WAL-fixed still passes.
6. **`Reopen` also re-checks `NameResolvesCommitted` after a killed restart**
   (`:238-241`). Previously `NameResolvesCommitted` only looked at the instant
   of `Kill` itself (`phase \in KillPoints /\ killed`) — a `Kill`ed state is
   terminal, so nothing modelled what the *next* boot observes. The invariant
   now also fires when `phase = "reopened"`:
   `(Scenario = "CE4" /\ ((killed /\ phase \in KillPoints) \/ phase =
   "reopened")) => "W" \in NamedDbContents`. This was checked to be a real,
   reachable arm (not merely folded into the pre-existing one): a probe run
   against `CE4_Fixed` confirmed a `Kill` at `KP_PRE_CHECKPOINT` followed by
   `Reopen` reaches `phase = "reopened"` with `"W"` still resolvable, 6 states
   deep, distinct from the clean-finish `Reopen` path.

## Run matrix and verbatim TLC summaries

### CE4 current — `RebuildProtocol_CE4_Current.cfg`

```text
Error: Invariant NameResolvesCommitted is violated.
22 states generated, 19 distinct states found.
The depth of the complete state graph search is 10.
Finished in 01s.
```

### CE4 fixed — `RebuildProtocol_CE4_Fixed.cfg`

Invariants: `NameResolvesCommitted`, `TypeOK`, `CursorMatchesAtDone`,
`NoReplayOnMatchedCursor`.

```text
Model checking completed. No error has been found.
26 states generated, 23 distinct states found.
The depth of the complete state graph search is 11.
Finished in 01s.
```

### CE6 current — `RebuildProtocol_CE6_Current.cfg`

```text
Error: Invariant BatchAtomic is violated.
30 states generated, 24 distinct states found.
The depth of the complete state graph search is 11.
Finished in 01s.
```

### CE6 fixed — `RebuildProtocol_CE6_Fixed.cfg`

Invariants: `BatchAtomic`, `TypeOK`, `CursorMatchesAtDone`,
`NoReplayOnMatchedCursor`.

```text
Model checking completed. No error has been found.
76 states generated, 62 distinct states found.
The depth of the complete state graph search is 13.
Finished in 01s.
```

### WAL-mode non-vacuity — `RebuildProtocol_NV_WAL_Current.cfg`

```text
Error: Invariant SwappedInWalMode is violated.
14 states generated, 13 distinct states found.
The depth of the complete state graph search is 8.
Finished in 01s.
```

### WAL-mode fixed — `RebuildProtocol_WAL_Fixed.cfg`

Invariants: `SwappedInWalMode`, `TypeOK`, `CursorMatchesAtDone`,
`NoReplayOnMatchedCursor`.

```text
Model checking completed. No error has been found.
26 states generated, 23 distinct states found.
The depth of the complete state graph search is 11.
Finished in 01s.
```

### TypeOK non-vacuity — `RebuildProtocol_NV_TypeOK.cfg` (new)

```text
Error: Invariant TypeOK is violated by the initial state.
Finished in 01s.
```

The four shipped defect invariants are each non-vacuous: CE4-current violates
`NameResolvesCommitted`, CE6-current violates `BatchAtomic`, NV-WAL-current
violates `SwappedInWalMode`, and NV-TypeOK violates `TypeOK`. `CursorMatchesAtDone`
and `NoReplayOnMatchedCursor` pass on every shipped config (Current and Fixed
alike, since the Phase-3 cursor write is unconditional on `Fixed`); their
non-vacuity — that `"done"` and `"reopened"` are actually reached, on both the
clean-finish and the killed-then-reopened paths — was established with
temporary, unshipped probe invariants during this pass, not asserted.

## bd-21ef.2.22 — reembed batch writer (`writer_open`, `BatchOpen`/`BatchCommitAndClose`, `NoWriterConnAtSwap`)

Refines the module per the `reembed-tla-audit.md` §4 sketch (bd-21ef.2.19,
"keep close-time checkpoint out of the lock window"): a new `writer_open`
variable, two new actions (`BatchOpen`, `BatchCommitAndClose`), a new
`RetainedConn` CONSTANT (non-vacuity toggle mirroring `Fixed`), and a new
invariant `NoWriterConnAtSwap == phase \notin {"p1","p2"} => ~writer_open`.
Transcribed close to the audit's sketch verbatim — no extra guard was added
beyond what the sketch specified (see finding below). `RetainedConn = FALSE`
was added to the `CONSTANTS` line of every existing `.cfg` in this directory
(TLC requires every declared CONSTANT bound).

### Run matrix

| Config | Result | States (gen/distinct) | Depth | vs. recorded |
|---|---|---|---|---|
| `CE4_Fixed` + `NoWriterConnAtSwap` | No error | 26 / 23 | 11 | identical to pre-change baseline |
| `NV_RetainedConn` (new; CE6, Fixed=TRUE, RetainedConn=TRUE) | `NoWriterConnAtSwap` violated | 14 / 12 | 4 | n/a (new config) — **but see finding: not the intended witness** |
| `CE4_Current` | `NameResolvesCommitted` violated (unchanged invariant) | 14 / 13 | 8 | was 22/19/10 — found sooner, see note below |
| `WAL_Fixed` | No error | 26 / 23 | 11 | identical to pre-change baseline |
| `CE6_Fixed` | No error | 325 / 235 | 15 | **was 76/62/13 — grew substantially, see finding** |
| `NV_TypeOK` | `TypeOK` violated at Init | (single state) | 0 | identical to pre-change baseline |
| `NV_WAL_Current` | `SwappedInWalMode` violated | 13 / 12 | 8 | was 14/13/8 — one state fewer, same depth |

### Finding: `NoWriterConnAtSwap` is violated by a trace that never exercises `RetainedConn`

Per the task brief, expected outcome for `CE4_Fixed` was "no error" (confirmed:
`BatchOpen`'s bound `"W" \notin wal_frames` is never satisfied under
`Scenario = "CE4"`, whose `Init` already sets `wal_frames = {"W"}` and no
action clears it before `phase` leaves `{"p1","p2"}` — so `BatchOpen` is
structurally dead for every CE4 config, `writer_open` stays `FALSE`
throughout, and the invariant holds vacuously there).

The non-vacuity config (`NV_RetainedConn`, `Scenario = "CE6"` since CE6's
`Init` has `wal_frames = {}`, so `BatchOpen` can actually fire) does report a
violation, but **the witnessing trace never reaches `BatchCommitAndClose`**:

```text
State 1  Init            phase=p1  writer_open=FALSE
State 2  Phase1Snapshot  phase=p2  writer_open=FALSE
State 3  BatchOpen       phase=p2  writer_open=TRUE
State 4  Phase2Replay    phase=p3  writer_open=TRUE   <- violates NoWriterConnAtSwap here
```

`Phase2Replay` (the action that produces `phase' = "p3"`, the first phase
excluded from `BatchOpen`'s `{"p1","p2"}` window) has no guard on
`writer_open`, so nothing stops rebuild from advancing past Phase 2 while a
batch is still open — regardless of `RetainedConn`. Re-running the identical
`Scenario = "CE6", Fixed = TRUE` config with `RetainedConn = FALSE` (the
shipped, correct behaviour) reproduces the byte-identical 4-state trace above
and the same violation. **The invariant is violated equally by the correct
design and the rejected alternative** — `NV_RetainedConn` does not
demonstrate what item 2 of the task asked it to demonstrate (that only the
rejected alternative violates `NoWriterConnAtSwap`), because the trace that
falsifies it never involves `BatchCommitAndClose` at all.

Root cause: the audit's sketch bounds `BatchOpen` to `phase \in {"p1","p2"}`
but does not add a corresponding guard to whichever action first transitions
`phase` out of that set (`Phase2Replay`) requiring `~writer_open`. Without
that guard, the model has no mutual-exclusion between rebuild's phase
progression and an open batch — which is precisely the flock-mediated
property the invariant is meant to certify. A `~writer_open` guard added to
`Phase2Replay` (modelling that rebuild's flock acquisition blocks until a
concurrent batch releases it) is the natural fix, but per instructions this
was **not** applied — it would change the spec to make the check pass rather
than reporting the gap the sketch left.

### Finding: `CE6_Fixed`'s state count grew (76/62/13 -> 325/235/15)

`BatchOpen`/`BatchCommitAndClose` are unconditional in `Next` (no `Scenario`
guard, unlike `WriterAppend`'s `Scenario = "CE6"` restriction). Under
`Scenario = "CE4"` this is inert (see above), so `CE4_Fixed`, `CE4_Current`,
and `WAL_Fixed` reproduce their pre-change reachable-state counts exactly.
Under `Scenario = "CE6"`, `wal_frames = {}` at `Init`, so `BatchOpen` *is*
reachable, and the two new actions add genuinely new interleavings to every
CE6 config's state graph, not just the new `NV_RetainedConn` config — hence
`CE6_Fixed`'s 3x growth. `CE6_Fixed`'s own invariants (`BatchAtomic`, `TypeOK`,
`CursorMatchesAtDone`, `NoReplayOnMatchedCursor`) all still pass with no
error at the larger count, so this is a coverage-scope change, not a
regression, but it is not what "confirm they still produce their recorded
results" asked for.

### Note: violation-run counts for `CE4_Current` and `NV_WAL_Current` shifted slightly

Both still violate the same invariant as before (`NameResolvesCommitted`,
`SwappedInWalMode` respectively), but at a shallower depth / lower generated
count than the pre-change baseline (`CE4_Current`: 14/13/8 vs. 22/19/10;
`NV_WAL_Current`: 13/12/8 vs. 14/13/8). TLC's breadth-first search stops at
the first counterexample found; adding two new disjuncts to `Next` shifts
successor-enumeration order even where the new actions never fire (here,
under `Scenario = "CE4"`), so the same violation can be found sooner. This is
the same phenomenon already documented for this module (see finding 3 above:
"CE6-current... now caught two states earlier"), not a new kind of
divergence — flagged for completeness, not as a discrepancy needing a
decision.

## CE4 — unlink-before-rename loses the name's committed WAL state

The initial live name maps to `old`; its main file is `{}`, while its committed
WAL frame is `{"W"}`, so opening the name correctly exposes `{"W"}`. Phase 2
builds a self-contained tmp containing `W`. In the current order checkpoint and
verification are ineffective (`Fixed = FALSE` skips the checkpoint move and the
WAL-empty gate), close/sync completes, and unlink executes first. Trace (this
session's run, `RebuildProtocol_CE4_Current.cfg`):

```text
State 1  Init                 phase=p1  live_db=old  files=[old:{}, tmp:{}]  wal_frames={W}
State 2  Phase1Snapshot       phase=p2  snapshot_boundary=1
State 3  Phase2Replay         phase=p3  tmp_db={W}
State 4  Phase3CatchUp        phase=KP_PRE_CHECKPOINT   cursor.offset=1, tail_sha=w
State 5  Checkpoint           phase=KP_POST_CHECKPOINT  (no-op: ~Fixed)
State 6  VerifyAndClose       phase=KP_POST_TMP_SYNC     files.tmp={W}
State 7  FirstNameOperation   phase=KP_POST_UNLINK       wal_frames={}  (unlink fires first, ~Fixed)
State 8  Kill                 phase=KP_POST_UNLINK  killed=TRUE
```

At state 8, `live_db.db` still names `"old"`, `files["old"] = {}`, and
`wal_frames = {}` (already unlinked), so `NamedDbContents = {}` — missing the
committed `"W"`. `NameResolvesCommitted` is violated. In the fixed run,
`Checkpoint` moves `W` into the old main file and `VerifyAndClose` gates on the
WAL being empty before close; rename precedes unlink, so every one of the six
kill points names either the checkpointed old file or the complete tmp file —
and, per finding 6, so does the state reached by killing and then reopening.

## CE6 — raw snapshot offset splits a span

The log begins as `<<begin, A>>`, an open span with `committed_len = 0`. The
current Phase 1 records raw `snapshot_boundary = 2`. While Phase 2 is
unlocked, the concurrent writer appends `B` and then the newline-terminated
`commit`, making `committed_len = 4`. Trace (this session's run,
`RebuildProtocol_CE6_Current.cfg`):

```text
State 1  Init            phase=p1  log=<<begin,A>>       committed_len=0
State 2  Phase1Snapshot  phase=p2  snapshot_boundary=2    (raw Len(log), ~Fixed)
State 3  WriterAppend    phase=p2  log=<<begin,A,B>>      committed_len=0
State 4  WriterAppend    phase=p2  log=<<begin,A,B,commit>>  committed_len=4
State 5  Phase2Replay    phase=p3  tmp_db={}               (Prefix(log,2) is not a span)
State 6  Phase3CatchUp   phase=KP_PRE_CHECKPOINT  tmp_db={B}  cursor.offset=4, tail_sha=span
```

Snapshot replay correctly drops the incomplete two-line prefix (`tmp_db = {}`
at state 5), but Phase 3's offset reader starts at offset 2; unable to see the
earlier `begin`, it treats tail line `B` as standalone and produces
`tmp_db = {"B"}` at state 6 — already at `KP_PRE_CHECKPOINT`, which
(post-finding-1) is enough to violate `BatchAtomic` without needing a `Kill` at
all: `"A" \notin tmp_db` but `"B" \in tmp_db`. The fixed run snapshots boundary
0 and Phase 3 parses the complete envelope from that committed boundary,
applying `{A, B}` together — confirmed exhaustively across the 62 distinct
states now reachable after finding 3 removed the replay-order pin, including
schedules where replay finishes before the writer appends `commit` at all (in
those, `tmp_db` stays `{}` on both sides — an empty pair is still an equal
pair, so `BatchAtomic` holds without asserting the wrong thing).

## WAL self-heal hazard

`SwappedInWalMode` names the cross-component assumption as an invariant: once
`live_db.db = "tmp"`, `tmp_mode` must be `"WAL"`. Per finding 5, `tmp_mode` no
longer starts at a Fixed-conditioned constant — it starts at `"DELETE"`
unconditionally, matching `rebuild.rs:379,425` forcing `journal_mode=DELETE`
today regardless of the swap order, and only the new `SetTmpWalMode` action
(gated on `Fixed`) ever changes it. Trace (this session's run,
`RebuildProtocol_NV_WAL_Current.cfg`):

```text
State 1  Init                  tmp_mode=DELETE  live_db=old
State 2  Phase1Snapshot        phase=p2
State 3  Phase2Replay          phase=p3  tmp_db={W}
State 4  Phase3CatchUp         phase=KP_PRE_CHECKPOINT
State 5  Checkpoint            phase=KP_POST_CHECKPOINT
State 6  VerifyAndClose        phase=KP_POST_TMP_SYNC   files.tmp={W}
State 7  FirstNameOperation    phase=KP_POST_UNLINK     (~Fixed: unlink first, live_db unchanged)
State 8  SecondNameOperation   phase=KP_POST_RENAME     live_db=tmp   tmp_mode still DELETE
```

At state 8, `live_db.db = "tmp"` but `tmp_mode = "DELETE"` (`SetTmpWalMode`
never fires because it is gated on `Fixed`), violating `SwappedInWalMode`. The
fixed model creates/finalizes the tmp by running `SetTmpWalMode` before
`FirstNameOperation`'s Fixed branch will even rename — which now also requires
`tmp_mode = "WAL"` as an explicit guard — and passes the invariant.

## Deviations from the plan

None semantic. All bounds are unchanged from the pre-fixup module (see above);
`NV_TypeOK`'s `MaxLogLen = 0` override is local to that one deliberately-broken
config, not a change to the shared envelope. Plain breadth-first TLC ran
cleanly in this session; the earlier note about a sandboxed RMI-bind failure
forcing `-dfid` did not reproduce here, so no iterative-deepening fallback was
used for any of these seven runs.

## Remaining gaps for T5a/T5b, as test obligations

These are gaps in what this abstract model can show, to carry forward as
concrete test obligations rather than treat as closed by TLC alone:

1. **`CursorMatchesAtDone`/`NoReplayOnMatchedCursor` are structurally
   guaranteed in this model, which is a real gap, not just a strong result.**
   `WriterAppend` can only fire while `phase = "p2"`, and every `KillPoint` is
   only reachable after `Phase3CatchUp` (which is the only writer of
   `cursor`) has already fired — so nothing in this module can ever advance
   `committed_len` after `cursor` is set. T5b's real implementation has no
   such phase-gate forcing the log-writer to stop appending once Phase 3
   starts; the code-level test obligation is to prove `cursor.offset` still
   equals the log's `committed_len` at the moment of a real
   `recover_if_needed` call, with concurrent appends continuing to land after
   the Phase-3 lock releases.
2. **`generation` is a constant `0` throughout this module.** `DurableBatch.tla`
   owns compaction and the generation-bump action; this module's
   `CursorMatchesAtDone` only exercises the offset half of D3's
   `(generation, offset, tail_sha)` triple. T5b's acceptance criterion —
   "rebuild followed by an immediate `recover_if_needed` is a no-op" — is only
   fully covered once a cross-module check (or a single combined model) also
   varies `generation` across a rebuild that follows a compaction.
3. **Busy-checkpoint retry-then-abort (D4) is out of this model's scope.**
   `Checkpoint` is an unconditional single step here; T5a's acceptance
   criterion that a persistently busy checkpoint aborts cleanly and leaves the
   live DB untouched needs its own test, not a TLC invariant over this module.
4. **The `Kill \union {"current_pre_rename"}` disjunct in `Kill`'s guard is
   dead** (`:203`): no action ever sets `phase = "current_pre_rename"`, so
   this arm is vestigial from an earlier draft. It is inert (never true) and
   was left alone as out of this fixup's scope, but it is worth deleting the
   next time this module is touched so the guard reads as what is actually
   reachable.
