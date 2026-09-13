# AgenticKbMcp

## Repository trust boundary

Effective in 0.3.0, a connected client invokes all 17 advertised MCP tools
with the server process's OS credentials. The user launching the process
chooses the repository and filesystem boundary. The package does not establish
caller identity, inspect Git permissions, authorize individual tools, use OPA
or Rego, or apply caller-keyed quotas. `--caller-id` is retired and rejected
before server startup.

Closed tool schemas, Rust structural validation, locking, bounded inputs,
permanent-entry guards, and atomic audit updates remain in force. They protect
request and data integrity; they are not caller authorization. Historical
`caller_id` data remains inert legacy attribution. See the [MCP repository
trust boundary](../docs/src/security/mcp-authorization.md) for migration and
rollback guidance.

## Installation

If [available in Hex](https://hex.pm/docs/publish), the package can be installed
by adding `agentic_kb_mcp` to your list of dependencies in `mix.exs`:

```elixir
def deps do
  [
    {:agentic_kb_mcp, "~> 0.2.0"}
  ]
end
```

Documentation can be generated with [ExDoc](https://github.com/elixir-lang/ex_doc)
and published on [HexDocs](https://hexdocs.pm). Once published, the docs can
be found at <https://hexdocs.pm/agentic_kb_mcp>.

## Supervised rebuilds

`kb_rebuild` acknowledges an accepted or already-READY rebuild; it does not
report replay completion. The OTP application owns the direct Rust worker,
which acquires the selected store's lifetime lock before READY. Busy recovery
candidates return an error, cancellation waits for terminal exit, and
inherited-stdin EOF stops the worker. The internal supervised launch mode is
not a public MCP or CLI API. See [the MCP lifecycle
contract](../docs/src/mcp.md#supervised-rebuild-lifecycle).

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
other, valid verdicts in that call are not applied either.
