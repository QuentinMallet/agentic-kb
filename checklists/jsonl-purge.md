# Checklist: jsonl-purge

## Phase 2: Implement
- [ ] [BLOCKING] Spec task (spec: prefix) created under epic AND dep edges wired (br dep add <impl-id> <spec-id> for each impl task)
- [ ] Spec task closed (TLA+ spec committed, TLC clean, code-reviewer + analyst approved)
- [ ] [BLOCKING if new source files] TDD: test file exists for each new .rs/.ex/.exs/.py module (proptest/StreamData/hypothesis)
- [ ] All epic tasks closed
- [ ] Commits atomic, beads updated at each commit

## Phase 3: Post-Implementation

### Block A: Code Quality
- [ ] [BLOCKING] Sync + rebase onto master
- [ ] [non-blocking] PM drift check (skip if --no-roadmap)
- [ ] [non-blocking] Code simplifier on changed files
- [ ] [non-blocking] Structural improvement (only if >300 lines changed: git diff --stat origin/master...HEAD | tail -1)

### Block B: Verification
- [ ] [BLOCKING] Spec compliance (/verify — Covered verdict required)
- [ ] [BLOCKING if matches] Security review (diff-scan sensitive paths)
- [ ] [BLOCKING if new source files] Test adequacy (TDD: proptest/StreamData/hypothesis; pr-test-analyzer for coverage)
- [ ] [BLOCKING if workflows] Local CI (act push / act -j ci)

### Block C: Documentation
- [ ] [BLOCKING] Doc cleanup (/doc-cleanup jsonl-purge)
- [ ] [non-blocking] CLAUDE.md currency check (if epic changed referenced files)
- [ ] [non-blocking] Gotcha frequency audit (bump hits, produce promote/demote/prune table)

### Block D: Review Loop
- [ ] [BLOCKING] Budget tier check (read advisory.txt: GREEN/YELLOW/RED)
- [ ] [BLOCKING] PR review (pr-review-toolkit, tier controls parallelism)
- [ ] [BLOCKING] Assess Critical findings (tactical → beads tasks, architectural → ralplan)
- [ ] [BLOCKING] Fix loop (execute fixes, re-review until zero Critical)
- [ ] [non-blocking] Important/Suggestions triage (present to user)
- [ ] [BLOCKING] User confirmation gate

## Phase 4: Merge
- [ ] Merge --no-ff to master (local only — do NOT push yet)
- [ ] [BLOCKING if workflows] act push passes on merged master
- [ ] git push origin master
- [ ] br epic close-eligible, br close <epic-id>, br sync --flush-only
- [ ] Worktree cleanup (git worktree remove, branch delete, remote prune)
- [ ] Downstream drift signal (note affected open epics)
- [ ] Skill extraction (optional, /skillify if reusable pattern emerged)
