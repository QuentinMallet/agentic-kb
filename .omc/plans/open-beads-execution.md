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
| `bd-pia3.1` startup model | Closed. TLC covers 31 states and six scenarios; commits `77f451e` and `78c9cdb`. |
| `bd-pia3` package startup | Closed. The original runtime closure fix is proven by its startup model and package smoke lane. The OPA integration now also has a later e848 package build; its strict `tools/list` JSON decoding remains an active integration blocker below. |
| `bd-bvy4.6` reliability specification | Closed. Shared specification is on `agentic` at `32cb…`; its TLA+ disposition covers the current reliability lanes. |
| `bd-dhi0.1` Option B contract tests | Closed. The implementation lane began from `a150081`; focused Rust test was 3/0 RED before source changes. |

## Active implementation lanes

| Lane / Beads | Head and current evidence | Required next result |
|---|---|---|
| OPA excision, `bd-dhi0.2` | Startup/CLI head is `e848bf4`. Nix package `/nix/store/gsqakdnj4k4dpkiy3wcpffphmz40pkx0-agentic-kb-mcp-0.2.0` builds; clean-PATH startup, retired caller flag, invalid `KB_BIN`, and OPA-closure checks pass. Rust functional suite is 876 passed, 0 failed, 4 ignored; Elixir is 70/0. | Fix the invalid Unicode wire encoding in the actual `tools/list` response and prove JSON decoding plus all 17 tools from the package. Then rebuild the final package and run `bd-dhi0.4`. |
| JSON-RPC validation, frames, version: `bd-bvy4.2`, `.3`, `.4` | `bd-bvy4-protocol` committed HEAD remains `34e18e6` after `248b6dc`, `8308efd`, `9ce8e94`; it is not yet accepted. A separate uncommitted `-noinput` prototype is green for 10 MiB recovery. | Finish EOF/lifecycle behavior and review the prototype before accepting or committing it; then re-run the relevant suite and Nix check. |
| Production lifecycle, `bd-bvy4.1` | Lifecycle integration head is `ddb1728`; its 72-test Mix suite and process scripts pass. | Keep it isolated pending integration and review; run the default and isolated suites again after integration with protocol/OPA changes. |

## Open Beads snapshot

Beads currently reports seven open items and five in progress.

| Status | IDs |
|---|---|
| In progress | `bd-dhi0.2`, `bd-bvy4.1`, `bd-bvy4.2`, `bd-bvy4.3`, `bd-bvy4.4` |
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

- The e848 package has a strict protocol blocker: `tools/list` contains a literal `\x{2014}` escape,
  which is invalid JSON. Startup smoke's grep checks pass, but a JSON decoder correctly rejects it.
- Protocol framing's `-noinput` prototype exercises 10 MiB recovery, but remains uncommitted pending
  EOF/lifecycle semantics and review; committed `34e18e6` is not accepted.
- Lifecycle head `ddb1728` is locally green (72 Mix tests and process scripts) but remains isolated
  until review and integration evidence exists.
- The Criterion benchmark started by `cargo test --all-targets --locked` was intentionally terminated
  after functional evidence was recorded; it is not a test failure.
- Active OPA documentation and historical changelog content must be separated during `bd-dhi0.3` so
  current claims change without rewriting historical release records.
