# S5 Decision Packet: Search Resource Caps At `search_entries`

Date: 2026-09-04
Task: `bd-21ef.3.17` / C3 S5
Status: prepared for lead ruling before clamp lands

## Scope

This packet is derived from the current code path:

- `src/commands/search.rs:108` sets CLI `inline_verify_k = limit`.
- `src/commands/mcp.rs:284-287` already clamps MCP `limit` to `MAX_LIMIT` and `inline_verify_k` to `MAX_INLINE_VERIFY_K`.
- `src/components/db.rs:1738+` `search_entries` had no boundary clamp before this prepared change.
- `src/components/db.rs:2194+` verification flattens all evidence rows for the first `verify_count = min(inline_verify_k, entries.len())` entries into `flat_tasks`, then schedules one verification task per evidence row.
- `src/components/db.rs:72` defines `MAX_EVIDENCE_ROWS_PER_ENTRY = 200`, and `fetch_evidence_for_entries` enforces it. This is a real enforced cap, not just a comment.

## A. Worst-Case Verification Fan-Out

Derived from the scheduling loop in `search_entries`:

- `verify_count = min(inline_verify_k, entries.len())`
- each of those entries contributes up to `MAX_EVIDENCE_ROWS_PER_ENTRY` evidence rows
- scheduled verification tasks therefore equal:

`scheduled_tasks = min(inline_verify_k, entries.len()) × MAX_EVIDENCE_ROWS_PER_ENTRY`

For the current CLI path before S5:

- CLI sets `inline_verify_k = limit`
- `search_entries` did not clamp `limit` or `inline_verify_k`
- result count is bounded by `limit`
- so the worst-case verification fan-out on the CLI path is:

`scheduled_tasks = limit × MAX_EVIDENCE_ROWS_PER_ENTRY`

Concrete example, current CLI with `kb search --limit 500`:

- `inline_verify_k = 500`
- `MAX_EVIDENCE_ROWS_PER_ENTRY = 200`
- derived worst-case scheduled verification tasks: `500 × 200 = 100,000`

This is derived from code inspection, not measured in this sandbox. Caller to measure on host if a measured datapoint is still required for the ruling log.

## B. Pre-Existing CLI/MCP Asymmetry

The asymmetry already exists today and predates S5:

- MCP already clamps `limit` to 100 and `inline_verify_k` to 20 in `src/commands/mcp.rs`.
- CLI sets `inline_verify_k = limit` in `src/commands/search.rs` and accepts unrestricted `usize`.
- `search_entries` previously trusted both front ends.

The contract question is therefore:

Which of two existing behaviours becomes the contract?

1. CLI-style verify-all up to the result cap.
2. MCP-style narrow verification cap at 20.

This is not a new regression question in the abstract; it is a choice between two already divergent behaviours.

## C. Proposed `verify_pool_size` Ceiling

Proposed constant:

`MAX_VERIFY_POOL_SIZE = 32`

Reasoning:

- the worker count is currently sourced from config and only lower-bounded at 1
- every worker is an OS thread
- a hostile or accidental large config value can still oversubscribe the host even after task-count caps
- the default should continue to track hardware via `num_cpus::get_physical().max(1)`
- an explicit ceiling should bound both configured and default values

Prepared implementation shape:

`effective_verify_pool_size = clamp(config_or_default, 1, MAX_VERIFY_POOL_SIZE)`

This preserves hardware-sensitive defaults while preventing an arbitrary configured value from spawning an arbitrary number of threads.

## D. Inline Verification Contract Options

O1:

- CLI keeps verify-all semantics up to a named `MAX_INLINE_VERIFY_K = MAX_LIMIT (=100)`
- MCP raises to the same value
- practical effect: `kb search --limit 50` continues to verify all 50 results
- worst-case bounded fan-out becomes `100 × 200 = 20,000` scheduled tasks

O2:

- both front ends clamp `inline_verify_k` to 20
- CLI prints a one-line note when `limit > 20`
- this is an explicit documented regression from current CLI behaviour
- worst-case bounded fan-out becomes `20 × 200 = 4,000` scheduled tasks

Recommendation: O1

Reasoning:

- the lead posture already forbids a silent CLI regression
- O1 preserves the existing CLI contract for `--limit <= 100`
- the implementation can still bound work materially by capping `limit` at 100 and `verify_pool_size` at a named constant
- O2 is viable only if the product decision is to accept and document the CLI behaviour change

Prepared implementation in this worktree is parameterised so the ruling is a one-line constant change:

- current prepared constant: `MAX_INLINE_VERIFY_K = MAX_LIMIT`
- if the lead rules for O2, change `MAX_INLINE_VERIFY_K` to `20`

## E. Is Recency `IN (...)` Batching Dead Code Once Limit Is Bounded?

Yes.

Current recency pass in `search_entries` builds an `IN (...)` query over the already-truncated `entries` vector.

- without MMR: `entries.len() <= limit`
- with MMR: `entries.len() <= 2 × limit` before the final post-MMR truncate

With `MAX_LIMIT = 100`, the maximum parameter count is therefore:

- `100` without MMR
- `200` with MMR

That is well below SQLite parameter ceilings in normal builds, so additional batching for this query would be dead code once the boundary cap exists.

## Prepared Implementation In This Worktree

The prepared code does the following:

- names `MAX_LIMIT`, `MAX_INLINE_VERIFY_K`, and `MAX_VERIFY_POOL_SIZE` in `src/components/db.rs`
- clamps `limit`, `inline_verify_k`, and `verify_pool_size` inside `search_entries`
- uses the clamped values for ranking pool sizes, recency pass, verification scheduling, and worker spawning
- leaves the MCP request clamps in place but marks them redundant
- adds explicit CLI `--limit` validation for `1..=MAX_LIMIT`
- adds test-only runtime stats so tests can assert scheduled verification task count and spawned worker count directly

## Caller Follow-Up After Ruling

If the lead accepts O1:

- no code-structure change is needed beyond keeping `MAX_INLINE_VERIFY_K = MAX_LIMIT`
- MCP clamp comment can remain as redundant normalization

If the lead accepts O2:

- change `MAX_INLINE_VERIFY_K` from `MAX_LIMIT` to `20`
- keep the CLI parser range at `1..=MAX_LIMIT`
- add the documented one-line CLI note when `limit > MAX_INLINE_VERIFY_K`
- update docs to record the explicit CLI verification regression

## Ruling

Lead ruling on 2026-09-04: accept O1.

- `MAX_LIMIT = 100`
- `MAX_INLINE_VERIFY_K = MAX_LIMIT (=100)`
- `MAX_VERIFY_POOL_SIZE = 32`
- MCP keeps its front-end `limit` and `inline_verify_k` clamps, but `inline_verify_k` is raised from `20` to `MAX_INLINE_VERIFY_K`; this is user-visible for MCP callers and now matches the CLI cap.
- Recency `IN (...)` batching is not added; once `limit` is bounded, that extra batching would be dead code.
