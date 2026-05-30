# Staged CLAUDE.md changes — agentic-kb defensibility Phase 1

## What is staged

Two additive changes to `~/.claude/CLAUDE.md`, implemented as tasks T-P2 and T-P3
of the `agentic-kb-defensibility-phase01` epic (beads br-jwe.20 and br-jwe.21):

- **T-P2 (AC20, AC22):** New subsection `### Phase 1 evidence schema` inserted after
  the `### kb path conventions` table. Documents the entry kind enum, code-only evidence
  restriction, soft-mandate warning behavior, `verified` flag semantics, and the SQL
  query for monitoring `evidence_status=missing` rate.

- **T-P3 (AC21):** Phase 1 evidence example blocks added inline to each of the four
  agent-specific KB write-protocol entries: `debugger / tracer`, `nixos-expert`,
  `executor`, and `code-reviewer`.

## Files

| File | Purpose |
|------|---------|
| `claude-md-phase01.md` | Full proposed content of `~/.claude/CLAUDE.md` after edits |
| `claude-md-phase01.patch` | Unified diff (original → proposed) for review |
| `README.md` | This file |

## Risk 3 mitigation

`~/.claude/CLAUDE.md` is user-global and lives outside the epic worktree git history.
Per ADR-A risk mitigation in the plan, edits are staged here rather than applied directly.
The file is committed on the epic branch so it travels through the normal
epic → session → master cascade review gates.

## How /post-impl Block C applies this

During Phase 3 post-implementation, `/post-impl` Block C (doc-cleanup) presents this
staged file for user confirmation before the cascade to master proceeds.

If **accepted:**
```bash
cp .omc/staged/claude-md-phase01.md ~/.claude/CLAUDE.md
```

If **rejected:** cherry-pick out the commit that introduced `.omc/staged/` from the
session branch before the epic → session → master cascade. The code commits are
unaffected.
