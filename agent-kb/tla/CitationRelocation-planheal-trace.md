# CitationRelocation — `PlanHeal ; <move> ; ApplyHeal` counterexample and run matrix

Companion to `CitationRelocation.tla`. Epic C3, task T0 (`bd-21ef.3.2`), part (a).
Decisions for parts (b) and (c) are in `decisions/c3-citation-relocation-t0.md`.

## What changed in the model

`EvidenceRow` gained a `path` field. Before the amendment the row carried no path at
all and relocation was modelled by changing `contentHash`, so the plan-then-commit race
was not expressible: there was nothing for a concurrent writer to move.

`Heal` was split into `PlanHeal` and `ApplyHeal`:

- `PlanHeal` is `run_stale_check` (`stale_check.rs:194`) building the relocation report
  outside the flock. It writes no row. It records a plan carrying the destination plus
  the premise the search ran against: `premisePath`, `premiseLive`, `premiseContent`.
  Only a safe search produces a plan, because only a row the report marks `Relocated`
  with a new path emits a `citation_healed` event (`stale_check.rs:233-236`).
- `ApplyHeal` is `heal_relocations` (`stale_check.rs:220`) committing under the flock
  (`:230`). It commits only if the premise still holds and otherwise discards the plan.

The constant `UnsafeApply` removes **only** the path half of the premise check. That is
the precise omission in the code: under the lock `heal_relocations` requeries
`citation_hash` (`:239`), a write-once column that cannot have changed, and never
requeries `citation_path`. Isolating that single omission is what makes the
counterexample name the real defect rather than a strawman with no premise checking.

New invariant:

```tla
NoStaleHealCommit ==
  (lastAction.kind = "ApplyHeal" /\ lastAction.committed)
    => ~lastAction.pathStale
```

## The counterexample

Reported by `CitationRelocation_UnsafeApply.cfg` (`MaxRows = 1`, `MaxCandidates = 2`,
`MaxPaths = 2`, `UnsafeApply = TRUE`) as `Invariant NoStaleHealCommit is violated`, at
search depth 4. Fields not named in a state are unchanged from the state above it.

```
State 1  <Initial predicate>
  rows[1] = [status |-> "Unverified", storedHash |-> 0, contentHash |-> 1,
             candidates |-> 1, excerptStrong |-> TRUE, path |-> 1]
  plans[1] = [kind |-> "none"]
  pass = 0

State 2  <PlanHeal>                      -- run_stale_check, outside the flock
  plans[1] = [kind |-> "plan", newPath |-> 1,
              premisePath |-> 1, premiseLive |-> TRUE, premiseContent |-> 1]
  rows[1] unchanged
  lastAction = [kind |-> "PlanHeal", row |-> 1, committed |-> TRUE,
                pathStale |-> FALSE]

State 3  <ReVerify>                      -- another writer's move becomes visible
  rows[1].path = 2                       -- 1 -> 2, committed by the other writer
  rows[1] otherwise unchanged: still Unverified, contentHash 1, candidates 1, strong
  plans[1] unchanged and now STALE
  pass = 1

State 4  <ApplyHeal>                     -- heal_relocations, under the flock
  rows[1] = [status |-> "Relocated", storedHash |-> 0, contentHash |-> 0,
             candidates |-> 1, excerptStrong |-> TRUE, path |-> 1]
  plans[1] = [kind |-> "none"]
  lastAction = [kind |-> "ApplyHeal", row |-> 1, before |-> "Unverified",
                pathStale |-> TRUE, liveStale |-> FALSE, committed |-> TRUE]
                                          -- committed /\ pathStale  =>  VIOLATION
```

In one paragraph: the row starts at path 1 with a hash mismatch and a safe relocation
verdict, so `PlanHeal` records a plan whose premise is "this row is at path 1"; a
concurrent writer then moves the row to path 2, which `ReVerify` observes, leaving the
plan stale but untouched; `ApplyHeal` re-checks the liveness, content and verdict halves
of the premise, all of which still hold, skips the path half because `UnsafeApply` is
set, and commits, writing path 1 over the path-2 move and marking the row `Relocated`.
The other writer's committed relocation is silently reverted, and the row is now
asserted `Relocated` to a destination chosen from a search that ran against a path the
row has since left.

Two notes on reading this trace. First, the minimal counterexample happens to plan an
**in-file** relocation (`newPath` equals `premisePath`), which is a real code case — the
repo walk returns the cited file itself when the excerpt moved within that file — and is
the more damaging variant, because the write looks like a no-op relocation while it is
in fact a revert of another writer's move. Second, the three-path run below confirms the
violation is not an artefact of having only two paths to choose between.

With `UnsafeApply = FALSE` the same behaviour is cut at State 4: `PathPremiseHolds` is
false, `ApplyHeal` takes the discard arm, `rows` is unchanged, `committed` is `FALSE`,
and the plan is dropped. That discard is what V3 (`bd-21ef.3.5`) must implement.

## Run matrix

`tlc` 2.19, private `-metadir` per run (see the reproduction note below).

| cfg | MaxRows | MaxCandidates | MaxPaths | UnsafeApply | result | distinct states | wall |
|---|---|---|---|---|---|---|---|
| `CitationRelocation.cfg` | 1 | 2 | 2 | FALSE | no error, 9 invariants | 23,156 | 7 s |
| `CitationRelocation_UnsafeApply.cfg` | 1 | 2 | 2 | TRUE | **VIOLATED `NoStaleHealCommit`** | 6,743 | 3 s |
| `CitationRelocation_NV_Discard.cfg` | 1 | 2 | 2 | FALSE | **VIOLATED `NoStalePlanEverDiscarded`** (intended) | 4,277 | 4 s |
| `CitationRelocation_Paths3.cfg` | 1 | 2 | 3 | FALSE | no error, 9 invariants | 104,778 | 24 s |
| `CitationRelocation_UnsafeApply_Paths3.cfg` | 1 | 2 | 3 | TRUE | **VIOLATED `NoStaleHealCommit`** | 22,270 | 5 s |
| `CitationRelocation_UnsafeApply_Rows2.cfg` | 2 | 2 | 2 | TRUE | **VIOLATED `NoStaleHealCommit`** | 553,950 | 22 s |
| `CitationRelocation_Rows2.cfg` | 2 | 1 | 2 | FALSE | no error, 9 invariants | 7,484,608 | 21 m 26 s |
| pre-amendment spec (no `path`, no `plans`) | 3 | 2 | n/a | n/a | no error, 8 invariants | 5,009,184 | 6 m 27 s |

A violating run's distinct-state count is the count at the point TLC aborted and varies by
a few percent between runs, because parallel workers reach the counterexample at different
points in the queue. The green runs' counts are exact and reproducible. Wall times were
measured with other agents' TLC runs competing for the same twelve cores. Worker counts:
two for the `MaxRows = 1` green runs, three for `CitationRelocation_Rows2.cfg`, four for
the violating runs and the pre-amendment baseline. Only `CitationRelocation_Rows2.cfg`
exceeds a three-minute budget; every cfg that gates the amendment runs in under 25 s.

Every violating run reports `NoStaleHealCommit` and nothing else, even though all nine
invariants are listed in each violating cfg. Removing the path premise breaks exactly
one property: the liveness half still blocks a commit onto a row that has become
`Verified` (`NoHealOnVerified`, `Monotonicity`), and the verdict half still blocks a
`Relocated` write when the search inputs no longer support one (`NonUniqueUnverified`,
`WeakExcerptUnverified`).

### The premise check is load-bearing, not decorative

`NoStaleHealCommit` would hold vacuously if a stale plan never actually reached
`ApplyHeal` in the safe model. `CitationRelocation_NV_Discard.cfg` rules that out. It runs
the main cfg's bounds with `UnsafeApply = FALSE` and lists one invariant, the probe

```tla
NoStalePlanEverDiscarded ==
  ~(lastAction.kind = "ApplyHeal" /\ ~lastAction.committed
    /\ lastAction.pathStale /\ ~lastAction.liveStale)
```

which TLC is expected to report **violated**, and does, at depth 4. The `~liveStale`
conjunct is what makes it sharp: it demands a discard driven by the path having moved
while the row was still live — the case `heal_relocations` cannot currently detect —
rather than one the liveness check alone would have caught. The final state of that
counterexample is

```
State 4  <ApplyHeal>
  rows[1] = [status |-> "Unverified", storedHash |-> 0, contentHash |-> 1,
             candidates |-> 0, excerptStrong |-> FALSE, path |-> 2]   -- unchanged
  plans[1] = [kind |-> "none"]                                        -- plan dropped
  lastAction = [kind |-> "ApplyHeal", row |-> 1, before |-> "Unverified",
                pathStale |-> TRUE, liveStale |-> FALSE, committed |-> FALSE]
```

So the safe model does reach the state the unsafe model commits from, and discards there.
`NoStaleHealCommit` is non-vacuously true in the main cfg.

### Why the main cfg is `MaxRows = 1`

The pre-amendment spec ran at `MaxRows = 3` in 6 m 27 s. Adding `path` multiplies the
per-row value count by `MaxPaths` and adds a per-row `plans` entry, which at
`MaxRows = 2, MaxCandidates = 2` exhausted the memory budget after 4 m 41 s with the
queue still growing.

The reduction is sound rather than merely convenient: **no action in this module reads
or writes any row other than the one it names**, and every invariant is either a
conjunct quantified independently over rows or a predicate over `lastAction`, which
names one row. There is no cross-row communication to lose. `CitationRelocation_Rows2.cfg`
re-checks that claim exhaustively at two rows with the candidate bound reduced to 1 —
7,484,608 distinct states, complete graph depth 9, no error — and
`CitationRelocation_UnsafeApply_Rows2.cfg` confirms the violation still fires at two rows
with the full candidate bound. The second row buys nothing but costs 21 minutes, which is
why it is not the main cfg.

### Bounds justification

- `MaxPaths = 2` is the minimum that lets a citation move; `MaxPaths = 3` is run as a
  robustness check on that dimension.
- `MaxCandidates = 2` covers the three cases the invariants distinguish: 0 (not found),
  1 (unique, the only relocatable verdict) and 2 (non-unique). `MaxCandidates = 1` in
  the two-row cfg still spans the 1 / not-1 boundary that `NonUniqueUnverified` names.
- `Hashes = {0, 1}` is unchanged: two values distinguish "matches the recorded hash"
  from "does not", which is the only distinction any invariant makes.
- `Passes = {0, 1}` is unchanged; `Monotonicity` and `NoSilentPromotion` are scoped to a
  single pass, so one pass boundary is enough to exercise both.

### Reproducing

Runs must use a private `-metadir`. The shared `states/` directory in this tree is
clobbered by concurrent `tlc -cleanup` runs from other agents, which fails a run with
`Error: when writing the disk (StatePoolWriter.run)` partway through.

```sh
mkdir -p /tmp/tlc-c3t0
cd .state/agent-kb/tla
for c in CitationRelocation CitationRelocation_UnsafeApply \
         CitationRelocation_NV_Discard \
         CitationRelocation_Paths3 CitationRelocation_UnsafeApply_Paths3 \
         CitationRelocation_UnsafeApply_Rows2 CitationRelocation_Rows2; do
  JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
    tlc -config "$c.cfg" -workers 4 -metadir "/tmp/tlc-c3t0/$c" CitationRelocation.tla
done
```
