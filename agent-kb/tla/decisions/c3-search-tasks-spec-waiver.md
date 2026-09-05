# Decision: scoped TLA+ gate waiver for C3's three pure-search tasks

**Date:** 2026-09-04
**Epic:** `bd-21ef.3` — C3: read-path integrity & performance (meta-epic `bd-21ef`, storage-correctness-2)
**Plan:** `.state/.omc/plans/c3-read-path.md`
**Spec task the waiver is measured against:** `bd-21ef.3.2` (T0 — `CitationRelocation.tla` amendment)
**Decision:** drop the T0 dependency edge from three tasks only. **Scoped**, not blanket.
**Grantor:** team lead, 2026-09-04, following the precedent set for C2's lock contract.

## Tasks waived

| Task | Bead | Files | Why no relationship to `CitationRelocation.tla` |
|------|------|-------|--------------------------------------------------|
| S3a — provenance dangling references | `bd-21ef.3.9` | `mcp.rs:1777`, `:1799` | Partitions an existing reply set: a referenced parent with no `entries` row moves from `roots` to a new `dangling` bucket, and a missing start node returns `entry_not_found`. Additive output shape; the traversal's Enter/Leave algorithm, cycle-vs-diamond discrimination, and depth cap are all unchanged and already correct. |
| S3b — context token budget | `bd-21ef.3.10` | `context.rs:122`, `:226`, `:317` | Changes *which representation* the token estimate is computed over (the emitted one rather than a plain-text stand-in) and relabels the figure an approximation. Pure estimation-accuracy fix; no state, no transitions, no protocol. |
| S4 — federated search contract | `bd-21ef.3.11` | `search.rs:127–160` | Replaces per-repo truncation and an accidental dedup with a global limit, an explicit `(origin_repo, id)` key, and one truncation. Query-time result assembly over already-materialized rows. |

## Why the gate does not bind here

`CitationRelocation.tla` models the citation verify/relocate/heal state machine: `storedHash` immutability, relocation only from a strong excerpt with exactly one candidate, no silent promotion of a `Relocated` row to `Verified`, and (after T0) the plan-then-apply discipline for healing under a lock. Every one of those propositions is about **evidence rows changing status over time**.

The three waived tasks touch none of it. They are read-path result assembly: none writes an event, none mutates an evidence row, none takes the write flock, and none introduces a state machine or an invariant that persists across calls. Their correctness properties are ordinary post-conditions on a single query's output, fully expressible as tests — and each carries such a test in its acceptance criteria.

The remaining nine C3 implementation tasks keep their T0 edge, including the two that *do* bear on the spec:

- **V1** (`bd-21ef.3.3`) is a refinement-mapping fix: the implementation's candidate count must correspond to the spec's `candidates` variable, which T0 pins the unit of.
- **V3** (`bd-21ef.3.5`) is the direct implementation of T0's `PlanHeal` / `ApplyHeal` split.

S1 (`bd-21ef.3.7`) also keeps its edge, because T0 part (c) records the determinism decision — whether ADR-2's total order warrants its own module or a documented no-change — and S1 implements whatever that decides.

## What this buys

Start-time parallelism rises from 2 to 4. Before the waiver, all nine implementation tasks sat behind T0, which Revision 2 had *expanded* (adding a `path` field to `EvidenceRow` is an add-a-variable change, not a one-liner), making it simultaneously the critical-path head and the least certain task. S3a and S3b become immediately ready alongside T0 and A0; S4 does not, because it independently depends on C2's `L1b` (`bd-21ef.2.4`) — the two edit the same hunk at `search.rs:127–160`.

## What this does not do

- It does not waive the gate for any other C3 task, now or later.
- It does not waive post-impl spec compliance (`/verify`, Covered verdict) for the three tasks; it waives only the *dependency edge* asserting they must wait on spec authoring.
- It does not license a later task to claim the waiver by analogy. A task that writes an event, mutates an evidence row, or takes the write flock is outside this waiver's terms by construction.

## Reviewer + analyst sign-off

Required at post-impl (`bd-21ef.3.14`) per the AGENTS.md TLA+ gate override clause:

- [x] code-reviewer confirms S3a, S3b and S4 introduced no state-machine logic, no event write, and no lock acquisition.
- [x] analyst confirms `CitationRelocation.tla` (as amended by T0) still covers every modified path, and that no invariant of it is reachable from the three waived tasks.

If either reviewer disagrees, the waiver is withdrawn for the disputed task and the T0 edge is restored before merge.

## Related

- `AGENTS.md §Formal methods` — the standing rule and its escape hatch.
- `AGENTS.md §Workflow §Phase 3` — TLA+ gate enforcement (`spec-dep-validator.sh`, ralph Step 1.5).
- `.state/.omc/plans/c3-read-path.md §Guardrails` — the "every implementation task carries a T0 edge" guardrail this waiver scopes.
- `.state/.omc/plans/open-questions.md` — C3-Q7, where the waiver was requested and granted.
- `.state/agent-kb/tla/decisions/stale-check-no-spec.md` — the earlier waiver this follows in form.
- C2's equivalent waiver for its lock contract (`bd-21ef.2.2`, `T1`) — the precedent the lead cited.

## Sign-off record (2026-09-05)

**Code-reviewer pass** (opus code-reviewer agent, `rev-waiver-c3`): S3a, S3b and S4 each
**CONFIRMED** to introduce no state-machine logic, no event write, and no lock acquisition.
**Verdict: SIGN-OFF GRANTED.** Caveat: `kb context`'s pre-existing
`query_hits::record_injection` telemetry write predates S3b and is not repository state;
noted for the record in case the gate is later read as the stricter "writes no file at all".
Full report: `signoffs/c3-waiver-reviewer-2026-09-05.md`.

**Analyst pass** (opus analyst agent, `audit-waiver-c3`): **SIGN-OFF GRANTED.**
`CitationRelocation.tla` as amended by T0 covers every modified path, and no invariant of it
is reachable from the three waived tasks. Full report: `signoffs/c3-waiver-analyst-2026-09-05.md`.
