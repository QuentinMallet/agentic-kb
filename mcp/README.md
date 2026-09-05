# AgenticKbMcp

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
`kb_peers_remove` for CLI parity, but the Elixir MCP server deliberately does
not expose them as agent tools. Peer-graph setup is an operator and lifecycle-hook
action, not an agent action. Revisit this boundary only when a concrete agent
workflow needs to edit the peer graph directly.

Controls from the threat model
(`KB security/threat-models/mcp-audit-surface`) bound audit verdict batches,
require notes for destructive verdicts, and preserve permanent entries. Caller
identity, rate limits, and OPA policy enforcement remain deferred follow-ups in
beads `bd-1orr`.
