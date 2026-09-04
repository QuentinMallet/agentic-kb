# Query-Hits Telemetry

Query-hit telemetry is operational data, not KB source-of-truth.

## Durability Class

Telemetry is stored in a separate SQLite file:

```text
.state/agent-kb/query-hits.db
```

This separation is intentional:

- It is best-effort. Failures are swallowed.
- It is not replayed from the event log.
- It is not an input to rebuild.
- It is kept out of the main entries DB so lock-free search paths do not depend
  on a file that rebuild may swap.

## Surfaces and `KB_INJECTION_SOURCE`

When `KB_INJECTION_SOURCE` is set, retrieval surfaces such as `kb search`,
`kb context`, and MCP search record which entries were injected into the agent
under that surface label. This writes into the `injections` table, not the
event log.

Later transcript digestion can mark whether those injected entries were acted
on by matching ids or cited files in tool-call turns.

## Audit Report

`kb_audit_report` includes `injection_telemetry` when telemetry is available.
That report includes:

- `total_injections`
- `acted_on_rate`
- `unknown_surface_rate`
- `per_surface[...].acted_on_rate`

Treat these as observational metrics, not transactional guarantees.
