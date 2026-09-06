# MCP Authorization

The Elixir MCP server (`AgenticKbMcp`) gates its mutating audit and expiry
tools behind a launch-time caller identity, an OPA policy decision, and a
per-caller rate limit. This page documents that boundary end to end: what a
deployer must configure, what a calling agent will see when denied, and what
happens when a dependency is missing.

## Launch requirement: `--caller-id`

The escript entry point (`AgenticKbMcp.CLI.main/1`) reads a single
process-launch argument:

```text
agentic-kb-mcp --caller-id <host-principal>
```

This value is read once before the OTP supervision tree starts and stored as
`Application.put_env(:agentic_kb_mcp, :launch_caller_id, ...)`. It is the
only source of caller identity: MCP `initialize.clientInfo` and tool
arguments are never consulted, and a caller cannot influence `input.caller`
by passing a `caller_id`-shaped argument (`AgenticKbMcp.Authorization`
explicitly drops `caller_id` and `clientInfo` from the context map before
building the policy input).

If `--caller-id` is omitted, `AgenticKbMcp.Authorization`'s caller id is
`nil`, and every authorization check fails immediately with
`unknown_caller` (see below) — **`kb_expire`, `kb_audit_run`, and
`kb_audit_record` fail permanently** for that process, with no retry path
short of restarting with the flag set.

## The OPA policy

Authorization decisions are evaluated by a bundled Rego policy at
`mcp/priv/policies/agentic_kb.rego`:

```rego
package authz

default allow := false

trusted_callers := {"agentic-kb-host"}

mutating_scopes := {
  "kb.audit.run", "kb.audit.traffic", "kb.audit.record",
  "kb.audit.expire", "kb.entry.expire", "kb.entry.expire.force",
}

allow if {
  input.caller in trusted_callers
  input.action in mutating_scopes
}
```

`default allow := false` makes any caller not listed in `trusted_callers`
fail-closed. The bundled policy ships with **one principal already
trusted**: `agentic-kb-host`. This is not "authorizes no host" — a
deployment that launches with `--caller-id agentic-kb-host` and never edits
the policy is authorized out of the box. Operators deploying under a
different launch identity must replace `trusted_callers` in
`agentic_kb.rego` with their own principal, or every mutating audit/expiry
call will be denied.

`AgenticKbMcp.OpaEvaluator.evaluate/2` shells out to the `opa` binary
(`opa eval --strict-builtin-errors --fail --format=json --data <policy_dir>
--input <input_file> --timeout <ms> data.authz.allow`) with a bounded
timeout, and `AgenticKbMcp.Authorization` wraps that call in its own
supervised task with a second, host-side deadline. `input.caller` is always
the launch-time principal, never client-supplied data, and `input.action`
is one of the mutating-scope strings below.

### The `opa` binary dependency

`OpaEvaluator.evaluate/2` resolves the binary from `OPA_BIN`, then
`System.find_executable("opa")`. If the resolved path is missing or not a
regular file, or the bundled policy directory is missing, evaluation
short-circuits to `{:error, :missing}` without invoking the binary — this
denies the request (surfaced as `policy_unavailable`, see below) rather than
allowing it. `flake.nix` wraps the escript with `open-policy-agent` on
`PATH` and adds it to `devShells.default`, so a `nix develop` or a Nix-built
deployment has `opa` available without further configuration; a manually
assembled deployment must ensure `opa` is on `PATH` or set `OPA_BIN`.

## Rate limits

`AgenticKbMcp.RateLimiter` enforces a fixed per-action quota per caller,
independent of the OPA decision:

| Action | Limit | Window |
|---|---|---|
| `kb.audit.run` | 20 | 60 s |
| `kb.audit.traffic` | 5 | 60 s |
| `kb.audit.record` | 20 | 60 s |
| `kb.audit.expire` | 10 | 60 s |
| `kb.entry.expire` | 10 | 60 s |
| `kb.entry.expire.force` | 5 | 60 s |

Action selection (`mcp_server.ex`'s `authz_actions/2`) is not always
one action per tool call:

- `kb_audit_run` with `mode: "traffic"` consumes both `kb.audit.run` and
  `kb.audit.traffic`; any other `kb_audit_run` call consumes only
  `kb.audit.run`.
- `kb_audit_record` consumes `kb.audit.record`, and additionally
  `kb.audit.expire` if the batch contains any verdict with `verdict: false`
  (a false verdict expires an entry).
- `kb_expire` consumes `kb.entry.expire.force` when `force: true` is set,
  otherwise `kb.entry.expire`.

Every action named for a given call must both pass the OPA decision and
have quota remaining; the rate limit is checked only after the policy
allows the call.

## Denial reasons

Every denial reaches the caller as a tool error of the form:

```
authorization denied: <reason>
```

| Reason | Cause |
|---|---|
| `unknown_caller` | No `--caller-id` was supplied at launch (`Authorization`'s caller id is `nil`). |
| `policy_denied` | OPA evaluated `data.authz.allow` to `false`, or the rate-limiter returned an error other than `:rate_limited` after an `allow`. |
| `rate_limited` | The caller has exhausted its quota for that action within the current window. |
| `policy_unavailable` | OPA evaluation did not complete with a boolean decision in time — a timeout, a missing/non-regular `opa` binary or policy directory, a malformed decision, or a runtime error. |
| `missing_authorizer` | The MCP server has no `Authorization` process configured at all (a startup/wiring defect, not a normal deployment state) and the tool requires one. |

`policy_denied` versus `policy_unavailable` distinguishes "the policy says
no" from "the policy could not be evaluated" — both fail closed, but only
`policy_unavailable` indicates a broken deployment (missing `opa`, wrong
`OPA_BIN`, or a timeout) rather than a correctly enforced denial.

## See also

- [MCP Surfaces](../mcp.md) — the port contract, including the host-injected
  `caller_id` field on `expire`, `audit_run`, and `audit_record`.
- `mcp/README.md` — a short summary of the same boundary from the Elixir
  package's own README.
