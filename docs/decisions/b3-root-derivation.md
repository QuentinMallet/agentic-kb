# B3: Repository-root derivation parity

## Decision

For a selected database, derive the repository root layout-aware, and when discovering a database at a candidate root prefer `<root>/.state/agent-kb/agent-kb.db` over `<root>/agent-kb/agent-kb.db`.

The recognized layouts are the canonical `<root>/.state/agent-kb/agent-kb.db` and the tolerated legacy `<root>/agent-kb/agent-kb.db`. If both databases exist at the same candidate root, the canonical database wins. Rust `Paths::discover` and Elixir `DbDiscovery` use that same candidate order.

## Rejected alternatives

- Always walking three parents was rejected because it is correct only for the canonical layout and resolves a legacy database to the parent of its repository.
- Giving the legacy database precedence was rejected because canonical storage is fleet-ratified and choosing legacy when both exist would prolong divergent state.
- Removing legacy discovery was rejected because deployed legacy checkouts remain supported and are already discoverable by the Elixir server.

## Fleet impact

On legacy-layout checkouts, both `kb_cite` and `kb_add` now hash path-only evidence against the repository containing `agent-kb/agent-kb.db`, rather than `kb_add` hashing against that repository's parent.
