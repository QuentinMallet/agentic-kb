# `CompactMaterialize.tla` — T0c counterexamples (CE5, CE7)

Beads task `bd-21ef.1.5` (C1 / T0c). Recorded 2026-09-04 with `tlc` 2.19
(rev 5a47802) from `~/.nix-profile/bin/tlc` on Oracle JDK 1.8.0_504. All commands
run from `/home/urist/Documents/perso/agentic-kb/.state/agent-kb/tla`.

The module owns the compaction / materialization half of C1: CE5 (the
`run_history` positional cap, Critical 4) and CE7 (a compacted log that still
validates against an `(offset, tail_sha)` cursor, closed by D3's generation
counter). Durability framing is `DurableBatch.tla` (T0a); the three-phase rebuild
is `RebuildProtocol.tla` (T0b). `CrossBatch.tla`'s disposition is recorded
separately in `decisions/crossbatch-disposition.md`.

## Bounds

Stated up front, and none of them was shrunk to make a config close.

| Bound | Value | Why |
|---|---|---|
| `MaxLog` | 4 | Total events appended over the whole behaviour, not the log length. Compaction shortens the log, so a `Len(log) < MaxLog` guard alone lets a model append/compact/append forever and leaves `gen` unbounded — the first run of `CE7_Fixed` reached depth 25 and 1.8 M distinct states before this was fixed. |
| `Cap` | 2 | Stands in for `RUN_HISTORY_CAP = 500` (`compact.rs:16`). Nothing depends on the magnitude, only on the cap existing. CE5 needs `Cap + 1 = 3` run events. |
| `EntryIds` | `{eA, eB}` (`{}` for CE5) | Two ids are enough for last-writer-wins plus one expire. |
| `RunIds` | `{r1, r2, r3}` (`{}` for CE7) | Exactly `Cap + 1`. |
| run row counts | `0..2*MaxLog` | Each run id can be inserted once per append and once per replay. |
| `Fixed` | `FALSE` / `TRUE` | Selects current vs. D3/D5 design; see the table in the module header. |
| `SkewEnabled` | `FALSE` / `TRUE` | Selects whether the non-cursor writers exist. A state-space bound, never an interleaving constraint: TLC still explores every interleaving of the writers that remain. |

Every config sets `CHECK_DEADLOCK FALSE`. No config pins a trace; `Next` is fully
general and every counterexample below was found by TLC's breadth-first search.

## Mechanics deviation

Two deviations from the standard invocation, both forced and both recorded.

**Private `-metadir`.** The shared `states/` directory in the spec tree is written
by several agents concurrently, and `-cleanup` on it made two runs abort with
`Unable to open states/<ts>/CompactMaterialize_1.fp`. `T0-counterexample.md`
already recommends `-metadir <scratch>` for the same reason.

**`-workers 1` for the violating configs.** Parallel breadth-first search is
nondeterministic in how many states it expands before hitting a violation, and in
which witness it returns: `CE5_Current` reported 77 and then 87 distinct states on
two `-workers 4` runs, and `CE7Strict_Current` returned witnesses of depth 9 and
then 8. Recorded counterexamples have to be reproducible, so every violating
config below was measured single-threaded, and the numbers were confirmed
identical on a repeat run. Passing configs stay on `-workers 4`: they exhaust the
state space, so their totals are worker-independent.

```
mkdir -p /tmp/tlc-t0c/meta-<cfg>
cd /home/urist/Documents/perso/agentic-kb/.state/agent-kb/tla && \
  JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-t0c" \
  tlc -config CompactMaterialize_<cfg>.cfg -workers <1|4> -cleanup \
      -metadir /tmp/tlc-t0c/meta-<cfg> CompactMaterialize.tla
```

Nothing else is non-standard: breadth-first search, no simulation mode, no depth
limit, no hand-pinned trace.

## Run matrix

Every run finished in under six seconds.

| # | Config | Workers | Expected | Observed | States generated / distinct | Depth | Time |
|---|---|---|---|---|---|---|---|
| 1 | `CE5_Current` | 1 | VIOLATED | `Error: Invariant MaterializationInvariant is violated.` | 112 / 70 | 6 | 01s |
| 2 | `CE5_Fixed` | 4 | PASS | `Model checking completed. No error has been found.` | 147 / 65 | 6 | 01s |
| 3 | `CE7_Current` | 1 | VIOLATED | `Error: Invariant RecoveryConverges is violated.` | 1,818 / 1,133 | 6 | 01s |
| 4 | `CE7_Fixed` | 4 | PASS | `Model checking completed. No error has been found.` | 33,745 / 9,728 | 10 | 02s |
| 5 | `CE7Strict_Current` | 1 | VIOLATED | `Error: Invariant RecoveryConverges is violated.` | 10,804 / 4,196 | 8 | 02s |
| 6 | `CE7Strict_Fixed` | 4 | PASS | `Model checking completed. No error has been found.` | 25,609 / 7,574 | 10 | 02s |
| 7 | `CE7Deep_Current` | 1 | VIOLATED | `Error: Invariant NoDivergentCommittedCursor is violated.` | 5,764 / 3,012 | 7 | 02s |
| 8 | `CE7Deep_Fixed` | 4 | PASS | `Model checking completed. No error has been found.` | 33,745 / 9,728 | 10 | 01s |
| 9 | `Safety_Fixed` | 4 | PASS | `Model checking completed. No error has been found.` | 169,678 / 49,037 | 10 | 05s |
| 10 | `Safety_Current` | 1 | VIOLATED | `Error: Invariant CompactionPreservesMaterialization is violated.` | 527 / 397 | 4 | 01s |
| 11 | `NV_MaterializationGuard` | 1 | VIOLATED | `Error: Invariant NV_MaterializationGuard is violated.` | 17 / 17 | 3 | 01s |
| 12 | `NV_Opened` | 1 | VIOLATED | `Error: Invariant NV_Opened is violated.` | 9 / 9 | 2 | 01s |
| 13 | `NV_CompactionNonTrivial` | 1 | VIOLATED | `Error: Invariant NV_CompactionNonTrivial is violated.` | 4 / 4 | 2 | 01s |
| 14 | `NV_DBPopulated` | 1 | VIOLATED | `Error: Invariant NV_DBPopulated is violated.` | 17 / 17 | 3 | 02s |
| 15 | `NV_TypeOK` | 1 | VIOLATED | `Error: Invariant TypeOK is violated by the initial state.` | — (initial state) | 1 | 04s |
| 16 | `NV_CompactionPreservesMaterialization` | 1 | VIOLATED | `Error: Invariant CompactionPreservesMaterialization is violated.` | 527 / 397 | 4 | 04s |
| 17 | `CE7StrictNoSkew_Current` | 1 | VIOLATED | `Error: Invariant RecoveryConverges is violated.` | 8,045 / 3,035 | 8 | 07s |
| 18 | `CE7StrictNoSkew_Fixed` | 4 | PASS | `Model checking completed. No error has been found.` | 11,759 / 3,894 | 10 | 03s |

There are 18 `CompactMaterialize_*.cfg` files beside this document, one per run
in this matrix — the 14 above plus runs 15–18, added by a later `bd-21ef.1.5`
review pass (see below).

Run 9 is the gate. It checks all four invariants — `TypeOK`,
`MaterializationInvariant`, `RecoveryConverges`,
`CompactionPreservesMaterialization` — over the full alphabet (`{eA, eB}` plus
`{r1, r2, r3}`) with the non-cursor writers enabled, exhausts 49,037 distinct
states and finds no error. Run 10 is the same model with `Fixed = FALSE` and fails
on the first invariant TLC reaches.

Runs 5–8 exist because breadth-first search returns the *shallowest* witness, and
the shallowest CE7 witness has `cursor.off = 0`. They ask the two harder questions
and get the same answer; see the CE7 section.

Runs 15–18 close two gaps a review pass flagged in runs 1–14. Run 15
(`NV_TypeOK`) gives `TypeOK` its own dedicated non-vacuity witness: `BadTypeInit`
seeds `cursor.off = MaxLog + 7` at `Init` — the same pattern `DurableBatch.tla`
uses — and TLC rejects it before generating a single successor state, proving
`TypeOK` actually excludes ill-typed states rather than merely being satisfied by
the ones this model happens to produce. Run 16
(`NV_CompactionPreservesMaterialization`) isolates that one invariant with no
others in the config to race it: run 10's four-invariant config also fails on
`CompactionPreservesMaterialization` first, at the same 527/397/depth-4 numbers,
so run 16 removes any ambiguity about which invariant TLC actually reached.
Runs 17–18 turn the CE7 section's "CE7 needs no non-cursor writer" claim into
machine evidence: `CE7StrictNoSkew_Current` reruns `CE7Strict_Current` with
`SkewEnabled = FALSE`, so `ApplyUntracked` is structurally absent from `Next`
rather than merely unused by the trace, and `RecoveryConverges` still fails in
the same 8-state shape (3,035 distinct states) with zero contribution from the
excluded writers. `CE7StrictNoSkew_Fixed` is green under the same constraint.

## Non-vacuity

`AgentKbEvidence.tla` once passed over a live data-loss bug because its invariant
was vacuously true, so every invariant here ships with a config in which TLC
reaches a violation. Runs 11–14 run against the **fixed** model, so the witnesses
apply to the design that ships.

| Invariant | Witness config | What the violation proves |
|---|---|---|
| `MaterializationInvariant` | `NV_MaterializationGuard` (run 11) | The guard `cursor.gen = gen /\ cursor.off >= Len(log)` is reachable with a non-empty log, so the implication is not vacuously true. |
| `RecoveryConverges` | `NV_Opened` (run 12) | `Open` is reachable, so `opened => …` has states to constrain. |
| `CompactionPreservesMaterialization` | `NV_CompactionNonTrivial` (run 13) | Compaction genuinely rewrites some reachable log, so the equality is not an identity on the reachable set. |
| `TypeOK` | `NV_DBPopulated` (run 14) | Reachable DB states are non-empty, so `TypeOK`'s `db` conjunct constrains something. |

Runs 11–14 are all *guard-reachability* witnesses: each shows the invariant's
antecedent or its variables have real work to do, not that the invariant can
actually be made false. Run 15 (`NV_TypeOK`) is the complementary,
stronger kind for `TypeOK` — a witness that TLC reports **violated** on a
concretely ill-typed state (`cursor.off = MaxLog + 7` at `Init`), which no
guard-reachability argument can substitute for. Run 16
(`NV_CompactionPreservesMaterialization`) is the same complementary kind for
`CompactionPreservesMaterialization`, run against the **current** (unfixed)
model in isolation rather than racing three other invariants as run 10 does.

Runs 1, 3, 5, 7, 10, 15, 16 and 17 are additionally live witnesses for the
invariants they violate.

## CE5 — the `run_history` positional cap

`CE5_Current` (run 1), 6 states, entries excluded so the cap is the only mechanism
left. TLC appends three distinct run events `r1`, `r2`, `r3` (states 2–4). `Open`
then applies all three and writes the cursor at `off = 3`,
`tail = [kind |-> "run", id |-> r3]`, leaving
`db.runs = (r1 :> 1 @@ r2 :> 1 @@ r3 :> 1)` (state 5). `Compact` rewrites the log
to `<<run r2, run r3>>` — the current retention rule keeps only the newest
`Cap = 2` run events (`compact.rs:217-221`), does not bump the generation, and
touches neither the DB nor the cursor (state 6). The cursor now reads
`off = 3 >= Len(log) = 2` with a matching generation, so nothing will ever be
replayed, while `db` holds a row for `r1` that the log can no longer produce:
`MaterializationInvariant` fails. Under `CE5_Fixed` the cap is gone, the compacted
log still holds all three run events, and keyed insertion saturates each count at
one; the model exhausts 65 distinct states green.

`Safety_Current` (run 10) reaches the same defect two states earlier and states it
more directly: with `log = <<run r1, run r2, run r3>>` reachable,
`Materialize(CompactFn(log)) # Materialize(log)` — under the cap compaction is
simply not a materialization-preserving rewrite, independent of any cursor.

## CE7 — a compacted log that still validates

`CE7Strict_Current` (run 5) is the strongest witness and the one to read. TLC
appends `upsert eA` twice, and `Open` applies both, so `db = {eA}`, `applied = 2`
and the cursor is honestly committed at `off = 2` with
`tail = [kind |-> "upsert", id |-> eA]` (states 2–4). Two more appends make the log
`<<upsert eA, upsert eA, upsert eB, upsert eA>>` (states 5–6). `Compact` keeps the
last upsert per id — indices 3 and 4 — and rewrites the log to
`<<upsert eB, upsert eA>>`, bumping no generation (state 7). Both removed lines lay
in the prefix the cursor covers, yet the cursor still validates:
`off = 2 <= Len(log) = 2`, and the line now ending at offset 2 is an `upsert eA`
line, byte-identical to the one the tail hash recorded. `Open` therefore takes the
incremental path, replays `SubSeq(log, 3, 2)` — nothing — and reports success
(state 8). The DB is left at `{eA}` while `Materialize(log) = {eA, eB}`: the
`upsert eB` line that compaction moved across the cursor is never applied, and
`RecoveryConverges` fails with no error raised anywhere. `CE7Strict_Fixed` bumps
the generation on compaction, so `Open` sees `cursor.gen # gen`, takes the full
rebuild and converges; 7,574 distinct states, green.

Two things about this trace matter. It needs no crash and no non-cursor writer —
plain appends, recovery, compaction and recovery again are enough, so "route all
seven writers through the cursor helper" (T4) does not close it. `CE7Strict_Current`
runs with `SkewEnabled = TRUE`, though, so that claim rested on the trace simply
not using `ApplyUntracked`, not on the writer being structurally unavailable.
Runs 17–18 (`CE7StrictNoSkew_Current` / `_Fixed`) close that gap: with
`SkewEnabled = FALSE`, `ApplyUntracked` is not one of the disjuncts of `Next` at
all, and `RecoveryConverges` still fails in the identical 8-state shape
(3,035 distinct states, `CE7StrictNoSkew_Fixed` green at 3,894) — so "CE7 needs
no non-cursor writer" is now machine evidence, not an observation about one
trace. And it is exactly the failure D3 predicts from hashing the *tail line*
rather than the whole prefix that `rebuild.rs:548-550` already hashes: the
prefixes `<<upsert eA, upsert eA>>` and `<<upsert eB, upsert eA>>` differ, so a
whole-prefix hash would have caught it too. The generation counter is the
cheaper of the two fixes because its check is O(1) and total.

The other two CE7 configs answer the objections a reviewer would raise.
`CE7_Current` (run 3) is the shallowest witness: a non-cursor writer applies
`upsert eA` while the cursor still reads offset 0, compaction of
`<<upsert eA, expire eA>>` empties the log, and `Open` validates the offset-0
cursor and leaves `eA` live against an empty log. That is the "entries that should
be stale stay live" half of D3's sentence; it is reachable in production only
through the D7 downgrade path, which makes it the weaker shape. `CE7Deep_Current`
(run 7) asks whether a divergent state can *survive* recovery while holding a
cursor that has actually committed a line with a matching generation, and TLC
answers yes in 7 steps: from `log = <<upsert eA, upsert eB, expire eA>>` with only
`upsert eA` applied and the cursor at 0, compaction yields `<<upsert eB>>`, and
`Open` replays it onto the stale DB to reach
`cursor = [gen |-> 0, off |-> 1, tail |-> upsert eB]` with `db = {eA, eB}` against
`Materialize(log) = {eB}`. `eA` was expired in the log compaction discarded and is
now permanently live behind a valid cursor. Both configs pass under `Fixed = TRUE`.

## Findings that bind implementation

1. **The cap is the whole of Critical 4.** Removing it (D5.2) is what makes
   compaction materialization-preserving; keyed insertion (D5.1) is what makes
   replay idempotent. The model needs both, and they close different holes.
2. **CE7 does not require a crash or a non-cursor writer.** The `CE7Strict` trace
   is reachable with well-behaved tracked writers alone, so "we route all seven
   writers through the cursor helper" (T4) does not close it. Only the generation
   check does.
3. **A tail hash is not a prefix hash.** `CursorValid` accepting a compacted log
   whose line at `offset` is byte-identical is the mechanism behind CE7. If T4
   ever drops the generation column, the fallback must be a whole-prefix hash, not
   a longer tail hash.
4. **Compaction must not be treated as a no-op for the cursor.** `compact.rs`
   currently rewrites the log and touches neither the DB nor `kb_meta`. Any commit
   that adds a cursor without adding the generation bump under the same lock
   reintroduces run 5 exactly.

## Test obligations

### T3 — `run_history` stable keying (`bd-21ef.1.8`)

- **Cap removal is load-bearing, not cosmetic.** Assert `Materialize(compact(log))
  = Materialize(log)` directly: build a log with `RUN_HISTORY_CAP + 1` run events,
  compact, rebuild into a fresh DB, and compare `run_history` row-for-row against
  a rebuild from the pre-compaction log. This is run 10's invariant in Rust. A
  test that only counts rows after compaction passes under the cap and must not be
  accepted as coverage.
- **Keyed insertion is checked under replay, not under append.** Apply the same
  log N times to one DB and assert `run_history` is N-invariant. The model's fixed
  arm saturates counts at one; the code's `ON CONFLICT DO NOTHING` on `run_id` is
  the same statement, and `db.rs:946-957` is the only place it can be made true.
- **Legacy `run_id`-less events replay deterministically.** The synthetic key must
  be a function of event content plus ordinal position, so two replays of one log
  produce identical keys. Assert equality of the whole table across two replays,
  not just its cardinality.
- **`SCHEMA_VERSION` 2 → 3 must fire the migration rebuild**, otherwise a DB
  carrying pre-keying rows materializes differently from a fresh rebuild of the
  same log.
- The `run_history` FK to `test_cases` must still hold after cap removal, since
  removing the cap retains older run events whose parent test case may itself have
  been compacted away.

### T4 — applied cursor and automatic recovery (`bd-21ef.1.9`)

- **The generation bump belongs to compaction, under the same lock as the
  rename.** Failing test first: compact a log, reopen, and assert a full rebuild
  ran. Run 5 is the regression this prevents, and it is reachable without any
  crash, so a crash-injection harness will not find it.
- **Construct run 5 literally in Rust.** Append `upsert A`, `upsert A`,
  `upsert B`, `upsert A`; apply the first two through the cursor helper; compact;
  reopen. Without the generation column the reopened DB is missing `B` and no
  error is raised. With it, the reopen must rebuild and produce `{A, B}`.
- **Assert the recovery table row-by-row**, since the model exercises three of its
  rows and they are distinguishable: generation mismatch → full rebuild;
  `offset > committed_len` → full rebuild; tail-sha mismatch at a valid offset →
  full rebuild. A single "reopen converges" test cannot tell which rule fired, and
  under the current design the wrong rule fires with the right outcome in exactly
  the cases run 5 excludes.
- **Idempotence must be asserted per `apply_event` arm, enumerated.** The model
  makes double apply reachable through `Open` whenever the DB is ahead of the
  cursor (the six non-cursor writers, and any D7-skewed binary). The entry arms are
  set-shaped and survive it; `run_history` does not until T3 lands. Any arm added
  later that is neither set-shaped nor keyed reopens run 3.
- **A cursor at offset 0 against a populated DB is a real state, not a fixture
  artefact.** It is what a rebuild over an empty log plus a downgraded binary
  produces, and it is the shape of run 3. The recovery table's "no cursor rows
  present" row does not cover it, because the rows *are* present.
- **T5b's Phase-3 cursor write must include the generation**, or the first
  `recover_if_needed` after a rebuild sees a generation mismatch and rebuilds
  again — the loop T5b exists to prevent, now reachable through a second column.
