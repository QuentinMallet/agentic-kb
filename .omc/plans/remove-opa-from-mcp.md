# Remove OPA from the MCP package

Status: design approved by direct user ruling; execution authorized 2026-09-12  
Planning mode: roadmap-aware RALPLAN-DR deliberate  
Roadmap item: `bd-0nxc` (P2 standalone task)  
Version scope: proposed `0.3.0` because removal changes the documented launch and denial contract while the project is `0.2.x`  
Threat-model workflow: skipped because `.omc/threat-model/config.toml` is absent; security analysis remains explicit below  
Meta-program binding: none proven; the stale closed `storage-correctness-2` state is not a cascade target

## Objective and stop condition

Remove the bundled OPA evaluator, policies, runtime dependency, `OPA_BIN`, and pseudo-identity wiring. Preserve every tool plus structural validation and data-integrity protections. Repository/filesystem write access to the backing Git repository and JSONL is the trust boundary. The MCP adds no caller authentication, authorization, per-tool permission gate, or Git-permission validation; ordinary OS filesystem permissions remain. Any retained caller attribution is untrusted metadata only.

The implementation is complete only when every tool has an explicit, tested authorization behavior under the selected boundary, the package starts and serves `initialize` and `tools/list` without OPA, documentation describes the resulting trust boundary accurately, and no OPA/policy artifact remains in the package closure or public launch contract.

This plan does not include the independent escript/`kb` runtime packaging fix in `bd-pia3`; OPA removal must not make OPA a prerequisite for initialization. Coordinate source overlap in `flake.nix`, `mcp/lib/agentic_kb_mcp/application.ex`, package smoke tests, and MCP documentation.

## RALPLAN-DR deliberate summary

### Principles

1. Security claims must match an enforceable identity boundary.
2. Removing OPA means removing the package dependency and policy runtime, not relocating OPA outside the package.
3. Every MCP tool must have an explicit authorization classification; permissive fallthrough is forbidden.
4. Keep the change narrow: preserve Rust database semantics, MCP schemas, validation, and rendering.
5. Package startup and authorization policy are separate contracts and should remain separately testable.

### Decision drivers

1. The user explicitly scoped agentic-kb-mcp out of mandatory OPA and per-caller package authorization: ability to edit the backing Git repository/JSONL is sufficient authority.
2. OPA currently gates only `kb_expire`, `kb_audit_run`, and `kb_audit_record`; other mutating tools already bypass it through `authz_actions/2`'s empty default.
3. The desired artifact is a smaller, self-contained MCP package without `open-policy-agent`, temporary policy-input files, or an `OPA_BIN` runtime branch.

### Options considered

#### Option A — historical alternative: host-owned authorization boundary

Remove all in-process authorization and caller-id logic. The MCP process accepts every advertised tool call, while a verified external host authenticates each caller and authorizes each call before dispatch, forwarding, or tool exposure. Static launch permission alone is insufficient evidence.

Pros:

- Smallest implementation and package closure.
- Accurately reflects stdio's process ownership model when the host is authoritative.
- Removes misleading per-caller claims that cannot be proven inside the server.

Cons:

- All three formerly gated tools become callable by any client that reaches the process.
- Safe only if the actual MCP host enforces identity and tool permissions before starting or forwarding to the process.
- This repository currently contains no evidence of that host protocol, so the recommendation is conditional rather than approved.

#### Option B — selected: repository/filesystem write access is the trust boundary

Remove OPA, caller-id authorization, and package permission gates while preserving every tool. Repository/filesystem write access to the backing Git repository and JSONL is sufficient authority. The MCP adds no new guard and does not validate Git permissions. This is the user's explicit scoped override of mandatory OPA and per-caller package authorization for agentic-kb-mcp.

Pros:

- Removes OPA without deleting product functionality.
- Matches the existing process-reachability behavior of most mutating tools.
- Preserves tool discovery and agent workflows.

Cons:

- Broadens access to the three operations that currently have package checks.
- Removes their denial and per-caller rate-limit behavior.
- Deployments must not expose writable-repository MCP capability to parties not trusted to edit that repository.

#### Option C — replace OPA with a real in-process authentication/authorization mechanism

Define and implement an authenticated host-to-server protocol, then enforce a complete per-tool policy in-process without OPA.

Pros:

- Can provide genuine caller-level authorization.
- Enables explicit policy for every tool.

Cons:

- No acceptable identity primitive or credential transport is specified.
- Materially expands scope into protocol, secret lifecycle, replay protection, and host integration.
- Conflicts with the request for OPA excision if used merely to rebuild an equivalent policy engine locally.

This option is not implementation-ready until the user selects the identity mechanism and host integration contract. A non-OPA mechanism is an explicit scoped override of the workspace's OPA-only default and must be recorded in the ADR.

### Approved security decision

Option B is selected by direct user ruling. The backing repository/JSONL write boundary is authority; the MCP adds no permission check. All tools remain available. Structural validation, closed schemas, bounds, locking, citation verification, and other data-integrity protections remain. Options A and C are historical alternatives only.

## Tool behavior matrix

The selected Option B must be encoded as a table-driven test so every tool is proven available without a package permission gate.

| MCP tool | Class | Option A | Option B | Option C |
| --- | --- | --- | --- | --- |
| `kb_search` | read | host boundary | host boundary | explicit read scope |
| `kb_get` | read | host boundary | host boundary | explicit read scope |
| `kb_tests` | read | host boundary | host boundary | explicit read scope |
| `kb_stale_check` | read/git inspection | host boundary | host boundary | explicit inspect scope |
| `kb_audit_report` | read/telemetry | host boundary | host boundary | explicit audit-read scope |
| `kb_provenance` | read | host boundary | host boundary | explicit read scope |
| `kb_add` | mutation | host boundary | host boundary | explicit entry-write scope |
| `kb_cite` | filesystem read | host boundary | host boundary | explicit citation-read scope |
| `kb_import` | bulk mutation/file read | host boundary | host boundary | explicit import scope |
| `kb_expire` | destructive logical mutation | host boundary | available; no package check | explicit expire/force scopes |
| `kb_run` | mutation | host boundary | host boundary | explicit run-write scope |
| `kb_test_add` | mutation | host boundary | host boundary | explicit test-write scope |
| `kb_reembed` | expensive mutation | host boundary | host boundary | explicit maintenance scope |
| `kb_compact` | destructive maintenance | host boundary | host boundary | explicit maintenance scope |
| `kb_rebuild` | expensive maintenance | host boundary | host boundary | explicit maintenance scope |
| `kb_audit_run` | audit mutation/expensive read | host boundary | available; no package check | explicit audit-run and traffic scopes |
| `kb_audit_record` | mutation/may expire | host boundary | available; no package check | explicit audit-record plus expire scope |

Option B preserves all mutations and opens the three currently gated operations under the explicitly approved repository/filesystem trust boundary.

## Implementation sequence after approval

### 1. Lock the selected contract with failing tests

- `mcp/test/agentic_kb_mcp_test.exs`
- `mcp/test/agentic_kb_mcp_test.exs` or a new focused `mcp/test/tool_access_contract_test.exs`
- `mcp/test/package_smoke_test.sh`
- `tests/mcp_authorization.rs` only if the Rust caller-id request contract changes

Prove every tool remains in `tools/list`, dispatches without an authorization process or permission gate, and preserves structural validation. Client `caller_id` remains an unknown schema field; any other retained caller attribution is untrusted metadata.

Add a package smoke assertion that `initialize` and `tools/list` work with a clean environment and no `OPA_BIN`/`opa`. Keep this test coordinated with `bd-pia3` rather than duplicating its runtime packaging work.

### 2. Remove runtime OPA and pseudo-identity wiring

- `mcp/lib/agentic_kb_mcp/application.ex`
- `mcp/lib/agentic_kb_mcp/mcp_server.ex`
- `mcp/lib/agentic_kb_mcp/cli.ex`
- `mcp/lib/agentic_kb_mcp/authorization.ex` (delete)
- `mcp/lib/agentic_kb_mcp/opa_evaluator.ex` (delete)
- `mcp/lib/agentic_kb_mcp/rate_limiter.ex` (decision below)

Remove `policy_dir`, the `Authorization` child, authorizer state, `authorize_tool/3`, `authz_actions/2`, `trusted_caller/1`, injected `caller_id`, `--caller-id` parsing, `OPA_BIN`, and OPA error mappings. Update Rust request structs/handlers only where removing injected `caller_id` requires the chosen contract to change.

- Delete `RateLimiter` and caller-based quotas. Any identity-free limiter is separate future work and is not introduced here.

### 3. Remove policy and package artifacts

- `mcp/priv/policies/agentic_kb.rego` (delete)
- `mcp/priv/policies/agentic_kb_test.rego` (delete)
- `mcp/test/opa_policy_contract_test.exs` (delete)
- `flake.nix`

Remove `open-policy-agent` from the MCP wrapper/runtime closure and from the development shell if no other repository workflow uses it. Verify that the built MCP package closure contains no OPA path and starts with `env -i` through the package smoke test.

### 4. Update contract documentation and release scope

- `mcp/README.md`
- `docs/src/mcp.md`
- `docs/src/security/mcp-authorization.md` (delete or replace with a trust-boundary page)
- `docs/src/SUMMARY.md`
- `docs/decisions/b1-request-contract.md`
- `CHANGELOG.md`
- `mcp/mix.exs`, `Cargo.toml`, and `flake.nix` during a separate release commit

Document the selected host/process boundary and remove all promises involving OPA, `--caller-id`, `OPA_BIN`, policy availability, action scopes, and per-caller quotas. Preserve the B1 closed-request contract: removing host-injected `caller_id` from Rust methods changes the internal port contract and must be reflected in the decision record and cross-language tests.

Treat this as a breaking change under the project's `0.y.z` rule: target `0.3.0`. Perform the version bump and changelog release section only in a dedicated `chore(release): v0.3.0` commit after the implementation is merged and approved.

### 5. Defer MCP module split until behavior stabilizes

After excision, a separate cleanup can split the 1,400-line `McpServer` into transport, tool registry, dispatch, and rendering modules. Do not couple that refactor to OPA removal. The excision should leave one explicit tool registry/access matrix seam that the later split can consume.

## Test and verification plan

### Unit

- Registry closure: every `tools/list` entry has exactly one access classification.
- Unknown tool and unknown argument behavior remains unchanged.
- All existing tools remain listed and callable under the repository/filesystem trust boundary.
- Retired `--caller-id` is explicitly rejected as unsupported, not silently accepted or ignored.
- Remove obsolete authorization, OPA parsing, timeout, caller spoofing, and rate-limit unit tests.
- Rust request deserialization tests cover the post-removal presence/absence of `caller_id` exactly.

### Integration

- Start the supervised application without OPA installed and exercise all retained tools through the real `tools/call` path using the fake Rust port.
- Exercise `kb_expire`, `kb_audit_run`, and `kb_audit_record` without a package permission gate under selected Option B.
- Malformed or client-supplied `caller_id` is rejected as an unknown schema field; retained attribution is untrusted metadata only.
- Run `mix test`, Rust targeted MCP tests, format checks, Clippy, and `cargo test` as applicable.

### End to end/package

- Build `.#mcp` and launch it with `env -i` using only the package wrapper.
- Send `initialize`, `tools/list`, and at least one retained read request.
- Confirm initialization succeeds without `opa`, `OPA_BIN`, policy files, or `--caller-id`.
- Inspect the Nix closure/references and fail if `open-policy-agent` remains.
- Scan active source, tests, package definitions, and current docs for `OPA_BIN`, Rego-policy, OPA evaluator, authorization-process, and caller-id contract references; preserve historical changelog entries.
- Verify `agentic-kb-mcp --caller-id anything` fails clearly under Option B.

### Observability

- Startup stderr must contain no OPA lookup/policy errors.
- Retain structured error visibility for Rust port failures and invalid MCP requests.
- If Option A is selected, authorization-denial telemetry belongs to the verified host boundary; document how it is observed.
- Any later identity-free throttling work must define observability without recording request content or secrets.

## Three-scenario pre-mortem

1. **OPA is removed without an understood authority boundary.** Early signal: Option A cannot demonstrate host rejection, or Option B lacks explicit acceptance of broadened access. Mitigation: block A until host enforcement is tested; block B until the user explicitly accepts removal of package checks.
2. **Cross-language port contracts drift when `caller_id` injection is deleted.** Early signal: Elixir fake-port tests pass while Rust typed deserialization rejects expire/audit requests. Mitigation: update the shared request-contract fixture and run real Elixir-to-Rust integration cases for all three methods.
3. **The startup fix and OPA excision conflict in `flake.nix` or package smoke coverage.** Early signal: either branch reintroduces OPA to satisfy PATH or drops the clean-PATH test. Mitigation: keep `bd-pia3` responsible for escript/`kb` runtime packaging, rebase the excision work after it, and assert OPA absence as an additive smoke criterion.

## Acceptance criteria

1. The ADR records the direct user ruling that repository/filesystem write access to the backing Git repository/JSONL is sufficient authority and the MCP adds no permission check.
2. `Authorization`, `OpaEvaluator`, Rego policies, `OPA_BIN`, OPA package references, caller-id launch parsing, and OPA denial documentation are absent.
3. Every advertised MCP tool has one explicit tested access behavior; no permissive catch-all determines mutation access.
4. Package initialization and `tools/list` succeed in a clean environment without OPA.
5. The selected behavior for expire/audit run/audit record passes unit, integration, and package-level checks.
6. Rust and Elixir request contracts agree after caller-id removal.
7. Documentation accurately states the host/process trust boundary and does not call argv metadata authentication.
8. No unrelated MCP refactor or startup-runtime implementation is bundled into this change.
9. Release notes identify the breaking contract and the eventual release uses `0.3.0` in a separate release commit.
10. The package contains no `RateLimiter`, caller-keyed quota, or per-tool permission gate and rejects retired `--caller-id`.
11. Active source, tests, package definitions, and current docs contain no OPA/policy runtime references; historical changelog entries remain intact.

## ADR

### Decision

Option B is approved by direct user ruling: repository/filesystem write access to the backing Git repository and JSONL is the trust boundary. The MCP adds no caller authentication, authorization, per-tool permission gate, or Git-permission validation. Ordinary OS filesystem permissions remain. Execution was authorized by direct user instruction on 2026-09-12.

### Drivers

- Remove OPA as an actual runtime/package dependency.
- Avoid claiming authentication from unverified argv metadata.
- Preserve a reviewable, explicit per-tool contract.

### Alternatives considered

- External host-owned per-caller/per-call boundary before dispatch or forwarding (A).
- Remove package checks while preserving every tool, explicitly overriding per-caller/per-call authority invariants (B).
- New authenticated in-process mechanism, explicitly overriding the OPA-only default for this scope (C).
- Withdrawing formerly gated MCP tools: rejected because the user requested OPA excision, not feature removal.
- Replacing OPA with hard-coded role checks: rejected because it recreates authorization policy without a trustworthy identity and violates the repository's declared OPA-only authorization convention.
- Moving OPA entirely to an external service: rejected because the user requested actual OPA dependency removal, not relocation.

### Why chosen

The user ruled that anyone able to edit the Git repository where the JSONL lives may perform any MCP operation. This preserves every tool and all non-permission structural/data-integrity protections while fully excising OPA.

### Consequences

- Public launch and denial behavior changes, requiring `0.3.0` under the project's pre-1.0 SemVer policy.
- Existing deployments using `--caller-id`, `OPA_BIN`, or custom Rego policies require migration.
- Rate limiting is removed unless separately reframed as identity-free resource control.
- Deployments must not expose writable-repository MCP capability to parties not trusted to edit that repository.

### Follow-ups

- Separate MCP module decomposition after the security contract stabilizes.
- Consider a complete authorization design only when an authenticated host protocol exists.
- Record reusable findings in agent-kb when `kb_*` MCP tools become available; they were unavailable during this planning pass, and the prohibited CLI was not used.

## Rollout and rollback

Rollout:

1. Land the independent `bd-pia3` package-runtime fix first or rebase onto its final `flake.nix` shape.
2. Implement Option B in the shared MCP integration worktree after epic/task decomposition.
3. Run unit and cross-language integration tests, then build and inspect the Nix package closure.
4. Update docs and migration notes before merge.
5. Merge only after post-implementation security review and user confirmation; publish `0.3.0` separately.

Rollback:

- Before release, revert the atomic excision commits while retaining the independent startup fix.
- After `0.3.0`, restore behavior only in a new patch/minor release as SemVer permits; do not move tags.
- Preserve the last `0.2.x` package for deployments that require the old OPA contract during migration.

## Roadmap and execution handoff

- `bd-dhi0` is the standalone OPA-excision epic; `bd-0nxc` is its approved planning child. The repository workflow orders execution as contract tests `.1` → implementation `.2` → post-implementation code/security gate `.4` → documentation `.3`.
- `bd-bvy4` remains separate with specification task `.6` ready.
- `bd-pia3` remains the startup-fix lane, with independent ready task `bd-pia3.1`; coordinate shared paths but do not make OPA a startup prerequisite.
- Design is approved with Option B selected, and execution is authorized.
