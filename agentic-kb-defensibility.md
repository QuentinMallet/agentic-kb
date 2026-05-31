# agentic-kb: scientific defensibility plan

**Status:** Phase 1–2 shipped; Phase 5 shipped (br-ei2)
**Scope:** changes to `agentic-kb` (Rust core + Elixir MCP) and to the AGENTS.md.in protocol that governs agent KB writes/reads.
**Goal:** raise the epistemic standing of KB outputs from "log of past agent beliefs" to "evidence-grounded knowledge with quantifiable trust."

---

## 1. Premise

The KB is queried before nearly every non-trivial agent decision and is written to after most non-trivial agent sessions. Its contents therefore shape:

- agent priors at session start,
- routing decisions inside skills (`/kb-explorer`, `/post-impl`, etc.),
- which historical context surfaces in `kb_search` hits used as context.

Because reads condition writes (agents are prompted to "query KB first"), the system has a positive-feedback structure. Without truth-preserving mechanisms, drift accumulates. The current design has no such mechanisms; defensibility requires adding them.

This document does not propose replacing the current system. The FTS5 + bge-small hybrid retrieval, the event-log + SQLite materialization model, the MCP transport, and the agentic-branch storage are kept. The proposal adds an evidence layer, a confidence model, and a contradiction layer on top.

---

## 2. Current state (factual)

Entry schema (from `src/models.rs`):

| Field | Purpose |
|-------|---------|
| `id` | ULID |
| `path` | category/topic path |
| `summary` | one-line summary |
| `content` | body (≤10K chars) |
| `tags` | array of strings |
| `version_ref` | git SHA at write time |
| `permanent` | survives compaction |
| `is_stale` | soft-delete marker |
| `embedding` | f32 vector, bge-small |

Retrieval is hybrid FTS + cosine. Staleness is detected by `kb_stale_check`, which is a per-file timestamp comparison plus an optional commit-SHA pass. Ownership rules (`code-reviewer` writes `conventions/*`, etc.) are documented in AGENTS.md.in but not enforced in the DB.

Writes have no evidence requirement, no contradiction check, no source attribution beyond `version_ref`. Reads have no confidence filter — top-K by hybrid score wins regardless of trust.

---

## 3. Failure surface

Concrete failure modes that have no current mitigation:

**F1. Truth decay without path change.** Function rewritten inside the same file: stale-check passes, entry "X uses JWT" survives even after X switches to Paseto. Confidence in entry: unchanged. Severity: high — interpretive entries (architecture, conventions) are the highest-trafficked.

**F2. Contradictory siblings.** "X uses Redis" and "X does NOT use Redis" embed adjacent, both retrieved, no resolution. Agent picks whichever appears first in context. Severity: high.

**F3. Provenance flattening.** An entry derived from "I ran the test and observed Y" and an entry derived from "I skimmed the docstring and guessed Y" present identically. Different evidence strengths get equal retrieval weight. Severity: medium.

**F4. Belief inheritance.** Agent A writes wrong entry. Agent B queries KB, sees entry, conditions on it, writes another entry citing the conclusion. Two entries now corroborate each other — yet they share a single false root. No causal trace exists to find the root. Severity: high (because corroboration count is a natural-feeling trust signal).

**F5. Citation rot.** Entry references `lib/foo.ex:42`. File still exists, function renamed and moved. Path-based stale-check misses; entry is wrong but reads as fresh. Severity: medium.

**F6. Tag/path collisions.** Two agents add entries at `architecture/auth` with overlapping scope but different conclusions. No conflict surfaced. Severity: medium.

**F7. Coverage opacity.** Cache-miss is detectable; cache-hit-on-wrong-question is not. Agents have no way to know whether the corpus answers their question. Severity: medium.

---

## 4. Defensibility criteria

The KB's outputs are defensible when:

**C1.** Every belief entry cites concrete evidence (commit SHA + file:line range, test output hash, or user statement transcript). Citations are machine-verifiable.

**C2.** Every retrieval result carries a quantitative confidence score derived from evidence freshness, corroboration provenance, age, and source weight — not from FTS rank alone.

**C3.** Contradictions are detected at write time. The author must explicitly supersede, fork, or accept the contradiction with a recorded reason.

**C4.** Provenance is recoverable: the causal chain from an entry to its grounding evidence (and to the prior entries it built on) is queryable.

**C5.** Periodic audit produces a calibration score: of N entries sampled and re-verified, what fraction are still true? This score is published and drives confidence priors.

**C6.** Entries are typed by epistemic kind. Different kinds have different write rules and decay rates.

These criteria are testable. C5 in particular gives a measurable defensibility metric.

---

## 5. Proposed mechanisms

### 5.1 Epistemic types

Add a required `kind` enum on every entry:

| Kind | Definition | Decay model | Citation required |
|------|------------|-------------|-------------------|
| `observation` | Direct observation of system behavior (test output, log line, command output) | Slow; cite the observation artifact | yes (hashed artifact) |
| `belief` | Interpretive claim about the system | Medium; tied to cited code freshness | yes (code citation) |
| `procedure` | Runbook, recipe, step sequence that produced an outcome | Slow if procedure tested recently | yes (test_id or last successful run) |
| `convention` | Normative rule ("we always X") | Owner-only writes; superseded explicitly | author + reasoning |
| `memory` | Raw session trace fragment | No retrieval by default; opt-in via flag | none |

Each kind gets a different retrieval policy. By default, `kb_search` returns `observation + belief + procedure + convention`; `memory` is excluded unless explicitly requested.

### 5.2 Evidence schema

Add an `evidence` table:

```sql
CREATE TABLE evidence (
  id TEXT PRIMARY KEY,                   -- ULID
  entry_id TEXT NOT NULL,                -- FK to entries
  kind TEXT NOT NULL,                    -- code | test | command | user | derived
  citation_path TEXT,                    -- file:start-end for code, test_id for test
  citation_sha TEXT,                     -- git SHA at evidence-write time
  citation_hash TEXT NOT NULL,           -- sha256 of the cited bytes
  citation_excerpt TEXT,                 -- optional snippet for human review
  derived_from TEXT,                     -- parent entry_id when kind=derived
  recorded_at INTEGER NOT NULL
);
```

Rules:

- Every `belief`, `procedure`, `observation` entry must have ≥1 evidence row.
- On read, `kb_search` re-verifies citation_hash for the most recent evidence (cheap: open file, hash range, compare). Mismatch → entry returned with `verified: false` flag, not filtered out — that's the reader's call.
- `derived` evidence (entry was written based on another entry, not direct observation) is allowed but carries lower confidence weight and exposes F4 directly.

### 5.3 Confidence score

**Shipped implementation (Phase 5, br-ei2):** Beta(1,1) posterior from audit history:

```
confidence = (successes + 1) / (successes + failures + 2)
```

- Fresh entry (no audits): `0.5` — uninformed prior
- After 1 pass: `2/3 ≈ 0.667`
- After 10 passes, 0 failures: `11/12 ≈ 0.917`
- Per `(kind × session_id)` — global fallback via `COALESCE(session_id, '__GLOBAL__')`

This is the Laplace smoothing formula. It is provably in [0,1] for all non-negative integer inputs (see TLA+ spec `agent-kb/tla/audit/Audit.tla`, invariant `ConfidenceInUnitInterval`).

**Original composite formula (not yet shipped; deferred to Phase 2+):**

```
confidence = w_e * evidence_freshness
           + w_c * corroboration_score
           + w_a * age_decay
           + w_s * source_weight
           - w_x * contradiction_penalty
```

The composite formula requires calibrated weights, an NLI contradiction pass (Phase 3), and age-decay data. The Beta posterior was shipped first because it is measurable immediately from audit data alone.

### 5.4 Contradiction detection at write

`kb_add` becomes a two-step:

1. Compute embedding for the new entry.
2. Cosine-search top-K (K=10) existing entries with overlapping `path` or `tags`.
3. For each candidate, run a lightweight NLI check (small local model, or a single cheap LLM call): does the new entry contradict the candidate?
4. If contradictions found, `kb_add` returns a `contradiction` response listing conflicting entry_ids. Agent must:
   - Supply `supersedes: [<id>, ...]` to mark old entries stale with reason, OR
   - Supply `acknowledged_contradictions: [<id>, ...]` with a `reason` string to allow coexistence, OR
   - Abandon the write.

This defeats F2 at write time, not at read time. NLI cost is small (one model call per write, batched).

### 5.5 Citation verification on read

When `kb_search` returns an entry, the MCP server resolves each evidence citation:

- For code citations: open file, hash bytes at the recorded range, compare to `citation_hash`.
- For test citations: look up the test_id's last run; verify it's still passing.
- For user citations: no verification (text is the artifact).

Verification is cheap (file open + hash) and runs in parallel for the top-K results. Results include a `verified: bool` field per evidence row. Entries with all-verified evidence get a confidence boost; entries with broken citations get a penalty.

This handles F1 and F5 — content changes inside an unchanged file path are now detected.

### 5.6 Truth audit protocol ✓ Shipped (Phase 5, br-ei2)

A new skill `/kb-audit` (`skills/kb-audit/SKILL.md`) runs on demand or weekly:

1. Sample N entries (live, `evidence_status='present'`), clamped to 1–50 via `audit_run` MCP method.
2. Caller verifies each entry against current repo state and submits verdicts via `audit_record`.
3. Results recorded in `audit_runs` table: `(run_id, entry_id, audited_at, verdict, evidence_ref)`.
4. Per `(kind × session_id)` precision = fraction `verdict=true`. Published via `audit_report`.
5. Per-kind precision feeds `source_weights`, which drives the Beta posterior confidence field on `kb_search` results.

**Done:** `audit_run` + `audit_record` + `audit_report` MCP methods shipped; `source_weights` table seeded by audit verdicts; `confidence` + `audit_n` fields on every `kb_search` result.

### 5.7 Provenance chain ✓ Shipped (Phase 5, br-ei2)

`derived_from` in evidence rows already enables this. MCP method `provenance`:

```
kb_provenance(entry_id, max_depth?) -> { roots: [entry_id...], graph: [{from, to}...], truncated: bool }
```

Returns the DAG of entries that this entry's evidence chain depends on, terminating at observation-kind entries (true roots). F4 becomes visible: "this belief rests on 2 distinct roots" vs "this belief rests on 1 root cited by 5 others."

Cycle detection uses iterative DFS (Enter/Leave frames) — diamonds (shared ancestry) are handled by the `visited` set; true back-edges are caught by the `in_progress` set and return `{error: "provenance_cycle_detected"}`. Max depth default 64, ceiling 1024.

**Done:** `provenance` MCP method shipped; cycle detection verified; `kind=derived` entries no longer blocked at `kb_add` gate.

---

## 6. API additions

Backward-compatible. Existing tools unchanged in signature; new optional fields and new tools:

```
kb_add(..., kind, evidence, supersedes?, acknowledged_contradictions?, session_id?)
  -> { id, contradictions?: [...] }

kb_search(..., min_confidence?, kinds?: [...], require_verified?: bool)
  -> [{ entry, score, confidence, audit_n, evidence: [...verified] }, ...]

kb_provenance(entry_id, max_depth?) -> { roots, graph, truncated }

audit_run(sample_size?) -> { run_id, samples }
audit_record(run_id, verdicts) -> { recorded, expired }
audit_report() -> { per_kind_session_precision, last_run_at, total_runs }
```

Existing entries (no `kind`, no `evidence`) get default `kind = "belief"` and treated as evidence-less (confidence defaults to 0.5 until first audit). A migration sweep can prompt the operator to retroactively add evidence to the highest-traffic legacy entries.

---

## 7. Validation plan

Defensibility is now testable:

**V1. Calibration improves over time.** After 6 weeks of operation, weekly C5 audits should show per-kind precision increasing (or plateauing at a measured ceiling), not random walk. Plot precision × time per kind.

**V2. Contradiction rate at write is non-trivial and falling.** Track the rate at which `kb_add` finds contradictions and how the author resolved them (superseded / acknowledged / abandoned). Should rise initially (catching backlog), then fall as the corpus self-cleans.

**V3. Verified-citation rate is high and stable.** Of evidence rows referenced by retrieval, fraction `verified: true` at read time. Should be >0.9 for active areas of the corpus. Drops indicate citation rot accumulating.

**V4. High-confidence entries audit higher.** Stratify the audit sample by confidence quartile. Top quartile should audit ≥0.9 precision; bottom quartile should be lower. If they're equal, the confidence formula is wrong.

**V5. Provenance roots are diverse.** Average distinct-root count per high-confidence entry. Should be ≥2. If it's 1, the corpus is over-relying on single sources.

Each metric is a query against the existing SQLite + audit_runs tables. A new dashboard tab in claude-smart-router's LiveView (it already runs Phoenix) can render these. No new infrastructure required.

---

## 8. Phased rollout

| Phase | Description | Status |
|-------|-------------|--------|
| **Phase 0** | Data model, no behavior change. `kind`, `evidence`, `audit_runs` tables. Default `kind="belief"`. `kb_search` unchanged. | ✓ Shipped |
| **Phase 1** | Citation opt-in. Agents supply `evidence` on `kb_add`. `kb_search` returns `verified` flag per evidence row. Confidence not yet shown. | ✓ Shipped |
| **Phase 2** | Confidence in results. Beta(1,1) `confidence` + `audit_n` fields on `kb_search`. `session_id` on entries; per-(kind×session_id) source weights. | ✓ Shipped (br-ei2) |
| **Phase 3** | Contradiction at write. `kb_add` runs NLI check. Agents must resolve. Update AGENTS.md protocol. | Planned |
| **Phase 4** | Mandatory evidence. Evidence required for `kind ∈ {observation, belief, procedure}`. Backfill skill for high-traffic legacy entries. | Planned |
| **Phase 5** | Provenance + audit cycle. `kb_provenance` API. `/kb-audit` skill. Feedback loop closed. | ✓ Shipped (br-ei2) |

Each phase ships and stabilizes before the next. Phase 3 is the expensive one (NLI per write).

---

## 9. Cost analysis

**Storage:** evidence table is ~5x entry table by row count; small file hashes + paths. <1MB per 10k entries.

**Per-write cost:**
- Phase 3 NLI check: one local-model call (~50ms with `fastembed_port` neighbor + a tiny NLI head) or one cheap LLM call. Budget: <200ms p95.
- Evidence hashing: file read + sha256 over the cited range. <10ms.

**Per-read cost:**
- Citation verification on top-K: K parallel file reads + hashes. <50ms for K=10.
- Confidence formula: arithmetic on already-fetched rows. <1ms.
- Source weights prefetch: one JOIN query with dynamic IN clause; does not add per-row subqueries.

**Per-audit cost:**
- Verifier subagent per sampled entry. Bounded by sample size (e.g., 20 entries/week × small-model verifier = trivial).

Total operational cost: small. Most of the cost lives at write time, which is fine — writes are far less frequent than reads.

---

## 10. Non-goals

- Replacing the hybrid retrieval. FTS+cosine stays.
- Distributed consensus. Single-source-of-truth remains the agentic event log; SQLite is materialization.
- Real-time contradiction across machines. NLI check at write is local; if two agents on two desktops write contradictions concurrently, contradiction is detected on the next sync + audit pass, not at write.
- Formal proof of correctness. The system is an empirical knowledge system, not a theorem prover. Confidence is calibrated empirically, not derived.
- Eliminating the operator. Operator remains in the loop for ownership rule enforcement, kind assignment edge cases, audit verdict overrides.

---

## 11. Open questions

1. **NLI provider.** Local small model (cheap, possibly imprecise) or cheap LLM call (precise, network)? Probably small local model behind `fastembed_port`-like wrapper.
2. **Weights in the confidence formula.** Initial values are guesses. The C5 audit is the calibration. Bayesian update of weights based on audit precision is a Phase 5+ refinement.
3. **Backfill prompting.** How aggressive should the migration be? Possibly opt-in: a `/kb-backfill <path>` skill the operator invokes per area.
4. **Per-project vs cross-project source weights.** A `code-reviewer` finding in machines_conf is weak evidence for a normatix architectural claim. The current MEMORY.md / KB split implicitly handles this by path scoping; the confidence formula may need a "scope match" multiplier.
5. **Audit verifier independence.** If the verifier is the same Claude model that wrote the entry, audit is correlated noise, not independent measurement. Consider routing the verifier to a different provider (codex, gemini) for some fraction of audits. See `skills/kb-audit/SKILL.md` §Verifier independence.

---

## 12. Decision

This proposal is **incrementally adoptable**. Phase 0 unlocks measurement (you can't improve what you can't measure); Phase 3 is the highest-leverage change (contradiction at write defeats the worst class of drift).

If you adopt only Phase 0+1+5: you gain measurement + provenance, without forcing agents to change write patterns. Cheapest path to "we can defend the claims."

If you adopt Phase 0+1+2+5: you gain measurement + confidence-aware retrieval. Skills can opt into high-confidence filtering for risky decisions.

---

## 13. Phase 5 implementation notes (br-ei2)

### Design decisions

**Beta(1,1) posterior over composite weighted formula.**
The composite formula in §5.3 requires calibrated weights, age-decay half-lives, and an NLI contradiction pass. None of those exist yet. The Beta(1,1) posterior is:
- Measurable immediately from audit verdicts alone
- Bootstrap-safe (0.5 for fresh entries, no cold-start edge cases)
- Monotonically increasing with successes
- Formally verified: `ConfidenceInUnitInterval` invariant in `agent-kb/tla/audit/Audit.tla`

The composite formula is the long-run target once Phases 3–4 ship.

**`audit_runs` is DB-only (not JSONL-sourced).**
Audit verdicts are operational metadata, not knowledge events. They do not affect the KB entry's canonical identity — only its confidence score at read time. Writing them to JSONL would pollute event replay with operational noise and create a source-ordering dependency between expire events and audit_runs rows. The expire event triggered by a `verdict=false` IS written to JSONL (maintaining event-sourcing integrity for the entry state); only the audit record itself is DB-only.

**JSONL-first ordering for expires.**
In `handle_audit_record`, when `verdict=false`, the expire event is appended to JSONL and applied to SQLite *before* the `audit_runs` INSERT. This preserves the invariant that JSONL is the canonical event log and SQLite is a materialization.

**`session_id` on entries; `COALESCE` sentinel.**
`entries.session_id` is SQL NULL for entries written before br-ei2. At the `source_weights` read path, `COALESCE(session_id, '__GLOBAL__')` maps legacy entries to the `__GLOBAL__` sentinel. This means audit history for any `(kind, NULL)` entry accrues to `(kind, '__GLOBAL__')`, which is the correct fallback weight for that kind. New entries written with `session_id` get their own per-session weight bucket.

**`kind=derived` unblocked.**
Phase 1 gated `kind=derived` to prevent provenance writes before the provenance API existed. br-ei2 ships the `provenance` MCP method; the gate is removed. Derived entries are now first-class.

**Idempotent audit record.**
`UNIQUE INDEX on (run_id, entry_id)` + `INSERT OR IGNORE` ensures replaying the same verdict is a no-op. This matches the event-sourcing replay-safety requirement.

### TLA+ specification

`agent-kb/tla/audit/Audit.tla` models four invariants:

| Invariant | Statement |
|-----------|-----------|
| `EntryMonotonicity` | `live → stale` is one-way; no entry returns to live after being staled |
| `ConfidenceInUnitInterval` | `(s+1)/(s+f+2) ∈ [0,1]` for all `s, f ∈ ℕ` |
| `ProvenanceAcyclicity` | `derived_from` edges form a DAG |
| `SourceWeightsAppendOnly` | `source_weights` rows never deleted; counts only increment |

TLC model-checked with `MaxEntries=3`, `MaxAudits=4`.

### Acceptance criteria status

| AC | Status |
|----|--------|
| `audit_run` MCP method: samples live entries with `evidence_status='present'`, `sample_size` clamped 1–50, returns `{run_id, samples}` | ✓ Done |
| `audit_record` MCP method: JSONL-first expire on `verdict=false`; `INSERT OR IGNORE` on `(run_id, entry_id)`; `source_weights` upsert | ✓ Done |
| `audit_report` MCP method: per-`(kind × session_id)` precision, `last_run_at`, `total_runs` | ✓ Done |
| `confidence` + `audit_n` fields on every `kb_search` result; Beta(1,1) posterior; single JOIN (not per-row subquery) | ✓ Done |
| `session_id` on `kb_add` → `entries.session_id`; legacy entries NULL; `COALESCE` sentinel at read path | ✓ Done |
| `provenance` MCP method: iterative DFS, cycle detection, `max_depth` default 64 | ✓ Done |
| `kind=derived` unblocked at `kb_add` gate | ✓ Done |
| `source_weights` table; schema migration for existing DBs | ✓ Done |
| `/kb-audit` skill (`skills/kb-audit/SKILL.md`) with verifier independence guidance | ✓ Done |
| Benchmark `bench_kb_search_confidence` for confidence prefetch cost on 50-entry corpus | ✓ Done |
| ≥19 unit tests, ≥6 proptests covering audit/confidence/provenance | ✓ Done |
| TLA+ spec `agent-kb/tla/audit/Audit.tla` with 4 invariants, TLC-verified | ✓ Done |
