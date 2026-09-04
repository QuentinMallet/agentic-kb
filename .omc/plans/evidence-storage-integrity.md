# Plan: evidence-storage-integrity

Repair of the entries/evidence storage layer following the code review completed
2026-09-01 at HEAD `12f44d5`. Source findings: KB `observations/exploration/evidence-storage`
[kb#7dc942f4-7ba0-41b3-91d4-42fb007a9340] and `gotchas/tla/compact-spec-fidelity-gap`
[kb#74a95214-5e5b-487e-930a-134427b3326d].

Mode: SHORT consensus (RALPLAN-DR). Risk class: **high** (PM Step 1.5) — driven by file count
plus foundation blast radius, not security surface. No auth/secrets/PKI paths, no multi-host
deploy impact.

**Revision 2.** Revision 1 was REJECTed by the critic pass and independently faulted by the
architect pass on the same root cause. Both findings are incorporated below; §"Review history"
records what changed and why.

---

## Context

`agentic-kb` is event-sourced: a JSONL event log is the source of truth, materialized into
SQLite. The standing invariant is `DB == Materialize(events.jsonl)`. Mission deliverable #1 is
*"Evidence-anchored storage: every KB entry cites the source code it claims to describe
(citation + hash verification)."*

Compaction silently destroys evidence rows. The mechanism, verified in code:

1. `compact.rs:174` sorts the retained set by **original log index**. For
   `upsert(A)@0, evidence_add(A,e1)@1, upsert(A)@2`, compaction keeps only the last upsert, so
   the compacted log is `[evidence_add@1, upsert@2]` — evidence first.
2. Rebuild replays into a **fresh** database (`rebuild.rs:299` removes the tmp DB, `:304`
   reopens it), so when `evidence_add@1` replays, `entries` is empty.
3. `db.rs:889-899` is orphan-tolerant: parent absent ⇒ `return Ok(())`. The INSERT never runs.
   The row is gone, silently, with no error.
4. The rebuilt entry still reports `evidence_status='present'`, because the upsert arm takes
   that value verbatim from the event payload (`db.rs:726`, written by
   `add_validation.rs:170-180`) rather than recomputing it.

So the failure is a **position-dependent event replayed at a position where its precondition no
longer holds**, plus a status field that lies about it. Note this corrects Revision 1, which
argued from "upsert does not clear evidence" — true (`db.rs:964` is the only
`DELETE FROM evidence` in the tree; no cascade, no DROP/recreate) but not the mechanism.

### The contract question underneath (ADR-1, decided below)

The spec and the code disagree about what an upsert *means*, and nothing in the codebase
resolves it:

| | `AgentKbEvidence.tla` | Implementation |
|---|---|---|
| upsert vs. evidence | `ApplyEventE` add arm **clears** the entry's evidence set (`tla:200`) | Evidence rows **survive** re-upsert (`db.rs:686-800`) |
| `evidence_status` | Always derived: `StatusOf(kind, evidence)` (`tla:201`) | **Payload-authoritative** on upsert (`db.rs:726`); **recomputed** on evidence_add (`db.rs:927`) |
| expire | Entry becomes absent, evidence **cleared** (`tla:229-236`) | `is_stale=1`; evidence **retained** (`db.rs:806-848` GCs only `entries_fts`, `entries_emb`, `cues`) |

This is why the epic cannot start with a compaction fix. Until the contract is decided, the
correct retention rule is undefined, and any fix trades one divergence for another: reordering
evidence after the upsert preserves the row but makes `evidence_status` recompute to `present`
where the original log yields `missing`. Both orders diverge; they just diverge in different
columns.

### Why all three existing safety nets missed this

| Safety net | Why it cannot express the defect |
|---|---|
| `AgentKbEvidence.tla` `CompactionEquivalenceE` (`tla:429`) | `ApplyEventE`'s add arm clears evidence, so the failing interleaving materializes to `{}` on **both** sides. Passes vacuously. `CompactedLogE` (`tla:283-290`) additionally regenerates from state rather than filtering, and `citation_healed` is absent from `EventActions` (`tla:93`) entirely. |
| `proptest_compact_preserves_live_state` (`compact.rs:623`) | `arb_raw_op()` (`compact.rs:684-693`) generates only `Upsert \| Expire \| Compact` — no evidence ops. **And** its oracle `materialize()` (`compact.rs:695-712`) returns only live entry IDs, so adding evidence ops without extending the oracle still cannot fail. |
| `test_cmd_compact_preserves_healed_citation_paths_through_rebuild` (`compact.rs:503-560`) | Does a real compact→rebuild→evidence round trip and passes — but its fixture has a **single** upsert at index 0, so the retained upsert is already ahead of the evidence. Appending one trailing re-upsert to this fixture is the cheapest failing test for T2. |

Repairing all three is part of the work, not overhead: a fix that leaves them unchanged has
zero regression cover.

---

## ADRs (decided; these gate the task graph)

### ADR-1 — Upsert preserves evidence. `evidence_status` is always derived, never payload-authoritative.

**Decision.** An entry upsert does **not** clear the entry's evidence set (code behaviour is
correct; the spec is wrong). `evidence_status` is **always** recomputed from the evidence row
set on replay; the upsert event's `evidence_status` payload field stops being authoritative.

**Alternatives considered.**
- *Upsert clears evidence (adopt the spec's contract).* Rejected: it makes any metadata-only
  update destructive. `kb add --id X` to fix a summary would silently destroy every citation on
  X — a footgun aimed directly at mission deliverable #1. It would also make the CLI and MCP
  writers behave differently from every incremental-evidence path.
- *Keep `evidence_status` payload-authoritative and have compaction rewrite the retained
  upsert's status to the post-replay value.* Rejected: compaction would have to materialize and
  then edit event payloads, making the log no longer a faithful record of what was written.

**Why chosen.** Deriving status is the only option under which both compaction orderings
converge on the same materialized state, which is what makes `CompactionEquivalenceE`
provable at all. Non-destructive upsert is the safer default for an evidence-anchored store.

**Consequences.** Item 8 (duplicate evidence on re-upsert) becomes a **real** problem rather
than one dissolved by the contract — `kb_core.rs:266` mints a fresh `Uuid::new_v4()` per row
and there is no content-uniqueness index (`db.rs:335-346` has only `PRIMARY KEY(id)`,
`idx_evidence_entry_id`, `idx_evidence_citation_path`, none unique on content). Its dedup key
is decided in ADR-3 and its implementation is deferred.

### ADR-2 — Entry expire GCs its evidence. `CompactionEquivalenceE` is restated as live-state equivalence.

**Decision.** Expiring an entry removes its evidence rows (adopting the spec's semantics, which
`AbsentEntriesClean` at `tla:378` already asserts and the code already violates). And
`CompactionEquivalenceE` is weakened from full-state to **live-state** equivalence.

**Why the weakening is required, not a concession.** `compact.rs:139-141` deliberately drops
entries whose last expire follows their last upsert. Full-state equivalence would therefore be
unsatisfiable — proving it would require compaction to *retain* expired entries, reversing an
intentional design decision (br-joj) and expanding scope enormously. Live-state equivalence is
exactly what the proptest oracle at `compact.rs:695-712` already computes.

**Consequence — this changes T2's retention rule.** With expire GC'ing evidence, the
interleaving `upsert(A)@0, evidence_add(A,e1)@1, expire(A)@2, upsert(A)@3` has `A` live
(`expire_last=2 < entry_last=3`) so `e1` is retained — but `Materialize(original)` no longer
contains `e1`, because expire@2 GC'd it. A naive "emit evidence after the parent's upsert" rule
**resurrects** it. The rule must be index-relative: retain an evidence event at index `i` for
entry `E` only if `i > last_expire_index(E)`. This is why ADR-2 is decided at T0 and not four
tasks downstream.

**Known documented gap.** `fetch_entry_by_id` (`db.rs:1121-1131`) has no `is_stale` filter, so
`kb_get` returns stale entries. Under live-state equivalence, a stale entry's loss through
compact+rebuild remains a known gap rather than an invariant violation. Recorded, not fixed here.

### ADR-3 — Citation ranges: range becomes optional, byte semantics are retained. Implementation deferred to its own epic.

**Decision on direction** (the open question asked of this plan): make the range **optional** —
no range means the whole file — and **keep byte-offset semantics** for explicit ranges.
Reinterpreting existing ranges as line numbers is rejected outright: it would silently
invalidate every citation already recorded, and byte semantics are a deliberate locked decision
(`verification.rs:193-194`: *"Byte offsets, NOT line numbers. (Plan locks this: 'byte range'
semantics per spec AC15.)"*).

**Scope decision: only the doc fix lands in this epic.** The behaviour change moves to a
follow-up epic, because it is a user-visible semantic change with corpus-wide blast radius and
it needs its own review pass:
- The defect is **not** in the Rust. `verification.rs:193-194` is already correct about byte
  offsets. The wrong text is `mcp/lib/agentic_kb_mcp/mcp_server.ex:107` — *"File path and
  optional line range, e.g. src/foo.rs:42-58"* — in the **Elixir** MCP subsystem, wrong on both
  halves (it is a byte range, and it is not optional). Revision 1 targeted `mcp.rs:557-568`,
  which contains no prose about ranges at all.
- Moving the `Err → Unverified` fold into `verify_evidence` changes its return type and touches
  **16** call sites (`stale_check.rs:515`, `cited_by.rs:153`, `db.rs:2196`, plus 13 in
  `verification.rs` tests), and `db.rs:2196` is not a simple fold but an `unwrap_or` nested in
  an `if let Some(root)`.
- The existing `path:0-<filesize>` workaround entries need a migration (emit `citation_healed`
  rewriting `path:0-N` → `path` where `sha256(whole file) == citation_hash`, verifiable before
  emitting) or the "corpus delta" benefit is zero — those entries break with
  `RangeOutOfBounds` (`verification.rs:272-274`) the moment a file's size changes.
- `MAX_RANGE_BYTES` (4 MiB, `verification.rs:32`) < `MAX_FILE_BYTES` (64 MiB,
  `verification.rs:28`), so whole-file citations in that band would hit `RangeTooLarge`.
- Parse ambiguity must be specified: *colon present ⇒ range MUST parse (hard error, as today);
  colon absent ⇒ whole file.* Without this, a typo'd range silently degrades into a whole-file
  citation verified against the wrong bytes.

Only the one-line Elixir doc correction rides in this epic (T2), since the doc is wrong
regardless of which direction is taken.

---

## Work Objectives

1. Decide and encode the storage contract (ADR-1, ADR-2) in a TLA+ model that is faithful
   enough to *fail* on the real defect.
2. Restore live-state `DB == Materialize(events.jsonl)` across compaction for evidence rows.
3. Repair all three safety nets so the defect class is regression-protected.
4. Determine and report what has already been lost from the live corpus.
5. Close the evidence-lifecycle gaps: derived status, expire GC, transaction atomicity.
6. Remove the rebuild-blocking failure mode from a torn event-log tail.

## Guardrails

**Must have**
- Failing test first, every task (repo TDD mandate) — and for compaction defects the test must
  rebuild into a **fresh** DB (as `compact.rs:547-552` does), not apply onto a warm one.
- `proptest` for property-based coverage (crate at `Cargo.toml:55`, v1.11.0).
- TLC run on every `.tla` modification.

**Must NOT have**
- No `bd-3mr.9` or `bd-tx0` scope — both separately tracked.
- No repo-wide `filter_map(|r| r.ok())` sweep. **43** sites exist in `src/` (45 repo-wide); only
  the 3 named in the review are in scope. The other 40 are a separate mechanical epic.
- No new TLA+ module — extend `AgentKbEvidence.tla`.
- No citation-range **behaviour** change (ADR-3); doc fix only.
- No silent skip of malformed *middle* log lines (T3).
- No provenance-graph redesign (see Deferred).

---

## Task Flow

```
T4 (corpus damage audit)  [P1 — MUST record its DB detector before T1 merges]
 └─> T1 ─┐
         │
T0 (contract + spec; ADR-1/2 encoded, TLC counterexample)
 ├───────┴─> T1 (evidence_status always derived)
 │             └─> T2 (compact retention + ordering + docs)  [P0]
 │                   └─> bd-3mr.9 (existing, external)
 └─> T6 (evidence GC on expire / stale-upsert / id churn)

independent lanes:
  T3 (torn-tail tolerance)          └─> bd-3mr.9
  T5 (transactionalize evidence apply arms)
  T7 (error propagation, 3 named sites)

T9 (post-impl) -> T8 (docs)
```

T1 therefore has **two** blockers: T0 (the contract must be decided) and T4 (the DB damage
signal must be captured before T1 destroys it). T4 itself has no blockers and can start
immediately — it is the only P1 that is ready on day one alongside T0.

`bd-3mr.9` (existing, P1) gains dependency edges on **T2 and T3**. The reason in both cases is
**mechanical rework, not a correctness constraint**:

- **T2** rewrites `compact.rs:116-188`, which includes the region around the `fs::rename` at
  `:188` where bd-3mr.9's generation counter or tail hash would land. Doing bd-3mr.9 first means
  redoing it. *(An earlier draft argued that fingerprinting the currently-broken output would
  "bake the defect into the identity check". That does not hold: bd-3mr.9 needs a change
  detector between rebuild phases P1 and P3, and a counter is content-independent while a tail
  hash only has to detect that the log changed. The edge is right; the old rationale was not.)*
- **T3** changes the event-log reader's return type at `rebuild.rs:129,262,319` — and
  `rebuild.rs:319-320` is precisely the Phase-3 catch-up slice bd-3mr.9 exists to fix. This is
  the *stronger* of the two collisions: same lines, not merely the same function.

Open question carried to that task: whether its fingerprint is order-sensitive, since T2
deliberately breaks the global index sort.

**Ready-queue consequence (must be tracked).** bd-3mr.9 is currently the only P1 in `br ready`
and one of only two ready items, and it is an open child of an already-closed epic (`bd-3mr`,
closed 2026-08-15). Once blocked it is surfaced nowhere. It is therefore **re-parented under
this epic**, and T9 carries an explicit unblock check.

---

## Detailed TODOs

### T0 — CONTRACT + SPEC: make `AgentKbEvidence.tla` faithful, force the counterexample — **P0**
`/home/urist/Documents/perso/agentic-kb/.state/agent-kb/tla/AgentKbEvidence.tla` (+ `.cfg`),
agentic branch. *(Note the `.state/` prefix — the repo-root `agent-kb/tla/` holds only an empty
`states/` directory.)*

Encode ADR-1 and ADR-2, then make `CompactedLogE` model the implementation.

**Acceptance**
- `ApplyEventE` add arm no longer clears evidence and no longer derives status from `{}`
  (`tla:198-203`); `AddEvent` (`tla:108-109`) carries what ADR-1 requires.
- `ApplyEventE` gains a `citation_healed` arm; `EventActions` (`tla:93`) includes it.
- expire arm encodes ADR-2 (entry stale-or-absent per the model's abstraction, evidence GC'd).
- `CompactedLogE` models filter-and-retain over original indices — last-upsert-per-entry
  retention, evidence retained by original index, retained set re-sorted — **not**
  regenerate-from-state.
- `CompactionEquivalenceE` restated as **live-state** equivalence per ADR-2.
- `AbsentEntriesClean` (`tla:378`) and `StatusConsistent` (`tla:365`) restated as ADR-1/ADR-2
  require, and still pass.
- **TLC produces a shape-specific counterexample**: the error trace contains, in order,
  `add(e1)`, `evidence_add(e1,v1)`, `add(e1)`, `compact`, and the violated state has
  `evidence[e1] = {}` on one side and `{v1}` on the other. A bare "some counterexample" does not
  satisfy this — it cannot distinguish the real defect from a modelling error introduced by the
  rewrite.
- **`MaxLogLen` bound, verified:** `AgentKbEvidence.cfg:6` sets `MaxLogLen = 4`, and the primary
  counterexample (`add`, `evidence_add`, `add`, `compact`) is exactly 4 events — reachable with
  **zero headroom**. Do not treat "no counterexample" as a pass without checking the bound.
- A second counterexample covers dropped `evidence_expire`. It needs **5** events and is
  therefore **unreachable at the current bound** — raise `MaxLogLen` to 5 for that run and budget
  for the state-space cost. (An explicit modelling note is the fallback if the run proves
  intractable, but attempt the raised bound first.)
- After T1/T2 land, the corrected model satisfies the restated `CompactionEquivalenceE`.

### T1 — `evidence_status` is always derived on replay (ADR-1) — **P0**
`src/components/db.rs:726` (upsert `ON CONFLICT` arm), `src/commands/add_validation.rs:170-180`.
Depends on T0.

**Acceptance**
- Failing test first: replaying `upsert(A, status=present)` with no evidence rows yields
  `evidence_status='missing'`, not `present`.
- The upsert arm recomputes `evidence_status` via `compute_evidence_status` (`db.rs:86-112`)
  instead of taking `excluded.evidence_status`.
- Legacy entries keep `n_a` (the `is_legacy` carve-out survives).
- Both compaction orderings of the T2 repro now converge on the same status — this is the
  property that makes T2 provable, so assert it directly.
- **BLOCKING — T4's DB-detector result is recorded before this task merges.** This change makes
  `evidence_status` derived on replay, so the next rebuild recomputes damaged entries from
  `present` to `missing` and the DB-based damage signal is destroyed. It is destroyed, not
  repaired: the evidence rows do not come back. Merging T1 first would silently discard the only
  direct measurement of user-visible damage. Enforced by a beads edge as well as this line.

### T2 — Compact retention rule, ordering, and the doc corrections — **P0**
`src/commands/compact.rs:116-178` and its test module; `src/models.rs:213`;
`mcp/lib/agentic_kb_mcp/mcp_server.ex:107,122`. Depends on T1.

Merged from Revision 1's T1+T2: they edit the same ~50-line block and the same test helper, so
splitting them meant the second would rewrite the first's code. One change.

**Acceptance**
- Failing test first, rebuilding into a **fresh** DB: compact
  `upsert(A)@0, evidence_add(A,e1)@1, upsert(A)@2`, rebuild, assert `e1` survives and
  `evidence_status` matches the row set. Cheapest form is appending a trailing re-upsert to the
  existing fixture at `compact.rs:503-560`.
- **Retention rule (ADR-2):** retain an evidence event at index `i` for entry `E` only if
  `i > last_expire_index(E)`. Covers the resurrection interleaving
  `upsert@0, evidence_add@1, expire@2, upsert@3`.
- **Emission rule:** retained evidence events are emitted after their parent's retained upsert,
  **preserving relative order among evidence events for that entry**. The relative-order clause
  is load-bearing: `citation_healed` (`db.rs:932-951`) is a bare `UPDATE` that no-ops if it
  precedes its own `evidence_add`.
- `evidence_expire` added to the retention match at `compact.rs:121` and subject to both rules
  above. *(Gate restated without a phase number: this must land before any production caller of
  `evidence_expire_event` exists — `events.rs:70` has none today. It is not gated on roadmap
  Phase 2, which already shipped.)*
- `citation_healed` obeys the same parent-ordering rule and stays ordered after its own
  `evidence_add`.
- **Oracle extended:** `materialize()` (`compact.rs:695-712`) returns evidence rows and
  `evidence_status` per entry, not just live entry IDs. Without this the proptest cannot fail on
  this bug no matter which ops are generated.
- **Generator extended:** `arb_raw_op()` (`compact.rs:684-693`) generates `evidence_add`,
  `evidence_expire`, and `citation_healed` interleaved with `expire`.
- Both extensions demonstrated to **fail on pre-fix code** and pass after.
- **Fixpoint preserved:** compacting an already-compacted log is a no-op (the existing assertion
  at `compact.rs:670-674`, extended to evidence).
- Docs: `models.rs:213` and `mcp_server.ex:122` corrected — `derived_from` holds a parent
  **entry** id (confirmed: `add_validation.rs:86` compares it to `entry_id`, `mcp.rs:1722-1723`
  walks it as an entry edge, `mcp.rs:3580` seeds it with `a_id`). `mcp_server.ex:107` corrected
  per ADR-3: byte range, not optional today.
- Note in the task: `ts` fields become non-monotonic in the compacted log once the global index
  sort is broken. Confirm no consumer depends on monotonic `ts`.

### T3 — Torn-tail tolerance in the event-log reader — **P1**
`src/components/events.rs:119-149`.

**Policy:** tolerate a malformed **final** line only, and **report** the truncation. Do not
blanket-skip unparseable lines — that would mask mid-log corruption, strictly worse than today's
hard-fail, and it would silently shift the Phase-3 slice arithmetic at `rebuild.rs:319-320`
(`snapshot_len` is an *event* count and the reader skips blank lines, so both counts must come
from the same skipping reader).

**Acceptance**
- Failing test first: a log whose last line is a partial `writeln!` reads successfully and
  returns all complete events; a malformed line in the *middle* still errors.
- **Implemented via `BufRead::read_until(b'\n', ...)`, not `lines()`.** `lines()` strips the
  terminator and cannot distinguish a torn tail from a complete-but-corrupt one —
  `{"a":1}\n{"b":` and `{"a":1}\n{"b":\n` produce identical output. Tolerate a failure only on a
  chunk that is both the final chunk **and** does not end in `\n`.
- **Both failure paths covered:** the JSON parse at `events.rs:144` *and* `let line = line?` at
  `events.rs:137`, which yields `InvalidData` when a torn tail splits a multi-byte UTF-8
  sequence. An implementation wrapping only `from_str` still hard-fails there.
- Truncation is surfaced in the return value (not a log warning — a warning is invisible to an
  MCP client).
- Soundness assumption recorded in the doc comment: `append_event` / `append_events_batch`
  (`events.rs:88-116`) write content then `\n` to an unbuffered `File`, so a crash can only
  truncate the tail, never produce a terminated-but-truncated line.
- Doc comment corrected: it claims the function stops "before any partial tail line" when it
  only stops at a **count**; the real guarantee comes from the caller passing `max = snapshot_len`
  under flock (`rebuild.rs:303`).
- **Compact interaction (must not be silent):** `compact.rs:87` reads the log and `:180-188`
  rewrites and renames it, so a tolerated torn tail is permanently erased by the next compact.
  Compact holds the flock (`compact.rs:85`), so this means a crashed prior writer and discarding
  an uncommitted event is defensible — but it must be loud, and the torn bytes are preserved to a
  sidecar before rename.
- Call sites corrected: **24** across **six** files (Revision 1 said ~20 across four, missing
  `retrieval_eval.rs` and `events.rs`). Only **six** are production —
  `rebuild.rs:129,262,319`, `compact.rs:87`, `mcp.rs:811`, `retrieval_eval.rs:195` — the other 17
  are in `#[cfg(test)]` modules. A return-type change is therefore cheap.

### T4 — Audit the live corpus for already-lost evidence — **P1**
New diagnostic. No dependencies of its own, but it **blocks T1** (see ordering constraint below).

Compaction rewrites `events.jsonl` without rebuilding (`compact.rs:186-188`), so loss is latent
until the next rebuild — and `kb add` calls `rebuild_if_schema_obsolete` on every invocation
(`add.rs:78`), so the trigger is routine. Nothing in the epic otherwise answers "has this
already happened, and to how much?"

**ORDERING CONSTRAINT (binding, not advisory): T4's DB detector must run and record its result
before T1 merges.** T4 has two detectors with different shelf lives:

| Detector | Valid | Why |
|---|---|---|
| **DB-based:** entries with `evidence_status='present'` and zero rows in `evidence` | **Only before T1 lands** | T1 makes `evidence_status` derived on replay, so the next rebuild recomputes those entries to `missing` and the signal disappears — the damage becomes invisible, not repaired |
| **Log-based:** `evidence_add` at an index before its entry's retained upsert in `events.jsonl` | Indefinitely | Rebuild-independent; reads the log, not the materialized state |

Losing the DB detector would cost the only direct measurement of *user-visible* damage, so the
constraint is enforced two ways rather than left to reading order: a beads edge (T1 blocked-by
T4) and a matching acceptance line on T1. Do not relax one without the other.

**Acceptance**
- **DB detector, run and recorded before T1 merges:** entries with `evidence_status='present'`
  and zero rows in `evidence`.
- **Log detector:** scans the live `events.jsonl` for any `evidence_add` whose entry's retained
  upsert appears at a **later** index — the signature of an already-compacted log that will lose
  the row on next rebuild. Stays valid after T1; this is the durable half.
- Both counts recorded separately. A DB-detector count that drops to zero after T1 is **not**
  evidence of repair and must not be reported as such.
- Reports counts and affected entry ids; does not mutate.
- **Duplicate evidence rows are NOT scored as damage.** Under ADR-1 (non-destructive upsert)
  duplicates are expected — `kb_core.rs:266` mints a fresh UUID per row and item 8's uniqueness
  index is deferred. The audit must distinguish "duplicated" from "lost".
- Result recorded in the epic and in the KB. If the count is non-zero, a repair task is filed as
  a follow-up (post-compaction, the `ts` ordering in the log is the only surviving evidence of
  the original order, so repair is best-effort by construction). **T9 gates on this:** the epic
  cannot close with a non-zero audit and no filed repair task.

### T5 — Transactionalize the evidence apply arms — **P2**
`src/components/db.rs:878-995` (`evidence_add`, `evidence_expire` arms). **Independent** — no
dependency on T2.

Scope note: `apply_event` is already called in an untransactioned loop by `rebuild.rs:308-311`
and `kb_core.rs:313-315`, so a crash mid-loop leaves partial state that only a rebuild repairs.
T5 narrows one window inside a larger one — worth doing, not worth blocking anything on.

**Acceptance**
- Failing test first: an injected failure between the evidence INSERT/DELETE and the
  `evidence_status` recompute leaves the DB consistent.
- Both arms wrapped in `BEGIN`/`COMMIT` with rollback on error, matching the `upsert`/`expire` arms.

### T6 — Evidence GC on expire, stale-upsert, and id churn (ADR-2) — **P2**
`src/components/db.rs:806-848` (expire arm) and `:738-750` (the `is_stale == 1` upsert branch);
`src/commands/compress.rs:173-196`; `src/components/kb_core.rs:207-224` (`replace_path`).
Depends on T0 (contract).

The `entries_emb` precedent has **two** triggers, not one: `db.rs:829` in the entry-expire arm
and `db.rs:744` in the upsert arm's `is_stale` branch. Evidence needs both. *(Revision 1 read
`db.rs:744` as a second expire site; it is a stale upsert.)*

**Acceptance**
- Failing test first **on the incremental path**: expire an entry, then query `evidence` on the
  live DB — orphan rows present pre-fix. *(Revision 1's criterion routed through compact+rebuild,
  which already passes: compaction drops an expired entry's upsert at `compact.rs:138-146`, so
  its evidence is filtered out anyway. That test cannot fail pre-fix.)*
- Evidence GC'd in the entry-expire arm, inside that arm's **existing** transaction
  (`db.rs:812`/`:847`) — not T5's, which covers different arms.
- Evidence GC'd in the `is_stale == 1` upsert branch (`db.rs:738-750`).
- `compress` (`compress.rs:173-196`, which re-adds evidence under a fresh entry id while
  `replace_path` expires the old entry) strands no rows.
- **Re-verify T2 under the new semantics:** run T2's proptest with `expire` interleaved among
  evidence ops, confirming the index-relative retention rule holds.
- **Dangling-`derived_from` check** (pulled forward from the deferred item 9): confirm GC does not
  create dangling references. If it can, cascade or mark — an acceptance criterion here, not a
  reason to open the provenance work.
- **`fetch_entry_by_id` gains an `is_stale` filter** (`db.rs:1121-1131`), pulled in from the
  deferred list because ADR-2 changes its standing. Before ADR-2 it was an unrelated pre-existing
  gap; after ADR-2 weakens the invariant to live-state, it is the specific reason stale-entry
  evidence loss is classified "known gap" rather than "violation" — while `kb_get` still hands
  those stale entries to agents. Small, and in a file this task already opens.

### T7 — Propagate row-decode errors at the 3 reviewed sites — **P3**
`src/components/kb_core.rs:220` (`existing_ids`), `src/commands/mcp.rs:1731` (provenance parents),
`src/commands/compress.rs:189` (evidence read). Independent.

**Scope is exactly these three.** 43 `filter_map(|r| r.ok())` sites exist in `src/`; the other 40
are out of scope.

**Acceptance**
- Each site propagates or logs the decode error instead of dropping the row.
- Test coverage that a decode failure is observable at each site.
- No behaviour change at the other 40 sites.

### T8 — DOCS
mdBook: the storage contract (ADR-1/ADR-2), compaction retention + ordering rules, torn-tail
policy, evidence lifecycle. Blocked by T9.

### T9 — POST-IMPL
`/post-impl` gates: sync + rebase, spec compliance (`/verify` Covered verdict), code review loop
to zero Critical, user confirmation. Blocks T8.

Two epic-specific checklist lines beyond the standard gates:
- **T4 outcome gate:** the epic may not close with a non-zero damage count and no filed repair task.
- **bd-3mr.9 unblock check:** confirm it is unblocked and still visible in `br ready` once T2 and
  T3 close.

---

## Success Criteria

- TLC produced the shape-specific counterexample before the fix, and passes the restated
  live-state `CompactionEquivalenceE` after.
- `proptest_compact_preserves_live_state` generates evidence ops, compares evidence rows and
  status, and demonstrably failed pre-fix.
- Compact + rebuild into a fresh DB preserves evidence rows and `evidence_status` for in-place
  entry updates, and does not resurrect evidence across an intervening expire.
- Compaction remains a fixpoint.
- The corpus damage audit has run and its result is recorded.
- A torn final line no longer blocks rebuild; a corrupt middle line still errors; erasure by a
  subsequent compact is loud and the bytes are preserved.
- No orphan evidence rows after entry expire, stale upsert, or id churn.
- Zero Critical findings at post-impl review.

---

## Pre-mortem

Three ways this epic fails, and what the plan does about each:

1. **T0 stalls.** The implementer rewrites `CompactedLogE`, TLC finds nothing, and the "blocking
   finding" clause fires with no path forward. *Mitigation:* T0 now names `ApplyEventE:198-203`
   as the primary defect and makes the counterexample shape-specific and bound-aware.
2. **T2 ships, then T0's final acceptance fails.** The status divergence (ADR-1) is discovered
   after the compaction work is done — the expensive ordering. *Mitigation:* T1 lands the derived
   status *before* T2, and asserts convergence directly.
3. **The fix is correct but the corpus is already damaged and nobody looks.** *Mitigation:* T4,
   early and independent.

---

## Deferred (explicitly out of scope, filed as follow-ups)

- **Citation-range behaviour change** — direction decided in ADR-3, implementation deferred to
  its own epic (16 call sites, corpus migration, `MAX_RANGE_BYTES` decision, parse-ambiguity
  rule). Only the Elixir doc fix rides in T2.
- **Item 8 — duplicate evidence on re-upsert.** Contract decided in ADR-1 (upsert preserves
  evidence, so duplicates are real). **Dedup key when taken up: `(entry_id, kind, citation_path,
  citation_hash)`**, as a uniqueness index on `evidence`. Implementation deferred: it changes
  insert semantics on a table this epic is actively restructuring.
- **Item 9 — provenance ergonomics.** `mcp.rs:1661` `handle_provenance`: dangling parents reported
  as roots with no existence check (`mcp.rs:1735-1737`); stale entries walked unfiltered; a cycle
  returns `provenance_cycle_detected` (`mcp.rs:1697-1703`) discarding the whole accumulated
  graph; no repair tooling. Cut entirely — different subsystem, no data loss, four separable
  concerns, and repair needs an `evidence_expire` emitter that does not exist. Belongs to a
  standalone epic against mission deliverable #4. Only the dangling-parent interaction is pulled
  forward, as an acceptance criterion on T6.
- **The other 40 `filter_map(|r| r.ok())` sites** — separate mechanical epic.
- **`kb verify --invariant`** — a live checker comparing the DB against a fresh in-memory
  materialization. Would have caught this bug and would catch regressions. High leverage, but it
  is new capability rather than repair; filed as a follow-up.
*(The `fetch_entry_by_id` `is_stale` filter was on this list in Revision 2 and has been pulled
into T6 at PM gate: ADR-2 changes its standing from unrelated pre-existing gap to the mechanism
that makes the tolerated loss invisible to callers.)*

---

## Review history

Revision 1 → 2, driven by a critic REJECT and a concurring architect pass:

| Change | Reason |
|---|---|
| Added ADR-1 (upsert/status contract) and ADR-2 (expire GC + live-state equivalence) | The epic's task graph was undefined without them; item 8's deferral had hidden the contract question rather than protecting T1 |
| T0 extended from `CompactedLogE` to `ApplyEventE` | The add arm clears evidence (`tla:200`), so the demanded counterexample was **unreachable** — T0 would have stalled at task zero |
| `CompactionEquivalenceE` restated as live-state | Full-state equivalence is unsatisfiable against `compact.rs:139-141`'s deliberate expired-entry drop |
| Corrected the stated mechanism of bug 1 | Revision 1 argued from "upsert does not clear evidence", which argues *against* its own conclusion; the real mechanism is the orphan-tolerant early return at `db.rs:889-899` on a fresh-DB rebuild |
| Added T1 (derived status) | Reordering alone trades a row divergence for a status divergence |
| Retention rule changed to index-relative-to-last-expire | ADR-2's expire GC would otherwise let T2 resurrect evidence |
| Merged old T1+T2; dropped the T2→T5 edge; split old T5 | Same-function tasks were not independently closeable; the transaction edge did not exist (the expire arm has its own transaction) |
| Added "extend `materialize()`" to T2 | The oracle compares only live entry IDs — extending the generator alone changes nothing |
| Added T4 (corpus damage audit) | No task answered whether loss had already occurred |
| T3 respecified on `read_until` | `lines()` cannot distinguish a torn tail from a corrupt one; the UTF-8 `InvalidData` path was uncovered |
| ADR-3 retargeted and descoped | The wrong doc is `mcp_server.ex:107` (Elixir), not `mcp.rs:557-568`; Rust was already correct |
| Counts corrected | 43 `filter_map` sites not 51; 24 reader call sites across 6 files not ~20 across 4; `db.rs:744` is a stale upsert not an expire |
| TLA+ path corrected to `.state/agent-kb/tla/` | Repo-root `agent-kb/tla/` holds only an empty `states/` |

Revision 3 (team-lead review, APPROVE with one revision):

| Change | Reason |
|---|---|
| T4's DB detector made a **binding** precondition on T1, via a beads edge (T1 blocked-by T4) plus a matching T1 acceptance line | T4's "run early — informs everything" was advisory only. Its DB detector (`evidence_status='present'` with zero evidence rows) is valid **only before T1 lands**: T1 makes status derived on replay, so the next rebuild recomputes those entries to `missing` and the signal vanishes. The damage becomes invisible rather than repaired, and the only direct measurement of user-visible loss is lost. Option (b) — dropping the DB detector for the log-based scan alone — was rejected: the DB query is cheaper and measures user-visible damage directly, so it is worth making binding rather than discarding |
| T4's two detectors documented separately with their shelf lives, and a guard added | A DB-detector count dropping to zero after T1 must not be reported as repair |

Open questions carried forward are recorded in `.omc/plans/open-questions.md`.
