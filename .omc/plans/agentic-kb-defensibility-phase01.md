# Plan: agentic-kb defensibility — Phase 0 + Phase 1 (code-kind only)

## Metadata

- **Slug:** agentic-kb-defensibility-phase01
- **Source spec:** `.omc/specs/deep-interview-agentic-kb-defensibility-phase01.md`
- **Source mission:** `.omc/MISSION.md`
- **Status:** pending approval
- **Generated:** 2026-05-30
- **Iteration:** 2 — Architect+Critic iteration 1 feedback incorporated

---

## RALPLAN-DR Summary

### Principles

1. **Schema-first, behavior-second.** Migration is the foundation; no write-path or read-path changes land before the DB schema is stable and rebuild replays cleanly (AC6 gate).
2. **Soft by default.** Enforcement is a flag, never a rejection. The system accepts imperfect writes and surfaces the gap (`evidence_status='missing'`) rather than blocking agents.
3. **Real corpus, real numbers.** Cost claims are measured against `agent-kb.db`, not synthetic data. If the budget breaks, degrade gracefully (narrow-K), never silently skip.
4. **Event log is the contract.** New event variants (`EvidenceAdd`/`EvidenceExpire`) are first-class citizens in the JSONL log; derived cache (`audit_runs`) stays DB-only and is never event-logged.
5. **Minimal blast radius on the protocol layer.** CLAUDE.md and MCP tool descriptions are written once, in the right place, covering all four affected agent roles — not scattered across multiple docs.

### Decision Drivers

1. **Migration safety vs. rollback complexity** — two new tables + two new columns; any approach that leaves the DB in a half-migrated state is unacceptable given `kb_rebuild` as recovery path.
2. **Atomic-event durability** — a partial write (`Add` event lands, `EvidenceAdd` does not) produces an orphaned evidence row on rebuild; the append must be durable under the existing flock idiom.
3. **Verification latency budget** — p95 ≤ 50ms for K=10 is the gate; the parallelism strategy must respect this without blocking unrelated results.

### Options Considered

#### (a) Migration safety

| Option | Pros | Cons |
|--------|------|------|
| **A: Tolerated-error idiom — `ALTER TABLE ADD COLUMN` with `let _ = conn.execute_batch(...)`, `CREATE TABLE IF NOT EXISTS` for new tables** | Matches established `permanent` column migration pattern at `db.rs:88`; idempotent re-runs; no partial-migration risk on new tables; simple | SQLite pre-3.37 has no `ADD COLUMN IF NOT EXISTS`; idempotency relies on tolerated duplicate-column error (safe, documented) |
| **B: Single BEGIN/COMMIT wrapping all ALTERs + CREATEs** | Atomic; either fully applied or rolled back | Unnecessary complexity; `IF NOT EXISTS` + tolerated duplicate-column error already provides idempotency; not how the existing codebase handles migrations |

#### (b) Atomic event batch (AC12)

| Option | Pros | Cons |
|--------|------|------|
| **A: `append_events_batch` — new fn in `events.rs` that loops `writeln!` per event under the existing flock held by the caller** | Extends the established flock+append idiom at `add.rs:64,104,123`; caller already holds flock across multiple sequential appends; orphan EvidenceAdd events (no matching Add) are filterable at apply time; orphan Add (no EvidenceAdd) surfaces as `evidence_status='missing'` per AC10 | Lock held slightly longer; acceptable since `kb_add` is not on the hot read path |
| **B: Single buffered write — accumulate Add + N EvidenceAdd in memory, write as one contiguous block** | Single I/O syscall region | Duplicates the buffer that `append_events_batch` avoids; "all-or-none" framing overstates the guarantee — flock is not a JSONL transaction |
| **C: ID-linked retry (write Add, then retry EvidenceAdd until persisted)** | Simple first pass | Rebuild sees orphaned Add without EvidenceAdd on crash; soft-mandate state doesn't rescue this — `evidence_status='missing'` is a declared state, not a crash artifact |

#### (c) Read-path verification parallelism (AC17)

| Option | Pros | Cons |
|--------|------|------|
| **A: `std::thread::scope` — spawn one thread per result, deterministic join** | No new dep; stable since Rust 1.63; no global pool that Phase 2 NLI (design §5.4) would contend with; deterministic K-thread join | Slightly more boilerplate than rayon one-liner |
| **B: `rayon::par_iter` over top-K evidence rows** | One-line parallelism | Adds `rayon` to `[dependencies]` (not dev-only); no top-level rayon dep in Cargo.toml; global pool contention risk with Phase 2 NLI workload |
| **C: Serial with async I/O (tokio::fs)** | No new threads | agentic-kb is sync Rust; introducing async runtime for one path is disproportionate |

#### (d) Proptest fixture for AC6 + AC13 (br-4vd #4)

| Option | Pros | Cons |
|--------|------|------|
| **A (T-S6a): In-memory rusqlite (`:memory:`) for reducibility proptest** | Fastest; zero filesystem I/O; high case count; deep shrinks; `citation_path` verification not exercised here (AC13 kind+evidence round-trip only) | Not suitable for AC15 citation_path verification (that's T-S6b's scope) |
| **B (T-S6b): Tempdir-backed integration test with concurrent EvidenceAdd writer thread** | Real filesystem + real SQLite; real path resolution; closest to production; mirrors `test_rebuild_concurrent_writes_converge` pattern at `rebuild.rs:191-225` | Slower; tempdir creation ~1ms/case; acceptable for the expected shrink depth |

#### (e) Benchmark harness (AC23–25)

| Option | Pros | Cons |
|--------|------|------|
| **A: `criterion` crate** | Statistical p50/p95/p99 wall-clock; standard in Rust ecosystem; `cargo bench` integration; HTML report | Adds dev-dependency; negligible compile cost |
| **B: Hand-rolled hyperfine-style (loop + `std::time::Instant`)** | Zero new dep | No statistical analysis; no outlier rejection; no CI-friendly output format; manually reproduce what criterion does |

---

## ADRs

### ADR-A: Migration safety

**Decision:** Use the tolerated-error `ALTER TABLE ADD COLUMN` idiom for new columns, and `CREATE TABLE IF NOT EXISTS` for new tables (`evidence`, `audit_runs`). Concretely: `let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN kind TEXT NOT NULL DEFAULT 'belief' ...;")` and `let _ = conn.execute_batch("ALTER TABLE entries ADD COLUMN evidence_status TEXT NOT NULL DEFAULT 'n/a' ...;")`, followed by `conn.execute_batch("CREATE TABLE IF NOT EXISTS evidence (...); CREATE TABLE IF NOT EXISTS audit_runs (...);")`.

**Drivers:** Migration safety (driver 1). This matches the prior `permanent` column migration pattern at `src/components/db.rs:86-88` exactly.

**Alternatives considered:** Single `BEGIN`/`COMMIT` transaction wrapping all statements (Option B).

**Why chosen:** Idempotency comes from `IF NOT EXISTS` on new tables + tolerated duplicate-column error on new columns — not from a transaction. This is the established codebase idiom. Re-running the migration on an already-migrated DB is safe: `ADD COLUMN` returns an error that is silently dropped via `let _`, and `CREATE TABLE IF NOT EXISTS` is a no-op. No intermediate half-migrated state is possible because each column addition and each table creation is independently idempotent. Legacy JSONL files without evidence produce zero `evidence` rows on rebuild — not an error.

**Consequences:** Migration code in `db.rs` mirrors the `permanent` column pattern. No phased rollout of schema. No transaction boundary needed.

**Follow-ups:** If future migrations require data transforms (not just column additions), revisit multi-step approach with explicit DB version tracking.

---

### ADR-B: Atomic event batch

**Decision:** Extend `src/components/events.rs` with a new `append_events_batch(events_path: &Path, events: &[serde_json::Value]) -> Result<()>` function that loops `writeln!` per event. The caller continues to hold the existing `flock` (as established by the pattern at `src/commands/add.rs:64,104,123` — flock acquired by caller, held across multiple sequential event appends). `kb_add` calls `append_events_batch` with `[Add event, EvidenceAdd event × N]` under the held lock.

**Drivers:** Atomic-event durability (driver 2). AC12 requires that Add + EvidenceAdd events land together.

**Alternatives considered:** Single buffered write (Option B — "all-or-none" overstates the guarantee); ID-linked retry (Option C — crash produces unrecoverable orphan).

**Why chosen:** Extends the established flock+append idiom rather than inventing a new one. The durability guarantee is: **all events land OR replay reconciles orphans.** Orphan `EvidenceAdd` events (no matching `Add`) are filterable at apply time. Orphan `Add` (no `EvidenceAdd`) surfaces as `evidence_status='missing'` per AC10 — the soft-mandate state IS the failure-tolerant semantic, not a defect. The three existing call sites (`add.rs:64`, `add.rs:104`, `add.rs:123`) demonstrate that flock-held multi-append is already the codebase contract.

**Consequences:** Lock held slightly longer per `kb_add` with evidence. Acceptable: `kb_add` is not on the hot read path.

**Follow-ups:** If concurrent write throughput becomes a bottleneck (Phase 4+), revisit to append-log with WAL.

---

### ADR-C: Read-path verification parallelism

**Decision:** `std::thread::scope` (stable since Rust 1.63) over top-K evidence rows in `src/components/verification.rs`.

**Drivers:** Verification latency budget (driver 3). AC17 requires parallel verification; must not block on disk for unrelated results.

**Alternatives considered:** `rayon::par_iter` (adds `rayon` to `[dependencies]`, no top-level rayon dep in Cargo.toml today, global pool contention risk with Phase 2 NLI design §5.4); async tokio (runtime mismatch).

**Why chosen:** No new runtime dependency. `std::thread::scope` provides deterministic K-thread join with explicit lifecycle — no global pool that a future Phase 2 NLI embedding workload (design §5.4) would contend with. Thread-per-result overhead for K=10 is acceptable and the scope join is deterministic. Blocking I/O per evidence row is isolated to scoped threads, never blocking the search caller.

**Consequences:** Slightly more boilerplate than rayon one-liner. `rayon` remains absent from `[dependencies]`. If p95 misses 50ms, narrow K via ADR-E outcome in AC25, not by changing the parallelism model.

**Follow-ups:** If K grows significantly (Phase 2+), evaluate a bounded thread pool within the scope to cap OS thread count.

---

### ADR-D: Proptest fixture — split T-S6a + T-S6b

**Decision:** Replace single T-S6 with two complementary tasks:

- **T-S6a:** In-memory rusqlite (`:memory:`) reducibility proptest in `tests/proptest_events.rs` — fast, high case count, deep shrinks. Maps to AC13 (kind+evidence round-trip). No filesystem I/O; no `citation_path` file resolution. Validates that arbitrary `Add`/`EvidenceAdd`/`EvidenceExpire`/`Compact` sequences produce DB state equal to direct reduction.
- **T-S6b:** Tempdir-backed integration test in `tests/proptest_events.rs` (same file, separate test fn) that invokes `Rebuild::execute_with` with a concurrent `EvidenceAdd` writer thread. Modeled on `test_rebuild_concurrent_writes_converge` at `src/commands/rebuild.rs:191-225`. Maps to AC6 (kb_rebuild 3-phase replay with concurrent evidence writes).

**Drivers:** AC6 requires `kb_rebuild` replay fidelity; AC13 requires round-trip through `events.jsonl` → DB. The two goals have different fixture requirements.

**Alternatives considered:** Single `tempdir`-backed proptest for both (slower; tempdir per shrunk case for AC13's high case count is unnecessary overhead given AC13 doesn't exercise file paths).

**Why chosen:** T-S6a uses `:memory:` for AC13 — no filesystem I/O needed, maximizes shrink depth. T-S6b uses `tempdir` + concurrent writer for AC6 — real filesystem required, mirrors the existing `test_rebuild_concurrent_writes_converge` pattern at `rebuild.rs:191-225`. Splitting avoids forcing slow tempdir setup onto the high-iteration proptest.

**Consequences:** Tests are split across two fns but the same file. T-S6b is slightly slower than T-S6a. Task Graph updated accordingly.

**Follow-ups:** Consider a shared DB fixture at module level for non-proptest unit tests to avoid tempdir churn.

---

### ADR-E: Benchmark harness

**Decision:** `criterion` crate added to `[dev-dependencies]` in `Cargo.toml`; benchmark in `benches/verification.rs`.

**Drivers:** AC23 requires p50/p95/p99 wall-clock reporting; AC24 gates on measured p95 ≤ 50ms.

**Alternatives considered:** Hand-rolled timing loop (no statistical analysis, no outlier rejection, no standard output format).

**Why chosen:** `criterion` provides statistical p95 out of the box with outlier rejection; `cargo bench` integration; commit message can report the exact criterion-printed number for AC24. Zero operational overhead (dev-dep only).

**Consequences:** `criterion` dep-add is task T-C1 and must land before any benchmark task (PM constraint #4).

**Follow-ups:** HTML reports can be committed to `benches/reports/` for longitudinal tracking (Phase 2+ optional).

---

## Task Graph

Tasks are ordered: migration first, then write, then read, then protocol, then cost. `criterion` dep-add is first in L-cost. Cross-lane proptest bundle is split into T-S6a (in-memory, AC13) and T-S6b (tempdir+concurrent, AC6).

### L-schema (AC1–AC6)

| ID | Title | ACs | Depends on | Files touched | Size |
|----|-------|-----|-----------|---------------|------|
| T-S1 | Extend Entry struct + add Evidence + AuditRun structs | AC1, AC2, AC3, AC4 | — | `src/models.rs` | S |
| T-S2 | DB schema migration (tolerated-error ADD COLUMN + CREATE TABLE IF NOT EXISTS) | AC1, AC2, AC3, AC4 | T-S1 | `src/components/db.rs` | M |
| T-S3 | Add EvidenceAdd + EvidenceExpire event variants | AC5 | T-S1 | `src/components/events.rs` | S |
| T-S4 | Wire EvidenceAdd/EvidenceExpire replay into kb_rebuild | AC6 | T-S2, T-S3 | `src/commands/rebuild.rs` | M |
| T-S5 | Verify legacy JSONL (no evidence) replays cleanly + audit_runs untouched | AC6 | T-S4 | `src/commands/rebuild.rs` | S |
| T-S6a | **[cross-lane]** Proptest (in-memory rusqlite): arbitrary Add/EvidenceAdd/EvidenceExpire/Compact → DB state equals direct reduction | AC13, br-4vd#4 | T-S4, T-W4 | `tests/proptest_events.rs` (new) | M |
| T-S6b | **[cross-lane]** Integration test (tempdir + concurrent EvidenceAdd writer): kb_rebuild 3-phase replay converges under concurrent evidence writes | AC6, br-4vd#4 | T-S4, T-W4 | `tests/proptest_events.rs` (new, same file) | M |

### L-write (AC7–AC13)

| ID | Title | ACs | Depends on | Files touched | Size |
|----|-------|-----|-----------|---------------|------|
| T-W1 | MCP kb_add: accept kind + evidence params | AC7 | T-S2, T-S3 | `src/commands/mcp.rs` | M |
| T-W2 | CLI kb_add: --kind + --evidence / --evidence-file flags | AC8 | T-S2, T-S3 | `src/commands/add.rs` | M |
| T-W3 | Write-time validation: kind enum + Phase-1 code-only evidence guard | AC9 | T-W1, T-W2 | `src/commands/mcp.rs`, `src/commands/add.rs` | S |
| T-W4 | Soft-mandate logic: evidence_status='missing' + stderr warning | AC10 | T-W3 | `src/commands/mcp.rs`, `src/commands/add.rs` | S |
| T-W5 | Batch JSONL append: append_events_batch fn + Add + N EvidenceAdd under caller's flock | AC11, AC12 | T-W4, T-S3 | `src/components/events.rs` | M |
| T-W6 | **[cross-lane, same deliverable as T-S6a]** Proptest round-trip coverage | AC13 | T-W5, T-S6a | `tests/proptest_events.rs` | — (merged) |

T-W6 is the same file/deliverable as T-S6a; it is listed here only for AC traceability. The single task is T-S6a.

### L-read (AC14–AC18)

| ID | Title | ACs | Depends on | Files touched | Size |
|----|-------|-----|-----------|---------------|------|
| T-R1 | New verification module: verify_evidence(row, repo_root) -> bool | AC15, AC16 | T-S1 | `src/components/verification.rs` (new) | M |
| T-R2 | kb_search: fetch evidence rows per result, run parallel verification via std::thread::scope | AC14, AC17 | T-R1, T-W5 | `src/commands/search.rs`, `src/commands/mcp.rs` | M |
| T-R3 | Narrow-K fallback: inline_verify_k=3 path when p95 > 50ms | AC18, AC25 | T-R2, T-C2 | `src/commands/search.rs` | S |

T-R3 depends on T-C2 (bench result) to determine which path ships. If bench passes, T-R3 is a no-op config default; if it fails, T-R3 implements the narrow-K path. Either way the code path must exist.

### L-protocol (AC19–AC22)

| ID | Title | ACs | Depends on | Files touched | Size |
|----|-------|-----|-----------|---------------|------|
| T-P1 | Elixir MCP: update kb_add + kb_search tool descriptions | AC19 | T-W1, T-R2 | `mcp/lib/agentic_kb_mcp/` (tool desc files) | S |
| T-P2 | CLAUDE.md: add Phase 1 subsection (kind enum, soft mandate, verified=false semantics, SQL query) | AC20, AC22 | T-W4, T-R2 | `~/.claude/CLAUDE.md` | S |
| T-P3 | CLAUDE.md: per-agent rows — evidence example blocks for code-reviewer, debugger/tracer, executor, nixos-expert | AC21 | T-P2 | `~/.claude/CLAUDE.md` | S |

### L-cost (AC23–AC25)

| ID | Title | ACs | Depends on | Files touched | Size |
|----|-------|-----|-----------|---------------|------|
| T-C1 | Add criterion to Cargo.toml [dev-dependencies] | AC23 | — | `Cargo.toml` | S |
| T-C2 | Bench: benches/verification.rs — kb_search end-to-end K=10 against agent-kb.db | AC23, AC24 | T-C1, T-R2 | `benches/verification.rs` (new) | M |

### L-cleanup

| ID | Title | ACs | Depends on | Files touched | Size |
|----|-------|-----|-----------|---------------|------|
| T-CL1 | Close beads task br-4vd | AC27 | T-S6a + T-S6b complete | — (beads op) | S |
| T-CL2 | Create beads task proptest-coverage-phase2 covering br-4vd #3, #5, #6, #7 | AC27 | T-CL1 | — (beads op) | S |

### Full dependency order (serialized critical path)

```
T-S1 → T-S2 → T-S3 → T-S4 → T-S5
                              ↓
T-C1 (independent; first in L-cost)
T-W1 → T-W3 → T-W4 → T-W5 → T-S6a
T-W2 ↗                       T-S6b (same depends: T-S4 + T-W4)
T-R1 (after T-S1)
T-R2 (after T-R1 + T-W5)
T-C2 (after T-C1 + T-R2)
T-R3 (after T-R2 + T-C2)
T-P1 (after T-W1 + T-R2)
T-P2 (after T-W4 + T-R2)
T-P3 (after T-P2)
T-CL1 → T-CL2 (after T-S6a + T-S6b)
```

---

## Acceptance Criteria

Inherited verbatim from spec. New ACs for PM additions follow.

### Schema & migration (C1)
- [ ] **AC1.** Migration adds `entries.kind TEXT NOT NULL DEFAULT 'belief' CHECK(kind IN ('observation','belief','procedure','convention','memory'))`. Legacy entries get `kind='belief'`.
- [ ] **AC2.** Migration adds `entries.evidence_status TEXT NOT NULL DEFAULT 'n/a' CHECK(evidence_status IN ('missing','present','n/a'))`. Legacy entries get `evidence_status='n/a'`.
- [ ] **AC3.** Migration creates `evidence` table per §5.2 schema (`id`, `entry_id` FK, `kind`, `citation_path`, `citation_sha`, `citation_hash` NOT NULL, `citation_excerpt`, `derived_from`, `recorded_at`). Index on `entry_id`.
- [ ] **AC4.** Migration creates `audit_runs` table (`entry_id`, `audited_at`, `verdict`, `evidence_ref`). DB-only; no event-log writes.
- [ ] **AC5.** New event variants `EvidenceAdd` (carries one evidence row) and `EvidenceExpire` (carries evidence id + reason) wired through `src/components/events.rs`. JSONL serialization matches existing event style.
- [ ] **AC6.** `kb_rebuild` 3-phase pipeline replays `EvidenceAdd`/`EvidenceExpire` events and reconstructs `evidence` table identical to pre-rebuild snapshot. Does not touch `audit_runs`. Integration test (T-S5 legacy replay) + T-S6b (tempdir + concurrent EvidenceAdd writer thread modeled on `rebuild.rs:191-225`) cover this criterion.

### Write path (C2)
- [ ] **AC7.** `kb_add` MCP tool (`src/commands/mcp.rs`) accepts optional `kind` (string, default "belief") and optional `evidence` (array of objects).
- [ ] **AC8.** `kb_add` CLI (`src/commands/add.rs`) accepts `--kind` and `--evidence` (repeatable JSON arg, or `--evidence-file path.json`).
- [ ] **AC9.** Write-time validation: `kind` matches the entry-kind enum; each evidence row's `kind` is `"code"` (other kinds rejected with `Phase 1 ships code only; <kind> deferred to Phase 2`).
- [ ] **AC10.** Soft mandate: if `kind ∈ {belief, procedure, observation}` AND evidence is missing/empty, write proceeds, set `evidence_status='missing'`, emit stderr warning `kb: entry <id> kind=<kind> has no evidence; evidence_status=missing`. Otherwise set `evidence_status='present'` (when evidence supplied) or `'n/a'` (other kinds).
- [ ] **AC11.** For each evidence row, persist exactly as supplied; do not re-hash file at write time.
- [ ] **AC12.** `kb_add` writes one `Add` event (with kind field) plus N `EvidenceAdd` events via `append_events_batch` under the caller's held flock (matching the established pattern at `add.rs:64,104,123`). Durability guarantee: all events land OR replay reconciles orphans — orphan `Add` events surface as `evidence_status='missing'` per AC10.
- [ ] **AC13.** Unit + proptest coverage (T-S6a, in-memory rusqlite): arbitrary `kind` + evidence combinations round-trip through events.jsonl → DB.

### Read path (C3)
- [ ] **AC14.** `kb_search` MCP tool returns each result with an `evidence` array containing all of that entry's evidence rows.
- [ ] **AC15.** Each returned evidence row carries a computed `verified` boolean = (file at `citation_path` at HEAD exists AND `citation_hash == sha256(bytes[start..end])`).
- [ ] **AC16.** If `citation_path` references a missing file, the range is out of bounds, or any I/O error occurs → `verified=false` (never panic, never partial-fail the search).
- [ ] **AC17.** Verification runs in parallel across top-K results (default K=10; configurable) using `std::thread::scope` for deterministic join; must not block on disk for unrelated results.
- [ ] **AC18.** Verification budget is enforced as the C5 fallback (see AC25).

### Agent protocol update (C4)
- [ ] **AC19.** MCP tool descriptions for `kb_add` and `kb_search` updated in Elixir MCP layer (`mcp/lib/agentic_kb_mcp/`) to document `kind`, `evidence`, and the `verified` flag on results. Mix tests pass (AC26).
- [ ] **AC20.** User CLAUDE.md "Agent knowledge base" section adds Phase 1 subsection: kind enum, code-only evidence, soft-mandate warning behavior, `verified=false` meaning.
- [ ] **AC21.** Per-agent KB write-protocol table rows for `code-reviewer`, `debugger/tracer`, `executor`, `nixos-expert` each get a "Phase 1 evidence example" code block with a representative `kb_add` call including `kind` + one `code` evidence row.
- [ ] **AC22.** SQL query for `evidence_status='missing'` rate documented in CLAUDE.md "Agent knowledge base" section. Example: `SELECT COUNT(*) FILTER (WHERE evidence_status='missing') * 1.0 / COUNT(*) FROM entries WHERE kind IN ('belief','procedure','observation');` — note: `COUNT(*) FILTER (WHERE ...)` requires SQLite ≥3.30 (2019); bundled rusqlite uses 3.46+ per Cargo.toml, so this is safe.

### Verification cost validation (C5)
- [ ] **AC23.** `cargo bench` via `criterion` in `benches/verification.rs` exercises `kb_search` end-to-end against `agent-kb.db` with K=10. Records p50/p95/p99 wall-clock.
- [ ] **AC24.** Benchmark gates on p95 ≤ 50ms; actual measured number reported in commit message.
- [ ] **AC25.** If p95 > 50ms, ship `kb_search` with `inline_verify_k=3` (top-3 verified inline; results 4–K returned with `verified=null` and `timing_log` field per result). Otherwise ship inline_verify_k=10.

### PM additions
- [ ] **AC26.** Elixir Mix tests cover `kb_add` and `kb_search` tool description parsing and param validation (tool description shape, `kind` field present, `evidence` array in search results). Tests live in `mcp/test/`.
- [ ] **AC27.** beads task br-4vd closed; follow-up beads task `proptest-coverage-phase2` created covering br-4vd items #3 (search relevance), #5 (embedding pipeline), #6 (compaction invariants), #7 (stale-check edge cases).

---

## Risk + Mitigation

PM advisory flags three high-risk areas:

### Risk 1: Cross-cutting schema foundations

`src/models.rs` and `src/components/db.rs` are touched by T-S1/T-S2 and are depended on by every downstream task. A type mismatch in `Evidence` struct or an incorrect CHECK constraint will cascade.

**Mitigations:**
- T-S1 is the first task; no write/read/bench work starts until T-S2 (migration) passes `cargo test`.
- Migration uses the tolerated-error idiom (ADR-A): each column addition and table creation is independently idempotent; no partial-migration state is possible.
- AC6 proptest (T-S6a + T-S6b) is the gate before L-write tasks close; T-S6b exercises the full rebuild pipeline with concurrent writes.

### Risk 2: Global CLAUDE.md touch (AC20–AC21)

`~/.claude/CLAUDE.md` is loaded every session. A malformed edit breaks all agent sessions.

**Mitigations:**
- T-P2 and T-P3 are additive-only (new subsection + new code blocks); no deletion of existing rows.
- Protocol lane (L-protocol) is last in execution order — after L-schema, L-write, L-read are stable and verified. No protocol changes land until code is confirmed working.
- Changes are reviewed by `/post-impl` code-review gate before merge.

### Risk 3: ~/.claude/CLAUDE.md global-write rollback risk (T-P2, T-P3)

`~/.claude/CLAUDE.md` lives outside the epic worktree. If the epic→session→master cascade needs to be rolled back, the standard `/post-impl` cascade rollback does not unwind changes to this file.

**Mitigations:**
- T-P2 and T-P3 edits to `~/.claude/CLAUDE.md` are staged as a **separate commit** on the session branch, isolated from the code commits.
- This separate commit is gated by an explicit user-accept step in `/post-impl` Phase 3 Block C (doc-cleanup) before the cascade proceeds.
- If the user rejects at the gate, the isolated commit can be cherry-picked out before the epic→session→master cascade, leaving the code commits intact.

---

## Verification Steps

Each lane is done when its ACs are checked and `cargo test` / `cargo bench` / Elixir `mix test` pass.

| Lane | Done when |
|------|-----------|
| L-schema | `cargo test` passes; `kb_rebuild` replays test JSONL with Add/EvidenceAdd/EvidenceExpire events and produces correct DB (T-S5); T-S6a proptest (in-memory) passes (≥100 shrunk cases, zero failures); T-S6b integration test (tempdir + concurrent writer) passes — evidence for AC6. |
| L-write | `kb_add` (MCP + CLI) accepts kind + evidence; soft-mandate warning appears on stderr for missing evidence; `events.jsonl` contains Add+EvidenceAdd block written via `append_events_batch` under caller flock; T-S6a proptest includes write round-trip (AC13). |
| L-read | `kb_search` returns evidence array with `verified` boolean; AC16 error cases return `verified=false`, no panic; `cargo test` for verification.rs covers missing file, out-of-bounds range, I/O error; `std::thread::scope` join deterministic for K=10. |
| L-protocol | Elixir `mix test` passes (AC26); CLAUDE.md diff reviewed and approved; per-agent example blocks render correctly in a session; global CLAUDE.md commit staged separately per Risk 3 mitigation. |
| L-cost | `cargo bench` runs against `agent-kb.db`; p95 ≤ 50ms → ship inline_verify_k=10; p95 > 50ms → ship inline_verify_k=3 with `timing_log` (AC25 output shape). Measured number in commit message (AC24). |
| L-cleanup | `br close br-4vd` exits 0; `br create --title proptest-coverage-phase2 ...` exits 0 with new task ID. |

---

## Out-of-scope (explicit)

The following are deferred and must not appear in implementation:

- **NG1–NG9** from spec (confidence scoring, NLI contradiction, /kb-audit, provenance API, mandatory evidence, background verification worker, cross-machine audit sync, backfill prompting, evidence kinds other than `code`).
- **br-4vd deferred items:** #3 (search relevance proptest), #5 (embedding pipeline proptest), #6 (compaction invariants proptest), #7 (stale-check edge cases proptest). These are captured in the `proptest-coverage-phase2` beads task created by T-CL2.
- **Phase 2+** features from `agentic-kb-defensibility.md` §5.3–5.7.
- **docs/ mdBook updates** — no mdBook pages added in this push; CLAUDE.md is the protocol surface.
