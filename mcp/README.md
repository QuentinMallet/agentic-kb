# AgenticKbMcp

## Authorization boundary

The stdio entry point accepts a single host-launch principal:

```text
agentic-kb-mcp --caller-id <host-principal>
```

Mutating audit and expiry tools are denied unless this launch value is present,
passes the bundled default-deny Rego policy, and stays within its per-caller
action quota. MCP `initialize.clientInfo` and tool arguments are never used as
identity. Runtime OPA evaluation uses a short-lived supervised port with both
OPA and host deadlines; a missing binary, timeout, undefined decision, or
evaluation error denies the request.

**TODO: Add description**

## Installation

If [available in Hex](https://hex.pm/docs/publish), the package can be installed
by adding `agentic_kb_mcp` to your list of dependencies in `mix.exs`:

```elixir
def deps do
  [
    {:agentic_kb_mcp, "~> 0.1.0"}
  ]
end
```

Documentation can be generated with [ExDoc](https://github.com/elixir-lang/ex_doc)
and published on [HexDocs](https://hexdocs.pm). Once published, the docs can
be found at <https://hexdocs.pm/agentic_kb_mcp>.

## Internal peer-graph port methods

The Rust line-JSON port implements `kb_peers_add`, `kb_peers_list`, and
`kb_peers_remove` for CLI parity, but `AgenticKbMcp.McpServer.tools/0`
deliberately does not expose them as agent tools. Peer-graph setup is an
operator and lifecycle-hook action, not an agent action. The port's
`audit_run`, `audit_record`, `audit_report`, and `provenance` methods are MCP
tools (`kb_audit_run`, `kb_audit_record`, `kb_audit_report`, and
`kb_provenance`); their declarations live in `McpServer.tools/0`.

`handle_audit_run` and `handle_audit_record` bound audit samples and verdict
batches to `MAX_AUDIT_VERDICTS` (50). Typed `AuditVerdict` rows require a
boolean verdict, `handle_audit_record` requires a non-empty note when it is
false, and `db::expire_guard` preserves permanent entries. Any one invalid
verdict rejects the whole `kb_audit_record` batch before any write — the
other, valid verdicts in that call are not applied either. Caller identity,
rate limits, and OPA policy enforcement remain deferred follow-ups in beads
`bd-1orr`.
