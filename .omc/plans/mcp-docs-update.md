# MCP documentation update plan

Status: preparation only. This plan is blocked by `bd-dhi0.4`, `bd-bvy4.8`,
and the implementation tasks listed by `bd-dhi0.3` and `bd-bvy4.7`.
The no-package-authorization documentation takes effect in 0.3.0; manifests
remain at 0.2 until the separate release commit.

## Files and exact changes

1. `docs/src/security/mcp-authorization.md`
   - Retitle and replace the live OPA/caller-identity content with the MCP
     repository trust-boundary contract: a client able to launch the server
     against a repository has the filesystem/host access needed for every
     advertised tool.
   - State that a connected client invokes all 17 advertised MCP tools,
     including `kb_expire`, `kb_audit_run`, and `kb_audit_record`, using the
     MCP process's OS credentials. Package checks establish neither caller
     identity nor Git permissions; the filesystem/repository boundary is
     selected by the user who launches the process. Keep structural
     validation, database integrity checks, and permanent-entry protection
     distinct from authorization.
   - Add migration and rollback guidance: remove `--caller-id`, `OPA_BIN`,
     and custom Rego setup. Downgrade after new caller-free audit
     candidate/record batch events is not replay-safe on 0.2.0 because its
     Rust database handling requires `caller_id`; require a pre-upgrade
     database plus JSONL snapshot, or use a compatible forward release.
     Upgrade does not rewrite historical events.

2. `docs/src/mcp.md`
   - Remove host-injected `caller_id` rows and OPA/rate-limit claims from the
     port-field table and explanatory text. Record that client-supplied
     `caller_id` is an undeclared argument and is rejected by the closed tool
     schemas.
   - Amend the B1 link with a version-scoped note that its former item 15
     applies to 0.2.0 only; the next release removes that port field and the
     package authorization boundary without changing the remaining closed
     request schemas.
   - Add the verified JSON-RPC contract: parse errors, malformed request
     errors, invalid parameters, unknown methods, and notification silence;
     malformed input must not dispatch a port operation.
   - Add the bounded stdio contract from the final implementation: 10 MiB
     maximum frame, deterministic oversize handling and resynchronization,
     plus empty and unterminated EOF behavior.
   - Add lifecycle guidance: production starts one supervisor tree, one
     `PortManager` when a database is present, and one `McpServer` as the sole
     owner of its direct native fd 0 port; EOF is a clean exit, stdin-port
     errors are nonzero exits, and child startup failures fail application startup. Reference the package
     smoke and process-level tests rather than promising an unsupported
     supervisor API.
   - Document `serverInfo.version` as derived from the application manifest,
     with protocol version independent of package version. Document the final
     module boundaries and that tool registry/schema definitions have one
     source of truth after the refactor lands.
   - Remove the dev-shell/package OPA PATH paragraph.

3. `mcp/README.md`
   - Replace its authorization-boundary section with a short trust-boundary
     summary and link to the retitled security page.
   - State that `--caller-id` is retired and rejected, and do not retain OPA,
     Rego, rate-limit, or launch-principal instructions.

4. `docs/decisions/b1-request-contract.md`
   - First restore the complete 77f451e version of the historical B1 decision
     if the current branch has a destructive rewrite. Then append an
     amendment; do not rewrite accepted history. Scope it to 0.3.0,
     superseding item 15 only, and preserve the deployed-pin snapshot and all
     0.2.0 facts.

5. `docs/src/SUMMARY.md` and `CHANGELOG.md`
   - Rename the security navigation label if the retained file is retitled.
   - Add an `Unreleased` Breaking entry, effective in 0.3.0, for the Option B
     trust-boundary change, retired launch/environment/policy configuration,
     and broadened audit and expiry access. Preserve every 0.2.0 changelog
     entry verbatim; do not bump manifests, README snippets, or create a
     release section before the separate release commit.

## Final checks

- Scan active documentation (`docs/`, `mcp/README.md`, and current release
  guidance) for `OPA`, `Rego`, `OPA_BIN`, `--caller-id`, caller-injected
  authorization, stale `0.1.0`, and unbounded-frame claims. Historical 0.2.0
  changelog and the B1 record are intentional exceptions and must be clearly
  version-scoped.
- Confirm the documented tool count and names from the final `tools/list`
  result, and confirm rejected `caller_id` with the final schema tests.
- Verify mdBook and the documented process/package commands after the final
  integration commit; update prose only when it matches those results.
