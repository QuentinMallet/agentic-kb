# agentic-kb agent guidelines

## Peer graph CLI (`kb peers`)

The `kb peers` subcommand tree manages cross-repo peer relationships stored in the
`peers` and `graphs` SQLite tables.

### Subcommands

```
kb peers add <target-repo-path> [--type epic|dep] [--epic-slug <slug>] [--ttl-days <n>]
    Register a peer repo. Prints the new peer UUID. Defaults: type=epic, no TTL.

kb peers list [--type epic|dep]
    List all peers as JSON array. Each entry: id, source_repo, target_repo,
    graph_type, epic_slug, ttl_expires_at, created_at.

kb peers remove <uuid>
    Delete a peer by UUID (idempotent, exit 0 even if missing).

kb peers show <repo-path>
    Show all peer edges where source_repo OR target_repo matches <repo-path>.

kb peers import <seeds-file.json>
    Bulk-import from JSON array: [{source_repo, target_repo, graph_type, epic_slug?, ttl_days?}]
    Idempotent (stamp-gated by sha256 of file content). Prints count of rows inserted, or "0" on stamp hit.

kb peers edge add <source-repo> <target-repo> --type <epic|dep> [--epic-slug <slug>] [--ttl-days <n>]
    Add a directed peer edge between two repo paths. Returns the new peer UUID.

kb peers edge list [--epic-slug <slug>]
    List all peer edges as JSON array, optionally filtered by epic slug.

kb peers edge remove <peer-uuid>
    Delete a peer edge by UUID (idempotent, exit 0 even if missing).

kb peers edge cleanup-epic <slug>
    Remove ALL peers and edges where epic_slug = <slug> across all graphs.
    Used at epic-close time to clean up transient epic-type peer entries.
```

### `graph_type` semantics

| graph_type | Purpose | TTL | Cleanup |
|------------|---------|-----|---------|
| `epic` | Transient cross-repo epic coordination | Auto-expires (default 30d, or `--ttl-days`) | `kb peers edge cleanup-epic <slug>` at epic-close |
| `dep` | Persistent nix input / build-time dependency | No TTL | Survives `cleanup-epic`; swept only by `kb peers remove` |

`dep`-type peers are automatically included in federated `kb search --peers` traversal.
`epic`-type peers are included only while active (non-expired).

### TTL sweep

`kb` runs a background TTL sweep at startup: any `epic`-type peer with `ttl_expires_at <
now` is automatically deleted. The sweep is idempotent and logged to stderr at debug level.

### Peer graph lifecycle

**Phase 1 (epic-register):** call `agentic-init multi-repo epic-register <slug> <foreign-path> <epic-id>`
in the orchestrating repo's worktree. This calls `kb peers add <foreign-path> --type epic --epic-slug <slug>`
and writes `.omc/cross-repo/<slug>.json`.

**Phase 4 (epic-close):** call `agentic-init multi-repo epic-close <slug>`. This calls
`kb peers edge cleanup-epic <slug>` to purge all transient epic-type peer edges, then
removes the `.omc/cross-repo/<slug>.json` state file.

**dep seeds:** `agentic-init dep-seed-generate <slug> [<local-path>]` writes
`.omc/peer-seeds/<slug>.json`. `agentic-init seeds` imports all files in that directory via
`kb peers import` (stamp-gated). This establishes persistent `dep`-type edges for nix
input repos that should be included in federated KB searches.

## Federated search

```
kb search <query> --peers [--max-hops <n>] [--local-only] [--slug <slug>]
```

With `--peers`, the search fan-out across dep-type peer repos (up to `dep_depth` hops,
default 1). Results are annotated with `origin_repo`. Use `--local-only` to restrict to
the current repo only.

## MCP tools

| Tool | Purpose |
|------|---------|
| `kb_peers_add` | Same as `kb peers add` |
| `kb_peers_list` | Same as `kb peers list` |
| `kb_peers_remove` | Same as `kb peers remove` |

Search tools accept `peers`, `reachable_from`, `max_hops`, `slug` fields in addition to
standard search fields.
