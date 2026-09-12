# MCP final post-implementation verification

Status: prepared only. Do not run any command in this document until cleanup and
documentation changes are committed, the integration worktree is frozen, and the
final commit SHA is recorded. This plan covers `bd-dhi0.4` and `bd-bvy4.8`; it
does not close either task, the two parent epics, or authorize a merge.

Run from `.state/worktrees/mcp-final-integration` at the final SHA. Preserve each
command's exit status and write its output under `/tmp` with that SHA in the name.

## Preconditions and scope

1. `br show bd-dhi0.4` and `br show bd-bvy4.8` must show all implementation
   dependencies closed. `bd-bvy4.8` also requires `bd-bvy4.5`; it cannot start
   while the behavior-preserving refactor remains open.
2. The worktree must be clean, rebased/synced as required by the post-implementation
   process, and source changes must remain frozen throughout this run.
3. Capture `FINAL_SHA=$(git rev-parse HEAD)` and `git diff --name-only master...HEAD`
   first. That inventory is the basis for the review and the Rust-reuse decision.

## Required verification order

1. **Static and formatting checks.** Run the CI-equivalent checks from the frozen
   worktree:

   ```bash
   nix flake check
   nix develop --impure --command cargo clippy --all-targets --all-features -- -D warnings
   nix develop --impure --command cargo fmt --check
   nix develop --impure --command bash -lc 'cd mcp && mix compile --warnings-as-errors'
   nix develop --impure --command bash -lc 'cd mcp && mix format --check-formatted'
   ```

2. **Reliability and MCP functional suite.** Run the complete Elixir suite and
   concrete production/process checks, retaining individual exit codes:

   ```bash
   nix develop --impure --command bash -lc 'cd mcp && mix test'
   nix develop --impure --command bash -lc 'cd mcp && bash test/default_test_runner_test.sh'
   nix develop --impure --command bash -lc 'cd mcp && MIX_ENV=prod bash test/application_process_test.sh'
   nix develop --impure --command bash -lc 'cd mcp && bash test/stdio_pipe_test.sh'
   ```

   This proves the `bd-bvy4.1` production lifecycle, JSON-RPC/notification,
   10 MiB frame/EOF/recovery, direct-input-port restart, and manifest-version
   contracts after all integration and cleanup moves.

3. **Rust checks.** Reuse the recorded `876 passed, 0 failed, 4 ignored`
   `cargo test --all-targets --locked` functional evidence only if the final
   inventory shows no changes to `Cargo.toml`, `Cargo.lock`, `src/`, `tests/`,
   or `proptest-regressions/` after the evidence commit. Otherwise run:

   ```bash
   nix develop --impure --command cargo test --all-targets --locked
   ```

   If Criterion starts after functional tests, record its status separately;
   do not describe an intentionally stopped benchmark as a test failure.

4. **Single final package build and strict package contract.** Build once only
   after all final source changes:

   ```bash
   nix build .#mcp
   package="$PWD/result/bin/agentic-kb-mcp"
   bash mcp/test/package_smoke_test.sh "$package"
   bash mcp/test/retired_caller_flag_test.sh "$package"
   bash mcp/test/startup_failure_test.sh "$package"
   nix-store -qR "$(readlink -f result)" | rg -i 'open-policy-agent|(^|/)opa($|[-/])'
   ```

   The closure scan must return no matches (treat `rg` status 1 as success).
   `package_smoke_test.sh` is the strict clean-PATH JSON decoder: it requires
   initialize and `tools/list`, exactly the ordered 17 tools, and the Unicode
   em dash in a decoded description. The other two scripts prove retired
   `--caller-id` rejection and invalid `KB_BIN` exits 1 with the startup
   diagnostic rather than hanging.

5. **OPA and active-contract inventory.** Scan final implementation/package
   wiring, separating intentional regression guards and version-scoped history
   from active runtime claims:

   ```bash
   rg -n -i 'open-policy-agent|OPA_BIN|opa eval|OpaEvaluator|RateLimiter|Authorization' \
     flake.nix mcp/lib mcp/mix.exs mcp/config mcp/priv
   rg -n 'caller_id' mcp/lib mcp/test src/commands/mcp.rs tests/mcp_option_b.rs
   ```

   Active code and package/dev wiring must have no OPA runtime contract. Tests
   may retain rejection assertions; historic migration/changelog material must
   be explicitly version-scoped by the documentation gate.

6. **Specification compliance.** Confirm no final change modifies
   `agent-kb/tla/McpBoundary.tla`, `McpPackageStartup.tla`, or
   `PortProtocol.tla`. Cite the existing positive `McpBoundary` TLC result
   (31 states, all six framing scenarios) and the expected unsafe counterexample
   (exit 12, `critical_BufferBounded`). If any of those models or their
   configurations changed, rerun the applicable TLC commands in an isolated
   `/tmp` metadir before claiming coverage.

7. **Reviews and records.** Run the required local code review and applicable
   security review against the final combined diff. Zero Critical findings is
   required; resolve or explicitly track Important findings. Record the final
   changed-file inventory, known risks, all command results, package path, and
   the exact final SHA in the post-implementation checklist.

8. **Documentation build.** After `bd-dhi0.3` and `bd-bvy4.7` content is
   complete, build the runnable documentation target:

   ```bash
   nix build .#doc
   ```

   This flake target produces the mdBook guide and Rust API documentation. It
   is a documentation gate, not a substitute for `.#mcp` package validation.

## Closure and merge boundary

`bd-dhi0.4` may close only after its six listed acceptance criteria are evidenced.
`bd-bvy4.8` may close only after its dependencies, including `.5`, and its five
listed criteria are evidenced. Documentation tasks remain separate. A clean final
gate leaves merge blocked until the user reviews the concrete result and explicitly
authorizes the Phase 4 merge.
