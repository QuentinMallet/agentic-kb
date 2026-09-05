# CitationRelocation — `PlanHeal ; <move> ; ApplyHeal` counterexample and cfg matrix

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

Two switches expose the review cases:

- `UnsafeApply = TRUE` removes only the path half of the premise check. This isolates the
  omission at `heal_relocations`: under the lock the code overwrites `citation_path`
  without re-reading it.
- `UnsafeVerdict = TRUE` additionally removes the content and verdict halves. This models
  the fuller code path where none of path, content, or verdict is rechecked under the
  lock.

`ConcurrentHeal(row, newPath)` is a pure path-move action. It changes only `path` and
leaves `pass` unchanged, mirroring `db.rs:1055`, which overwrites only `citation_path`
and starts no verification pass. `ReVerify` remains in the model for the separate case
where a later pass re-hashes and refreshes the search evidence.

## The path-omission counterexample

Under `CitationRelocation_UnsafeApply.cfg` the expected violation is
`NoStaleHealCommit`. The shorter race now uses `ConcurrentHeal`, because the other
writer's commit need not start a new pass.

```text
State 1  <Initial predicate>
  rows[1] = [status |-> "Unverified", storedHash |-> 0, contentHash |-> 1,
             candidates |-> 1, excerptStrong |-> TRUE, path |-> 1]
  plans[1] = [kind |-> "none"]
  pass = 0

State 2  <PlanHeal>                      -- run_stale_check, outside the flock
  plans[1] = [kind |-> "plan", newPath |-> 1,
              premisePath |-> 1, premiseLive |-> TRUE, premiseContent |-> 1]
  rows[1] unchanged

State 3  <ConcurrentHeal>               -- db.rs:1055, path-only overwrite
  rows[1].path = 2
  rows[1] otherwise unchanged
  plans[1] unchanged and now STALE
  pass unchanged

State 4  <ApplyHeal>                     -- heal_relocations, under the flock
  rows[1] = [status |-> "Relocated", storedHash |-> 0, contentHash |-> 0,
             candidates |-> 1, excerptStrong |-> TRUE, path |-> 1]
  plans[1] = [kind |-> "none"]
  lastAction = [kind |-> "ApplyHeal", row |-> 1, before |-> "Unverified",
                pathStale |-> TRUE, liveStale |-> FALSE,
                contentStale |-> FALSE, verdictStale |-> FALSE,
                committed |-> TRUE]
```

The row starts at path 1 with a hash mismatch and a safe relocation verdict, so
`PlanHeal` records a plan against that snapshot. Another writer then commits a path-only
move to path 2. `ApplyHeal` reaches the lock, rechecks liveness, content, and verdict,
skips the path half because `UnsafeApply` is set, and commits the stale destination,
overwriting the other writer's move. With `UnsafeApply = FALSE` the same state is
discarded instead.

The minimal counterexample still happens to be an in-file relocation (`newPath =
premisePath`), which is a real code case when the cited excerpt moved within the same
file. A stale destination with an otherwise unchanged premise is not represented as a
separate action here; that remains a code-side obligation for V2/V3.

## The sharp discard probe

`CitationRelocation_NV_Discard.cfg` is a non-vacuity probe for the safe model. It checks
only:

```tla
NoStalePlanEverDiscarded ==
  ~(lastAction.kind = "ApplyHeal" /\ ~lastAction.committed
    /\ lastAction.pathStale /\ ~lastAction.liveStale
    /\ ~lastAction.contentStale /\ ~lastAction.verdictStale)
```

The added `~contentStale /\ ~verdictStale` conjuncts are the sharpness fix. Without
them, the exhibited discard could end with `candidates = 0` and `excerptStrong = FALSE`,
which means the search verdict went stale and the discard was not path-driven. The probe
now fires only on a discard where the path changed while liveness, content, and verdict
all still matched the recorded premise.

## Cfg matrix

**Recorded 2026-09-05.** Every `CitationRelocation*.cfg` in this directory was run to
completion, sequentially (one TLC process at a time, never two concurrently), via the
private-metadir runner (`tlc -workers 4`, TLC2 2.19 of 08 August 2024, JVM tmpdir and
`-metadir` both under `/tmp/tlc-orch`, never the checked-in tree's shared `states/`
directory). This replaces the earlier "intended" table, which recorded a plan rather than
an outcome. The runner applies a fixed `-workers 4` to every cfg, so the previously listed
per-cfg worker counts (2/3/4) were never actually honored by any run and are dropped from
this table in favor of what was actually used.

| cfg | MaxRows | MaxCandidates | MaxPaths | UnsafeApply | UnsafeVerdict | expected outcome | observed outcome | distinct states | depth | wall time |
|---|---|---|---|---|---|---|---|---|---|---|
| `CitationRelocation.cfg` | 1 | 2 | 2 | FALSE | FALSE | green: all 9 invariants hold | **green** — no error found | 26,296 | 7 | 6s |
| `CitationRelocation_NV_Discard.cfg` | 1 | 2 | 2 | FALSE | FALSE | violated: `NoStalePlanEverDiscarded` only | **violated: `NoStalePlanEverDiscarded`** | 6,716 | 4 | 2s |
| `CitationRelocation_Paths3.cfg` | 1 | 2 | 3 | FALSE | FALSE | green: all 9 invariants hold | **green** — no error found | 120,798 | 7 | 29s |
| `CitationRelocation_Rows2.cfg` | 2 | 1 | 2 | FALSE | FALSE | green: all 9 invariants hold | **green** — no error found | 9,705,472 | 11 | 16min 43s |
| `CitationRelocation_UnsafeApply.cfg` | 1 | 2 | 2 | TRUE | FALSE | violated: `NoStaleHealCommit` | **violated: `NoStaleHealCommit`** | 5,392 | 4 | 1s |
| `CitationRelocation_UnsafeApply_Isolation.cfg` | 1 | 2 | 2 | TRUE | FALSE | green: remaining 8 invariants hold | **green** — no error found | 26,380 | 7 | 2s |
| `CitationRelocation_UnsafeApply_Paths3.cfg` | 1 | 2 | 3 | TRUE | FALSE | violated: `NoStaleHealCommit` | **violated: `NoStaleHealCommit`** | 12,296 | 4 | 1s |
| `CitationRelocation_UnsafeApply_Rows2.cfg` | 2 | 2 | 2 | TRUE | FALSE | violated: `NoStaleHealCommit` | **violated: `NoStaleHealCommit`** | 583,383 | 4 | 12s |
| `CitationRelocation_UnsafeVerdict.cfg` | 1 | 2 | 2 | TRUE | TRUE | violated: `NoStaleHealCommit` and `NonUniqueUnverified` and/or `WeakExcerptUnverified` | **violated: `NonUniqueUnverified`** | 4,087 | 4 | 0s |

Every observed outcome matches its documented expected outcome. No cfg failed to parse.
See §Discrepancies below for the one case (`UnsafeVerdict`) that needs an explanatory note,
which is not a contradiction.

The isolation row is the point of `CitationRelocation_UnsafeApply_Isolation.cfg`: with
the path premise removed and `NoStaleHealCommit` omitted, the other eight invariants
stayed green, confirmed by this run. That is stronger than saying the unsafe run "happens"
to report only one failure.

## Discrepancies

None that contradict a documented expectation. One clarification:

- **`CitationRelocation_UnsafeVerdict.cfg`** — the matrix's expected-outcome cell was
  phrased as "`NoStaleHealCommit` and `NonUniqueUnverified` and/or `WeakExcerptUnverified`"
  because this cfg's invariant list (confirmed present in the `.cfg` file) includes all
  three, and TLC's breadth-first model checker reports and halts at the *first* invariant
  violation it encounters, not every invariant that would eventually fail. The recorded run
  halted on `NonUniqueUnverified` at state 4 (an `ApplyHeal` action with `contentStale` and
  `verdictStale` both `TRUE`, `committed |-> TRUE`). This is consistent with, not a
  contradiction of, the documented expectation — the "and/or" phrasing was written to
  anticipate exactly this first-violation-wins behavior. `NoStaleHealCommit` was not
  independently re-confirmed to fire in isolation under this cfg (that is what
  `CitationRelocation_UnsafeApply.cfg`, run separately above, already establishes).
- Per-cfg worker counts in the previous table version (2/3/4) were never actually used by
  any TLC invocation; the runner hard-codes `-workers 4`. Corrected in the table above by
  omitting a per-row worker column and stating the fixed value once.

## Why the main cfg is `MaxRows = 1`

The per-row argument still holds: every row action names one row, every plan is stored
per row, and every safety invariant is a per-row conjunct or a predicate over
`lastAction`, which also names one row.

One subtlety is `pass`: it is global, and `ReVerify` writes it for all rows at once. That
does not create a hidden cross-row dependency here because no guard reads `pass`; it is
used only in the invariants that scope monotonicity and promotion to one pass boundary.
The two-row cfg therefore remains a robustness check, not a source of extra behaviour.

## Bounds and the search-image note

- `MaxPaths = 2` is the minimum that lets a citation move; `MaxPaths = 3` is the path
  robustness check.
- `MaxCandidates` models the saturating map `candidates = min(actual, MaxCandidates)`.
  The only distinction the invariants need is 0 / 1 / more-than-1.
- The cited file contributes 1 to the count, not "exactly once" as an implementation
  mechanism but as part of the semantic unit of `candidates`.
- `CapExceeded` has no image in this module's state space. A capped search is outside the
  refinement map; it must not be degraded to any `candidates` value and in particular not
  to the unique in-file case.

## Reproducing on the host

Runs must use a private `-metadir`. The shared `states/` directory in this tree is
clobbered by concurrent `tlc -cleanup` runs from other agents.

```sh
mkdir -p /tmp/tlc-c3t0
cd agent-kb/tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation.cfg -workers 2 \
  -metadir /tmp/tlc-c3t0/CitationRelocation CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_NV_Discard.cfg -workers 4 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_NV_Discard CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_Paths3.cfg -workers 2 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_Paths3 CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_Rows2.cfg -workers 3 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_Rows2 CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_UnsafeApply.cfg -workers 4 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_UnsafeApply CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_UnsafeApply_Isolation.cfg -workers 4 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_UnsafeApply_Isolation CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_UnsafeApply_Paths3.cfg -workers 4 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_UnsafeApply_Paths3 CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_UnsafeApply_Rows2.cfg -workers 4 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_UnsafeApply_Rows2 CitationRelocation.tla

JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c3t0" \
  tlc -config CitationRelocation_UnsafeVerdict.cfg -workers 4 \
  -metadir /tmp/tlc-c3t0/CitationRelocation_UnsafeVerdict CitationRelocation.tla
```
