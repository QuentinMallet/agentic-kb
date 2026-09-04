# T0 — `AgentKbEvidence.tla` counterexamples (epic `evidence-storage-integrity`)

Beads task `bd-evidence-storage-integrity-w3xo.1`. Originally recorded 2026-09-01 at repo HEAD
`12f44d5`; revised the same day after T1/T2 landed and `CompactedLogE` was rewritten to model the
implemented compact algorithm, then again to close CE3, CE4 and CE5. The whole matrix now passes.

These traces are the *point* of the T0 rewrite: before it, `AgentKbEvidence.tla` passed over a
live data-loss defect because its `ApplyEventE` add arm cleared the entry's evidence set, so the
failing interleaving materialized to `{}` on both sides of `CompactionEquivalenceE`. See
`gotchas/tla/compact-spec-fidelity-gap` in the agent KB.

## Status

| Counterexample | Shape | State |
|---|---|---|
| CE1 | evidence reordered ahead of its parent upsert | **closed** by the T2 emission rule |
| CE2 | `evidence_expire` dropped, evidence resurrected | **closed** by the T1 retention arm |
| CE3 | legacy upsert re-grandfathers a de-legacied entry | **closed** by the `legacy_add` amendment + the matching `db.rs` fix |
| CE4 | legacy upsert trailing an explicit-kind upsert | **closed** by the writer-producible-alphabet guard on `DoLegacyAdd` |
| CE5 | post-expire `evidence_add` revived by compaction | **closed by contract** — the live-at-index retention bound is in the model; the matching code lands under `bd-evidence-storage-integrity-w3xo.7` |

## Tooling

`tlc` 2.19 (rev 5a47802) from `~/.nix-profile/bin/tlc`; Oracle JDK 1.8.0_504. All commands are run
from `/home/urist/Documents/perso/agentic-kb/.state/agent-kb/tla`.

## Run matrix (current module)

| # | Config | Spec | `MaxLogLen` | `EntryIds` / `EvidenceIds` | Expected | Observed |
|---|---|---|---|---|---|---|
| 1 | `AgentKbEvidence_NoCompact.cfg` | `SpecNoCompact` | 4 | `{e1,e2}` / `{v1,v2}` | PASS | PASS — 159,545 distinct states, 3 min 00 s |
| 2 | `AgentKbEvidence_CE1.cfg` | `SpecCE1` | 4 | `{e1}` / `{v1}` | PASS | PASS — 31 distinct states, < 1 s |
| 3 | `AgentKbEvidence_CE2.cfg` | `SpecCE2` | 5 | `{e1}` / `{v1}` | PASS | PASS — 192 distinct states, < 1 s |
| 4 | `AgentKbEvidence.cfg` | `Spec` | 4 | `{e1,e2}` / `{v1,v2}` | PASS | PASS — 159,545 distinct states, 2 min 47 s |

All four pass.

Run 1 is the regression gate. It exercises every action except `DoCompact` and checks
`Invariants` (`TypeInvariantE`, `OrphanTolerated`, `StatusConsistent`, `AddIgnoresClaimedStatus`,
`OrphanAddIsSoftMandate`, `AbsentEntriesClean`, `EvidenceKindRestricted`), `PartitionEquivalent`
and `CompactionEquivalenceE`. It proves the ADR-1/ADR-2 rewrite of `ApplyEventE` — including the
CE3 `legacy_add` amendment — is internally consistent, so a run-4 failure would be attributable to
compaction and not to the rewrite.

Runs 1 and 4 report the same distinct-state count (159,545). That is expected rather than a
coincidence: `DoCompact` rewrites `log` without touching the materialized state, and now that
compaction is state-preserving every compacted log it produces is a log the writer actions already
reach. It is also a useful signal — if a future change makes compaction lossy again, run 4's count
will rise above run 1's before the invariant even fails.

Both counts dropped from the pre-guard 168,421 to 159,545. That delta is exactly the CE4 guard
removing the `legacy_add`-after-`add` sequences from the reachable set; no invariant coverage was
lost with it (see CE4 below).

Runs 2 and 3 are the pre-fix counterexample harnesses, retained as named regression gates. They
now pass; a failure in either is a regression in the compaction rules, not the original defect.
Neither harness includes `DoLegacyAdd`, so the CE4 guard does not affect them — their counts are
unchanged from the pre-guard module.

Add `-metadir <scratch>` to keep TLC's `states/` output out of the tree.

```
tlc AgentKbEvidence -config AgentKbEvidence_NoCompact.cfg -workers 6 -deadlock
tlc AgentKbEvidence -config AgentKbEvidence_CE1.cfg      -workers 4 -deadlock
tlc AgentKbEvidence -config AgentKbEvidence_CE2.cfg      -workers 4 -deadlock
tlc AgentKbEvidence -config AgentKbEvidence.cfg          -workers 6 -deadlock
```

## `CompactedLogE` — now models the implemented algorithm

The T0 acceptance item required `CompactedLogE` to stop describing the pre-fix compact and start
describing the one T1/T2 landed (`src/commands/compact.rs`). It now encodes:

* **Retention.** An evidence event (`evidence_add` | `citation_healed` | `evidence_expire` — T1
  added the third arm) survives iff its parent entry is live at the end of the log, its index is
  strictly greater than `expire_last[parent]`, and its parent was **live at its own index**.
* **Emission.** Retained upserts are walked in original-index order and each live entry's retained
  evidence events are spliced in immediately after that entry's retained (last) upsert, preserving
  their relative order.

That emission rule is what closes CE1, and the third retention arm is what closes CE2.

The live-at-index bound is the CE5 fix and is the one rule here that models the TARGET algorithm
rather than the code as it stands: today `compact.rs:186-196` uses a weaker `i > entry_first`
bound. The live-at-index form subsumes it (being live at `i` implies some upsert precedes `i`), so
the first-upsert bound is not restated in the model. See CE5 below for why the weaker bound is
self-consistent today and why the two changes must land together.

## Historical runs (pre-fix module, 2026-09-01, HEAD `12f44d5`)

Recorded here for provenance; they no longer reproduce against the current module.

| # | Config | Observed then |
|---|---|---|
| 1 | `_NoCompact.cfg` | PASS — 168,421 distinct states, 4 min 09 s |
| 2 | `_CE1.cfg` | FAIL — 33 distinct states, 2 s |
| 3 | `_CE2.cfg` | FAIL — 65 distinct states, 1 s |
| 4 | `AgentKbEvidence.cfg` | FAIL — 4,902 distinct states, 2 s |

### CE1 — primary counterexample: evidence reordered ahead of its parent upsert (CLOSED)

`SpecCE1` restricts the action set to `add` / `evidence_add` / `compact` and the constants to one
entry id and one evidence id, so the shortest violating trace was unique rather than search-order
dependent. `Invariant CompactionEquivalenceE is violated`, trace of exactly **4 events** — zero
headroom at `MaxLogLen = 4`:

| Event | `evidence[e1]` after | `estatus[e1]` after | `log` |
|---|---|---|---|
| 1. `add(e1, belief)` | `{}` | `missing` | `[add]` |
| 2. `evidence_add(e1, v1@p0)` | `{v1}` | `present` | `[add, evidence_add]` |
| 3. `add(e1, belief)` | `{v1}` | `present` | `[add, evidence_add, add]` |
| 4. `compact` | `{v1}` | `present` | `[evidence_add, add]` ← **reordered** |

`DbState` still held `evidence[e1] = {[eid: v1, kind: code, path: p0]}`; `MaterializeE(log)` over
the compacted log held `evidence[e1] = {}`, because `evidence_add` replayed at index 1, before
`e1` existed, and the orphan-tolerant arm (`db.rs:889-899`) discarded it silently. Derived status
followed the rows down from `present` to `missing`.

Verbatim from the TLC output, state 5:

```
State 5: <DoCompact line 487, col 5 to line 488, col 58 of module AgentKbEvidence>
/\ estatus = [e1 |-> "present"]
/\ log = <<[id |-> "e1", action |-> "evidence_add", evidence |-> [kind |-> "code", eid |-> "v1", path |-> "p0"]],
           [kind |-> "belief", id |-> "e1", action |-> "add", claimed_status |-> "present"]>>
/\ is_legacy = [e1 |-> FALSE]
/\ evidence = [e1 |-> {[kind |-> "code", eid |-> "v1", path |-> "p0"]}]
/\ entries = [e1 |-> [type |-> "present", kind |-> "belief"]]
```

**Resolution.** The T2 emission rule splices an entry's retained evidence events in *after* its
retained upsert, so the inversion is no longer constructible.

### CE2 — secondary counterexample: `evidence_expire` dropped, evidence resurrected (CLOSED)

The pre-T1 `compact.rs:121` matched `("evidence_add","evidence") | ("citation_healed","evidence")`
and never `("evidence_expire","evidence")`, so compaction dropped those events outright.

`SpecCE2` forbids re-upsert (`DoAddFreshBelief` fires only on an absent entry), which removes
CE1's shape from the search space and leaves the resurrection as the only violation. Trace:

| Event | `evidence[e1]` after | `estatus[e1]` after |
|---|---|---|
| 1. `add(e1, belief)` | `{}` | `missing` |
| 2. `evidence_add(e1, v1@p0)` | `{v1}` | `present` |
| 3. `evidence_expire(e1, v1)` | `{}` | `missing` |
| 4. `compact` → log becomes `[add, evidence_add]` | `{}` | `missing` |

`DbState` held `evidence[e1] = {}`; `MaterializeE(log)` over the compacted log held `{v1}` — an
evidence row the operator deleted came back on the next rebuild.

**Deviation from the plan.** The plan and the beads task predicted this counterexample needs 5
events and is unreachable at `MaxLogLen = 4`. It needs **4**. The prediction assumed a re-upsert
was required to separate this shape from CE1; isolating the shape with a restricted action set
instead makes the extra event unnecessary. `MaxLogLen = 5` was still used for the CE2 run as
instructed, and it is tractable — 65 distinct states, ~1 s — so the headroom costs nothing and is
kept as budget for future evidence-lifecycle shapes.

**Resolution.** T1 added the `("evidence_expire","evidence")` arm to the match, so the event is a
retention candidate like the other two.

### Third witness, found by the pre-fix full model (run 4)

The pre-fix run 4 (`Spec`, all actions) reported a **3-event** violation that neither CE harness
targets:

```
1. legacy_add(e1)            is_legacy = TRUE,  estatus = "n_a"
2. evidence_expire(e1, v1)   is_legacy = FALSE, estatus = "missing"   (no row to delete)
3. compact                   log becomes [legacy_add]
```

A stray `evidence_expire` naming a row that does not exist still recomputes `evidence_status`
unconditionally when the parent entry exists (`db.rs:968-984`), so it flips a grandfathered legacy
entry from `n/a` to `missing` in the live database. Compaction then dropped the event, and a
rebuild reported `n/a` again. This is the same dropped-`evidence_expire` defect as CE2, surfacing
in the status column instead of the row set, and it is cheaper to reach. It was a genuine
divergence, not a modelling artifact: `compute_evidence_status` (`db.rs:86-112`) uses
`COALESCE(kind,'belief')`, so a legacy entry with no evidence derives `missing`, while the legacy
upsert wrote `n/a`.

It is also the reason CE1 needs its own restricted harness: on the full action set, TLC's
breadth-first search reaches this shorter trace first, and a bare "some counterexample" would not
have distinguished the reordering defect from the dropped-expire defect.

**Resolution.** T1's retention arm keeps the `evidence_expire`, so it is no longer dropped.

## CE3 — legacy upsert re-grandfathers a de-legacied entry (CLOSED)

Found by the extended compact proptest in `.state/worktrees/evidence-storage-integrity`, shrunk to
`[Upsert(c legacy), EvidenceExpire(c), Upsert(c legacy)]`. Note that every `CompactOp::Upsert` the
generator emits is a *legacy* upsert — the event carries no `kind` field
(`src/commands/compact.rs`, `proptest_compact_preserves_live_state`).

Pre-amendment semantics wrote `evidence_status = 'n/a'` unconditionally for a legacy event, on
fresh INSERT and on ON CONFLICT alike (`db.rs` upsert arm; spec `legacy_add` arm set `sts='n_a'`,
`lgy=TRUE`). So:

| Event | `estatus[c]` after (original) | `estatus[c]` after (compacted `[legacy_up, ev_expire]`) |
|---|---|---|
| 1. `legacy_up(c)` | `n_a` | `n_a` |
| 2. `evidence_expire(c)` | `missing` | `missing` |
| 3. `legacy_up(c)` | `n_a` ← **re-grandfathered** | — (dropped: not the last upsert) |

The original log ends at `n/a`, the compacted log at `missing`. No emission order fixes this: the
re-grandfathering itself is the defect. It is payload-style authority of exactly the kind ADR-1
exists to eliminate — a legacy upsert asserting a status rather than deriving one.

### The `legacy_add` amendment (ADR-1 corollary)

A legacy upsert may **initialize** a fresh entry's grandfather; it may never **re-grandfather** an
existing one. Encoded as:

```tla
[] ev.action = "legacy_add" ->
      LET ents2 == [ents EXCEPT ![ev.id] = PresentEntry("belief")]
          lgy2  == [lgy EXCEPT ![ev.id] =
                       sts[ev.id] # StatusOf("belief", evs[ev.id])]
      IN <<ents2, evs, sts, lgy2>>
```

Two things are worth spelling out.

**`sts` is left entirely UNCHANGED.** An absent entry already carries `"n_a"`
(`AbsentEntriesClean`), so the fresh-insert case initializes to the grandfather for free, and no
separate fresh/existing branch is needed in the spec. This is the arm's whole content: evidence is
preserved (ADR-1) and status is not touched.

**`is_legacy` becomes a DERIVED predicate** — "this entry's status is a grandfather rather than a
derivation" — rather than independent state. That is forced, not chosen. The arm overwrites `kind`
with the AC1 backfill default `"belief"` (matching `kind=excluded.kind` in the code's ON CONFLICT
clause), so an entry that legitimately carried `"n_a"` under `kind="convention"` acquires a status
the belief rule would not derive; only re-raising the grandfather keeps `StatusConsistent` true.
On every entry whose kind was already belief-like, the predicate reproduces the preceding flag
exactly, which is the "preserves the current `is_legacy` flag" clause of the decision. Literal
preservation of the flag was tried first and breaks `StatusConsistent` in the run-1 regression
gate.

The matching code change is in `src/components/db.rs`: the upsert arm now selects between two
`ON CONFLICT` clauses, and the legacy one omits `evidence_status=excluded.evidence_status`. Fresh
INSERTs still bind `'n/a'`, and the explicit-kind recompute is untouched.

## CE4 — legacy upsert trailing an explicit-kind upsert (CLOSED)

Reported by run 4 against the amended module. Three events, `MaxLogLen = 4`:

```
State 2: add(e1, kind=belief)      entries[e1] present belief, evidence {}, estatus "missing"
State 3: legacy_add(e1)            estatus stays "missing"  (preserved, per the CE3 amendment)
State 4: compact                   log becomes <<legacy_add(e1)>>
```

Verbatim, state 4:

```
/\ estatus = [e1 |-> "missing", e2 |-> "n_a"]
/\ log = <<[id |-> "e1", action |-> "legacy_add"]>>
/\ is_legacy = [e1 |-> FALSE, e2 |-> FALSE]
/\ evidence = [e1 |-> {}, e2 |-> {}]
/\ entries = [e1 |-> [type |-> "present", kind |-> "belief"], e2 |-> [type |-> "absent"]]
```

`DbState` holds `estatus[e1] = "missing"`, `is_legacy[e1] = FALSE`. The compacted log is the bare
`legacy_add`, which replays into a fresh database as `"n_a"` / `is_legacy = TRUE`.

**This is a real divergence in the code, not only in the model.** Replaying
`[upsert(e1, kind="belief"), upsert(e1, no kind)]` leaves `evidence_status='missing'` (derived by
the first event, preserved by the second). Compaction retains only the last upsert, and replaying
`[upsert(e1, no kind)]` into a fresh database yields `'n/a'`.

**It is structural, not a detail of the amendment.** Compaction keeps only the *last* upsert per
live entry, so any information a *non-last* upsert contributes to the final status is destroyed.
"Preserve on conflict" is precisely such a contribution. Three candidate status rules were checked
by hand against all three shapes:

| legacy upsert on an EXISTING entry | CE3 `[leg, ev_exp, leg]` | `[leg, leg]` | CE4 `[add(belief), leg]` |
|---|---|---|---|
| re-grandfather to `n_a` (pre-fix) | **diverges** | ok | ok |
| preserve current status (this fix) | ok | ok | **diverges** |
| derive from `(belief, evidence)` | ok | **diverges** | **diverges** |

No rule on the legacy-upsert arm alone closes all three.

### Resolution — writer-producible-alphabet guard

`DoLegacyAdd` now carries a guard: it cannot fire on an id that already has an explicit `add` in
the log.

```tla
DoLegacyAdd ==
    \E id \in EntryIds :
        /\ ~\E i \in 1..Len(log) : log[i].action = "add" /\ log[i].id = id
        /\ WriteThrough(LegacyAddEvent(id))
```

The guard restricts the event alphabet to logs a writer can actually produce. It does **not**
weaken `CompactionEquivalenceE`, and it does not touch any invariant.

**Why the shape is not writer-producible.** Three steps:

1. The only production writer of entry upsert events is `kb_core::add`
   (`src/components/kb_core.rs:244-258`), and it has emitted `"kind"` unconditionally since
   Phase 0. Nothing in the tree appends a kindless upsert.
2. A kindless upsert can therefore only come from a log segment written by pre-Phase-0 code, which
   is chronologically *before* every explicit-kind upsert. So no append order can put a
   `legacy_add` after an `add` for the same id.
3. Compaction cannot manufacture the shape either. It only ever *selects* original events, and it
   retains exactly one upsert per live entry — the last one. A kindless LAST upsert implies every
   upsert for that entry is kindless (by step 2), so no explicit `add` can precede it in any
   compacted log derived from a real one.

**Relation to CE3.** CE3's shape is arguably unreachable by the same chronology argument, since
evidence events are also Phase 1. It was still treated as a real defect, and fixed in the code,
because it was found by a proptest whose generator emits *only* kindless upserts — that generator
is a realistic model of a pre-Phase-0 log segment being replayed, and the fix costs nothing.
CE4 differs in that closing it in code is not free: as the table above shows, no rule on the
legacy-upsert arm alone closes CE3 and CE4 together, so the choice is between the guard and a
`compact.rs` change (retaining an entry's FIRST upsert when its LAST is kindless) that buys
nothing on any writer-producible log. The guard is the documented decision.

CE3 remains reachable under the guard — its trace is legacy-only — so the regression value of this
module is retained.

## CE5 — post-expire `evidence_add` revived by compaction (CLOSED BY CONTRACT)

Found in a scratch experiment while checking the CE4 guard before adopting it (guarded module,
run 4 config). Because a guard only *removes* behaviours, this trace is reachable in the unguarded
module too; the unguarded run 4 simply reports the shorter CE4 first. The trace contains no
`legacy_add` at all — it is independent of the CE3 amendment and of the CE4 guard.

```
1. add(e2, convention)      present convention, evidence {},  estatus "n_a"
2. expire(e2)               absent,             evidence {},  estatus "n_a"   (ADR-2 GC)
3. evidence_add(e2, v1)     ORPHAN — parent absent, filtered, no state change
4. add(e2, belief)          present belief,     evidence {},  estatus "missing"
5. compact                  log becomes <<add(e2, belief), evidence_add(e2, v1)>>
```

`entry_first[e2] = 1`, `entry_last[e2] = 4`, `expire_last[e2] = 2`, so `e2` is live. The evidence
event at index 3 passes both retention bounds (`3 > 1` and `3 > 2`) and is emitted after the
retained upsert — where it now *applies*. `DbState` holds `evidence[e2] = {}` / `"missing"`; the
compacted log materializes `{v1}` / `"present"`.

**This is an ADR-2 boundary conflict, not a compaction bug against today's code.** The
implementation's expire arm is `UPDATE entries SET is_stale=1` (`db.rs`, expire branch) — the
entries row survives — and `evidence_add`'s orphan guard is
`SELECT COUNT(*) FROM entries WHERE id=?1`, which counts stale rows. So against the code as it
stands today, the index-3 `evidence_add` *does* apply at its original position, and re-emitting it
after the revive upsert reproduces that faithfully. The `compact.rs` comment says exactly this:
"the first-upsert boundary is global, so evidence between an expire and a revive upsert remains
eligible: the stale entry existed when it applied."

ADR-2 as ratified says the opposite: expire GCs the entry's evidence and makes it indistinguishable
from a never-created entry, which turns the index-3 event into an orphan no-op. Under ADR-2 the
retention rule needs a third bound — the evidence event must have applied to a *live* entry at its
original index, i.e. the last upsert before `i` must postdate the last expire before `i` — and the
`compact.rs` comment above becomes wrong.

The module header already anticipates this gap ("the distinction is real only at the
implementation level, where expire leaves an `is_stale=1` row behind that a compacted log cannot
reproduce"). CE5 is that gap materializing now that `CompactedLogE` models the real algorithm.

### Resolution — third retention bound, in the model now

`RetainedEvidenceIdxs` carries the live-at-index bound:

```tla
LastIdxBefore(events, id, acts, i) ==
    LET S == { j \in 1..(i - 1) : events[j].action \in acts /\ events[j].id = id }
    IN IF S = {} THEN 0 ELSE MaxOfSet(S)

LiveAtIdx(events, id, i) ==
      LastIdxBefore(events, id, EntryUpsertActions, i)
    > LastIdxBefore(events, id, {"expire"}, i)
```

`RetainedEvidenceIdxs` therefore keeps an evidence event iff its parent is live at the end of the
log, `i > expire_last[parent]`, and `LiveAtIdx(events, parent, i)`. Applied to the CE5 trace, the
index-3 `evidence_add` has `LastIdxBefore(upserts, 3) = 1` and `LastIdxBefore(expire, 3) = 2`, so
`1 > 2` is false and the event is dropped; the compacted log is the bare `add(e2, belief)`, which
materializes `{}` / `"missing"` — equal to `DbState`.

**The model now describes the target, not the code.** That is deliberate. The implementation is
currently self-consistent *without* this bound, because a stale row still accepts evidence, and it
would be self-consistent *with* the bound once ADR-2's GC lands. What is not admissible is landing
one half. Beads task `bd-evidence-storage-integrity-w3xo.7` has been amended to require the four
changes as a single unit:

1. expire / stale-upsert evidence GC (ADR-2 proper),
2. the `evidence_add` orphan guard tightened to `is_stale = 0`,
3. this third retention bound in `compact.rs`, replacing the weaker `entry_first` bound,
4. removal of the GC shim in the compact proptest's `materialize` oracle.

Until that task lands, the divergence between this module's retention rule and `compact.rs:186-196`
is known and intentional; it is the one place where `CompactedLogE` deliberately runs ahead of the
implementation.
