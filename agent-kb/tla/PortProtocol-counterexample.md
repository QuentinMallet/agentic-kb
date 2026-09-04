# C2/T1 — `PortProtocol.tla` counterexamples

Task `bd-21ef.2.2`, recorded 2026-09-04. The module compares the current implementation
(`FixedDesign = FALSE`) with ADR-3 (`FixedDesign = TRUE`).

## Bounds and tooling

The useful bounded instance is `MaxRequests = 2`, `D = 2`, `TickBound = 4`. Two request ids are
necessary for stale-reply cross-talk. `TickBound > D` is necessary to exhibit a receive window
repeatedly reset beyond the absolute deadline. These are the declared bounds; they were not
silently reduced.

TLC is `2.19` (rev `5a47802`) on Oracle JDK `1.8.0_504`. Commands are issued from
`.state/agent-kb/tla/`, with `CHECK_DEADLOCK FALSE` and a private `-Djava.io.tmpdir` so concurrent
agents on the same host do not clobber each other's metadir:

```sh
mkdir -p /tmp/tlc-c2t1
cd .state/agent-kb/tla
JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c2t1" \
  tlc -config PortProtocol.cfg                 -workers 4 -cleanup PortProtocol.tla
JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c2t1" \
  tlc -config PortProtocol_CurrentDesign.cfg   -workers 4 -cleanup PortProtocol.tla
JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c2t1" \
  tlc -config PortProtocol_CurrentDeadline.cfg -workers 4 -cleanup PortProtocol.tla
JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=/tmp/tlc-c2t1" \
  tlc -config PortProtocol_CurrentCrash.cfg    -workers 4 -cleanup PortProtocol.tla
```

## Run matrix

Constants below are `MaxRequests / D / TickBound`. Each of the three "current" configs is a
deliberately violating single-property check; the fixed config checks all four invariants
together.

The three violating configs run with `-workers 4`: TLC's parallel breadth-first search stops as
soon as any worker reports the invariant violation, so the exact generated/distinct counts and
which of the two symmetric request ids (`1` or `2`) appears in the reported trace vary slightly
run to run — confirmed by re-running `PortProtocol_CurrentDesign.cfg`, which produced the same
7-state, 4-action shape with ids `1` and `2` swapped and different generated/distinct counts. The
counts below and the traces in CE1-CE3 are copied verbatim from one specific run each; the
**shape** (action sequence and depth) is what is load-bearing and reproduces every time. The fixed
config (`PortProtocol.cfg`) explores to exhaustion regardless of worker interleaving, so its counts
(7497 / 3368) are stable.

| # | Config | Design | Checked properties | Constants | Expected | Observed |
|---|---|---|---|---|---|---|
| 1 | `PortProtocol.cfg` | ADR-3 fixed | `TypeOK`, `NoCrossTalk`, `BoundedDeadline`, `CrashIsPrompt` | `2 / 2 / 4` | PASS | **PASS** — no error; 7497 states generated, 3368 distinct, depth 16; finished in 1 s at 2026-09-04 14:50:54 |
| 2 | `PortProtocol_CurrentDesign.cfg` | current | `TypeOK`, `NoCrossTalk` | `2 / 2 / 4` | FAIL | **FAIL** — `NoCrossTalk` violated, depth 7; 414 states generated, 274 distinct, 146 left on queue at the point of the error; finished in 1 s at 2026-09-04 14:50:48 |
| 3 | `PortProtocol_CurrentDeadline.cfg` | current | `TypeOK`, `BoundedDeadline` | `2 / 2 / 4` | FAIL | **FAIL** — `BoundedDeadline` violated, depth 5; 103 states generated, 77 distinct, 41 left on queue; finished in 1 s at 2026-09-04 14:50:50 |
| 4 | `PortProtocol_CurrentCrash.cfg` | current | `TypeOK`, `CrashIsPrompt` | `2 / 2 / 4` | FAIL | **FAIL** — `CrashIsPrompt` violated, depth 6; 211 states generated, 145 distinct, 80 left on queue; finished in 1 s at 2026-09-04 14:50:52 |

`TypeOK` did not fail on any run — every reported error is the named safety property, not a
modeling-domain error. Run 1's count (3368 distinct at 2 variables fewer than the pre-fix module —
see Findings below) is the number to watch for regression: a future edit that raises it without an
accompanying reachability argument likely reintroduced a variable or transition this pass removed.

## CE1 — `NoCrossTalk` (current design)

TLC's breadth-first search found a *shorter* witness than the hand-derived one from the first
recording of this file: it does not need a `Timeout` at all. Two enqueued replies for the same
outstanding id are enough, because nothing in the current (`FixedDesign = FALSE`) `ConsumeReply`
checks the id before delivering.

| State | Action | `issued` | `outstanding` | `mailbox` | `clientState` | `received` | `deliveredFor` |
|---|---|---|---|---|---|---|---|
| 1 | *(init)* | `{}` | `0` | `<<>>` | `idle` | `0` | `0` |
| 2 | `Send(1)` | `{1}` | `1` | `<<>>` | `waiting` | `0` | `0` |
| 3 | `EnqueueReply(1)` | `{1}` | `1` | `<<1>>` | `waiting` | `0` | `0` |
| 4 | `EnqueueReply(1)` | `{1}` | `1` | `<<1,1>>` | `waiting` | `0` | `0` |
| 5 | `ConsumeReply` (id matches) | `{1}` | `0` | `<<1>>` | `done` | `1` | `1` |
| 6 | `Send(2)` | `{1,2}` | `2` | `<<1>>` | `waiting` | `0` | `0` |
| 7 | `ConsumeReply` (id does not match) | `{1,2}` | `0` | `<<>>` | `done` | `1` | `2` |

State 7 violates `NoCrossTalk == clientState # "done" \/ received = deliveredFor`: the client is
`done` with `received = 1` but `deliveredFor = 2` — it accepted the stale reply to request 1 as
the answer to request 2. Verbatim from the TLC output:

```
State 7: <ConsumeReply line 113, col 3 to line 133, col 55 of module PortProtocol>
/\ mailbox = <<>>
/\ elapsed = 0
/\ restarts = 0
/\ ticks = 0
/\ received = 1
/\ deliveredFor = 2
/\ outstanding = 0
/\ issued = {1, 2}
/\ clientState = "done"
/\ portState = "up"
/\ crashedAt = 0
```

With `FixedDesign = TRUE`, state 7's `ConsumeReply` takes the id-mismatch branch instead: it
discards the stale reply, charges the discard to request 2's absolute deadline (`elapsed' =
elapsed + 1`, guarded by `elapsed < D`), and stays `waiting` for id 2's real reply.

## CE2 — `BoundedDeadline` (current design)

Shortest witness at `D = 2`, exactly as originally predicted: three ticks of `Progress` with no
window reset, because the current design's `Progress` action does not shrink any budget as
`FixedDesign = FALSE`.

| State | Action | `clientState` | `elapsed` | `ticks` |
|---|---|---|---|---|
| 1 | *(init)* | `idle` | `0` | `0` |
| 2 | `Send(1)` | `waiting` | `0` | `0` |
| 3 | `Progress` | `waiting` | `1` | `1` |
| 4 | `Progress` | `waiting` | `2` | `2` |
| 5 | `Progress` | `waiting` | `3` | `3` |

State 5 violates `BoundedDeadline == clientState # "waiting" \/ elapsed <= D`: the client is still
`waiting` with `elapsed = 3 > D = 2`. Verbatim:

```
State 5: <Progress line 138, col 3 to line 145, col 64 of module PortProtocol>
/\ mailbox = <<>>
/\ elapsed = 3
/\ restarts = 0
/\ ticks = 3
/\ received = 0
/\ deliveredFor = 0
/\ outstanding = 1
/\ issued = {1}
/\ clientState = "waiting"
/\ portState = "up"
/\ crashedAt = 0
```

With `FixedDesign = TRUE`, `Progress`'s guard (`IF FixedDesign THEN elapsed < D ELSE TRUE`) forbids
the third tick outright — `elapsed` cannot exceed `D` while still `waiting`, so this state is
unreachable and `Timeout` (guarded `elapsed >= D`) is the only way out of `waiting` once the budget
is spent.

## CE3 — `CrashIsPrompt` (current design)

Included for completeness (`T1`'s acceptance criteria call out `NoCrossTalk` and `BoundedDeadline`
by name; this one is checked by the same run matrix and shares the same shape).

| State | Action | `portState` | `clientState` | `elapsed` | `crashedAt` |
|---|---|---|---|---|---|
| 1 | *(init)* | `up` | `idle` | `0` | `0` |
| 2 | `Send(2)` | `up` | `waiting` | `0` | `0` |
| 3 | `PortCrash` | `down` | `waiting` | `0` | `0` |
| 4 | `Progress` | `down` | `waiting` | `1` | `0` |
| 5 | `Progress` | `down` | `waiting` | `2` | `0` |
| 6 | `Progress` | `down` | `waiting` | `3` | `0` |

State 6 violates `CrashIsPrompt`: `portState = "down"`, `clientState = "waiting"`, and
`elapsed = 3 > crashedAt + D = 0 + 2`. The current design's `PortCrash` never observes the crash
(`clientState` is left `waiting` because nothing corresponds to `:exit_status`/`trap_exit`), so the
client keeps ticking `Progress` against a dead port indefinitely. With `FixedDesign = TRUE`,
`PortCrash`'s `IF FixedDesign /\ clientState = "waiting"` branch moves the client straight to
`"crashed"` in the same step, so state 4 above is unreachable — the environment assumption (crash
is observable via `:exit_status`) is what makes the property meaningful at all.

## Conformance table

Maps each `Next` disjunct to the Elixir clause (or message) in `mcp/lib/agentic_kb_mcp/port_manager.ex`
that implements it, at both the pre-P1 (current) and post-P1 (ADR-3 fixed) state of the code. This
is not a spec obligation TLC checks — it is the traceability P1's implementer needs to read this
model against the file it is changing.

| Action | Current (`FixedDesign = FALSE`) | After P1 (`FixedDesign = TRUE`, ADR-3) |
|---|---|---|
| `Send` | `handle_call({:request, request, timeout}, _from, state)` builds the line and calls `Port.command/2` (`:88-92`); `request["id"]` is generated by the caller and threaded through, but never checked on the way back. | Same call site; `id` becomes the value `collect_response` compares against on every reply (rule 2). |
| `Reply` (enqueue) | The Rust process's stdout line arrives as `{^port, {:data, {:eol, line}}}`; there is no enqueue step in Elixir — the OS pipe is the "mailbox" the model abstracts. | Unchanged — the OS pipe is still the mailbox; correlation happens on the consume side. |
| `Reply` (consume, match) | `collect_response`'s `{:ok, response} -> response` clause (`:147`) returns *any* decoded non-`progress` object — there is no match check. | The same clause gains `when response["id"] == id`, returning the reply. |
| `Reply` (consume, discard) | Does not exist: the current code has no branch that can reject a reply, so a non-matching final response is indistinguishable from a matching one. | A new clause on a non-matching `id`: log at `warn` with both ids, increment a `discarded` counter, and recurse into `collect_response` again without resetting `timeout` (rule 2, rule 3). |
| `Progress` | The `{:ok, %{"type" => "progress"} = prog}` clause (`:143-146`) logs and recurses with the *same* `timeout` value passed to the `receive...after` block, which restarts the clock (rule 3's defect). | The recursive call carries a recomputed remaining budget against an absolute monotonic deadline captured once at the start of `collect_response`, so a progress tick consumes budget instead of resetting it. |
| `Timeout` | The `after timeout -> %{"id" => id, "type" => "error", "code" => "timeout", ...}` clause (`:156-158`) fires when the per-receive `timeout` window elapses — which is repeatedly reset by ticks, so it is really "no message for `timeout` ms", not "the absolute deadline passed". | Same clause shape, but its firing condition is "the absolute deadline has been reached", matching this model's `Timeout` guard `elapsed >= D`. |
| `PortCrash` | Dead code today: `handle_info({port, :closed}, ...)` (`:101-104`) and `handle_info({:EXIT, port, reason}, ...)` (`:106-109`) exist but neither message is ever delivered, because `init/1` never calls `Process.flag(:trap_exit, true)` and `Port.open` never requests `:exit_status`. A real Rust crash is invisible until the next `Port.command/2` raises `ArgumentError`. | `init/1` adds `Process.flag(:trap_exit, true)`; `Port.open` adds `:exit_status`. `collect_response` gains a `{^port, {:exit_status, _}}` clause (primary mechanism) and keeps the existing `handle_info` clauses reachable as the `trap_exit` complement (rule 1, rule 5); either path returns `port_closed` and `handle_call` replies with `{:stop, :port_closed, reply, state}`. |
| `Restart` | No restart path exists in `port_manager.ex` at all — a crashed port is simply gone (see `PortCrash` row above). | Still not in `port_manager.ex`: a restart is the OTP supervisor relaunching the crashed port-owning process, entirely outside the `GenServer`'s own code. This model's `Restart` action has no guard tied to `Timeout` (see the comment on `Restart` in `PortProtocol.tla`) because rule 7 governs what triggers a restart (never a client-side timeout), not whether a supervisor may later restart a port that crashed for an unrelated reason. |

## Status

All four acceptance-criteria configs are committed and green in the direction each is meant to
demonstrate: the fixed design passes `TypeOK`, `NoCrossTalk`, `BoundedDeadline` and `CrashIsPrompt`
together, and each current-design config produces the deliberately targeted violation. No result
above is asserted without the run that produced it; the verbatim TLC blocks are copy-pasted, not
transcribed by hand.
