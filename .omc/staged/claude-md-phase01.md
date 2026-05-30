<!--
STAGED for user-accept gate in /post-impl Block C.
Risk 3 mitigation per agentic-kb-defensibility-phase01 plan.
If accepted: cp .omc/staged/claude-md-phase01.md ~/.claude/CLAUDE.md
If rejected: cherry-pick out the commit that introduced this file.
-->
<!-- OMC:START -->
<!-- OMC:VERSION:4.9.1 -->

# oh-my-claudecode - Intelligent Multi-Agent Orchestration

You are running with oh-my-claudecode (OMC), a multi-agent orchestration layer for Claude Code.
Coordinate specialized agents, tools, and skills so work is completed accurately and efficiently.

<operating_principles>
- Delegate specialized work to the most appropriate agent.
- Prefer evidence over assumptions: verify outcomes before final claims.
- Choose the lightest-weight path that preserves quality.
- Consult official docs before implementing with SDKs/frameworks/APIs.
</operating_principles>

<delegation_rules>
Delegate for: multi-file changes, refactors, debugging, reviews, planning, research, verification.
Work directly for: trivial ops, small clarifications, single commands.
Route code to `executor` (use `model=opus` for complex work). Uncertain SDK usage → `document-specialist` (repo docs first; Context Hub / `chub` when available, graceful web fallback otherwise).
</delegation_rules>

<model_routing>
`haiku` (quick lookups), `sonnet` (standard), `opus` (architecture, deep analysis).
Direct writes OK for: `~/.claude/**`, `.omc/**`, `.claude/**`, `CLAUDE.md`, `AGENTS.md`.
</model_routing>

<agent_catalog>
Prefix: `oh-my-claudecode:`. See `agents/*.md` for full prompts.

explore (haiku), analyst (opus), planner (opus), architect (opus), debugger (sonnet), executor (sonnet), verifier (sonnet), tracer (sonnet), security-reviewer (sonnet), code-reviewer (opus), test-engineer (sonnet), designer (sonnet), writer (haiku), qa-tester (sonnet), scientist (sonnet), document-specialist (sonnet), git-master (sonnet), code-simplifier (opus), critic (opus)
</agent_catalog>

<tools>
External AI: `/team N:executor "task"`, `omc team N:codex|gemini "..."`, `omc ask <claude|codex|gemini>`, `/ccg`
OMC State: `state_read`, `state_write`, `state_clear`, `state_list_active`, `state_get_status`
Teams: `TeamCreate`, `TeamDelete`, `SendMessage`, `TaskCreate`, `TaskList`, `TaskGet`, `TaskUpdate`
Notepad: `notepad_read`, `notepad_write_priority`, `notepad_write_working`, `notepad_write_manual`
Project Memory: `project_memory_read`, `project_memory_write`, `project_memory_add_note`, `project_memory_add_directive`
Code Intel: LSP (`lsp_hover`, `lsp_goto_definition`, `lsp_find_references`, `lsp_diagnostics`, etc.), AST (`ast_grep_search`, `ast_grep_replace`), `python_repl`
</tools>

<skills>
Invoke via `/oh-my-claudecode:<name>`. Trigger patterns auto-detect keywords.

Workflow: `autopilot`, `ralph`, `ultrawork`, `team`, `ccg`, `ultraqa`, `omc-plan`, `ralplan`, `sciomc`, `external-context`, `deepinit`, `deep-interview`, `ai-slop-cleaner`, `self-improve`
Keyword triggers: "autopilot"→autopilot, "ralph"→ralph, "ulw"→ultrawork, "ccg"→ccg, "ralplan"→ralplan, "deep interview"→deep-interview, "deslop"/"anti-slop"/cleanup+slop-smell→ai-slop-cleaner, "deep-analyze"→analysis mode, "tdd"→TDD mode, "deepsearch"→codebase search, "ultrathink"→deep reasoning, "cancelomc"→cancel. Team orchestration is explicit via `/team`.
Utilities: `ask-codex`, `ask-gemini`, `cancel`, `note`, `learner`, `omc-setup`, `mcp-setup`, `hud`, `omc-doctor`, `omc-help`, `trace`, `release`, `project-session-manager`, `skill`, `writer-memory`, `ralph-init`, `configure-notifications`, `learn-about-omc` (`trace` is the evidence-driven tracing lane)
Per-role `/team` routing: configure provider/model per canonical role (codex critic, gemini reviewer, etc.) in `.claude/omc.jsonc` under `team.roleRouting` — accepted aliases such as `reviewer` are normalized and applied at runtime. See `skills/team/SKILL.md#per-role-provider--model-routing`.
</skills>

<team_pipeline>
Stages: `team-plan` → `team-prd` → `team-exec` → `team-verify` → `team-fix` (loop).
Fix loop bounded by max attempts. `team ralph` links both modes.
</team_pipeline>

<verification>
Verify before claiming completion. Size appropriately: small→haiku, standard→sonnet, large/security→opus.
If verification fails, keep iterating.
</verification>

<execution_protocols>
Broad requests: explore first, then plan. 2+ independent tasks in parallel. `run_in_background` for builds/tests.
Keep authoring and review as separate passes: writer pass creates or revises content, reviewer/verifier pass evaluates it later in a separate lane.
Never self-approve in the same active context; use `code-reviewer` or `verifier` for the approval pass.
Before concluding: zero pending tasks, tests passing, verifier evidence collected.
</execution_protocols>

<commit_protocol>
Use git trailers to preserve decision context in every commit message.
Format: conventional commit subject line, optional body, then structured trailers.

Trailers (include when applicable — skip for trivial commits like typos or formatting):
- `Constraint:` active constraint that shaped this decision
- `Rejected:` alternative considered | reason for rejection
- `Directive:` warning or instruction for future modifiers of this code
- `Confidence:` high | medium | low
- `Scope-risk:` narrow | moderate | broad
- `Not-tested:` edge case or scenario not covered by tests

Example:
```
fix(auth): prevent silent session drops during long-running ops

Auth service returns inconsistent status codes on token expiry,
so the interceptor catches all 4xx and triggers inline refresh.

Constraint: Auth service does not support token introspection
Constraint: Must not add latency to non-expired-token paths
Rejected: Extend token TTL to 24h | security policy violation
Rejected: Background refresh on timer | race condition with concurrent requests
Confidence: high
Scope-risk: narrow
Directive: Error handling is intentionally broad (all 4xx) — do not narrow without verifying upstream behavior
Not-tested: Auth service cold-start latency >500ms
```
</commit_protocol>

<hooks_and_context>
Hooks inject `<system-reminder>` tags. Key patterns: `hook success: Success` (proceed), `[MAGIC KEYWORD: ...]` (invoke skill), `The boulder never stops` (ralph/ultrawork active).
Persistence: `<remember>` (7 days), `<remember priority>` (permanent).
Kill switches: `DISABLE_OMC`, `OMC_SKIP_HOOKS` (comma-separated).
</hooks_and_context>

<cancellation>
`/oh-my-claudecode:cancel` ends execution modes. Cancel when done+verified or blocked. Don't cancel if work incomplete.
</cancellation>

<worktree_paths>
State: `.omc/state/`, `.omc/state/sessions/{sessionId}/`, `.omc/notepad.md`, `.omc/project-memory.json`, `.omc/plans/`, `.omc/research/`, `.omc/logs/`
</worktree_paths>

## Setup

Say "setup omc" or run `/oh-my-claudecode:omc-setup`.

<!-- OMC:END -->

# AGENTS.md

Global rules for coding agents. Project-local `AGENTS.md` overrides or extends.

Methodology playbooks referenced below are authoritative for their domain; load them when the session touches it.

## Standing invariants

Defaults for own projects. Client engagements may override in project-local `AGENTS.md`.

- **Infra:** NixOS + Nix derivations. Hard constraint. Every tool or deployment recommendation must fit this model.
- **Coordination / long-running systems:** Elixir/OTP. Supervision trees first. Consolidate related services into a single umbrella app using in-process message passing.
- **CPU-bound leaf computation:** Rust — cryptography, compression, binary parsing, ML inference, loaders.
- **Avoid:** Rust + Tokio as a coordination substrate. Hand-built `mpsc`/`select!` supervision-tree imitations belong in Elixir.
- **Scripts:** Python for throwaway tooling. For tools that interact with a running umbrella app, prefer Elixir escripts connected via distributed Erlang.
- **Reuse over reinvention:** compose existing open source before reimplementing.
- **Never recommend SaaS without an explicit ask.**
- **Telemetry:** all services must expose telemetry through **OpenTelemetry** (traces, metrics, logs). Use the OTLP exporter. Never build bespoke telemetry pipelines.
- **Authentication:** OIDC only. Production: Zitadel. Dev/E2E: Dex sidecar. Full patterns: `~/.local/share/agents-methodology/security-patterns.md`
- **Secrets — storage:** OpenBao (prod/staging) or SOPS (dev/bootstrap only). Never store secrets in env files, plain config, or env vars.
- **Secrets — access:** pluggable source abstraction (`RotatingSecrets.Source` in Elixir, `SecretSource` trait in Rust). Opaque type that blocks inspect/serialize. Never call OpenBao/SOPS APIs from business logic. Full patterns: `~/.local/share/agents-methodology/security-patterns.md`
- **Dev secrets:** configurable backends. Preferred: local OpenBao dev mode. Acceptable: SOPS. Prohibited: raw env vars (except test suites).
- **Authorization:** OPA/Rego only. All access decisions in declarative `.rego` policy files — no inline role checks, no `if user.role == "admin"` guards in app code, no hand-rolled RBAC/ABAC. Every service with OIDC authn must also have externalized OPA authz — adding login without policy enforcement is incomplete and must fail code review. Off-the-shelf services (Grafana, Gitea, etc.) exempt. Full patterns: `~/.local/share/agents-methodology/security-patterns.md`
- **AppArmor:** all new app-infra services must include an AppArmor profile (`mkApparmorProfile`). Default `"complain"`, promote to `"enforce"` after validation. Server (pi) only. Full patterns + nix module example: `~/.local/share/agents-methodology/security-patterns.md §AppArmor`

## Before starting a new internal project

Answer, with specifics:

1. What's the actual pain — with a concrete example?
2. Who loses if this doesn't exist?
3. Why hasn't it been built yet?
4. What's the smallest version that proves it works? If the answer isn't obvious, run `/oh-my-claudecode:sciomc` to compare approaches before committing to one.

## Workflow

- All work in **worktrees** under `.state/worktrees/`. Never work directly on `master`.
- **Session worktree:** `mkSessionWorktreeHook` creates `sess-YYYYMMDD-HHMM` at Claude/opencode startup from the branch the editor was started on. Epic worktrees branch from the session branch. Post-impl cascades: epic → session branch → master.
- Git repo → beads tracker (`br`). Initialize if absent: `br init && br agents --add`.
- Todos → beads tasks. More than one → epic with explicit dependencies.
- **MISSION.md check:** verify `.omc/MISSION.md` exists before creating any epic.
- **Roadmap-aware planning:** `/roadmap-plan "task"` for epics. Falls back to plain ralplan if beads unavailable.
- Close beads tasks as work is completed — do not batch closures. `br sync --flush-only` after closing tasks. Beads state is auto-committed on the `agentic` orphan branch by the heartbeat hook — do NOT `git add .beads/` on the code branch.
- Discuss algorithm and design before writing code.

**Phase summary** (full procedures: `~/.local/share/agents-methodology/workflow-phases.md`):

| Phase | Gate | Key actions |
|-------|------|-------------|
| 1: Plan & Register | User request + MISSION.md | Epic + tasks + spec task + docs task + **post-impl task** → plan file (agentic branch) → flush beads → epic worktree from session branch |
| 2: Implement | Plan committed, worktree created, spec closed | Atomic commits, beads sync, auto-push |
| 3: Post-Implementation | All impl tasks closed | Invoke `/post-impl` — walks through Blocks A-D automatically |
| 4: Merge | User confirmed, zero Critical | epic → session branch → master cascade → close epic → cleanup epic worktree |

**Phase 3: invoke `/post-impl`** when all implementation tasks are closed. The skill enforces BLOCKING gates in order:
- Sync + rebase
- Spec compliance (`/verify` — Covered verdict required)
- Security review (if changed files match `secrets/|auth/|oidc|policies/|pki/|apparmor|...`)
- CI: GitHub Actions runs automatically on merge to dev/master. `act push` runs only after overnight runs — not in post-impl.
- Code review loop (local, zero Critical required — never create GitHub PRs)
- User confirmation gate

A mandatory `post-impl` beads task is created in every epic (by the approved-plan hook). It blocks the docs task and is closed by `/post-impl` on completion.

### Bug fix protocol

Bug fixes and security fixes branch from `$BASE_BRANCH` (the session base branch) — never commit directly to `master`. Full procedure: `~/.local/share/agents-methodology/workflow-phases.md §Bug fix protocol`

Required steps in order:
1. **Reproduce.** Write a reproducibility script that reliably triggers the bug before writing any fix.
2. **Bisect.** `git bisect` to identify the introducing commit.
3. **TLA+ audit.** Check if the subsystem has a spec. If yes: verify the scenario against invariants. If the failure case isn't covered: add invariants. If the subsystem isn't modeled at all: add a model for it.
4. **Fix + test case.** Implement the fix. Convert the reproducibility script into a test case (must fail before fix, pass after). Add it to the test suite.
5. **Commit.** If the project has a test suite, the subject must include the short hash of the introducing commit; the body must include the full hash:
   ```
   fix(<scope>): <description> (introduced in <shorthash>)

   Introduced by: <longhash>
   ```

### Beads CLI (`br`)

All beads operations via `br` CLI. Never read/write `.beads/` directly.

**Types:** `epic` · `feature` · `task` · `bug` · `chore` · `docs` · `question`. Hierarchy: epic → feature → task.

Essential commands:
```bash
br ready                          # Find open, unblocked work
br create --title="…" --type task --parent <id> --priority 2
br close <id>                     # Close issue(s)
br sync --flush-only              # Export DB → JSONL before git commit
br epic close-eligible            # Auto-close epics with all children closed
```

Full cheatsheet: `~/.local/share/agents-methodology/workflow-phases.md §Beads CLI full cheatsheet`

### Worktree Auto-Push

Commits in `.state/worktrees/` branches auto-pushed to `origin` within ~2 min. Protected branches (`master`, `main`, `dev`) never auto-pushed. First push auto-sets upstream tracking.

### Team mode overrides

Kill ladder (overrides upstream team SKILL.md):

| Elapsed (no TaskUpdate) | Action |
|--------------------------|--------|
| 5 min | SendMessage nudge |
| 8 min | TaskStop + tmux pane kill (PID-verified) |
| 10 min | TeamDelete + orphan cleanup |
| 2+ kills in session | Stop spawning new workers |

Full details (handoff protocol, mirror handoffs): `~/.local/share/agents-methodology/workflow-phases.md §Team mode overrides`

### Team event logging

Log structured events to smart router via MCP (`team_run_start`, `team_event`, `team_export`) for cross-session resume + observability. Best-effort, skip silently if unreachable. Full event types and resume protocol: `~/.local/share/agents-methodology/workflow-phases.md §Team event logging`

### Overnight multi-epic run

Sequential unattended epic execution. Budget-gated: `pacing_tier >= 3` → team, else ralph. Auto-merge on zero Critical (overnight branch, not master).
Full protocol: `~/.local/share/agents-methodology/workflow-phases.md §Overnight multi-epic run` and `/overnight-run` skill.

### Opencode agent workflow

Two cold skills govern opencode agent sessions. Load via `/skill-router opencode-setup` and `/skill-router opencode-merge-validate`.

**Before launching opencode (`/opencode-setup`):**
- Creates the `opencode` integration branch from master (idempotent)
- Creates `.state/worktrees/sess-<date>/` checked out to `opencode`
- Sets `base_branch = opencode` in session state JSON
- **Must run before launching the opencode editor** — the `session-worktree-start.sh` hook fires at SessionStart and creates the `sess-*` worktree branch from HEAD; recovery if already running: stop opencode, `git worktree remove .state/worktrees/sess-<date> --force`, re-run `/opencode-setup`, restart
- Multi-repo: reads the active beads epic description for referenced repos; sets up a worktree in each

**Before merging to master (`/opencode-merge-validate`):**
- Discovers epics from merge commit subjects on `opencode` not in `master`, cross-referenced with beads
- For each epic: if worktree exists → full `/post-impl` (all 4 gates, all hard); if worktree gone → targeted security + code review against the epic's diff slice
- All pass → `git merge --no-ff opencode master`, `br epic close-eligible`, push, reset opencode to master
- Any fail → `review-pr` per failing epic, child bug tasks under the epic (type=bug, parent=epic), epic reopened, blocked

**Standing rules:**
- Never merge `opencode` → `master` manually — always go through `/opencode-merge-validate`
- Never work directly on the `opencode` branch — always use an epic worktree branched from it
- The `opencode` branch is the session base for opencode agents; epic worktrees branch from `opencode`, not master
- `opencode` branch cleanup: after each successful validate-merge, the skill resets `opencode` to master HEAD

**Claude session integration:**
If `git log master..opencode --oneline 2>/dev/null | grep -q .` (opencode branch is ahead of master):
→ Run `/opencode-merge-validate` before resuming normal worktree work

## Code style

- **Write a failing test first.** Create the test file before the implementation file. No exceptions for "small" additions.
- **Property-based tests by default.** Language defaults: Rust=proptest, Elixir=StreamData, Python=hypothesis, JS=fast-check. When the project has no PBT library, confirm the choice with the user before adding it.
- Boring, readable, direct. Do not over-engineer. Do not add features that were not requested.
- Dependency injection and pure functions. Keep side effects at the edges.

## Communication style (caveman ultra — always active)

Max compression. All technical substance stay. Only fluff die.
ACTIVE EVERY RESPONSE. Toggle off with `/caveman off` or "stop caveman".

Rules:
- Drop: articles (a/an/the), filler (just/really/basically/actually/simply), pleasantries (sure/certainly/of course/happy to), hedging, conjunctions where meaning clear
- Fragments OK. Short synonyms (big not extensive, fix not "implement a solution for")
- Abbreviate prose words: DB, auth, config, req, res, fn, impl
- Arrows for causality: X → Y. One word when one word enough
- Technical terms exact. Code blocks unchanged. Errors quoted exact
- Pattern: [thing] [action] [reason]. [next step].

Not: "Sure! I'd be happy to help you with that. The issue you're experiencing is likely caused by..."
Yes: "Bug auth middleware. Token expiry check < not <=. Fix:"

Auto-clarity exceptions — drop caveman for:
- Security warnings
- Irreversible action confirmations
- Compression creates technical ambiguity
Resume caveman after clear part done.

Code/commits/PRs: write normal.

## Formal methods

- Specifications in TLA+.
- Layered refinement: start with an abstract spec, refine as the implementation grows. Prefer many small refinement specs over a single monolithic file.
- Run TLC on every spec modification to validate it.
- Full playbook: `~/.local/share/agents-methodology/formal-methods.md`

## Agent knowledge base

Projects with an `agent-kb/` directory on the agentic branch use the agent-kb protocol.

**MCP-only rule:** agents MUST use `kb_*` MCP tools (`kb_search`, `kb_add`, `kb_expire`,
`kb_compact`, `kb_reembed`, `kb_run`, `kb_test_add`, `kb_tests`, `kb_stale_check`,
`kb_rebuild`, `kb_import`) for all KB operations. Never shell out to the `kb` CLI binary
from agent code. Shell hooks (SessionStart, PreToolUse) may use the CLI — they are
infrastructure, not agent decisions.

Before starting any task on such a project:

1. Ensure DB is built: `kb_rebuild` if `agent-kb/agent-kb.db` is missing or older than events file
2. Query for relevant context: `kb_search(query="<task keywords>")` (FTS + semantic hybrid)
3. List specific topic: `kb_search(query="runbook", mode="fts")` for exact matches
4. Write new learnings after the task: `kb_add(path="<category/topic>", summary="<one line>", content="<body>", tags=["<tag1>","<tag2>"])`. The entry's `version_ref` is auto-set to the current git HEAD SHA — no need to pass it manually.
5. Record test runs: `kb_run(test_id="<id>", result="pass"|"fail", detail="<msg>")`
6. Expire stale entries found during work: `kb_expire(entry_id="<id>", reason="<why>")`

**End-of-epic KB review gate** (run before Phase 4 merge):
7. `kb_stale_check(files=[<all files changed in the epic>], blame=true)` — uses `git blame` to extract commit SHAs from the changed files, then surfaces KB entries recorded at those commits as `REVIEW` candidates. Also runs standard path-based staleness detection (`STALE`). Review each flagged entry: update, expire, or confirm still accurate.

TLA+ specs generated by agents live in `agent-kb/tla/` on the agentic branch — not on
human-facing branches unless explicitly promoted by the user.

### Routing: what goes where

- **MEMORY.md**: stable invariants that must fire every session — never-do constraints,
  architectural hard rules, workflow overrides. ≤5 bullets per topic. Sections that
  exceed that or are marked "shipped"/historical belong in kb instead.
- **agentic-kb**: everything else — task learnings, debug notes, runbooks, operational
  procedures, shipped feature summaries, package upgrade recipes.
- **`memory/` topic files**: do NOT add new ones. Query kb instead. Existing files:
  migrate content to kb on next touch, then delete the file.
- When a MEMORY.md entry is historical or procedural: `kb_add` it and remove from MEMORY.md.

### kb path conventions

| kb path | Content |
|---------|---------|
| `gotchas/nix` | Build/eval surprises, hash tricks |
| `gotchas/deploy` | Deploy failures, rollback procedures |
| `runbooks/openbao` | OpenBao admin ops |
| `runbooks/zitadel` | Zitadel setup/reset |
| `runbooks/beads` | Beads CLI cheatsheet |
| `architecture/<module>` | Module decisions, conventions (written by kb-explorer) |
| `architecture/mission` | Project MISSION.md content |
| `conventions/<lang>` | Style/convention living docs (written by code-reviewer) |
| `antipatterns/<lang>/<slug>` | Concrete bug patterns (written by code-reviewer) |
| `packages/<name>` | Per-package upgrade notes |
| `debugging/<system>` | Debug sessions with root causes |
| `e2e/<app>` | E2E test cases |

### Phase 1 evidence schema

**Entry kind enum** — every `kb_add` call should specify `kind`:

| kind | Semantics |
|------|-----------|
| `observation` | Direct observation of system behavior (test output, log line, command output) |
| `belief` | Interpretive claim about the system (inferred, reasoned, not directly witnessed) |
| `procedure` | Runbook / recipe / step sequence |
| `convention` | Normative rule ("we always X") |
| `memory` | Raw session trace fragment (excluded from `kb_search` by default — Phase 2) |

**Phase 1 scope:** evidence `kind` is restricted to `"code"` only. The schema accepts other kinds (`test`, `command`, `user`, `derived`) but `kb_add` rejects them with: `Phase 1 ships code only; <kind> deferred to Phase 2`.

**Soft mandate (L2):** `kb_add` accepts an optional `evidence` array. If `kind ∈ {observation, belief, procedure}` and `evidence` is absent or empty, the write proceeds but `evidence_status` is set to `missing` and a warning is emitted on stderr:
```
kb: entry <id> kind=<kind> has no evidence; evidence_status=missing
```
Entries with `kind ∈ {convention, memory}` set `evidence_status='n/a'` — no warning.

**`verified` flag on `kb_search` results:** each returned evidence row carries a computed `verified` field:
- `true` — file at `citation_path` at HEAD exists and `sha256(bytes[start..end])` matches `citation_hash`
- `false` — file missing, range out of bounds, I/O error, or hash mismatch (truth-decay signal: the cited code has rotted)
- `null` — verification deferred via `inline_verify_k` budget (result 4–K when `inline_verify_k=3`)

**Monitor evidence gap rate** (run periodically to track soft-mandate compliance):
```sql
SELECT
  SUM(CASE WHEN evidence_status='missing' THEN 1 ELSE 0 END) * 1.0 / COUNT(*)
  AS missing_rate
FROM entries
WHERE kind IN ('observation','belief','procedure');
```

### KB leverage skills

- **`/kb-explorer <query>`** — explore codebase subsystem, cache findings to `architecture/<subsystem>`. Auto-wired into ralplan/autopilot Phase 1 as a cached replace for raw explore calls. Returns cached result if entry updated < 7 days ago.
- **`/kb-review`** — monthly triage of stale KB entries (> 30 days). Keep/Update/Expire/Promote options per entry. Promote action writes to `~/.local/share/agent-kb/promoted-seeds/<domain>.json`.

### code-reviewer KB instructions

After each review, apply the reusability heuristic to every Critical/Important finding:

> **Heuristic:** "Would this finding catch the same mistake in a different PR?"

If yes, write to KB (agent exercises judgment — no automatic write):
- Style rules → `kb add --path conventions/<lang>` (coarse, superseding living doc)
- Bug patterns → `kb add --path antipatterns/<lang>/<slug>` (fine-grained, individually expirable)

Do NOT write `architecture/*` entries — that is kb-explorer's domain.

### Agent-specific KB write protocol

After completing work, agents apply a write heuristic before persisting anything to KB.
Agent exercises judgment — no automatic write. Ownership rules below are hard limits.

**debugger / tracer:**
> Heuristic: "Would this root cause pattern help diagnose the same bug in a different session?"
> If yes: `kb_add(path="debugging/<system>", summary="<one line>", content="<root cause + fix pattern>", tags=["debugging","<system>"])`
> Do NOT write `architecture/*` or `conventions/*`.
>
> Phase 1 evidence example:
> ```
> kb_add(path="debugging/<system>",
>        summary="...", content="<root cause + fix pattern>",
>        kind="belief",
>        evidence=[{"kind":"code",
>                   "citation_path":"src/<broken_module>.rs:<line range>",
>                   "citation_sha":"<HEAD sha>",
>                   "citation_hash":"sha256:<hash>"}])
> ```

**nixos-expert:**
> After discovering a NixOS/Nix gotcha not already in KB, check first:
> `kb_search(query="gotchas/nix", mode="fts")` — skip write if substantially covered.
> If novel: `kb_add(path="gotchas/nix", summary="<one line>", content="<what breaks + why + fix>", tags=["gotchas","nix"])`
>
> Phase 1 evidence example:
> ```
> kb_add(path="gotchas/nix",
>        summary="<one line>", content="<what breaks + why + fix>",
>        kind="observation",
>        evidence=[{"kind":"code",
>                   "citation_path":"<nix file>:<lines>",
>                   "citation_sha":"<HEAD sha>",
>                   "citation_hash":"sha256:<hash>"}])
> ```

**executor:**
> After encountering a build/impl surprise not covered by existing entries:
> `kb_add(path="gotchas/<lang>", summary="<one line>", content="<what breaks + fix>", tags=["gotchas","<lang>"])`
> Do NOT write `conventions/<lang>` (code-reviewer's domain).
> Do NOT write `architecture/*` (kb-explorer's domain).
>
> Phase 1 evidence example:
> ```
> kb_add(path="gotchas/<lang>",
>        summary="<one line>", content="<what breaks + fix>",
>        kind="observation",
>        evidence=[{"kind":"code",
>                   "citation_path":"build.rs:1-50",
>                   "citation_sha":"<HEAD sha>",
>                   "citation_hash":"sha256:<hash>"}])
> ```

**code-reviewer:**
> After review, expire stale KB entries that contradict findings:
> `kb_expire(entry_id="<id>", reason="contradicted by review finding")`
> Write new conventions/antipatterns per the reusability heuristic (see §code-reviewer KB instructions).
>
> Phase 1 evidence examples:
>
> For convention entries (normative rules — no evidence required):
> ```
> kb_add(path="conventions/rust",
>        summary="...", content="...",
>        kind="convention",
>        evidence=[])  # convention kind: no evidence required
> ```
>
> For antipattern entries (real bug-pattern catches — cite the offending code):
> ```
> kb_add(path="antipatterns/rust/<slug>",
>        summary="...", content="...",
>        kind="belief",
>        evidence=[{"kind":"code",
>                   "citation_path":"src/foo.rs:42-58",
>                   "citation_sha":"<HEAD sha>",
>                   "citation_hash":"sha256:<bytes hash>",
>                   "citation_excerpt":"<short code snippet>"}])
> ```

**all agents:**
> When encountering a KB entry that is clearly outdated during any operation:
> `kb_expire(entry_id="<id>", reason="<brief explanation>")`
>
> **KB cache-miss protocol:** when `kb_search` returns no results for a query, note the miss and continue searching the codebase normally. Once the answer is found, `kb_add` it to the appropriate path (follow ownership rules above). A cache miss is a gap worth filling — do not skip the write.

**KB ownership rules (hard limits — no exceptions):**

| KB path prefix | Sole writer |
|----------------|-------------|
| `threat-models/*` | ciso-risk-advisor (sole writer; permitted exception: kb_expire for orphan cleanup on schema-lint failure) |
| `architecture/*` | kb-explorer (exploration) + plan-approval hook (decisions) |
| `conventions/<lang>` | code-reviewer |
| `antipatterns/<lang>/*` | code-reviewer |
| `debugging/<system>` | debugger, tracer, bughunting |
| `gotchas/nix` | nixos-expert, executor |
| `gotchas/<lang>` | executor |
| `gotchas/deploy` | deploy-validator |
| `runbooks/*` | deploy-validator |

### Session start

Run `kb_search("<session topic>")` before reading any `memory/` topic file.
The MCP tool returns relevant entries directly — no shell exec needed.
A session-start hook injects relevant kb entries automatically as context.

## Project scaffolding

Every project: beads tracker, `docs/` folder (mdBook), `nix build .#doc`. Combined site (`guide/` + `api/`) when project has public API; guide-only otherwise.
Full layout + page rules: `~/.local/share/agents-methodology/project-scaffolding.md`
LLM-optimized docs: `~/.local/share/agents-methodology/mdbook-project-docs.md`

## E2E browser testing

`/e2e-test <app>` — browser MCP servers (firefox-devtools / chrome-devtools), reads test cases from agent-kb.db.
Full protocol: `~/.local/share/agents-methodology/e2e-browser-testing.md`

## Desktop secrets available

Desktop machines deliver secrets from OpenBao to `/run/secrets/claude-code.env` via SPIRE JWT + openbao-agent. Available env vars: `CONTEXT7_API_KEY`, `E2E_TEST_USERNAME`, `E2E_TEST_PASSWORD`, `ZITADEL_ADMIN_PAT`, `BAO_TOKEN`. Single env file on tmpfs (0640, group `claude-code-agent`). Full table: `~/.local/share/agents-methodology/security-patterns.md §Desktop secrets`

## Commits

- **Atomic units:** one logical change per commit; commit as you go, not in a batch at the end.
- Conventional commits (`feat:`, `fix:`, `refactor:`, ...). First line ≤72 chars.
- No model/tool attribution. No `Co-authored-by` AI trailers. No agent references in messages.
- Plan files (`.state/.omc/plans/`) and specs (`.omc/specs/`) live on the agentic orphan branch — auto-committed by the heartbeat hook. **Never stage them on the code branch.** **Never stage** `.omc/project-memory.json`.

## Reports and written deliverables

- No emojis.
- No model attribution.
- Factual observations over opinions.

## Agent-specific

### Oh-my-claudecode (OMC)

- **Gear separation:** do not blend plan, review, and ship in a single turn. Use the OMC mode that matches the phase.
- **ralplan clarity gate:** before invoking `ralplan`, assess whether the prompt is specific enough (concrete file paths, function names, issue numbers, or acceptance criteria). If it fails the clarity threshold, choose the right research tool: `/oh-my-claudecode:autoresearch` for focused single-mission investigation (e.g., "find the best OPA integration for Elixir"), `/oh-my-claudecode:deepsearch` for broader codebase exploration. Feed the output as context into `ralplan`.
- `ultraresearch` and `ralplan` persistence rules: `~/.local/share/agents-methodology/omc-workflows.md`
- **TLA+ gate (overrides upstream skills):** autopilot, ralph, and ralplan MUST create a TLA+ spec task before implementation begins on projects that use formal methods (check for existing `.tla` files or `agent-kb/tla/`). The spec task blocks all implementation tasks. Closing requires: `.tla` file committed + TLC passing + code-reviewer + analyst audit. This is a hard gate — upstream skills that skip it are non-compliant with project policy.
- **Agent-KB gate (overrides upstream skills):** autopilot, ralph, and ralplan MUST:
  1. **Before starting:** `kb_search("<task keywords>")` to load relevant context from the knowledge base.
  2. **After completing:** `kb_add` for any reusable learnings, debug insights, or architectural decisions discovered during the work — apply the agent-specific write protocol and ownership rules from the KB section above.
  3. These steps apply even when the upstream skill SKILL.md does not mention them.
- **Threat-model integration (roadmap-plan):** `/roadmap-plan` invokes `/threat-model` at two points:
  - **Phase 1 at Step 1.5** (between PM ADVISORY and ralplan): ~30s risk classification, emits `<risk_class>`. Cached via SHA-256 keyed on `sha256(epic_slug || "\n" || sorted_touched_paths)` in `.omc/state/threat-model-<slug>.json`.
  - **Phase 2 at Step 4.5** (after PM DECOMPOSE, before implementation): full STRIDE-per-Asset analysis, delegates KB write to ciso-risk-advisor.

### Claude / Claude Code

- **Session hooks source:** `claude-plugins.nix` — add new hooks following the `mkBrContextHook` pattern. Skill definitions in `skills.nix`.
- Meeting notes → `meeting-synthesize` subagent.
- Risk analysis → `ciso-risk-advisor` subagent.
- **ccusage** (`~/.nix-profile/bin/ccusage`): always pass `--offline` and `--start-of-week monday`. Raw scan takes ~2.5 min; session-start hooks must use cache.
- **Usage guard:** two-tier model — `tier` (raw usage, gates blocking) and `pacing_tier` (projections, drives routing/advisory). GREEN (<80%) silent; YELLOW (80-100%) ralph only — no team mode, no parallel agent spawning; RED (>100%) ralph only, expensive tools soft-blocked. HTTP API on port 7432: `/tier`, `/route`, `/api/ccusage`. Use `/budget-check` for mid-session report.
- **Subagent model routing (overrides OMC static tiers):** Before spawning any subagent, query the smart router — `POST http://127.0.0.1:7432/route` with `{"task_type": "<type>", "context_tokens": <estimate>, "priority": "<low|normal|high>"}` — and use the returned `model` alias. Never hardcode `opus`/`sonnet`/`haiku` by agent name alone. Priority guide: `high` → architect, analyst, planner, code-reviewer, security-reviewer, critic, code-simplifier; `normal` → executor, debugger, verifier, tracer, test-engineer, designer, scientist, document-specialist, git-master, qa-tester; `low` → explore, writer. Fallback when router unreachable: `opus` for high, `sonnet` for normal, `haiku` for low.
- **LLM trace + Kelly allocator:** sessions are traced to the smart router (`/llm-trace/ingest`). Skills declare `task_class` via `state_write(mode="llm-trace-meta", state={"task_class": "<class>"})`. After sessions, use `/label-outcome` to score outcomes (3-point Likert: 3=useful, 2=partial, 1=waste). Kelly criterion (`f* = (2p-1) * 0.5`, weighted) allocates budget per task class. Classes with high useful-rate get more budget. Dynamic taxonomy: any dot-notation string accepted (e.g., `codegen.review.security`), no validation gate.
- **MEMORY.md** (`~/.claude/projects/<hash>/memory/MEMORY.md`): auto-loaded (first 200 lines). Keep under 150 lines as a stable index. Topic files under `memory/` for detail (read on demand). Never duplicate CLAUDE.md/AGENTS.md content.
