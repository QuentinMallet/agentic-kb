# ADR 0001 — FTS5 content='entries' migration: Phase-5 P1 override

**Status:** Accepted
**Date:** 2026-06-11
**Epic:** agentic-kb-fts5-content-table-migration (child of agentic-kb-improvement-catalog)
**Referenced by:** T2 (schema design), T3 (dual-write), T4 (parity verification), T5a (opt-in cutover), T5b (default-flip cutover), T6 (rollback drill), T7 (deprecation)

---

## Context

The current FTS5 table (`entries_fts`) is contentless: it stores only the tokenised index, not the original row text. Every search hit therefore requires a JSON round-trip back to the `entries` table to recover the entry content before it can be returned to the caller. This has two concrete costs:

1. **Latency.** Each search result requires a secondary lookup against `entries`. On a large corpus the shuttle dominates query time.
2. **Storage.** The contentless index duplicates enough token metadata that the on-disk savings are smaller than expected; the `entries` table already holds the canonical text, so the separate FTS copy adds footprint without eliminating the source row.

Migrating to `content='entries'` with triggers eliminates the JSON-shuttle and allows the FTS engine to satisfy queries directly from the content table. The triggers keep the FTS index in sync with row-level mutations without manual double-writes in application code.

### Phase-5 P1 — the rule being overridden

Phase-5 principle P1 states: schema migrations must be additive only (no DROP TABLE, no removal of columns). P1 exists to protect crash safety and rebuild correctness: the rebuild invariant `DB == Materialize(events.jsonl)` must hold across any migration boundary.

Migrating from contentless to `content='entries'` requires replacing the FTS table definition and installing new triggers. This is non-additive. An override requires an explicit ADR with documented mitigations sufficient to preserve the rebuild invariant.

---

## Decision

Migrate the FTS5 table from contentless to `content='entries'` with triggers, via a dual-write transitional state and a parity-gated cutover split into two phases (opt-in then default-flip). Override Phase-5 P1 under the terms of this ADR.

---

## Rationale for the P1 override

P1 was formulated before the JSON-shuttle cost was quantified on a large corpus. The rule is sound in general but overly broad when applied to a FTS auxiliary table whose sole purpose is to accelerate queries over a canonical source table (`entries`) that is itself not being mutated.

The override is bounded:

- The canonical data store (`entries`, `events.jsonl`) is untouched. The rebuild invariant remains provable over those tables.
- The migration path is fully reversible at every stage. Rollback is a single config flip (`KB_FTS_READ_PATH=contentless`), not a re-rebuild.
- Non-additivity is limited to the FTS auxiliary layer and is deferred until after the event-gated deprecation threshold is satisfied.

Future migrations that cite this ADR as precedent must demonstrate equivalent mitigations: a reversible transitional state, a measured parity gate, and an event-gated deprecation path.

---

## Dual-write mitigation

During the transitional state both FTS tables are populated from the same write path:

- `entries_fts` (contentless, existing): populated by existing application code. Read path during the transitional phase.
- `entries_fts_v2` (content='entries', new): populated by row-level triggers installed on `entries` INSERT/UPDATE/DELETE. Application code does not write to this table directly.

Because triggers fire on every row mutation, every `apply_event` write that reaches `entries` automatically populates `entries_fts_v2` without a second application-level write. The dual-write phase eliminates the big-bang risk: there is no point during the migration at which a rollback would require a full corpus rebuild. The old contentless table remains populated and queryable throughout the grace period.

Trigger idempotency is enforced via `INSERT ... ON CONFLICT(id) DO UPDATE`. An event replayed twice during a rebuild produces no duplicate FTS rows.

Divergence detection during the dual-write phase:
- Debug builds: `debug_assert_eq!(old_fts_result, new_fts_result)` on every search.
- Release builds: `tracing::warn!("fts5_dual_write_divergence", ...)` emitted if results differ. No central alerting is wired up; this is local observability only.

---

## Parity-gate definition

Cutover to `entries_fts_v2` as the default read path is blocked until parity is demonstrated on a frozen corpus. The parity gate is a measured property, not a code-review judgement call.

**Corpus requirements:**
- Entries spanning representative content classes: code citations, markdown excerpts, unicode (CJK and emoji), regex-like punctuation.
- Both `entries` rows and associated `evidence` rows, so the full search-result code path is exercised, not only FTS hits.

**Query suite coverage floor:**
- All existing kb-search integration test queries.
- At least 10 queries per token class: ASCII word, ASCII phrase, CJK, emoji, code identifier, markdown punctuation, regex-like punctuation. Queries produced by committed per-class generators, not hand-curated.
- Frequency stratification: 50 top-1% queries, 50 middle-frequency queries, 50 bottom-1% queries (frequency measured against the corpus token distribution).
- At least 20 boolean queries (AND/OR/NOT combinations).
- At least 20 zero-result queries (tokens guaranteed absent from the corpus).

**Acceptance bar:** zero result divergence across the entire query suite. Any single divergent result blocks cutover. There is no partial-pass threshold.

The parity report is emitted as a JSON file (`target/fts5-parity-report.json`) and committed alongside the cutover PR.

---

## Rollback affordance

A single environment variable controls the read path at all times:

```
KB_FTS_READ_PATH=contentless         # use entries_fts (default pre-cutover)
KB_FTS_READ_PATH=content_entries     # use entries_fts_v2 (opt-in and post-default-flip)
```

Because dual-write keeps both tables populated throughout the grace period, flipping `KB_FTS_READ_PATH` back to `contentless` is sufficient to revert the read path without any data loss or rebuild. The rollback affordance is preserved until the deprecation task (T7) removes the old table.

---

## Event-gated deprecation thresholds

The old contentless table is not dropped until all four of the following signals are observed. Thresholds are event-based, not calendar-based:

1. At least 1000 post-cutover writes applied through the dual-write path.
2. Zero rollback invocations: no `KB_FTS_READ_PATH=contentless` flip in any environment since the default-flip task landed.
3. Parity report re-run on the most recent post-cutover corpus shows zero divergence.
4. At least one successful rollback drill (T6) executed against post-cutover data.

The PR that removes the old table records the values of all four signals at the time of the drop.

---

## Rejected alternatives

### Option B — Big-bang migration in a single PR

One PR drops the contentless table, creates `entries_fts_v2`, and runs a full rebuild.

**Steelman:** the rebuild invariant (`DB == Materialize(events.jsonl)`) is always available as a recovery path, so a corrupted FTS table is theoretically recoverable.

**Rejected because:** rebuild can take minutes to hours on a large corpus. During that window the system is unsearchable. A trigger bug discovered post-cutover forces a rebuild against a possibly-tainted event log. The blast radius is the entire search surface for the duration of the rebuild. This option has no transitional state and no rollback affordance short of a full rebuild.

### Option C — Permanent dual-index

Keep the contentless table, add `entries_fts_v2` as a second FTS source, and merge results at query time.

**Steelman:** no migration risk; both indexes coexist without a cutover step.

**Rejected because:** this doubles write amplification on every `apply_event`, doubles the FTS storage footprint, and creates a permanent maintenance liability. The goal of this migration is to reduce the JSON-shuttle overhead and shrink the index footprint. A permanent dual-index defeats both goals.

### Option D — Stay contentless, accept the JSON-shuttle cost

Defer the migration indefinitely and accept the performance and storage costs as a known limitation.

**Rejected because:** the JSON-shuttle cost scales with corpus size. As the KB grows the search latency degrades monotonically. The cost is already quantified in the parent epic's bench results. Deferral is a choice to accept permanent degradation, not a neutral outcome.

---

## Pre-mortem: three failure scenarios

### Scenario 1 — Trigger double-fire during rebuild catch-up

During rebuild's Phase 2 catch-up, the same event can be applied by the rebuild thread and the catch-up phase simultaneously. If the trigger is not idempotent, this fires twice and produces duplicate FTS rows.

**Mitigation:** triggers use `INSERT ... ON CONFLICT(id) DO UPDATE` as the explicit conflict key (T2 acceptance criterion). The parent epic's `concurrent-mcp-rebuild-two-writers` integration test is reused as the regression gate for the dual-write phase (T3 acceptance criterion). Any trigger implementation that fails the idempotency unit test does not land.

### Scenario 2 — Parity divergence on the long tail

The parity test passes on the frozen corpus, but a production-shaped corpus surfaces result divergence on rare token combinations: CJK characters, emoji sequences, regex-like punctuation, or deeply nested markdown.

**Mitigation:** the parity corpus explicitly includes these content classes, and the query suite has per-class coverage floors with committed generators (not hand-curated samples). The parity acceptance bar is zero divergence, not a percentage threshold. The T5b (default-flip) gate re-runs the parity report on the most recent post-opt-in corpus, not the original frozen fixture. Any divergence discovered during the opt-in window blocks the default flip.

### Scenario 3 — Rollback broken in production after cutover

The config flip back to the old FTS table works in test environments, but production has accumulated post-cutover writes the old contentless table never received because dual-write was inadvertently disabled or a trigger silently failed.

**Mitigation:** the dual-write task (T3) includes an integration test that inserts events and asserts both tables return identical results. The rollback drill task (T6) exercises a rollback against a corpus that contains both pre-cutover and post-cutover events, verifying the rolled-back read path against a third reference built by direct replay. The deprecation task (T7) is gated on a successful rollback drill on post-cutover data. Dual-write divergence warnings in release builds (see Dual-write mitigation section) surface any trigger failure before cutover.

---

## Consequences

- Phase-5 P1 has a documented override. Future non-additive schema changes may cite this ADR as precedent if they can demonstrate equivalent mitigations (reversible transitional state, measured parity gate, event-gated deprecation).
- Write amplification doubles during the dual-write phase: every entry upsert touches both FTS tables via triggers.
- Storage footprint temporarily increases during the dual-write phase. The savings targeted by this migration are only realized after T7 (deprecation) removes the old table.
- One extra PR boundary (T5a opt-in / T5b default-flip) is introduced to accumulate real-world search signal before the change is universal.
- After deprecation, the `KB_FTS_READ_PATH` config flag is removed and `entries_fts_v2` becomes the sole FTS table.

## Follow-ups

- After deprecation lands: propose a Phase-5 revision scoping P1's additive-only rule to non-FTS (canonical data) schema changes.
- Audit other locations where the contentless FTS pattern is used. Current assessment: this is the only instance; confirm during T2.
- At deprecation: update this ADR with the four gate signals captured at the time of the drop.

## Deprecation record

**Deprecated:** _pending production gate signals_

Gate signals at drop time (update when `maybe_drop_contentless_fts` fires in production):

| Signal | Required | Actual |
|--------|----------|--------|
| `post_cutover_writes` | ≥ 1000 | — |
| `rollback_invocations` | == 0 | — |
| `parity_rerun_divergence` | == 0 | — |
| `rollback_drill_passed` | == 1 | — |
