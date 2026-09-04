# Spec recovery audit — TLA+ modules deleted in 12f44d5

Task: `bd-w45u` (chore). Executed 2026-09-03 against master `058f82b`.

## Provenance

Commit `12f44d5` — *chore(repo): remove branch-local generated artifacts* (2026-08-18) —
deleted the entire `agent-kb/tla/` directory from the code branch. Its stated constraint was
"TLA+ artifacts are maintained on the agentic branch", but **the specs were never migrated
there**: they were deleted from master and not added to `.state/agent-kb/tla/` (the agentic-branch
worktree that is the canonical spec home). Eleven modules were lost, along with every reference
to them in code, tests, and docs.

`bd-cnh8.2` found and restored `CitationRelocation.tla` / `.cfg` (recovered from `3ed5a1f`) —
that is the precedent this audit generalizes. This document covers the remaining ten modules.

Recovery source for every module below: `git show 12f44d5^:agent-kb/tla/<name>` (the deletion
commit's parent holds the newest content for all of them; no module was modified after its last
substantive commit and before the deletion).

## Disposition table

| Module | Disposition | Evidence |
|---|---|---|
| `CrossBatch` | **Restored — TLC pass** | Models the cross-invocation boundary of `kb_core::add`; boundary invariant `db = Materialize(jsonl)`. Behavior live: `src/components/kb_core.rs:384` still batches through `events::append_events_batch`. 21 states, all invariants hold. |
| `InnerGap` | **Restored — TLC pass** | Layer-1 per-event append/apply gap for one `kb_core::add` call. Live: `kb_core.rs:136` documents the single-batch JSONL-first contract; `events::append_events_batch` at `src/components/events.rs:117`. 7,145 states / 4,141 distinct, all invariants hold. |
| `CueBatch` | **Restored — TLC pass** | Cue rows ride the upsert event (no separate cue events). Live: `kb_core.rs:316` and `src/components/db.rs:261` both assert exactly this. 2,109,193 states / 797,097 distinct, all invariants hold. |
| `FTS5DualWrite` | **Restored — TLC pass** | Contentless `entries_fts` → content-mirroring `entries_fts_v2` dual write. Live: both tables and their triggers still exist (`db.rs:211`, `db.rs:218`, `db.rs:228-249`); the v1 deprecation gate at `db.rs:506-558` has not fired, and `perf/contentless-fts-cleanup-deferred` records the cleanup as deliberately deferred. Model checking completed, no error. |
| `ingest_replace_path` | **Restored — TLC pass** | First-chunk-only `replace_path` in the N-chunk ingest loop. Live and still doc-referenced: `src/commands/ingest.rs:72` plus `replace_path: i == 0` in the same function. Invariants + `AllChunksEventuallyPresent` property hold. |
| `session_id_propagation` | **Restored — TLC pass** | `OMC_SESSION_ID` → event `session_id` → `entries.session_id`. Live: `src/commands/add.rs:160-170` derives the `(session, session_id)` pair exactly as modelled; `add.rs:744-793` is the corresponding test. Invariants + `EventAlwaysApplied` property hold. |
| `WorkingSetBudget` | **Restored — TLC pass** | Greedy, non-truncating budgeted context selection. Live: `src/commands/context.rs:243-252` (`greedy_select`, whole-entry fit test, relevance floor). 1,244,160 states / 829,440 distinct, all six invariants hold, 3 min 05 s. |
| `EvalSplitFreeze` | **Restored — TLC pass** | Sealed eval split + uniform-first audit sampling. Live: `Split::Sealed` and `validate_sealed_manifest` at `src/components/retrieval_eval.rs:23/264`, `EVAL_SPLIT_REFUSAL` at `retrieval_eval.rs:353`. Largest run in the audit: 152,829,843 states generated / 9,441,432 distinct, depth 18, 5 min 39 s, all twelve invariants hold. |
| `RebuildSwap` (+ `RebuildSwap_compact.cfg`) | **Retired — models a superseded protocol** | See "Retired" below. |
| `replace_path_atomic` | **Retired — never parsed** | See "Retired" below. |

### `EvalSplitFreeze` fingerprint-collision note

`EvalSplitFreeze` explores a state space larger than the other nine modules combined
(152.8M states generated at `MaxCases = 3`, `MaxRuns = 2`). TLC's own estimate of the
probability that two distinct states collided on a fingerprint — and that it therefore did not
check every reachable state — is 7.3E-5 optimistic / 2.3E-6 from the actual fingerprints. That
is small but it is three orders of magnitude larger than the other runs in this audit, so a
re-check at a larger fingerprint set (`-fpbits`) is worth doing before this module's green
result is leaned on for anything load-bearing.

### Deadlock-check caveat for `CrossBatch`, `InnerGap`, `CueBatch`

These three `.cfg` files do not set `CHECK_DEADLOCK FALSE`, so a plain
`tlc -config X.cfg X.tla` reports `Error: Deadlock reached` and exits 11 — a bounded-model
artifact (these models legitimately reach terminal states once `MaxLog` is exhausted), **not**
an invariant violation. All three pass every invariant when the deadlock check is disabled:

```
~/.nix-profile/bin/tlc -workers auto -deadlock -config CueBatch.cfg CueBatch.tla
```

The `.cfg` files were restored **byte-identical to their recovered content** so provenance is
exact; adding `CHECK_DEADLOCK FALSE` to the three configs (matching `RebuildSwap.cfg`'s own
precedent) is filed as a follow-up rather than done silently here.

## Retired

### `RebuildSwap.tla`, `RebuildSwap.cfg`, `RebuildSwap_compact.cfg` — superseded protocol

**Not restored.** The module passes TLC on its own terms (`RebuildSwap.cfg`: 179,808 states /
73,048 distinct, no error; `RebuildSwap_compact.cfg`: violates `SnapshotIndexRemainsValid` as
its header documents it should). It is retired anyway, because *what it models is no longer the
protocol in master* — restoring it would put a green-looking, TLC-passing file next to code it
does not describe.

The module's own header states it "models the CURRENT protocol", in which Phase 3 holds the
write lock for an O(full log) parse and the catch-up cursor is a **positional** index clamped
with `snapshot_len.min(...)`. `RebuildSwap_compact.cfg` exists specifically to reproduce
`bd-3mr.9` (compaction during the lock-free Phase 2 makes that positional index address the
wrong log).

`bd-3mr.9` was **closed 2026-09-03**, and the fix replaced the mechanism the spec models:
`src/commands/rebuild.rs:331-354` now snapshots a **byte length plus a SHA-256 hash of the
complete prefix** under a brief Phase-1 lock; `rebuild.rs:400` re-verifies that prefix
(`prefix_matches`) after re-acquiring the lock in Phase 3 and **restarts the whole rebuild**
(bounded by `MAX_ATTEMPTS`, then bails) if the log's identity changed; catch-up then reads from
the byte offset (`read_events_from_offset`), never from a positional index. There is no
full-log re-parse under the Phase-3 lock any more.

Consequences for the spec: `SnapshotIndexRemainsValid` no longer corresponds to a reachable
hazard, `FullLogParse`'s unconditional `Len(jsonl)` lock charge no longer models the code's
cost structure, and the retry/bail path is not modelled at all.

**Re-derivation target if this is picked up again:** model the byte-offset + prefix-hash
snapshot, the identity re-check under the Phase-3 lock, and the bounded-retry/bail branch. The
module's documented limitation still applies — it is safety-only (no `WF_vars`), so a
catch-up-outside-the-lock design cannot have its termination proven by it. Recover the old text
with `git show 12f44d5^:agent-kb/tla/RebuildSwap.tla`.

### `replace_path_atomic.tla` — never parsed; documentation-only parent

**Not restored.** SANY rejects the module outright:

```
***Parse Error***
Encountered "[" at line 68, column 54 and token "log"
```

Line 68 is `Materialize(log) == LET upserted == { log[i].id : i \in DOMAIN log, log[i].kind = "upsert" }`
— `{ e : x \in S, P }` is not valid TLA+ set-comprehension syntax (a filter needs its own
`{ j \in DOMAIN log : ... }` subexpression). The module also `INSTANCE`s `InnerGap` and
`CrossBatch` without substituting their extra variables (`phase`, `batch_events`, `apply_idx`),
which SANY would reject as well once the parse error is fixed. It ships no `.cfg` and was never
model-checked.

The two-layer refinement it narrates is real and *is* verified — by `InnerGap.tla` (layer 1) and
`CrossBatch.tla` (layer 2), both restored and green above. The parent module carried only prose.
Code references to it should point at the two layer modules directly (see below). Recover the
old text with `git show 12f44d5^:agent-kb/tla/replace_path_atomic.tla`.

## Dead references — retarget list (follow-up, not applied here)

13 reference sites across 10 files still name the deleted `agent-kb/tla/` path. **None were
edited by this audit**: the code-branch worktrees are in active use by other agents, so the
retargeting is filed for the fmt/cleanup component instead.

Every target below is the canonical agentic-branch path `.state/agent-kb/tla/`.

| Site | Current text | Retarget to |
|---|---|---|
| `src/components/kb_core.rs:33` | `agent-kb/tla/replace_path_atomic.tla` | `.state/agent-kb/tla/InnerGap.tla` (layer 1) and `.state/agent-kb/tla/CrossBatch.tla` (layer 2) — parent module retired, see above |
| `src/components/kb_core.rs:316` | `agent-kb/tla/CueBatch.tla` | `.state/agent-kb/tla/CueBatch.tla` |
| `src/components/db.rs:261` | `agent-kb/tla/CueBatch.tla` | `.state/agent-kb/tla/CueBatch.tla` |
| `src/components/verification.rs:9` | `agent-kb/tla/CitationRelocation.tla` | `.state/agent-kb/tla/CitationRelocation.tla` |
| `src/commands/ingest.rs:72` | `agent-kb/tla/ingest_replace_path.tla` | `.state/agent-kb/tla/ingest_replace_path.tla` |
| `src/models.rs:157` | `agent-kb/tla/CitationRelocation.tla` | `.state/agent-kb/tla/CitationRelocation.tla` |
| `tests/cues.rs:5` | `agent-kb/tla/CueBatch.tla` | `.state/agent-kb/tla/CueBatch.tla` |
| `tests/citation_relocation.rs:3` | `agent-kb/tla/CitationRelocation.tla` | `.state/agent-kb/tla/CitationRelocation.tla` |
| `docs/src/cue-anchors.md:48` | `agent-kb/tla/CueBatch.tla` | `.state/agent-kb/tla/CueBatch.tla` |
| `agentic-kb-defensibility.md:138` | `agent-kb/tla/audit/Audit.tla` | `.state/agent-kb/tla/audit/Audit.tla` |
| `agentic-kb-defensibility.md:320` | `agent-kb/tla/audit/Audit.tla` | `.state/agent-kb/tla/audit/Audit.tla` |
| `agentic-kb-defensibility.md:341` | `agent-kb/tla/audit/Audit.tla` | `.state/agent-kb/tla/audit/Audit.tla` |
| `agentic-kb-defensibility.md:367` | `agent-kb/tla/audit/Audit.tla` | `.state/agent-kb/tla/audit/Audit.tla` |

`audit/Audit.tla` and `audit/Audit.cfg` were never lost — they already live at
`.state/agent-kb/tla/audit/`. Those four sites need a prefix fix only.

Already-correct sites, listed so a future sweep does not "fix" them:
`src/commands/compact.rs:1092`, `docs/src/evidence-storage.md:116`,
`docs/src/evidence-storage.md:130`, `docs/src/citation-semantics.md:190`.

### KB-side follow-up

KB entry `gotchas/tla` (`kb#3feee5a5-6aad-4c4a-b23a-a1b3a97eee39`) carries a code-evidence
citation on `agent-kb/tla/EvalSplitFreeze.tla:1-20`, currently `status=BROKEN`. It should be
re-cited against `.state/agent-kb/tla/EvalSplitFreeze.tla` now that the file is back.

### Also worth doing

The stub directory `agent-kb/tla/states/` still exists untracked at the repo root. It is a
leftover TLC scratch dir from before the deletion and is what makes `agent-kb/tla/` look like it
still exists. Removing it would stop future readers from assuming the old path is live.
