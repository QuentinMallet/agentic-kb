# MCP rebuild lifecycle model evidence

`McpRebuildLifecycle.tla` models the OTP-owned boundary around one Rust
rebuild child. It does not model Rust replay or index computation. Its
`OutputChunk` action is an abstract retained-output counter, rather than proof
of a concrete file rotation or log-drain implementation.

- `McpRebuildLifecycle_Fixed.cfg` explores accepted and failed launches,
  duplicate coalescing, completion, cancellation followed by observed exit,
  owner death, and explicit fresh requests. It checks the critical lifecycle,
  binding, bounded-log, and no-auto-retry ticket invariants plus quiescence.
- `McpRebuildLifecycle_Detached.cfg` represents the original detached-child
  shape. Its required counterexample violates
  `critical_NoOrphanAfterOwnerDeath` after an accepted launch and owner death.
- `McpRebuildLifecycle_WrongStore.cfg` represents a rebuild bound to a store
  other than the active MCP store. Its required counterexample violates
  `critical_ExactStoreBinding` after an accepted launch.
- `McpRebuildLifecycle_FailedAck.cfg` represents returning the successful
  acknowledgement after an executable/port launch failure. It violates
  `critical_AcknowledgementOnlyAfterAcceptedLaunch`.
- `McpRebuildLifecycle_UnboundedOutput.cfg` represents retaining every
  independent stdout/stderr chunk. It violates `critical_BoundedLogState`.

`OwnerDeathKillsChild` is an assumption about the planned native
parent-death mechanism, not a claim that TLC proves BEAM or kernel behavior.
The implementation task must prove it with an OS-PID process test for normal
EOF, owner crash, and whole-VM termination.

## TLC runs

Commands were run from `.state/agent-kb/tla` with TLC 2.19 and `-workers auto`
(12 workers). Logs are retained for this session in `/tmp/mcp-rebuild-fixed-tlc.log`,
`/tmp/McpRebuildLifecycle_Detached-tlc.log`, and
`/tmp/McpRebuildLifecycle_WrongStore-tlc.log`,
`/tmp/McpRebuildLifecycle_FailedAck-tlc.log`, and
`/tmp/McpRebuildLifecycle_UnboundedOutput-tlc.log`.

| Configuration | Result | Evidence |
| --- | --- | --- |
| `McpRebuildLifecycle_Fixed.cfg` | pass | 154 generated, 76 distinct states, depth 8; all invariants and `EventuallyQuiescent` hold. |
| `McpRebuildLifecycle_Detached.cfg` | expected failure | TLC exit 12 after 49 generated / 29 distinct states. Trace: accepted launch, coalesced request, owner death, then `phase = "stopped"`, `child = "live"`, and `active = 1`, violating `critical_NoOrphanAfterOwnerDeath`. |
| `McpRebuildLifecycle_WrongStore.cfg` | expected failure | TLC exit 12 after 2 generated / 2 distinct states. Trace: accepted launch assigns `workerStore = "other-store"` while `active = 1`, violating `critical_ExactStoreBinding`. |
| `McpRebuildLifecycle_FailedAck.cfg` | expected failure | TLC exit 12 after 2 generated / 2 distinct states. A failed launch advances `acknowledgedAttempt` while `acceptedAttempt` remains zero, violating `critical_AcknowledgementOnlyAfterAcceptedLaunch`. |
| `McpRebuildLifecycle_UnboundedOutput.cfg` | expected failure | TLC exit 12 after 63 generated / 34 distinct states. Independent output chunks raise retained bytes above `MaxLogBytes`, violating `critical_BoundedLogState`. |

The first fixed run exposed a model omission: without `WF_vars(Launch)`, an
initial client request could stutter indefinitely and violate quiescence. The
final model adds launch fairness and the fixed run above is the accepted result.
