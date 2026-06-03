# Epic: kb stale-check performance and correctness fixes

**Slug:** `kb-stale-check-fixes`
**Risk class:** medium (PM Advisory 2026-05-28)
**Roadmap fit:** greenfield slot — no open epics, no conflicting in-flight work
**Mission gate:** skipped — MISSION.md superseded by agentic-kb itself; supersession audit task added (T7)

## Problem

The `kb stale-check` subcommand and its MCP twin (`kb_stale_check`) are unusably slow and crash on real-world inputs. Audit against `/home/urist/Documents/boulot/normatix` reproduced a hard panic on the user's 26-file `blame=true` call:

```
panic: end byte index 40 is not a char boundary; it is inside '—' (bytes 38..41)
  of `summary feat: Clever Cloud deployment — strip Horde/libcluster, add CC config + health + E2E gate`
location: src/commands/stale_check.rs:159
exit: 101
```

The MCP client retries → user-visible hang. Even without the panic, the command spawns ~1500 git subprocesses for a 26-file query and runs every SQL lookup as a full table scan.

Five distinct defects identified, plus an orchestration-duplication refactor needed to apply any fix without double-editing.

## Constraints

- Must not change the public CLI subcommand name or top-level argument shape.
- `kb_stale_check` MCP tool may evolve its output shape only for Bug 5 (additive `unreachable` bucket).
- DB migration must be additive — no `schema_version` bump (`CREATE INDEX IF NOT EXISTS`).
- Bug 1 is hotfix-class: per `AGENTS.md §Bug fix protocol`, branch from `master`, subject includes introducing-commit shorthash, body includes full hash, with a failing reproducibility test that flips green after the fix.
- TLA+ gate: pure helpers, no new state-machine logic. Document the skip decision in T6 (TLA+ spec decision task); reviewer can override.
- KB lives on the `agentic` orphan branch; version_refs in entries point at code-branch SHAs that may be unreachable from current HEAD. Bug 5 fix must handle this.

## Sequencing (PM Advisory)

```
T0 refactor (extract shared helper)
   └── T1 bug 1 panic         (hotfix-class, branch from master)
   └── T2 bug 4 blame scope   (reduces input set for T4)
       └── T3 bug 3 SQL index + fast-path + hoist prepare
           └── T4 bug 2 dedupe + git rev-list --count
               └── T5 bug 5 UNREACHABLE bucket (MCP output contract change — last)
T6 TLA+ decision (parallel, before T1)
T7 machines_conf agentic-config supersession audit (parallel, no dependencies)
T8 docs (after T5, before post-impl)
T9 post-impl (mandatory; blocked by T8)
```

## Tasks

### T0 — Refactor: extract shared stale_check orchestration

**Why:** `src/commands/stale_check.rs::execute` and `src/commands/mcp.rs::handle_stale_check` (lines 551–681) duplicate the entire orchestration. Applying bugs 1–5 to both copies doubles edit + review cost.

**What:**
- Extract a single `pub fn run_stale_check(conn: &Connection, files: &[String], commits: &[String], blame: bool, repo_root: Option<&Path>) -> StaleCheckReport` into a new `src/commands/stale_check/mod.rs` module (split the existing single file).
- `StaleCheckReport { stale: Vec<StaleEntry>, review: Vec<ReviewEntry>, unreachable: Vec<UnreachableEntry> }` — the third bucket is for T5 but the struct lands here.
- CLI render function maps `StaleCheckReport` → human-readable `STALE/REVIEW/UNKNOWN` lines.
- MCP serialize function maps `StaleCheckReport` → existing JSON shape (T5 will add the `unreachable` array).

**Done when:**
- Both call sites import the shared helper.
- Existing tests still pass.
- Zero duplicated SQL string or git subprocess invocation across `stale_check.rs` and `mcp.rs`.

### T1 — Bug 1 (CRITICAL hotfix): UTF-8 panic in extract_blame_shas

**Why:** Live crash. Reproduced. MCP server dies on the first multibyte glyph in `git blame --porcelain` header lines.

**What:**
- Replace `line[..40].chars().all(...)` and `line[..40].to_string()` with byte-level operations:
  ```rust
  let b = line.as_bytes();
  b.len() > 40
      && b[..40].iter().all(u8::is_ascii_hexdigit)
      && b[40] == b' '
  ```
  Map: `String::from_utf8_lossy(&b[..40]).into_owned()` (the 40-byte slice is guaranteed ASCII by the predicate).

**Failing reproducibility test (must land before fix):**
- Unit test feeds a synthetic porcelain blob containing a `summary feat: Clever Cloud deployment — strip ...` header. Pre-fix: panics. Post-fix: returns expected SHA set without the header line.
- Integration test: run `extract_blame_shas` against a tempdir repo with a commit whose summary contains an em-dash. Assert no panic, correct SHAs.

**Commit policy:**
- Subject: `fix(stale-check): byte-slice porcelain SHA filter to fix UTF-8 panic (introduced in <shorthash>)`
- Body: `Introduced by: <longhash>` plus normal Constraint/Confidence/Scope-risk trailers.
- `git bisect` identifies the introducing commit before the fix lands.

### T2 — Bug 4 (MAJOR semantics): blame scope to commits-that-touched-file

**Why:** `extract_blame_shas` runs `git blame --porcelain <file>` and emits SHAs from the entire file's history. For a 1000-line file the SHA set is huge and floods the next SQL query. Wrong semantics for the docstring's stated goal ("surface KB entries that were current when the changed code was written").

**What:**
- Rename `extract_blame_shas` → `commits_that_touched_file`.
- Implementation: `git log --pretty=%H -- <file>` (deduped via `HashSet`).
- Update the porcelain-parsing unit tests to log-format tests.

**Done when:**
- For a fixture repo with file history (3 commits touching foo.rs) vs unrelated history (50 commits touching bar.rs), the function returns exactly the 3 foo.rs SHAs.

### T3 — Bug 3 (MAJOR perf): SQL index + fast-path + hoisted prepare

**Why:** All path lookups are full table scans. No index on `entries.path`. Leading-wildcard `LIKE` defeats any index. `conn.prepare()` is called inside the per-file loop.

**What:**
- Add `CREATE INDEX IF NOT EXISTS idx_entries_path ON entries(path);` to `ensure_schema` in `src/components/db.rs`.
- Split path matching into two queries:
  - **Fast path** (uses index): `WHERE path = ?1 AND is_stale = 0`.
  - **Substring fallback** (full scan, only on cache miss): `WHERE (path LIKE '%' || ?1 || '%' ESCAPE '\\' OR ?1 LIKE '%' || path) AND is_stale = 0`.
- Hoist both `Statement` preparations outside the file loop; reuse via `query_map` per file.
- Verify migration: open an existing DB (one without the index), confirm `CREATE INDEX IF NOT EXISTS` adds it without error.

**Done when:**
- `EXPLAIN QUERY PLAN` on the fast-path query shows `SEARCH USING INDEX idx_entries_path`.
- `EXPLAIN QUERY PLAN` on substring fallback still scans (expected).
- Test: stale-check with 100 entries × 26 files runs the fast path in O(file_count × log(N)) not O(file_count × N).

### T4 — Bug 2 (MAJOR perf): dedupe + git rev-list --count

**Why:** `commits_since` spawns one `git log --oneline VERSION..HEAD -- path` per matched row. 26 files × ~60 entries ≈ ~1500 subprocesses × ~5ms = ~7s fork/exec overhead. `--oneline` materializes commit messages just to count them.

**What:**
- Collect `(version_ref, stored_path)` tuples from matched rows into a `HashSet` before invoking git.
- Replace `git log --oneline VERSION..HEAD -- path` with `git rev-list --count VERSION..HEAD -- path`.
- Cache the count per tuple; reuse for every entry sharing that tuple.

**Done when:**
- Reproducer (26-file blame=true call against normatix) completes in < 500ms.
- Subprocess-count assertion test: with N entries sharing 4 distinct `(version_ref, path)` tuples, exactly 4 git invocations occur.

### T5 — Bug 5 (MEDIUM correctness): UNREACHABLE bucket for unreachable version_refs

**Why:** KB lives on `agentic` orphan branch but version_refs point at code-branch SHAs that may be unreachable (deleted branch, GC). `git rev-list` exits non-zero → currently returns `None` → silently treated as "not stale". User explicitly flagged this concern previously.

**What:**
- Before invoking `git rev-list --count`, probe with `git cat-file -e <version_ref>` to distinguish reachable vs unreachable. Or capture `git rev-list` stderr and parse for `unknown revision`.
- Add `UnreachableEntry { id, path, summary, version_ref }` to `StaleCheckReport`.
- CLI: emit `UNKNOWN [path] summary id=... version_ref=...` lines (per docstring `UNKNOWN`-style was suggested).
- MCP: add `unreachable: [...]` array to the result JSON.
- Update `mcp/lib/agentic_kb_mcp/mcp_server.ex` formatter (mcp_server.ex:472+) to render the new bucket.
- **Verify PreToolUse hook compatibility:** the `kb-stale-check` hook wired at `~/.claude/settings.json:219` (nix-store binary `/nix/store/.../kb-stale-check`) consumes CLI output. Confirm it (a) tolerates the new `UNKNOWN` line prefix without erroring, or (b) update the hook source to surface or filter the new bucket per design intent. If the hook is built from this repo's nix derivation, the update lands in this task; if it lives in `machines_conf` or elsewhere, open a follow-up issue and reference it in the T5 commit.

**Done when:**
- Fixture: build a tempdir repo, record a KB entry with `version_ref` set to a SHA that exists, then `git reset` / detach to make it unreachable. Run stale-check, assert entry lands in `unreachable` not silently skipped.
- MCP test: response shape includes `unreachable` key (additive — old `stale` and `review` still present, structure backward-compatible).
- PreToolUse hook compat: manual test confirms the hook either ignores `UNKNOWN` lines without erroring, or has been updated to handle them.

### T6 — TLA+ spec decision

**Why:** Project has `agent-kb/tla/`. Standing rule requires a TLA+ spec task before implementation.

**What:**
- Audit existing specs for any state machine covering stale-check semantics.
- Decision: skip new spec. Rationale: bugs are pure helpers — str-slicing safety (T1), git subprocess strategy (T2), SQL query shape (T3), commit-set scope (T4), output bucket (T5). No new state machine or invariant.
- Document the decision in `agent-kb/tla/decisions/stale-check-no-spec.md` so the gate is explicit-not-implicit.
- Code reviewer + analyst sign off on the skip during post-impl.

**Done when:**
- Decision doc committed to agentic branch.
- Plan-approval reviewer confirms skip.

### T7 — machines_conf agentic-config supersession audit

**Why:** User flagged that `.omc/MISSION.md` has been superseded by agentic-kb itself. The agentic config in `machines_conf` (`agentic-config`) likely still references MISSION.md gates. Audit + update needed so other projects don't keep hitting the MISSION.md absence gate when the convention has moved on.

**What:**
- Locate `machines_conf` repo / config files referencing `MISSION.md`.
- Document the supersession: agentic-kb (via `kb_search`/MCP) provides the per-project mission/context that MISSION.md previously held.
- Update agentic-config to either:
  - Drop the MISSION.md gate entirely, OR
  - Soften it to a warning when `agent-kb/` is present in the project.
- Update `AGENTS.md` / hooks accordingly so `kb_search("mission")` is the canonical source instead of file existence.

**Done when:**
- machines_conf changes **proposed** (PR opened or direct commit pushed; merge is **not** required to close this task — explicit to avoid cross-repo merge blocking on post-impl gate).
- AGENTS.md updated to reflect new gate semantics.
- This very project (agentic-kb) passes the new gate without an explicit MISSION.md file.

**Note:** parallel to T0–T5, no code dependencies.

### T8 — Docs

**What:**
- Update `kb_stale_check` MCP tool description in `mcp_server.ex:87-95` to document the new `unreachable` bucket and the blame semantics change.
- Update `kb stale-check --help` text (CLI subcommand docstring `src/commands/stale_check.rs:11-18`) — note the blame-scope change.
- Add a kb entry: `kb_add(path="runbooks/kb", summary="stale-check buckets: STALE / REVIEW / UNKNOWN", content="<bucket semantics + when each fires>", tags=["runbooks","kb"])`.
- Update agent-kb README if it references the old behavior.

**Done when:**
- Tool description in MCP matches actual output shape.
- KB runbook entry exists.

### T9 — Post-impl (mandatory gate, blocks merge)

`/post-impl` runs Blocks A–D in order:
- Block A: sync + rebase (drift detection)
- Block B: spec compliance via `/verify`
- Block C: code review loop (zero Critical findings required)
- Block D: user confirmation gate

Created by AGENTS.md hook; blocks docs task closure until done.

## Acceptance Criteria (whole-epic)

- **Reproducer no longer crashes:** the exact 26-file blame=true call against normatix KB returns valid JSON, no panic, no MCP-server restart.
- **Perf budget met:** the reproducer completes in < 500ms wall-clock.
- **Index present:** `EXPLAIN QUERY PLAN` shows `idx_entries_path` used on fast-path query.
- **Blame scope corrected:** for blame=true input, the SHA set returned by the helper is exactly the set of commits that touched the file (`git log --pretty=%H -- file`), not the file's full blame-line history.
- **Unreachable handled:** orphan-branch / unreachable-version fixture returns an entry in the `unreachable` bucket (not silent skip).
- **Regression test for panic:** synthetic porcelain blob with em-dash header → unit test passes.
- **No duplicated orchestration:** SQL strings and git subprocess invocations exist in exactly one place after T0.
- **Backward compat:** existing `stale` and `review` keys in MCP JSON unchanged in shape.
- **MISSION.md gate sanity:** AGENTS.md / machines_conf no longer surface the MISSION.md gate for projects with `agent-kb/`.

## Risk

- **Schema migration silent failure** if SQLite < 3.8 (`CREATE INDEX IF NOT EXISTS` requires 3.8). Mitigation: bundled rusqlite ships ≥ 3.40 — non-issue.
- **`git rev-list --count` vs `git log` behavior diff** for the empty-range case — both should output 0 / empty. Add a test pinning this.
- **`git cat-file -e` ref probe** adds one subprocess per distinct `version_ref` — acceptable since dedupe (T4) collapses these. Worst case ≈ 4 invocations for normatix's 4 distinct version_refs.
- **MCP output contract change (T5)** may break external consumers if any inspect the schema strictly. Mitigation: bucket is additive (`unreachable: []` on old behavior matches new default for reachable repos).

## Out of Scope

- No changes to `kb_search`, `kb_add`, or other MCP tools.
- No FTS or semantic-search work.
- No changes to `kb rebuild`, `kb compact`, `kb reembed` paths.
- No e2e browser tests (Rust unit + integration tests only).
