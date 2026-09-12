# MCP trust boundary

The agentic-kb MCP package has no package-level authentication or
authorization. Access to the backing repository and JSONL through ordinary
filesystem permissions is sufficient authority to invoke every tool.

The package does not use OPA, Rego policies, launch-time caller identities,
per-tool permission checks, Git-permission checks, or caller-keyed rate
limits. `agentic-kb-mcp --caller-id …` is rejected before the OTP application
starts.

This boundary does not relax data-integrity controls. Tool schemas and Rust
port request structs reject unknown fields, writers retain locking and atomic
updates, and permanent-entry and audit-batch validation remain enforced.

Older SQLite databases and JSONL events can contain `caller_id` values from
earlier releases. They are historical, untrusted attribution only: current
requests do not supply identity, and no current operation compares or grants
authority from those values.
