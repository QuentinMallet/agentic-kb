# MCP repository trust boundary

Effective in **0.3.0**, agentic-kb MCP has no package-level authentication or
per-tool authorization. A connected client can invoke all 17 advertised MCP
tools, including `kb_expire`, `kb_audit_run`, and `kb_audit_record`, using the
MCP process's OS credentials.

The user who launches the process selects the repository and filesystem
boundary. The package does not establish caller identity, inspect Git
permissions, or add an authority boundary through OPA, Rego, caller-keyed
quotas, or launch-time principal metadata. A package check proves neither
identity nor Git permission.

This does not relax data-integrity controls. MCP schemas and Rust port request
structs reject unknown fields; writers retain locking and atomic updates; and
permanent-entry and audit-batch validation remain enforced. Those checks
validate requests and protect stored data; they do not authorize a caller.

## Migration and rollback

For 0.3.0, remove `--caller-id`, `OPA_BIN`, and custom Rego or OPA setup from
MCP launch configuration. `--caller-id` is retired and rejected before server
startup. A client-supplied `caller_id` is likewise an undeclared tool argument
and is rejected by the closed schemas.

Upgrading does not rewrite historical JSONL events or database rows. Existing
`caller_id` values remain inert legacy attribution: live MCP operations neither
supply nor consult them.

Downgrade requires care after 0.3.0 has written caller-free audit
candidate/record batch events. Version 0.2.0 Rust database handling requires a
`caller_id` for those events, so those new events are not replay-safe on a 0.2.0
binary. Before upgrading, retain a database and JSONL snapshot for rollback, or
use a compatible forward release. Do not rewrite historical events to perform
a downgrade.
