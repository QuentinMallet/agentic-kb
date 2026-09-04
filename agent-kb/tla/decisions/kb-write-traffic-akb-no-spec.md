# Decision: no TLA+ spec for kb-write-traffic-akb epic (bd-r05y)

**Date:** 2026-09-02
**Epic:** bd-r05y — kb-write-traffic-akb (agentic-kb side)
**Partner epic:** machines_conf bd-57dda (kb-write-traffic-mc)
**Plan:** `.omc/plans/kb-write-traffic.md` (r3.1)
**Decision:** skip new TLA+ spec. Recorded waiver, not an omission.

## Standing rule

`AGENTS.md §Formal methods` requires a TLA+ spec for any new state machine in this project. The standing rule's escape hatch: explicit documented skip, signed off by reviewer + analyst at post-impl.

## Why skip

The bd-r05y epic covers three tasks on the agentic-kb side of the kb-write-traffic program (cross-repo with machines_conf bd-57dda). None introduces new temporal semantics:

| Task | Nature | New state machine? | Temporal semantics? |
|------|--------|-------------------|-------------------|
| B1 — kb_add MCP description rewrite | tool-description string change | no | no |
| B2 — cheap evidence (sha/hash resolution) | pure function from citation_path to (sha, hash) | no | no |
| B3 — regression test / property test | guard logic: no kind filter in retrieval or FTS indexing | no | no |

The genuine temporal surface in this program is the Stop-hook digest liveness (whether a live-update stop hook can issue fresh evidence before expiring old rows). That surface is modelled on the machines_conf side under bd-57dda.6, creating no coverage gap here.

B1 is a documentation enhancement to the kb_add call site (the tool description string that appears when the tool is invoked). No invariant changes, no state.

B2 is a pure function that maps `citation_path` (a string) to a pair `(sha, hash)` (computed by the verifier). It is testable with property tests; it holds no state and participates in no concurrent handshake.

B3 is a regression test (property-based) that asserts the kind filter is applied at both retrieval (`kb_search`) and FTS indexing time. It is itself a property test — a guard, not a system component.

## Cost/benefit

Writing a spec for kb-write-traffic-akb would:
- Introduce new temporal complexity (the digest liveness is not local to this epic).
- Repeat the existing agent-kb protocol invariants without adding new ones.
- Delay work on B3 (a regression guard for a known issue) behind spec authoring.

Skipping it:
- Keeps spec focus on the temporal surface (Stop-hook liveness), which is modelled on the machines_conf side.
- Lets B3 ship immediately as a regression guard.
- Is documented here so the post-impl gate is explicit-not-implicit.

## Explicit instruction for spec-dep-validator

The `spec-dep-validator` must not treat missing spec dependencies on B1, B2, or B3 as a gate failure when this record is cited. The validator should accept the reference to this waiver record and proceed.

## Reviewer + analyst sign-off

Required at post-impl (bd-r05y.6) per AGENTS.md TLA+ gate override clause:

- [ ] code-reviewer confirms no new state-machine logic introduced across B1–B3.
- [ ] analyst confirms the Stop-hook digest liveness surface is covered by bd-57dda.6 on the machines_conf side.

If either reviewer disagrees, the override is withdrawn and a small spec covering the digest-liveness constraint on the kb side is added to `.state/agent-kb/tla/` before merge.

## Related

- AGENTS.md §Formal methods — standing rule.
- AGENTS.md §Workflow §Phase 3 — TLA+ gate enforcement.
- `.omc/plans/kb-write-traffic.md` — cross-repo plan (machines_conf).
- machines_conf `bd-57dda.6` — Stop-hook digest liveness spec (temporal surface).
- `.state/agent-kb/tla/decisions/stale-check-no-spec.md` — precedent for documented skip.
