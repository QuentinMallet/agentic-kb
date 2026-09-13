# MCP Beads execution manifest

Snapshot: 2026-09-13, Phase 4 push and epic closure complete.

## Current decision boundary

All tasks, including the OPA excision epic (`bd-dhi0`) and MCP reliability epic
(`bd-bvy4`), are closed. There are no ready Beads and no missing implementation,
documentation, or post-implementation work.

The concrete merge candidate is branch `mcp-final-integration` at
`28104b5fb33a2c3b82c60917282eb410ab7240a1`, based on master
`740d4ea9c31697e80de27807fac5590dd9398be6`. It is a standalone program whose
merge target is `master`. The local branch tracks `origin/mcp-final-integration`;
this audit performed no fetch, push, rebase, merge, or release bump.

## Closed scope

| Work | Final evidence |
|---|---|
| Startup and package closure, `bd-pia3` / `.1` | Package startup model fixed. `McpPackageStartup_Buggy.cfg` has the expected `EventuallyReady` counterexample (`launching -> failed`, 3 generated / 2 distinct states, exit 13); `McpPackageStartup_Fixed.cfg` passes (3 generated / 2 distinct, depth 2, exit 0), recorded in `f259bf2`. |
| Option B OPA excision, `bd-dhi0.1` / `.2` / `.4` | OPA/auth/caller/rate-limit runtime code and package/dev wiring are absent; Rust/Elixir caller-free contracts are covered. Final package proof at `1bcf948`: `/nix/store/wcrk7nbnghl2gjw14bihsisra1bx45ly-agentic-kb-mcp-0.2.0` passes strict clean-PATH JSON decoding, Unicode, exactly 17 ordered tools, retired `--caller-id`, invalid `KB_BIN`, and OPA-free closure checks. |
| Reliability, `bd-bvy4.1` through `.4` / `.6` / `.8` | Production lifecycle, JSON-RPC validation/notifications, 10 MiB framing/EOF/restart recovery, and manifest version tests integrated. `McpBoundary` retains its six framing scenarios and 31-state positive disposition; the unsafe configuration is the expected bounded-buffer counterexample. |
| Behavior-preserving cleanup, `bd-bvy4.5` | Tool registry, request-map, and renderer extraction committed with characterization coverage; no new dependency or public MCP contract change. Sol review found no Critical or Important finding. |
| Documentation, `bd-dhi0.3` / `bd-bvy4.7` | `28104b5` documents the repository trust boundary, version-scoped B1 amendment, migration/rollback guidance, lifecycle/framing/version contracts, and Unreleased breaking change while preserving historical release records. `nix build .#doc` passed. |

## Verification record

- `nix build .#mcp` passed at `1bcf948`; strict package smoke, retired-caller,
  startup-failure, source active-contract, and closure scans passed.
- Final integrated Elixir suite: 95 tests, 0 failures. Default-runner, production
  lifecycle, and real OS-pipe framing/restart scripts passed.
- Rust functional evidence: 876 passed, 0 failed, 4 ignored. No behavioral Rust
  change followed it. `c1b90bb` contains only rustfmt output in three Rust files;
  fresh `cargo fmt --all -- --check` passed.
- `nix flake check`, Rust Clippy with warnings denied, Mix compile with warnings
  denied, and Mix format check passed.
- CISO and Sol final reviews reported zero Critical or Important findings.

The package artifact predates `c1b90bb` (formatter-only) and `28104b5`
(documentation-only). It is behaviorally applicable to the final source but is
not byte-identical to a package rebuilt from the final commit; this was an accepted
verification scope decision, not a claim of identical derivation output.

## Final inventory and known limits

The candidate diff from master contains the OPA removal, lifecycle/JSON-RPC/frame/
version fixes, registry/request/renderer split, tests, TLA+ evidence updates, and
documentation changes. The generated `mcp/agentic_kb_mcp` artifact is removed from
source control. No unresolved implementation or review finding remains.

Phase 4 is complete. The user explicitly authorized the concrete
`mcp-final-integration -> master` merge and the subsequent normal push to
`origin/master`; both epics were closed individually through `br` and flushed.
No worktree cleanup, release bump, tag, or publication was requested or performed.

## Phase 4 local status

The user then authorized the merge. Local master contains clean non-fast-forward
merge commit `2ba68d6640c40ae254cfeee60dc84432b2f600f5`
(`merge: integrate MCP reliability and Option B`) and includes candidate
`28104b5`. The user subsequently explicitly authorized `git push origin master`.
It succeeded: `740d4ea..2ba68d6  master -> master`. An authoritative
`git ls-remote origin refs/heads/master` returned
`2ba68d6640c40ae254cfeee60dc84432b2f600f5`, exactly the approved merge commit.
`bd-dhi0` and `bd-bvy4` were then closed individually and each closure was flushed.
No worktree cleanup occurred.

The original package path became unavailable (`ENOENT`) for a later stale-check,
but that artifact gap is resolved: a fresh local final-integration escript with
the current `target/debug/kb` invoked `kb_stale_check` over all 51 paths in
`740d4ea..master`, including deleted paths, with `blame:true`. It reported no
stale or unreachable entries. One advisory REVIEW entry remains:
`conventions/cross-repo/evidence-contract-notification`
(`ed43f414-c2e7-4500-ad77-d2c13e57d523`), matched by commit/blame.

## Merge-boundary review record

Sol approved the combined candidate at
`28104b5fb33a2c3b82c60917282eb410ab7240a1` against base
`740d4ea9c31697e80de27807fac5590dd9398be6`: the worktree was clean and
the review reported no concerns. The generic `nix build .#all` merge command
does not apply because this flake exposes no `all` package. Its project-specific
build surfaces are `.#default` (the Rust `kb` package), `.#mcp`, and
`.#doc`; `.#mcp` transitively realized the default Rust package and docs were
verified separately. The package proof at `1bcf948`, formatter-only
`c1b90bb`, and documentation-only `28104b5` preserve the recorded
provenance. Rust 876/0/4, Mix 95/0, TLC, Clippy, flake-check, and process evidence
remain applicable. The local merge and remote push are complete; the remote tip is
exactly `2ba68d6640c40ae254cfeee60dc84432b2f600f5`. The documented future 0.3.0
release bump/tag remains a separate, unrequested action.
