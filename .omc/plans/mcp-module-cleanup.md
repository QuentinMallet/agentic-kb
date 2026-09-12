# MCP module cleanup plan

Status: draft for independent review. Scope: `bd-bvy4.5` only; no behavior change, dependency, authorization layer, or transport rewrite.

## Behavior to lock before moves

- Keep the ordered 17-tool `tools/list` registry and every exact schema. Before moves, add a literal expected 17-tool method/field fixture driven through public `tools/call` and a capturing fake port. Assert absent optional fields stay absent and expire/audit requests contain no `caller_id`; do not derive expected maps from implementation constants.
- Keep closed argument validation, including unknown `caller_id` rejection, audit-verdict validation, no-DB precedence, unknown-tool errors, and Option B anonymous access. Reuse `agentic_kb_mcp_test.exs` and `option_b_access_test.exs`.
- Keep every `render_result/1` envelope and exact text behavior. Existing rendering fixtures lock entries, metadata, evidence, excerpts, audit output, and errors; add only missing request-map fixture coverage.
- Keep JSON-RPC classification and bounded framing in `JsonRpc`, `Transport`, and `Transport.Stdio` from `bd-bvy4-protocol`. Do not create another reader/framer module.
- Keep application child ownership and injectable child-spec seam from `bd-bvy4-lifecycle`. `McpServer` remains the GenServer lifecycle coordinator and the sole owner of its direct native fd 0 port.

## Smells and smallest seams

1. **Registry and validation are embedded in a 1,385-line GenServer.** Extract `AgenticKbMcp.ToolRegistry` from `mcp_server.ex`: ordered `tools/0`, schema-derived allowed fields, and audit verdict validation. Export only `tools/0` and `validate_args/2`; preserve the current list order/data verbatim.
2. **Port request construction is a long sequence of near-identical clauses.** Extract `AgenticKbMcp.PortRequest`: pure `build/2` returns `{:port, request}`, `:rebuild`, or `{:error, :unknown_tool}`. It owns only literal method maps and optional-field omission. `McpServer` retains no-DB precedence, `PortManager` calls, rebuild side effects/static text, errors, and Renderer invocation. Do not introduce `dispatch/3`, a second coordinator, generic factory, or action registry.
3. **Rendering is pure but co-located with stdio lifecycle.** Extract `AgenticKbMcp.Renderer`: `render_result/1`, `format_entries/2`, and private entry/evidence/excerpt helpers unchanged. Characterization tests move only after green snapshots prove byte-for-byte output.
4. **Coordinator responsibility is obscured by the above.** Leave `McpServer` with init/direct-port wiring, decoded request routing, `tools/call` orchestration, JSON-RPC response emission, and ID generation. It is the sole input owner; it delegates registry, dispatch, and rendering, and does not own policy or caller identity.

## Execution order

1. Add the registry/order, tool-request-map, and renderer snapshots; run focused MCP tests.
2. Extract ToolRegistry; run Elixir suite.
3. Extract Renderer; run rendering/fixture tests then Elixir suite.
4. Extract PortRequest; run the public-tools/call capturing-port fixture, Option B, Rust cross-language MCP tests, and package smoke.
5. Review the final thin coordinator for dead helpers only; delete rather than add wrappers.

## Boundaries and verification

No move may alter tool names/order, schemas, method names, port field omission, rendered bytes, notification handling, frame limits, lifecycle restart behavior, or startup errors. Each move is one commit and gets an independent cleanup review. Final gates: Elixir format/tests, focused Rust MCP tests, package smoke, and the existing protocol/lifecycle suites after their branches are integrated.
