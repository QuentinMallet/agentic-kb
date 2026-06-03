# Deep Interview Spec: agentic-kb defensibility — Phase 0 + Phase 1 (citation, opt-in)

## Metadata
- Interview ID: agentic-kb-defensibility-impl-push
- Rounds: 6 (Round 0 topology + Rounds 1–6 ambiguity)
- Final Ambiguity Score: 17.75%
- Type: brownfield
- Generated: 2026-05-30
- Threshold: 0.20
- Threshold Source: default
- Initial Context Summarized: no (input was a single doc + scope statement, in-budget)
- Status: PASSED

## Clarity Breakdown (final, brownfield weights)
| Dimension | Score | Weight | Weighted |
|-----------|-------|--------|----------|
| Goal Clarity | 0.90 | 0.35 | 0.315 |
| Constraint Clarity | 0.85 | 0.25 | 0.213 |
| Success Criteria | 0.70 | 0.25 | 0.175 |
| Context Clarity | 0.80 | 0.15 | 0.120 |
| **Total Clarity** | | | **0.8225** |
| **Ambiguity** | | | **0.1775** |

## Topology
All 5 components confirmed active in Round 0; no deferrals.

| Component | Status | Description | Coverage / Deferral Note |
|-----------|--------|-------------|--------------------------|
| C1 — Schema & migration | active | Add `entries.kind` column; create `evidence` and `audit_runs` tables; backfill legacy `kind=belief`. | Acceptance criteria + rebuild integration covered §Acceptance Criteria 1–6. |
| C2 — Write path (`kb_add`) | active | Accept optional `kind` + `evidence` on MCP and CLI; soft-mandate flag; emit `EvidenceAdd` events for kind=code. | Acceptance criteria 7–13. |
| C3 — Read path (`kb_search`) | active | Return per-result evidence rows with HEAD-byte-hash `verified` boolean. Code citations only in Phase 1. | Acceptance criteria 14–18. |
| C4 — Agent protocol update | active | Update MCP tool description, user CLAUDE.md "Agent knowledge base" section, and per-agent KB write-protocol table rows. | Acceptance criteria 19–22. |
| C5 — Verification cost validation | active | Bench inline verification against current `agent-kb.db`; narrow K-fallback if p95 > 50ms. | Acceptance criteria 23–25. |

## Goal
Ship Phase 0 (data model, no behavior change) plus the opt-in portion of Phase 1 (citation, single-kind) of the agentic-kb defensibility plan. After this push:

1. The KB schema can carry typed entries (`kind` enum) and evidence rows linked to entries.
2. Agents can opt into citing code at `kb_add` time; missing evidence on `belief`/`procedure`/`observation` entries is flagged (not rejected).
3. `kb_search` returns per-result code-citation evidence with a HEAD-byte-hash `verified` boolean, surfacing F1 (truth decay) loudly.
4. Inline verification cost is empirically validated against the live corpus; if it misses budget, the read path degrades gracefully (top-K=3 inline, rest logged).
5. The agent ecosystem (CLAUDE.md + MCP tool description + per-agent write protocols) instructs agents how to cite evidence and what `verified=false` means at retrieval.

This is the smallest unit that defends the headline claim "F1 truth decay is detected on the read path" while leaving Phase 2+ (confidence scoring, contradiction at write, provenance, audit cycle) untouched.

## Constraints

### Decided in interview (locked, non-negotiable in this push)
- **L1. Verification target = HEAD.** `verified` boolean on each evidence row = `sha256(bytes of citation_path at current working-tree HEAD over recorded byte range) == citation_hash`. `citation_sha` is recorded but not used for verification in Phase 1 (kept for future provenance).
- **L2. Enforcement = soft mandate.** `kb_add` accepts optional `evidence`; for `kind ∈ {belief, procedure, observation}` with empty evidence array, emit stderr warning AND set `entries.evidence_status = "missing"`. Never reject the write.
- **L3. Cost harness = real corpus, narrow-K fallback.** Benchmark inline verification against the current `agent-kb.db`. If p95 ≤ 50ms for K=10 → ship inline. If exceeds → narrow inline to K=3, log timings for K=4–10 results, no background worker in this push.
- **L4. Event-log boundary.** `entries.kind` extends the existing `Add` event payload. New event variants `EvidenceAdd` and `EvidenceExpire` carry evidence rows in the JSONL log on the agentic orphan branch. `audit_runs` is DB-only (regeneratable cache; not event-logged).
- **L5. Protocol coverage.** Update three docs: (a) MCP `kb_add`/`kb_search` tool descriptions in `mcp/lib/...`, (b) user CLAUDE.md "Agent knowledge base" section, (c) per-agent KB write-protocol rows for `code-reviewer`, `debugger/tracer`, `executor`, `nixos-expert` (existing rows in user CLAUDE.md).
- **L6. Evidence kinds in Phase 1 = `code` only.** `kb_add` rejects evidence rows with `kind ∈ {test, command, user, derived}` with a clear error message naming Phase 2. Single verification function; all other kinds are explicit Phase 2 work.

### Inherited from the existing system (not changed)
- SQLite + FTS5 + bge-small hybrid retrieval stays. No change to FTS schema or embedding pipeline.
- Event log on agentic orphan branch stays the source of truth. SQLite remains materialization (§10 of plan).
- `kb_rebuild` 3-phase atomic swap stays the rebuild model; new event types must replay cleanly (acceptance criterion 6).
- MCP-only rule for agents: `kb_*` MCP tools, never shell out to `kb` CLI. Implementation work may touch both, but agent-facing protocol stays MCP.

## Non-Goals
- **NG1.** Confidence score (§5.3 of plan). Phase 2 work; not in this push.
- **NG2.** Contradiction detection at write (§5.4, NLI check). Phase 3.
- **NG3.** Truth audit protocol / `/kb-audit` skill (§5.6). Phase 5.
- **NG4.** Provenance graph (`kb_provenance`, §5.7). Phase 5.
- **NG5.** Evidence kinds other than `code` — `test`, `command`, `user`, `derived` semantics deferred to Phase 2 with explicit "Phase 2" rejection messages from `kb_add` in Phase 1.
- **NG6.** Background-verification worker. If C5 cost validation fails the inline budget, the fallback is narrow-K (top-3 only), not async verification. Async verification is Phase 2 design work.
- **NG7.** Mandatory evidence (§5.2 "MUST have ≥1 evidence row"). Phase 4. Phase 1 ships soft mandate (L2).
- **NG8.** Backfill / migration prompting to retro-cite legacy entries (§8 Phase 4). Phase 4.
- **NG9.** Cross-machine audit history sync. Audit runs are DB-only by L4; not synced.

## Acceptance Criteria

### Schema & migration (C1)
- [ ] **AC1.** Migration adds `entries.kind TEXT NOT NULL DEFAULT 'belief' CHECK(kind IN ('observation','belief','procedure','convention','memory'))`. Legacy entries get `kind='belief'`.
- [ ] **AC2.** Migration adds `entries.evidence_status TEXT NOT NULL DEFAULT 'n/a' CHECK(evidence_status IN ('missing','present','n/a'))`. Legacy entries get `evidence_status='n/a'`.
- [ ] **AC3.** Migration creates `evidence` table per §5.2 schema (`id`, `entry_id` FK, `kind` (TEXT, CHECK code|test|command|user|derived; only `code` accepted at write in Phase 1), `citation_path`, `citation_sha`, `citation_hash` NOT NULL, `citation_excerpt`, `derived_from`, `recorded_at`). Index on `entry_id`.
- [ ] **AC4.** Migration creates `audit_runs` table (`entry_id`, `audited_at`, `verdict`, `evidence_ref`). DB-only; no event-log writes.
- [ ] **AC5.** New event variants `EvidenceAdd` (carries one evidence row) and `EvidenceExpire` (carries evidence id + reason) wired through `src/components/events.rs`. JSONL serialization matches existing event style.
- [ ] **AC6.** `kb_rebuild` 3-phase pipeline replays `EvidenceAdd`/`EvidenceExpire` events and reconstructs `evidence` table identical to pre-rebuild snapshot. Does not touch `audit_runs`. Proptest: arbitrary sequence of `Add`/`EvidenceAdd`/`EvidenceExpire`/`Compact` → DB state equals direct reduction.

### Write path (C2)
- [ ] **AC7.** `kb_add` MCP tool (`src/commands/mcp.rs`) accepts optional `kind` (string, default "belief") and optional `evidence` (array of objects).
- [ ] **AC8.** `kb_add` CLI (`src/commands/add.rs`) accepts `--kind` and `--evidence` (repeatable JSON arg, or `--evidence-file path.json`).
- [ ] **AC9.** Write-time validation: `kind` matches the entry-kind enum; each evidence row's `kind` is `"code"` (other kinds rejected with `Phase 1 ships code only; <kind> deferred to Phase 2`).
- [ ] **AC10.** Soft mandate: if `kind ∈ {belief, procedure, observation}` AND evidence is missing/empty, write proceeds, set `evidence_status='missing'`, emit stderr warning `kb: entry <id> kind=<kind> has no evidence; evidence_status=missing`. Otherwise set `evidence_status='present'` (when evidence supplied) or `'n/a'` (other kinds).
- [ ] **AC11.** For each evidence row, persist exactly as supplied; do not re-hash file at write time (the agent's recorded `citation_hash` IS the write-time claim).
- [ ] **AC12.** `kb_add` writes one `Add` event (with kind field) plus N `EvidenceAdd` events atomically (single JSONL append region; either all events land or none).
- [ ] **AC13.** Unit + proptest coverage: arbitrary `kind` + evidence combinations round-trip through events.jsonl → DB.

### Read path (C3)
- [ ] **AC14.** `kb_search` MCP tool returns each result with an `evidence` array containing all of that entry's evidence rows.
- [ ] **AC15.** Each returned evidence row carries a computed `verified` boolean = (file at `citation_path` at HEAD exists AND `citation_hash == sha256(bytes[start..end])`).
- [ ] **AC16.** If `citation_path` references a missing file, the range is out of bounds, or any I/O error occurs → `verified=false` (never panic, never partial-fail the search).
- [ ] **AC17.** Verification runs in parallel across top-K results (default K=10; configurable). Implementation may use a thread pool or rayon. Must not block on disk for unrelated results.
- [ ] **AC18.** Verification budget is enforced as the C5 fallback (AC25): if measured p95 > 50ms, narrow to top-K=3 inline + log remaining timings without verification.

### Agent protocol update (C4)
- [ ] **AC19.** MCP tool descriptions for `kb_add` and `kb_search` updated in the Elixir MCP layer (`mcp/lib/agentic_kb_mcp/`) to document `kind`, `evidence`, and the `verified` flag on results.
- [ ] **AC20.** User CLAUDE.md "Agent knowledge base" section adds a Phase 1 subsection explaining: kind enum, code-only evidence in Phase 1, soft-mandate warning behavior, `verified=false` meaning at read time.
- [ ] **AC21.** Per-agent KB write-protocol table rows for `code-reviewer`, `debugger/tracer`, `executor`, `nixos-expert` each get a "Phase 1 evidence example" code block showing a representative `kb_add` call with `kind` + one `code` evidence row.
- [ ] **AC22.** A SQL query is documented in the CLAUDE.md "Agent knowledge base" section that returns the current `evidence_status='missing'` rate, e.g. `SELECT COUNT(*) FILTER (WHERE evidence_status='missing') * 1.0 / COUNT(*) FROM entries WHERE kind IN ('belief','procedure','observation');`

### Verification cost validation (C5)
- [ ] **AC23.** A `cargo bench` (or `criterion`) benchmark in `benches/verification.rs` exercises `kb_search` end-to-end against the current `agent-kb.db` with `K=10`. Records p50/p95/p99 wall-clock.
- [ ] **AC24.** Benchmark gates on p95 ≤ 50ms; reports actual measured number in commit message.
- [ ] **AC25.** If p95 > 50ms, ship `kb_search` with `inline_verify_k=3` config (top-3 verified inline; results 4–K returned with `verified=null` and a `timing_log` field per result). Otherwise ship with inline_verify_k=10.

## Assumptions Exposed & Resolved

| Assumption | Challenge | Resolution |
|------------|-----------|------------|
| "§10 says event-log is source-of-truth for everything new." | Round 4 contrarian: derived/computed state doesn't need to be in the event log. | Audit runs are DB-only cache; evidence rows ARE eventful (L4). |
| "Phase 1 'opt-in' means agents can ignore the new fields with no signal." | Round 2: §5.2 says ≥1 evidence row is required for belief/procedure/observation — direct contradiction with §8 "opt-in". | Soft mandate (L2): optional at write, flagged when missing. Resolves the contradiction by adding `evidence_status` as a measurable signal. |
| "Verification means checking the cited bytes match what was recorded." | Round 1: against what version — HEAD or `citation_sha`? Different semantics; different value. | HEAD-only (L1). `verified=false` means "code rotted under the citation" — the exact signal the plan exists to detect. |
| "Doc §9 cost claim (<50ms p95) is good enough; ship inline." | Round 3: reality might miss; what's the architectural fallback? | Real-corpus benchmark; narrow-K fallback (L3). No background worker. |
| "All 5 evidence kinds from §5.2 must ship in Phase 1." | Round 6 simplifier: only `code` has full semantics in §5.5; others are underspecified. | Phase 1 = `code` only (L6). Others explicit Phase 2 with rejection error messages. |
| "Protocol update is implicit — agents will discover via tool description." | Round 5: where do the instructions actually land? | Three writes (L5): tool desc + global CLAUDE.md + per-agent rows. Highest first-week adoption. |

## Technical Context

### Brownfield map (from `explore` + direct file inspection)
- **Entry schema:** `src/models.rs:42` (Rust struct) + `src/components/db.rs:40` (SQLite DDL). Existing columns: `id, path, summary, content, tags, version_ref, permanent, is_stale, created_at, updated_at`. Embeddings in separate `embeddings` table (`db.rs:53`), not as an Entry column (the doc §2 said otherwise — minor doc inconsistency, ignored).
- **Event log:** `src/components/events.rs` — JSONL writer. Existing events: `Add`, `Expire`, `Compact`. New variants `EvidenceAdd` and `EvidenceExpire` to be added here.
- **kb_add entry points:** MCP `src/commands/mcp.rs` (950 LOC) and CLI `src/commands/add.rs` (449 LOC). Both must accept new parameters.
- **kb_search entry points:** MCP `src/commands/mcp.rs` and CLI `src/commands/search.rs` (315 LOC).
- **kb_rebuild:** `src/commands/rebuild.rs` (226 LOC) — 3-phase atomic swap. AC6's proptest is the gate.
- **MCP layer:** Elixir at `mcp/` (Mix project). Tool descriptions in `mcp/lib/agentic_kb_mcp/`. Updated for AC19.
- **User CLAUDE.md location:** `~/.claude/CLAUDE.md` (global; loaded as `claudeMd` in this session). Has an "Agent knowledge base" section and per-agent KB write-protocol table — both editable per AC20/AC21.

### Migration safety
- All new columns have defaults; existing rows get defaults at `ALTER TABLE` time. No data loss.
- `evidence` and `audit_runs` are new tables; `IF NOT EXISTS` guards.
- `kb_rebuild` must replay new event types without error from JSONL files that pre-date this push (legacy events have no evidence; rebuild produces zero `evidence` rows for legacy entries — they retain `evidence_status='n/a'` until they're rewritten with `kind`).

### Cost model
- Per-write: 1 `Add` event + N `EvidenceAdd` events (typically N=1–3). JSONL append. ~1ms.
- Per-read: existing FTS+cosine + per-evidence file open + sha256 over byte range. Doc §9 claims <50ms p95 for K=10 verifications. AC23–25 measure.
- Storage: evidence table ~3–5× entry table by row count (most entries get 1–3 evidence rows). For 10k entries: ~1MB additional.

### File touch list (estimated)
- `Cargo.toml` — possibly add `criterion` to `[dev-dependencies]` for AC23
- `src/models.rs` — extend Entry struct with `kind`, `evidence_status`; new `Evidence`, `AuditRun` structs
- `src/components/db.rs` — schema additions, migrations
- `src/components/events.rs` — `EvidenceAdd` / `EvidenceExpire` event types
- `src/commands/add.rs` — CLI flags + soft-mandate logic
- `src/commands/search.rs` — return evidence rows; verification call site
- `src/commands/mcp.rs` — MCP signature changes for `kb_add` and `kb_search`
- `src/commands/rebuild.rs` — replay new event types
- new: `src/components/verification.rs` — `verify_evidence(row, repo_root) -> bool`
- new: `benches/verification.rs` — criterion bench
- `mcp/lib/agentic_kb_mcp/...` — tool descriptions
- `~/.claude/CLAUDE.md` — Agent knowledge base section + per-agent table rows
- Doc updates in `docs/` (mdbook) if applicable

## Ontology (Key Entities, final round)

| Entity | Type | Fields | Relationships |
|--------|------|--------|---------------|
| Entry | core domain | id, path, summary, content, tags, version_ref, permanent, is_stale, **kind**, **evidence_status**, created_at, updated_at | has many Evidence; cited by AuditRun |
| Evidence | core domain (new) | id, entry_id (FK), kind (Phase 1: "code" only), citation_path, citation_sha, citation_hash, citation_excerpt, derived_from, recorded_at | belongs to Entry |
| EntryKind | supporting enum | observation \| belief \| procedure \| convention \| memory | applied to Entry |
| EvidenceKind | supporting enum | code \| test \| command \| user \| derived (Phase 1 accepts code only) | applied to Evidence |
| EvidenceStatus | supporting enum (new) | missing \| present \| n/a | applied to Entry |
| Citation | supporting (Phase 1 = code-only shape) | path (file:start-end), sha (git), hash (sha256 of bytes) | embedded in Evidence |
| AuditRun | supporting (DB-only) | entry_id, audited_at, verdict, evidence_ref | references Entry; not event-logged |
| Verified flag | supporting (computed, not persisted) | bool | computed per Evidence at read time |
| HEAD-byte-hash verification | procedure (computed) | input: Evidence; output: bool | applied to code evidence at read |
| EvidenceEvent | supporting (event variants) | EvidenceAdd(row) \| EvidenceExpire(id, reason) | appended to JSONL event log |

## Ontology Convergence
| Round | Entity Count | New | Changed | Stable | Stability Ratio |
|-------|-------------|-----|---------|--------|----------------|
| 1 | 8 | 8 | - | - | N/A |
| 2 | 9 | 1 (EvidenceStatus) | 0 | 8 | 89% |
| 3 | 9 | 0 | 0 | 9 | 100% |
| 4 | 10 | 1 (EvidenceEvent) | 0 | 9 | 90% |
| 5 | 10 | 0 | 0 | 10 | 100% |
| 6 | 10 | 0 | 0 | 10 | 100% |

Converged across rounds 5–6 (two consecutive 100% rounds).

## Interview Transcript
<details>
<summary>Full Q&A (Round 0 + 6 scored rounds)</summary>

### Round 0 — Topology confirmation
**Q:** Five top-level components proposed (schema/migration, write path, read path, agent protocol, cost validation). Is this the right shape?
**A:** Looks right — all 5 active.

### Round 1 — Read path / verification target
**Q:** Verification = HEAD bytes vs `citation_hash` (rot detector) or `git show <citation_sha>:path` (integrity check) or both?
**A:** Option A — HEAD only.
**Ambiguity:** 61.25% (Goal min 0.45, Constraints min 0.30, Criteria min 0.20, Context min 0.55)

### Round 2 — Write path / enforcement model
**Q:** §5.2 says evidence required; §8 says Phase 1 is opt-in. Pick: pure opt-in / soft mandate (warn + flag) / hard mandate / per-kind tiered.
**A:** B — Soft mandate.
**Ambiguity:** 46% (worst component C5 cost validation now isolated)

### Round 3 — C5 cost validation harness
**Q:** Real corpus + narrow-K fallback / synthetic stress + background-worker / both + warning flag / skip and instrument.
**A:** A — real corpus, narrow K on fail.
**Ambiguity:** 32.5% (worst component C1 schema)

### Round 4 — Contrarian: event-log boundary
**Q:** §10 says everything event-logged. Counter: derived state shouldn't be. Where does the boundary sit for `entries.kind` / `evidence` / `audit_runs`?
**A:** B — Evidence event-logged, audit_runs DB-only.
**Ambiguity:** 27.75% (worst component C4 protocol)

### Round 5 — Protocol-update placement
**Q:** MCP tool desc / global CLAUDE.md / per-agent rows / new /kb-cite skill — which combinations ship?
**A:** B — tool desc + CLAUDE.md + per-agent rows.
**Ambiguity:** 21.75% (worst component C3 read path)

### Round 6 — Simplifier: evidence-kind scope
**Q:** §5.2 names 5 evidence kinds. Which subset is the simplest defensible Phase 1?
**A:** A — `code` only.
**Ambiguity:** 17.75% ≤ 20% threshold met.

</details>
