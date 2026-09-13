# MCP rebuild lifecycle model evidence

`McpRebuildLifecycle.tla` models the OTP-owned boundary around Rust rebuild
children. It does not model Rust replay or index computation. A transient
replacement contender PID may overlap the incumbent after old-owner death and
new-owner replacement,
but only the incumbent holding the Rust per-store lifetime lock may compute,
emit READY, or advance the successful acknowledgement. A busy contender emits
ERROR and exits before replay. The lock releases at OS child exit, rather than
Elixir's later observation of that exit.

`OutputControlFrame` models only bounded READY/ERROR control frames and the
capped Elixir status/error retention. It is not proof of a concrete log
rotation implementation, or of Unix/BEAM parent-death behaviour.

## Contract checked

- A client receives the successful `kb_rebuild` acknowledgement only after
  the same attempt has acquired the lifetime lock and emitted READY.
- Two transient PIDs may overlap, but `active` counts lock-owning replay
  work and remains at most one per selected store.
- A busy contender emits ERROR and exits before replay, READY, or successful
  acknowledgement. Its nonblocking try-lock, ERROR, and exit are one atomic
  model event; real PID overlap/timing is verified by production OS-PID tests.
- Old-owner death enters an explicit `orphaned` state; the guard's fair exit
  releases the lock at OS child exit, then new-owner observation gates a fresh
  attempt. A cancellation timeout enters `unknown` and likewise retains the
  slot until observed child exit. An explicit unknown-state recovery probe is
  lock-fenced: a busy candidate errors/exits atomically and cannot acknowledge.
- Retained control/status bytes are capped. This is an abstract retention
  property, not a proof of concrete file rotation.

The fixed configuration covers accepted and failed launches, lock acquisition
then READY, duplicate coalescing, an atomically rejected busy contender,
completion,
cancellation followed by observed exit, owner death, and fresh requests.
`Detached`, `WrongStore`, `FailedAck`, and `UnboundedOutput` retain their
original negative intent.

## TLC runs

Commands ran from `.state/agent-kb/tla` with TLC 2.19 and `-workers auto`
(12 workers). Final logs are retained in
`/tmp/mcp-rebuild-lock-v5-fixed.log` and
`/tmp/mcp-rebuild-lock-v5-{Detached,WrongStore,FailedAck,UnboundedOutput}.log`.

| Configuration | Result | Evidence |
| --- | --- | --- |
| `McpRebuildLifecycle_Fixed.cfg` | pass | 252 generated, 153 distinct states, depth 12; all invariants, `EventuallyQuiescent`, and fair guard reaping hold. |
| `McpRebuildLifecycle_Detached.cfg` | expected failure | TLC exit 13 after 67 generated / 40 distinct states; the detached configuration violates temporal `EventuallyGuardReapsOrphan`. |
| `McpRebuildLifecycle_WrongStore.cfg` | expected failure | TLC exit 12 after 3 generated / 3 distinct states; lock acquisition and READY use `other-store`, violating `critical_ExactStoreBinding`. |
| `McpRebuildLifecycle_FailedAck.cfg` | expected failure | TLC exit 12 after 2 generated / 2 distinct states; a failed launch advances the acknowledgement, violating `critical_AcknowledgementOnlyAfterAcceptedLaunch`. |
| `McpRebuildLifecycle_UnboundedOutput.cfg` | expected failure | TLC exit 12 after 56 generated / 50 distinct states; control-frame retention exceeds `MaxLogBytes`, violating `critical_BoundedLogState`. |

`OwnerDeathKillsChild` remains an explicit assumption about the inherited
stdin EOF guard. Production verification must prove normal EOF, owner crash,
and whole-VM termination with OS PID/start-time evidence; TLC cannot prove
those kernel effects.
