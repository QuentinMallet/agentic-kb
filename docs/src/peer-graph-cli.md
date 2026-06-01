# Peer Graph (kb peers)

Peer graphs associate KB entries across multiple repositories. Two graph types support different lifecycle models:

- **epic graph type**: transient, auto-TTL when epic closes. Used for one-off cross-repo context during active feature work.
- **dep graph type**: persistent, auto-included in search via configurable depth traversal. Used for stable cross-repo relationships (e.g., lib A → app B dependency, spec A → implementation B).

## CLI reference

```
kb peers add <target-repo-path> --type epic|dep [--epic-slug <slug>] [--ttl-days <n>]
kb peers list [--type epic|dep]
kb peers remove <peer-id>
kb peers show <repo-path>
kb peers edge add <source> <target> --type <edge-type> [--epic-slug <slug>] [--ttl-days <n>]
kb peers edge list [--epic-slug <slug>]
kb peers edge remove <edge-id>
kb peers edge cleanup-epic <slug>
kb peers import <seeds-file.json>
```

### Commands

- `add <target-repo-path>`: register new peer repo (must exist on disk)
- `list`: show all registered peers, optionally filtered by graph type
- `remove <peer-id>`: unregister peer (breaks all edges referencing it)
- `show <repo-path>`: display peer info and metadata (paths, TTLs, edge counts)
- `edge add`: create directed edge between two peers (source → target)
- `edge list`: enumerate all edges, optionally filtered by epic
- `edge remove`: delete single edge
- `edge cleanup-epic`: bulk-remove all edges tagged with an epic slug (called on epic close)
- `import <seeds-file.json>`: load peer graph from seed file (idempotent, SHA-256 stamp-gated)

## TTL and cleanup lifecycle

TTL sweep runs automatically on every `kb` invocation (when database opens). Expired peers and edges are removed from the graph.

- Epic edges: default 30 days; override with `--ttl-days N`
- Dep edges: no TTL by default (persistent); optional TTL via `--ttl-days N` for temporary dependencies
- Cleanup on epic close: invoke `kb peers edge cleanup-epic <slug>` when closing an epic; this removes all edge rows tagged with that slug
- Expired graph rows: cleaned up lazily when orphaned (all peers gone, all edges gone)

## Seed import workflow

Bulk import from seed JSON file. File format: array of peer objects.

```json
[
  {
    "source_repo": "/path/to/machines_conf",
    "target_repo": "/path/to/agentic-kb",
    "graph_type": "dep",
    "epic_slug": null,
    "ttl_days": null
  },
  {
    "source_repo": "/path/to/machines_conf",
    "target_repo": "/path/to/some-app",
    "graph_type": "epic",
    "epic_slug": "multi-repo-refactor",
    "ttl_days": 60
  }
]
```

Import is **idempotent**: per-file SHA-256 stamp prevents re-importing the same seed twice. Per-row deduplication prevents duplicate edges.

Usage: `kb peers import peer-seeds/<slug>.json` from machines_conf via `agentic-init dep-seed-generate`.

## dep_depth config

`kb_config.dep_depth` (default: 1) controls transitive edge inclusion in `kb search` results.

- **depth=1**: auto-include KB entries from directly-referenced peers
- **depth=2**: include entries from peers-of-peers (one hop further)
- **depth=N**: include up to N hops of dep-type edges in search graph traversal

Per-query override: `kb search --dep-depth N` to use N instead of the config default.

**Note:** epic-type edges are never auto-included in search; they exist only for explicit inspection via `kb peers edge list --epic-slug <slug>`.
