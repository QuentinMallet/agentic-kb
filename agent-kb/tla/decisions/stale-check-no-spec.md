# Decision: no TLA+ spec for `kb stale-check` fix epic (br-yyb)

**Date:** 2026-05-28
**Epic:** br-yyb — kb stale-check performance and correctness fixes
**Plan:** `.omc/plans/kb-stale-check-fixes.md`
**Decision:** skip new TLA+ spec.

## Standing rule

`AGENTS.md §Formal methods` requires a TLA+ spec for any new state machine in this project. The standing rule's escape hatch: explicit documented skip, signed off by reviewer + analyst at post-impl.

## Why skip

The br-yyb epic covers five bugs and one orchestration refactor in `src/commands/stale_check.rs` and `src/commands/mcp.rs`. Each is a pure-helper change with no new state machine or invariant:

| Task | Nature | New invariant? | New state? |
|------|--------|----------------|------------|
| T0 — extract shared orchestration | refactor (no behavior change) | no | no |
| T1 — UTF-8 panic in `extract_blame_shas` | str-slicing → byte-slicing (memory safety) | no | no |
| T2 — blame scope correction | `git blame --porcelain` → `git log --pretty=%H` (set semantics) | no | no |
| T3 — SQL index + fast-path + hoist prepare | query plan / index addition | no | no |
| T4 — dedupe + `git rev-list --count` | per-tuple memoization, subprocess strategy | no | no |
| T5 — UNREACHABLE bucket | adds a third output bucket distinguishing unreachable refs from no-change refs | no — additive output shape, no protocol round-trip | no |

The existing `AgentKb.tla` models the KB write/expire/replay protocol (event append → DB apply, idempotence, supersession ordering). `stale-check` is a read-only query against the resulting state — it makes no transitions, holds no concurrency-sensitive state across calls, and its output shape change (T5) is a one-shot additive field, not a protocol handshake.

The five bugs are correctness/perf issues at the *implementation* level, not the *specification* level:

- T1 is memory safety (Rust runtime invariant: str slicing requires char boundaries). This is a property the type system + tests enforce; no spec lifts above it.
- T2/T4 change *how* we ask git for the same information. The set semantics is "commits that touched this file in the recorded → HEAD range" — already implicit in the existing helper's docstring. Refining the implementation doesn't refine the spec.
- T3 is a SQL query-plan optimisation. The post-condition (return set of matching entries) is unchanged; only the execution strategy changes.
- T5 is the only candidate for a small refinement: distinguishing "unreachable" from "no change". This is observable in the output but does not introduce a new state machine — it's a partition of an existing reply set.

## Cost/benefit

Writing a stale-check spec would:
- Repeat the existing `AgentKb.tla` invariants without adding new ones.
- Require modeling git as a partial oracle (unreachable refs), which buys little because the failure mode is observable (T5 explicitly surfaces it).
- Delay a hotfix-class panic (T1) behind spec authoring.

Skipping it:
- Lets T1 ship immediately as a hotfix from master, per AGENTS.md bug-fix protocol.
- Keeps spec surface area focused on the actual state machine (KB writes), not its read helpers.
- Is documented here so the post-impl gate is explicit-not-implicit.

## Reviewer + analyst sign-off

Required at post-impl (br-yyb.10) per AGENTS.md TLA+ gate override clause:

- [ ] code-reviewer confirms no new state-machine logic introduced across br-yyb.1–br-yyb.6.
- [ ] analyst confirms existing `AgentKb.tla` invariants still cover the modified read paths.

If either reviewer disagrees, the override is withdrawn and a small refinement covering `(stale, review, unreachable)` partition semantics is added under `agent-kb/tla/StaleCheck.tla` before merge.

## Related

- AGENTS.md §Formal methods — standing rule.
- AGENTS.md §Workflow §Phase 3 — TLA+ gate enforcement.
- `.omc/plans/kb-stale-check-fixes.md §T6` — task definition.
- `agent-kb/tla/AgentKb.tla` — existing KB protocol spec.
