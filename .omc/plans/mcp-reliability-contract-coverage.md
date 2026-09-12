# MCP reliability contract and coverage map

Status: specification complete; pending independent review for `bd-bvy4.6` closure.

## Boundaries

`McpBoundary.tla` models only the stateful byte framer: accumulation, the 10 MiB abstraction, oversize discard through newline, realignment to a following valid frame, and bounded partial-frame EOF. `PortProtocol.tla` owns Rust-port correlation, deadlines, crash observation, and restart behavior. JSON-RPC classification, lifecycle wiring, tool presence, OPA removal, and version derivation are concrete process/test contracts rather than tautological model constants.

The model nondeterministically selects a shared byte-step machine input from six finite scenarios: empty EOF, exact-limit newline, exact-limit EOF, ordinary multiple frames, oversize-newline-valid recovery, and oversize EOF. `MaxFrame=2` abstracts exactly 10,485,760 bytes (10 MiB), which concrete tests bind. It tracks rejected-frame dispatches explicitly and requires zero. Oversize recovery discards through newline before accepting the aligned frame; bounded partial EOF dispatches once; oversize EOF never becomes a partial-frame dispatch.

## Contracts and test-first coverage

| Contract | Preconditions | Success/failure behavior | Compatibility constraint | First failing test / evidence path |
| --- | --- | --- | --- | --- |
| JSON-RPC classifier | Raw/decoded input | Invalid JSON: `-32700`, `id:null`. A structurally invalid object, including one without `id`, is not a notification: `-32600`, `id:null`. A valid request requires `jsonrpc:"2.0"`, string method, object params when present, and MCP string/integer id. ID-bearing invalid method params: `-32602`; unknown method: `-32601`; neither dispatches | Response id echoes valid request id; result/error remain exclusive | New `mcp/test/json_rpc_validation_test.exs`; property asserts exact response count, code/id, and dispatch count for each class |
| Notification semantics | Structurally valid request object with `jsonrpc:"2.0"`, string method, optional object params, and no id | Declared `initialized`/`notifications/*`: response count 0. A structurally valid no-ID `tools/call` with valid params is an unsupported MCP notification: emit zero responses and dispatch zero backend operations. Valid no-id `tools/call` with invalid method params likewise has response count 0 and dispatch count 0. Invalid no-id objects: one `-32600` response with `id:null`, dispatch count 0, because they are not valid notifications | Preserve the current MCP request-only `tools/call` contract and declared notification taxonomy; MCP request ids are string/integer and never null | Property matrix plus regression sending `{"jsonrpc":"2.0","method":"tools/call","params":{"name":"kb_search","arguments":{"query":"x"}}}` and asserting `response_count=0`, `port_dispatch_count=0`; retain invalid-params no-id fixture |
| Bounded stdio framing | Arbitrary byte stream | Frames through 10 MiB accepted; byte 10 MiB+1 yields one bounded error and discard-through-newline; following frame remains aligned | One-line JSON transport retained; Rust port already uses the same 10 MiB value | New `mcp/test/stdio_frame_test.exs`; boundary sizes, chunk splits, oversized terminated/unterminated frame, recovery frame, EOF |
| Production supervision | Production child specs constructed with/without DB; reader/server/port may fail | EOF causes one clean exit; stdin error exits visibly nonzero; child startup failure fails application startup. One McpServer owns one linked reader. Abnormal reader/server termination restarts that child pair once under its declared supervisor policy; PortManager crash/restart remains owned by `PortProtocol.tla` and must not create a second reader/server | Coordinate with `bd-pia3.1`; do not alter its startup closure model | New `mcp/test/application_lifecycle_test.exs` with injected IO/child specs and existing fake ports; assert child counts, restart count, exit status; package smoke remains `bd-pia3` |
| Version metadata | Successful initialize | `serverInfo.version` equals `Application.spec(:agentic_kb_mcp, :vsn)` normalized to string | Protocol version remains independent; release value has one manifest source | Fixed regression in `json_rpc_validation_test.exs`; compare initialize response to Mix app version |
| Behavior-preserving split | Tests green before moves | Transport, registry/schema, dispatch, and rendering extractions change no schemas, tool names/order, requests, or rendered results | No dependency and no public tool removal | Existing schema/render/dispatch suite is characterization lock; add module-boundary tests only where behavior lacks coverage |
| OPA removal Option B | Backing repository/JSONL access already granted by OS/filesystem | Every tool remains callable; remove OPA, injected Elixir-to-Rust `caller_id`, package auth, per-tool gates, `--caller-id`, and caller-keyed limiter | Structural validation and data integrity remain. Client `caller_id` is rejected for every tool; stored historical attribution is data only and no caller identity field remains on the live wire | Revise authorization tests into all-tool availability/validation tests; exact Rust request-shape tests; package closure scan; retired flag rejection |

## Proposed module seam

- `AgenticKbMcp.Transport.Stdio`: bounded framing, EOF, output only.
- `AgenticKbMcp.JsonRpc`: decoded request classification and JSON-RPC errors.
- `AgenticKbMcp.ToolRegistry`: tool schemas and closed argument-field registry.
- `AgenticKbMcp.ToolDispatch`: request mapping to `PortManager`; no permission layer.
- `AgenticKbMcp.Renderer`: current result and entry formatting.
- `AgenticKbMcp.McpServer`: thin lifecycle coordinator.

Before moves, snapshot fixtures pin exact ordered tool schemas/tool order, every tool-to-port method and field map, and representative renderer outputs. Re-run the identical snapshots after each extraction. Extract in the listed order only after missing-behavior tests fail then pass; the split itself changes no behavior.

## Verification map

- Unit: request classifier properties, notification silence, frame state transitions, exact boundary constants, manifest version, registry closure, all tools retained, `caller_id` unknown-field rejection.
- Integration: real GenServer boundary with fake Rust port; malformed requests never reach the port; oversize frame followed by valid request recovers; production child specs cover DB/no-DB and startup failure.
- End to end: `bd-pia3` clean-PATH package smoke sends initialize/tools-list; OPA work adds absence scans and verifies retired `--caller-id` rejection without changing startup closure ownership.
- Observability: stderr reports startup/frame failures without writing protocol noise to stdout; oversized payload bytes are not logged; no OPA lookup or authorization-denial telemetry remains.

## Implementation acceptance

1. All `critical_` invariants in `McpBoundary.tla` map to named tests above.
2. 10 MiB is one shared Elixir constant and concrete boundary tests cover limit, limit+1, recovery, and EOF.
3. Invalid requests/params never dispatch; request responses and notification silence follow the table.
4. Production supervision is testable without compiling the application tree out of the test environment.
5. Initialize version derives from package metadata.
6. The split preserves every tool/schema/request/rendering contract and adds no dependency.
7. Option B removes package authorization while preserving every tool and all structural/data-integrity protections.
8. `PortProtocol.tla` remains unchanged and its existing TLC configuration stays green.

TLC evidence:

- `tlc -workers auto McpBoundary.tla` — pass, 31 states generated/distinct, depth 8, both temporal properties checked.
- `tlc -workers auto -config McpBoundary_Unsafe.cfg McpBoundary.tla` — expected exit 12 counterexample: the unsafe oversize path reaches `maxBuffered=3` and violates `critical_BufferBounded` with `MaxFrame=2`.

## Protocol sources

- JSON-RPC 2.0 defines request shape, notification silence, parameter structure, response IDs, and `-32700/-32600/-32601/-32602`: https://www.jsonrpc.org/specification
- MCP 2024-11-05 requires JSON-RPC 2.0, string/integer non-null request IDs, and no ID on notifications: https://modelcontextprotocol.io/specification/2024-11-05/basic/messages

KB persistence note: no `kb_*` MCP tool is available in this session; the prohibited `kb` CLI was not used.
