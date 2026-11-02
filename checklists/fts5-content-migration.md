# Post-impl checklist: fts5-content-migration

Epic: br-fts5-content-migration-31t
Base branch: sess-20260610-1904
Started: 2026-06-11

## Block A — Code Quality
- [x] [BLOCKING] Sync + rebase (rebased onto sess-20260610-1904, pushed)
- [x] [non-blocking] PM drift check (no plan file found — skip)
- [x] [non-blocking] Code simplifier (deslop pass ran — skip)
- [x] [non-blocking] Structural improvement (>300 lines; deslop covered — skip)

## Block B — Verification
- [x] [BLOCKING] Spec compliance — TLC 3347 states, 0 errors, 4 invariants
- [x] [BLOCKING if matches] Security review — no match, skip
- [x] [BLOCKING] Codex cross-model review — approve (2nd pass after C1/I2/I4 fixes)
- [x] [BLOCKING if new src] Test adequacy — no new non-test source files, skip
- [x] [non-blocking] Test coverage analysis — 23 unit + 2 integration pass

## Block C — Documentation
- [x] [BLOCKING] Doc cleanup — no .omc/ artifacts found for this slug

## Block D — Review Loop
- [ ] [BLOCKING] Budget tier check
- [ ] [BLOCKING] Code review (local)
- [ ] [BLOCKING] User confirmation gate

## Phase 4 — Merge
- [x] [BLOCKING] Merge-boundary codex verdict — approve
- [x] Merge epic → sess-20260610-1904 (a7f8e47)
- [x] Session cascade → master (820fbd1)
- [x] Close epic — br-fts5-content-migration-31t closed
- [x] Worktree removed, branch deleted local+remote
- [x] KB: ADR → decisions/fts5-content-migration
- [x] KB: gotchas/rust × 2 (v1-no-op pattern, BTreeSet ordering)
- [x] KB stale check — all up to date

## Codex verdict (Block B)
verdict: reject
rationale: C1 — apply_event writes to entries_fts after T8 drop will fail (no such table); post-drop writes not regression-tested. I1 — rollback_invocations never incremented, signal is a no-op. I2 — debug Vec vs Set in divergence check. I4 — stale docstring in search_entries.
concerns:
- C1: apply_event upsert+expire still write to entries_fts with `?` propagation; after drop, every KB event fails
- I1: rollback_invocations stays at 0 forever; gate signal (2) is a no-op
- I2: debug divergence assert compares ordered Vec, not set — spurious panics on tie-break ordering
- I4: search_entries docstring still says default is "contentless" after US-6 flip

## Merge-boundary codex verdict
verdict: approve — nix build kb-0.1.0 clean, no catalog drift, all fixes confirmed
