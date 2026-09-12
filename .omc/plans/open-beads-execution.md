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
| `bd-pia3` package startup | Closed. The recovered Nix build realizes `/nix/store/gic258s971kd9mgkxi8vl6fr4w65aqz7-agentic-kb-mcp-0.2.0`; package smoke and runtime fix evidence is on the startup lane. |
| `bd-bvy4.6` reliability specification | Closed. Shared specification is on `agentic` at `32cb…`; its TLA+ disposition covers the current reliability lanes. |
| `bd-dhi0.1` Option B contract tests | Closed. The implementation lane began from `a150081`; focused Rust test was 3/0 RED before source changes. |

## Active implementation lanes

| Lane / Beads | Head and current evidence | Required next result |
|---|---|---|
| OPA excision, `bd-dhi0.2` | `bd-bvy4-lifecycle`, base `a150081`; OPA excision is implemented with uncommitted review fixes. Rust focused tests are 129/0; full Elixir suite 70/0. Latest Nix build is live and may predate the CLI review fix. | Commit review fixes; run the final package build/startup/closure check after the CLI fix; then run `bd-dhi0.4`. |
| JSON-RPC validation, frames, version: `bd-bvy4.2`, `.3`, `.4` | `bd-bvy4-protocol` HEAD `34e18e6`, after `248b6dc`, `8308efd`, `9ce8e94`. Full suite is 101/0 and Nix build is valid. | Fix the Critical oversized-frame performance defect: per-byte `IO.binread(:stdio, 1)` makes the 10 MiB pipe test time out with exit code 124. Re-run the full suite and Nix check. |
| Production lifecycle, `bd-bvy4.1` | `bd-bvy4-lifecycle` HEAD `643ec83` atop `a150081`; 72/0 with `mix test --no-start`. | Fix the Critical default-runner failure: ordinary `mix test` must exit successfully at EOF and must not eagerly require default configuration. Re-run both default and isolated suites. |

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

1. Land each active lane only after its Critical findings and lane-specific verification are resolved.
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

- The latest live Nix build may predate the OPA CLI review fix; a fresh final package check is required.
- Protocol framing has a confirmed Critical per-byte read performance defect: the 10 MiB pipe test times out with exit code 124 despite its current 101/0 suite.
- Lifecycle tests pass only under `--no-start`; default application startup still needs the EOF and
  default-configuration correction.
- Active OPA documentation and historical changelog content must be separated during `bd-dhi0.3` so
  current claims change without rewriting historical release records.
