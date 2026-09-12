# Open Beads execution manifest

Snapshot: 2026-09-12. This coordination artifact reports authoritative Beads state and
worktree evidence. It does not itself change task status, dependency edges, or product files.

## Objective and constraints

Implement the remaining agentic-kb-mcp Beads work with native Sol, Terra, and Luna agents only.
Do not use CSR. The user authorized implementation but has not authorized merge. Preserve unrelated
dirty worktree state and keep implementation lanes isolated until their review and integration gates.

The selected OPA contract is Option B: anyone with filesystem/Git write access to the backing JSONL
may invoke every MCP tool. The package adds no authentication, authorization, caller-based quota, or
Git-permission validation. Structural and data-integrity validation remain required.

## Closed evidence

| Work | Authoritative evidence |
|---|---|
| `bd-pia3.1` startup model | Closed. Fresh TLC runs: `McpPackageStartup_Buggy.cfg` has the expected `EventuallyReady` counterexample (`launching -> failed`; 3 generated / 2 distinct states, exit 13); `McpPackageStartup_Fixed.cfg` passes (`No error has been found`; 3 generated / 2 distinct states, depth 2, exit 0). Commits `77f451e` and `78c9cdb`. The 31-state, six-scenario TLC coverage belongs to `McpBoundary`, not this startup model. |
| `bd-pia3` package startup | Closed. The original runtime closure fix is proven by its startup model and package smoke lane. |
| `bd-bvy4.6` reliability specification | Closed. Shared specification is on `agentic` at `32cb…`; its TLA+ disposition covers the current reliability lanes. |
| `bd-dhi0.1` Option B contract tests | Closed. The implementation lane began from `a150081`; focused Rust test was 3/0 RED before source changes. |
| `bd-dhi0.2` OPA implementation | Closed at `0294f1e`. Active OPA/auth/caller/rate-limit code and package/dev wiring scans are clean; a clean-locale local escript JSON-decodes initialize and all 17 tools; retired caller and invalid-startup checks pass; Mix is 70/0. The final integrated Nix package/closure gate remains `bd-dhi0.4`. |

## Active implementation lanes

| Lane / Beads | Head and current evidence | Required next result |
|---|---|---|
| JSON-RPC validation, frames, version: `bd-bvy4.2`, `.3`, `.4` | Protocol worktree now includes approved direct-port change `49b4994` atop `7310e35` and `34e18e6`. `McpServer` is the sole direct fd 0 owner; the six framing scenarios and their specification gates are unchanged. | Integrate with lifecycle only after the final EOF/lifecycle review, then re-run the relevant suite and Nix check. |
| Production lifecycle, `bd-bvy4.1` | Lifecycle integration head is `ddb1728`; its 72-test Mix suite and process scripts pass. | Keep it isolated pending integration and review; run the default and isolated suites again after integration with protocol/OPA changes. |

## Open Beads snapshot

Beads currently reports seven open items and four in progress.

| Status | IDs |
|---|---|
| In progress | `bd-bvy4.1`, `bd-bvy4.2`, `bd-bvy4.3`, `bd-bvy4.4` |
| Open implementation/refactor | `bd-bvy4.5` |
| Open documentation | `bd-dhi0.3`, `bd-bvy4.7` |
| Open post-implementation gates | `bd-dhi0.4`, `bd-bvy4.8` |
| Parent epics | `bd-dhi0`, `bd-bvy4` |

`bd-bvy4.5` remains deliberately separate from OPA removal. It needs regression coverage before
the behavior-preserving MCP-server split. Documentation tasks remain blocked behind their relevant
implementation and post-implementation prerequisites. Do not close a parent merely because a child
lane has a passing local suite.

## Integration and completion order

1. Land each active lane only after its Critical findings and lane-specific verification are resolved. The cleanup plan has Sol approval; the documentation plan has CISO approval after three corrections, but neither approval closes a task.
2. Integrate startup/OPA changes carefully because they share package and CLI surfaces; retain the
   clean-environment startup test and prove OPA is absent from the final closure.
3. Integrate protocol and lifecycle reliability changes after their independent default-runner and
   pipe-bound defects are fixed; run the relevant full Elixir suite after integration.
4. Execute the isolated `bd-bvy4.5` refactor with regression coverage, then complete `bd-bvy4.7`.
5. Run `bd-dhi0.4` and `bd-bvy4.8`: specification compliance, test suites, Nix startup/closure,
   security/code review, changed-file inventory, and documented residual risks.
6. Complete `bd-dhi0.3` and `bd-bvy4.7`. Merge remains blocked until those gates are complete and
   the user explicitly confirms a concrete final result.

## Current risks

- The final Nix package/closure proof, including clean-PATH JSON decoding of `tools/list`, is deferred
  to `bd-dhi0.4` after integration; it is not implied by the local OPA implementation proof.
- The approved direct-port protocol implementation preserves the six framing scenarios; its remaining
  integration risk is EOF/lifecycle behavior with the isolated lifecycle lane.
- Lifecycle head `ddb1728` is locally green (72 Mix tests and process scripts) but remains isolated
  until review and integration evidence exists.
- The Criterion benchmark started by `cargo test --all-targets --locked` was intentionally terminated
  after functional evidence was recorded; it is not a test failure.
- Active OPA documentation and historical changelog content must be separated during `bd-dhi0.3` so
  current claims change without rewriting historical release records.
