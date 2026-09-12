# B1 MCP request contract

The MCP bridge sends closed, anonymous requests to the Rust port. Repository
and JSONL filesystem access is sufficient authority for every tool; the
package has no caller identity or per-tool authorization layer.

`expire` requires `entry_id`; `audit_run` accepts `sample_size` and `mode`;
and `audit_record` requires `run_id` with bounded, validated verdict rows.
All current request structs reject unknown fields, including `caller_id`.

Existing JSONL and SQLite rows may retain historical attribution fields. They
are read only for compatibility and never influence a live operation,
idempotency comparison, or permission decision.
