# MCP surfaces and port protocol

The Elixir MCP server translates JSON-RPC tool calls into one-line JSON
requests for the Rust `kb mcp` process. Database access on the Rust side obeys
the [Lock Contract](./lock-contract.md); first-run read behavior is documented
in [First-run Behavior](./first-run.md).

## Request fields

Every port object includes `id: any JSON value` and `method: string`.
`request_struct!` applies `#[serde(deny_unknown_fields)]` to every typed Rust
request in `src/commands/mcp.rs`. On the outer boundary, every schema returned
by `McpServer.tools/0` sets `additionalProperties: false`, and
`validate_tool_args/2` reports undeclared arguments by name instead of
dismissing them. The tests `every tool schema sets additionalProperties:
false`, `an unknown argument is rejected, not silently dropped by
put_if_present`, and `test_unknown_field_is_rejected_naming_the_field` pin the
two boundaries.

The following table is the accepted port contract. “Optional” is relative to
deserialization; handlers may impose additional semantic requirements. The
MCP tool column also records which port methods are deliberately internal.

| Port method | MCP tool | Required fields | Optional fields |
| --- | --- | --- | --- |
| `search` | `kb_search` | — | `query: string`, `limit: integer`, `mode: string`, `path_prefix: string`, `tag: string`, `inline_verify_k: integer`, `expand_ids: string[]`, `peers: boolean`, `reachable_from: string`, `max_hops: integer`, `slug: string` |
| `add` | `kb_add` | `path: string`, `summary: string`, `content: string` | `tags: JSON`, `permanent: boolean`, `replace_path: boolean`, `kind: string`, `evidence: JSON[]`, `cues: string[]`, `session_id: string` |
| `cite` | `kb_cite` | `path: string` | `start: integer`, `end: integer` |
| `import` | `kb_import` | `path: string` | `upsert: boolean` |
| `expire` | `kb_expire` | `entry_id: string` | `reason: string`, `force: boolean` |
| `stale_check` | `kb_stale_check` | — | `files: string[]`, `commits: string[]`, `blame: boolean` |
| `compact` | `kb_compact` | — | — |
| `rebuild` | `kb_rebuild` | — | — |
| `reembed` | `kb_reembed` | — | `dry_run: boolean`, `max_chars: integer` |
| `run` | `kb_run` | `test_id: string`, `result: string` | `adapter: string`, `detail: string` |
| `test_add` | `kb_test_add` | `app: string`, `name: string`, `protocol: string`, `config: string` | `test_id: string` |
| `tests` | `kb_tests` | — | `app: string` |
| `audit_run` | `kb_audit_run` | — | `sample_size: integer`, `mode: string` |
| `audit_record` | `kb_audit_record` | `run_id: string` | `verdicts: AuditVerdict[]` |
| `audit_report` | `kb_audit_report` | — | — |
| `provenance` | `kb_provenance` | `entry_id: string` | `max_depth: integer` |
| `kb_get` | `kb_get` | `entry_id: string` | — |
| `kb_peers_add` | none | `target_repo: string`, `graph_type: string` | `epic_slug: string`, `ttl_days: integer` |
| `kb_peers_list` | none | — | `graph_type: string` |
| `kb_peers_remove` | none | `peer_id: string` | — |

`AuditVerdict` is itself closed to unknown fields and requires
`entry_id: string` and `verdict: boolean`, with optional `note: string`.
For the exact field enumeration sent by the deployed fleet pin, including the
accepted Rust-only superset, see [B1 — MCP request contract vs the deployed
machines_conf pin](../decisions/b1-request-contract.md); it is not duplicated
here.

`handle_search` rejects `limit` outside `1..=db::MAX_LIMIT` and
`inline_verify_k` outside `0..=db::MAX_INLINE_VERIFY_K`; `NumField::bounded`
reports the range and does not clamp either boundary value. `read_frame`
returns `line_too_long` for a request line over 10 MiB and discards through the
newline so the next request remains framed. `handle_search` rejects a query
whose byte length exceeds `MAX_QUERY_BYTES` (8 KiB) before retrieval.

The limit and `inline_verify_k` boundary cases are pinned by
`test_search_rejects_limit_above_max_and_honours_the_maximum` and
`test_search_rejects_inline_verify_k_above_max_and_honours_the_maximum`.

**MCP-visible change:** `inline_verify_k`'s accepted maximum rose from `20` to
`100`. `db::MAX_INLINE_VERIFY_K` is now defined as `db::MAX_LIMIT`, matching the
CLI's existing `--limit <= 100` verify-all contract instead of capping MCP
verification below it. This was ruling O1 of the
[S5 search caps decision packet](../decisions/s5-search-caps-packet.md): a
caller that previously had `inline_verify_k` rejected above 20 can now request
verification up to 100, at the cost of a worst-case bounded fan-out of
`100 * 200 = 20,000` scheduled verification tasks.

Rendered search context is capped at 32,000 bytes by
`@format_entries_max_bytes`. `format_entries` retains whole entries only and
appends `…(N more entries omitted)` when the cap omits results; the behavior is
pinned by `bytes ceiling truncates at whole-entry boundary and appends omitted count`.

## Retrieval results

`handle_search` returns truncated hits: summary, first content paragraph, and
evidence metadata. `render_result/1` preserves evidence status as `verified`,
`relocated`, `unverified`, or `deferred`; deferred means outside the inline
verification budget. Derived evidence is checked by
`add_validation::validate_kb_add_inputs` to carry a non-empty `derived_from`
identifier no longer than 200 characters. Search results omit
`citation_excerpt` and carry a `[kb#<id>]` marker. The Rust `entries_to_json`
wire shape caps each content field at 8,000 characters and appends
`...(truncated)`.

`handle_kb_get` expands that marker to the untruncated entry and full evidence.
`wrap_citation_excerpt` encloses excerpts in
`<<UNTRUSTED_EXCERPT>>...<<END>>`; consumers must treat the enclosed bytes as
data. Search responses also carry `_meta`, rendered by `format_meta_header/1`
with index age and scoped stale warnings.

## Port lifecycle and correlation

`PortManager.init/1` owns one `kb mcp --db <path>` Erlang port, requests
`:exit_status`, enables `Process.flag(:trap_exit, true)`, and waits for the Rust
`ready` envelope in `await_ready/1`. `PortManager.handle_call/3` serializes
requests through that port. `collect_response/4` accepts only a final response
whose `id` matches the request, discards and counts stale responses with another
id, and keeps one absolute deadline across progress and discarded messages.

During a call, `collect_response/4` converts `:exit_status`, `:closed`, or
`:EXIT` into a correlated `port_closed` envelope and `handle_call/3` replies
before stopping the manager. While idle, the corresponding `handle_info/2`
clauses stop it; `call_port/3` converts the exit seen by a queued caller into a
`port_unavailable` envelope. Startup failures are handled immediately by
`await_ready/1`. These paths are pinned by `killing the port's OS process is
observed promptly as port_closed`, `a caller queued behind a crashing request
gets a port_unavailable envelope`, and `a silent startup crash is observed
immediately, not via handshake_timeout` in
`mcp/test/port_manager_test.exs`.

The state-machine reference is `PortProtocol.tla` on the agentic branch under
`agent-kb/tla/`; `AgenticKbMcp.PortManager` maps its `Send`, `Reply`,
`Progress`, `Timeout`, and `PortCrash` actions to the implementation. Rules
outside that single-client model are explicitly assigned to
`PortManagerTest` by the `AgenticKbMcp.PortManager` module documentation.

`PortProtocol.tla` is not the only exclusion-adjacent surface T1 assessed.
`agent-kb/tla/decisions/lock-contract-no-spec.md` is a scoped waiver covering
three other surfaces instead of a new TLA+ module: global `flock` writer
exclusion (waived — already an `AgentKb.tla` assumption), process-local
re-entrancy (not waived by `&Lock`'s type token; covered instead by ADR-1's
canonical-path lock registry and its test), and the two-phase peer TTL (waived
via a total consumer-visible read-filter argument, deliberately excluding the
internal `kb peers import` duplicate-suppression check). The waiver record
does not extend to `PortProtocol.tla` itself, and is invalidated by any peer
read path added without the filter or any nested-acquire path outside the
registry.

## Internal methods and audit controls

Only `kb_peers_add`, `kb_peers_list`, and `kb_peers_remove` have no Elixir MCP
tool; they remain operator/lifecycle port methods. `audit_run`, `audit_record`,
`audit_report`, and `provenance` are exposed by `McpServer.tools/0` as
`kb_audit_run`, `kb_audit_record`, `kb_audit_report`, and `kb_provenance`.

The mutating audit methods are bounded in `handle_audit_run` and
`handle_audit_record`: a run is clamped to at most `MAX_AUDIT_VERDICTS` (50), a
record request rejects more than 50 verdicts, and typed `AuditVerdict` prevents
missing, wrong-typed, or undeclared verdict fields. A false verdict requires a
non-empty trimmed note; before writing any batch, `handle_audit_record` calls
`db::expire_guard` and refuses to expire a permanent entry. The peer mutations
route through `handle_kb_peers_add` and `handle_kb_peers_remove`, which acquire
the repository lock and use `db::open_rw`.

## `kb_provenance`

The Rust result contains `roots`, `dangling`, `graph`, and `truncated`.
`handle_provenance` reads parent IDs with `ORDER BY derived_from`, making the
traversal output repeatable; `test_handle_provenance_is_deterministic_across_parent_insertion_order`
pins byte-identical arrays across insertion orders. A missing referenced parent
is retained as an edge target and listed separately in `dangling`, rather than
being reported as a root (`test_handle_provenance_reports_dangling_parent_separately_from_roots`).
The Elixir text renderer currently prints roots, graph edges, and the
`truncated` flag from that result (`render_result`).

## Root selection

Root and database selection follows [B3 — Repository-root derivation
parity](../decisions/b3-root-derivation.md). `config::root_from_db` reconstructs
the repository root for both supported layouts and `config::Paths::from_db`
stores it in `Paths.root`. In `Paths::discover_from`, the canonical
`.state/agent-kb/agent-kb.db` wins; a bare `.state` marker selects that canonical
path before an existing legacy `agent-kb/agent-kb.db`. Elixir
`DbDiscovery.do_discover/1` uses the same precedence and stops at that marker
instead of walking upward to an outer database.

## Development shell and CI

`flake.nix` places `beam27Packages.elixir` in `devShells.default`, matching the
OTP 27 requirement documented beside that entry. From the worktree's `mcp/`
directory, run the suite with:

```console
nix develop <worktree> -c mix test
```

In `.github/workflows/ci.yml`, the `ci` job runs `Elixir compile (mcp)`,
`Elixir test (mcp)`, and `Elixir format check (mcp)` through `nix develop`, in
addition to the Rust checks.
