# Plan: C2 — exclusion & boundary discipline

Component epic `bd-21ef.2` of meta-epic `bd-21ef` (storage-correctness-2). Source findings: the
4-lens deep review of master `2e2051d` (2026-09-04), lens 4 findings 2–14. Lens 4 finding 1
(Critical, rebuild swap) belongs to C1. Lens 3 finding 4 is **reassigned to C3** — see Cross-component
ordering.

Mode: **DELIBERATE** consensus (RALPLAN-DR). Risk class: **high** — this component changes the
mutual-exclusion contract every other writer depends on, and the trust boundary where untrusted
agent input enters the process. Blast radius is system-wide: C1 and C3 both call the surfaces C2
rewrites, and `B1` is a breaking change to the fleet's only sanctioned KB write surface.

Branch: from the `storage-correctness-2` aggregator. Post-impl merges component → aggregator only;
the aggregator reaches master when C1, C2 and C3 all close.

Revision r2 — incorporates architect and critic consensus passes. Changes from r1: ADR-1 signatures
corrected (the lock is not derivable from the DB path); `kb_core::add_locked` added as an ADR-1
deliverable; ADR-3 rewritten (the crash clauses could not fire, and the timeout layering did not
work); `L1` split into `L1a`/`L1b` after the call-site count was measured at 107, not ~40; lens 4
finding 13 reinstated; lens 3 finding 4 reassigned to C3; new ADR-7 on the read/recovery tension;
Principle 2 restated to what the design actually delivers.

---

## Principles

1. **A read must not change content.** An operation the caller believes is a read may not alter
   stored data as a side effect. Today every `open_db` issues two unlocked `DELETE`s. (Stated as
   "content", not "writes": a read connection may still write WAL/`-shm` recovery state, and must,
   or it cannot read a database left hot by a crashed writer.)
2. **No mutating connection without a live, path-matching lock guard.** The obligation is carried in
   the signature (`open_rw(&Paths, &Lock)`) and backed by a runtime guard (`PRAGMA query_only` on
   read opens, a re-entrancy registry in `acquire_lock`). This is *not* a proof of deadlock-freedom
   and the plan does not claim one — see ADR-1's Consequences.
3. **Invariants belong at the shared boundary, not at one adapter.** A cap enforced only by the MCP
   adapter is not a cap. (Shared with C3 principle 3 — deliberately identical.)
4. **Reject, never coerce — at the outermost layer that sees the field.** For agent-authored input
   that layer is Elixir, not Rust: `put_if_present` already drops unknown keys before Rust can see
   them. A fix applied only in Rust does not satisfy this principle.
5. **A count is a claim.** `embedded: N` and `failed: 0` are assertions about durable state; they
   may only be emitted when the writes actually succeeded.
6. **A correlated protocol correlates, and bounds.** A reply is matched to its request or discarded
   — and discarding must consume the caller's deadline, or correlation buys liveness failure.

## Decision drivers (top 3)

1. **C1 and C3 are blocked behind the lock contract.** C1's applied-cursor work and C3's `V3` both
   edit the `open_db` region C2 rewrites. Its ordering dominates the meta-epic schedule, which is
   why `L1` is split: only the genuinely small piece (`L1a`) is on the critical path.
2. **Silent wrong answers over loud failures.** Four findings (F2 cross-talk, F6 false success,
   F7 wrong-repo hashing, F8 coerced defaults) produce a plausible answer that is wrong. These rank
   above findings that merely fail.
3. **The Elixir half has never been compiled.** No landing in `mcp/` was compile-checked in three
   prior epics because the devShell has no Elixir. This gates `P1` and `S1` — half the component —
   and its true cost is unknown until `T0` runs.

---

## Context

Three surfaces, one component, because they are the three places the process boundary is crossed.

**Exclusion** (`src/components/db.rs`, `src/commands/add.rs` `acquire_lock`, `src/commands/peers.rs`,
`src/commands/reembed.rs`, `src/commands/mcp.rs`) — who may mutate the DB and the event log, under
what proof.

**Port protocol** (`mcp/lib/agentic_kb_mcp/port_manager.ex`, `src/commands/mcp.rs` startup) — the
request/response correlation, timeout and crash semantics between the BEAM and the Rust port.

**Request boundary** (`mcp/lib/agentic_kb_mcp/mcp_server.ex` schemas + `put_if_present`,
`src/commands/mcp.rs` `handle_request` + handlers, `src/commands/add_validation.rs`) — where
untrusted agent-authored JSON becomes typed values.

### What the code actually does (verified at HEAD `2e2051d`)

**Exclusion.** `acquire_lock` (`add.rs:208–222`) is a single blocking exclusive `flock` — `fs2`'s
`lock_exclusive` is `flock(2)`, associated with the open file description — with no timeout,
returning an RAII `Lock(File)` guard (`add.rs:224`). No shared-lock variant exists. `open_db`
(`db.rs:157–171`) unconditionally sets `PRAGMA journal_mode=WAL` (`:161`), runs `ensure_schema`
(DDL), stamps `schema_version` when fresh, and calls `sweep_expired_peers` (`db.rs:114–121`), which
issues two `DELETE`s. **No `open_db` call site is a pure read at the SQL level** — including
`kb search`, `kb context`, `kb cited-by`, `handle_kb_get`, `handle_provenance`, and rebuild's own
nominal read phases.

`open_db` has **107 call sites** in `src/` (measured, not estimated), 47 of them in `mcp.rs`, and
roughly half are `#[cfg(test)]` fixtures. This count, not the production sites, is what makes the
C1/C3 rebase expensive — hence `L1a`'s test-fixture helper.

Unlocked mutating call sites:

| Site | File:line | Mutation |
|---|---|---|
| `kb peers add` | `peers.rs:132,151` | `INSERT INTO graphs`, `INSERT INTO peers` |
| `kb peers remove` | `peers.rs:227,231` | `DELETE` ×2 |
| `kb peers import` | `peers.rs:481,499` | `INSERT` ×2 |
| `kb peers edge-add` | `peers.rs:611,629` | `INSERT` ×2 |
| `kb peers edge-remove` | `peers.rs:736,740` | `DELETE` ×2 |
| `kb peers edge-cleanup-epic` | `peers.rs:773,777` | `DELETE` ×2 |
| `handle_kb_peers_add` | `mcp.rs:1894,1907` | `INSERT` ×2 |
| `handle_kb_peers_remove` | `mcp.rs:1979,1985` | `DELETE` ×2 |
| `kb reembed` | `reembed.rs:108` | `INSERT OR REPLACE INTO entries_emb` |
| `handle_reembed` | `mcp.rs:1126` | `INSERT OR REPLACE INTO entries_emb` |
| `check_embed_mode_vintage` | `db.rs:632`, called from `reembed.rs:101`, `mcp.rs:1116` | `INSERT OR IGNORE INTO kb_meta` |
| rebuild schema stamp | `rebuild.rs:146–149` | `INSERT OR REPLACE INTO kb_meta` under the *schema-upgrade* lock, not `paths.lock` |
| peer-DB open in federated search | `search.rs:133` | `ensure_schema` DDL + sweep against **another repository's** DB |
| every `open_db` | `db.rs:169` | `sweep_expired_peers` — 2 × `DELETE` |

Separately, `query_hits::record_hits` writes a **second SQLite database**
(`paths.query_hits = <root>/.state/agent-kb/query-hits.db`, `config.rs:249`) from `kb search`,
`kb context` and MCP search (`search.rs:233`, `context.rs:104`, `mcp.rs:325,354`). That is telemetry
in a separate file with its own lifecycle; it is **explicitly out of scope** and the contract table
records the exemption rather than pretending it does not exist.

`peers.rs` contains zero occurrences of the string `lock`. The locked mutation paths
(`kb_core::add:238`, `expire.rs:46`, `run.rs:42`, `test_add.rs:45`, `handle_audit_run:1428`,
`handle_audit_record:1514`, `compact.rs:88`, `migrate_citations.rs:247`, `stale_check.rs:230`,
rebuild phases 1 and 3) are internally consistent: lock acquired *before* `open_db`, single global
exclusive blocking lock, RAII release. The discipline exists; it is simply not universal.

**No path re-acquires the lock today, and that is load-bearing.** `rebuild_if_schema_obsolete` uses
a *different* lock file (`rebuild.rs:129`) and releases `paths.lock` before `execute_with`
(`rebuild.rs:253–270`), with the self-deadlock documented at `rebuild.rs:124–128`. `compact`'s
`_lock` (`compact.rs:88`) is still live at the `maybe_vacuum_after_compact` call (`compact.rs:283`),
so a `&Lock` threads cleanly there. The re-entrancy hazard is **created** by putting `handle_import`
under a lock, not present today: `kb_core::add` acquires the lock itself (`kb_core.rs:238`), so any
caller already holding it self-deadlocks.

**Two lock layouts exist.** `Paths::from_root` (`config.rs:236–251`) puts the lock at
`<root>/.state/.lock` with the DB at `<root>/.state/agent-kb/agent-kb.db` — not siblings. The MCP
legacy fallback (`mcp.rs:85–90`) puts it at `<db_dir>/agent-kb.lock` — siblings. "The lock governing
this DB path" is therefore a property of `Paths`, not of the path, which is why `open_rw` takes
`&Paths`.

**Port protocol.** `collect_response/3` (`port_manager.ex:139`) takes an `id` and never compares it;
the first decoded non-`progress` object is returned (`:147`). After a timeout (`:159`) the Rust side
keeps working and its reply stays in the mailbox, so the *next* call consumes it.

`init/1` (`:43–62`) never calls `Process.flag(:trap_exit, true)`, and `Port.open` (`:47–53`) passes
no `:exit_status` — verified: zero occurrences of `trap_exit`, `exit_status` or `Process.flag`
anywhere under `mcp/lib/`. **The existing `handle_info({port, :closed})` and
`handle_info({:EXIT, port, reason})` clauses (`:101`, `:106`) are already dead code.** `{Port, :closed}`
arrives only as the reply to an explicit `Port.close/1`; `{:EXIT, Port, _}` only reaches an owner
that traps exits. When the Rust process dies on its own, neither message is delivered. The lens 4
diagnosis ("remain queued until `handle_call` returns") is the wrong root cause, and the true
symptom is worse: a port crash is invisible **permanently**, surfacing later as an `ArgumentError`
from `Port.command/2`.

The outer `GenServer.call` timeout and the inner deadline are both `@call_timeout` (`:12`, `:25`),
and `call_port(request, timeout)` passes the caller's value as both. The inner `after timeout`
window *resets* on every progress tick while the outer is absolute from call start, so an operation
exceeding 30 s wall-clock already fails at the outer today regardless of ticks. `await_ready`
(`:117`) has no close/exit clause, so a fatal Rust startup error — stderr, then `exit` — surfaces as
`handshake_timeout` with the real cause lost.

**Request boundary.** The Elixir adapter builds every port request from an explicit `put_if_present`
whitelist (`mcp_server.ex:432–440, 446–458, …`), so an agent argument the whitelist does not know is
**silently dropped before Rust sees it**. This — not the Rust side — is the outermost coercion
point, and no Rust-side change can fix it.

In Rust, `handle_request` (`mcp.rs:126–162`) parses into an untyped `Value`; the request object is
**flat** (`id` and `method` are siblings of the params, `:129–131`). Handlers then use
`.get(..).and_then(..).unwrap_or(default)`: a negative or string `limit` becomes `10` (`:250`,
`:284`); `expand_ids` ignores `maxItems: 32`; mixed-type array members are dropped by `filter_map`
(`:230`) rather than rejected; missing or wrong-typed `summary`/`content` become empty strings.
Input is read by `stdin.lock().lines()` (`:125`) into an unbounded `String`. The parse-error
envelope (`:133`) carries no `id`.

`add_validation.rs:89–98` rejects only a self-referential `derived_from`. `mcp_server.ex:122`
documents it as plainly optional.

`kb_core::add` derives the repository root by walking three parents (`kb_core.rs:145–149`), while
`root_from_db` (`mcp.rs:47–68`) special-cases `.state/agent-kb/`. On the legacy
`<root>/agent-kb/agent-kb.db` layout that `db_discovery.ex:18` may select, `add` lands one directory
**above** the repo root, so `kb_cite` and `kb_add` resolve path-only evidence against different
repositories in the same process. `Paths` has no `root` field (`config.rs:181–194`).

`handle_audit_record` (`mcp.rs:1583–1631`) commits three things separately per verdict; the weight
update is gated on `inserted > 0`, so a crash between the audit-row insert and the weight update is
permanent — the retry's `INSERT OR IGNORE` returns 0 and the repair never runs. `conn` at `:1520` is
not `mut`; the repo's existing precedent for this is `conn.unchecked_transaction()`
(`older_than.rs:315`, rusqlite 0.31).

`handle_reembed` returns **three different response shapes** for one method (`:1061`, `:1108`,
`:1136`) and `render_result` pattern-matches on shape — the same drift class as finding 12, which
only names `kb_add`.

Rust dispatch (`mcp.rs:180–207`) implements `audit_run`, `audit_record`, `audit_report`,
`provenance`, `kb_peers_add/list/remove`; the Elixir `@tools` list exposes none. `handle_add`
produces `similar_existing` and the renderer (`mcp_server.ex:634`) prints only the new id.

**Toolchain.** `mcp/mix.exs` declares `elixir: "~> 1.18"`, zero deps, OTP 27's `:json`. `mcp/test/`
contains exactly one test file plus `test_helper.exs`. The flake devShell (`flake.nix:148–173`) has
no Elixir and no Erlang; `nixpkgs` provides `beam27Packages.elixir` at 1.18.4 on OTP 27.

---

## ADR-1 — The lock-discipline contract

**Decision.** Split `open_db` into four functions with different obligations, and carry the write
obligation in the signature.

```rust
/// Opens an existing, schema-current DB. PRAGMA query_only=ON. No DDL, no sweep.
/// Errors with DbUninitialized if the DB or the entries table is absent.
pub fn open_ro(db_path: &Path) -> Result<Connection>

/// Opens for mutation. Takes Paths (the lock is NOT derivable from the DB path —
/// two layouts exist) and proof that paths.lock is held.
pub fn open_rw(paths: &Paths, lock: &Lock) -> Result<Connection>

/// Rebuild's private tmp DB: no governing lock, never the live DB.
/// Asserts the path is not paths.db.
pub fn open_scratch(db_path: &Path) -> Result<Connection>

/// Creates parent dirs, ensures schema, stamps schema_version, sweeps expired peers.
/// Acquires and RELEASES paths.lock internally. Returns no connection — callers
/// then use open_ro or open_rw, so nothing escapes the split.
pub fn open_or_init(paths: &Paths) -> Result<()>
```

`Lock` gains a `path: PathBuf` so `open_rw` asserts it is the *right* lock, not merely *a* lock.

**`kb_core::add` splits too**, and this is an ADR-1 deliverable rather than a consequence discovered
later: `add_locked(&Lock, &Connection, ..)` carrying the real logic, plus a thin `add(..)` wrapper
that acquires and delegates. Without it, any caller already holding the lock — `handle_import` under
`L2`, any future locked batch path — self-deadlocks on `flock`, exactly as `rebuild.rs:124–128`
already documents.

**`acquire_lock` gains a process-local re-entrancy registry**: a canonicalized-path set behind a
mutex; a second in-process acquire of the same path returns an error naming the first acquisition
site instead of blocking forever. Roughly twenty lines, and it is the mechanism that actually
prevents the pre-mortem's Scenario 1 — the type system cannot.

**Peer TTL policy.** Expiry becomes **read-time filter plus locked physical sweep**: peer reads add
`AND (expires_at IS NULL OR expires_at >= datetime('now'))`, and `sweep_expired_peers` runs only
under the lock, from `compact`, `rebuild`, and the peer-mutating commands. A read then never needs
to change content to be correct. This introduces a new observable state — logically expired but
physically present — which the spec waiver must address (see T1).

**Schema creation policy.** `open_ro` on an absent or unschemaed DB returns `DbUninitialized`. The
read commands (`kb search`, `kb context`, `kb cited-by`, `handle_kb_get`, `handle_provenance`) map
that to an **empty result with a one-line note on stderr**, not to an error exit — preserving
today's first-run UX while removing the silent create. This is a user-visible decision and is called
out for sign-off in `L1a`.

**Drivers.** Principle 1 and principle 2; and the schedule driver — C1 and C3 need a signature.

**Alternatives considered.**

- *Option B — keep one `open_db`, make it non-mutating, lock only around writes.* Smaller diff, no
  signature churn for C1/C3. **Rejected, but the rejection is narrower than r1 claimed.** ADR-1 does
  not deliver compile-time exclusion (the `open_scratch` escape hatch and the freely-callable
  `acquire_lock` both punch through it). What it delivers over Option B is a signature that makes an
  omission visible at review, plus a home for `add_locked`. `query_only` and the re-entrancy
  registry are obtainable under Option B too. **Lead ruling: Option B stays a recorded live fallback,
  and the decision point is `L1a`'s post-impl review — if the 107-site blast radius produces a
  red-flag diff, the component drops to Option B before `L1c` deletes anything.** This is why `L1a`
  deprecates rather than deletes: the fallback stays reachable until `L1c`. Option B plus
  those two guards captures most of the value at a fraction of the 107-call-site blast radius** —
  this is a live fallback, not a strawman.
- *Option C — drop the flock, rely on WAL + `BEGIN IMMEDIATE`.* **Rejected:** the flock also guards
  the event-log file (append, truncate-repair, compact/rebuild renames) and the DB-file swap. SQLite
  transactions cannot span a `fs::rename`. This removes exclusion exactly where the Criticals live.
- *Option D — `SQLITE_OPEN_READ_ONLY` file flag instead of `PRAGMA query_only`.* **Rejected:** a
  read-only *file handle* cannot write `-shm`, so it cannot recover a hot WAL; a reader arriving
  after a writer crash fails instead of recovering. `query_only` gates write statements at the VDBE
  layer while leaving WAL-index recovery permitted. The architect pass confirms the mechanism;
  `L1a` still carries a test for it, because ~42 non-test call sites depend on the premise.

**Consequences.**

- **107 call sites**, ~47 of them in `mcp.rs` and roughly half `#[cfg(test)]`. `L1a` ships a
  test-fixture helper (`test_db(root) -> (Paths, Connection)`) so the fixture churn is one
  substitution, not 50 hand edits.
- **`open_db` is deleted in two steps, not one.** `L1a` introduces the new functions and leaves
  `open_db` as a `#[deprecated]` thin wrapper over `open_or_init` + `open_ro`, so C1 and C3 get a
  rebase window instead of a detonation mid-task. A final task deletes it once both siblings have
  rebased. The r1 criterion "removed, not deprecated" was correct in isolation and maximally hostile
  with three agents on one aggregator.
- **`open_ro` drops `PRAGMA journal_mode=WAL`, and that pragma is load-bearing today.** Rebuild
  builds its tmp DB in `journal_mode=DELETE` (`rebuild.rs:379,425`) and renames it over the live DB;
  `open_db`'s unconditional WAL pragma is what silently heals the mode on the next open. Under
  ADR-1 a read open cannot do that, so the DB stays in rollback-journal mode — readers and writers
  blocking each other — until a write path opens it. **This is a hard C1 dependency:** either C1's
  swap sets WAL on the tmp DB before the rename, or `open_or_init` becomes a mandatory post-swap
  step. Recorded in Cross-component ordering; C2's guardrails forbid C2 from touching the swap.
- **What `&Lock` proves** is: at the point of a mutating open, a live lock guard exists whose path
  matches. Not deadlock-freedom, not that `acquire_lock` was not called twice. Principle 2 is
  written to that scope.
- `query_only` blocks writes to the temp database as well, so a future read path reaching for a
  `CREATE TEMP TABLE` fails at runtime. No such path exists today (`src/` has no `CREATE TEMP`); the
  contract table records it as a standing constraint.

## ADR-2 — Reembed exclusion granularity

**Decision.** Selection unlocked and cheap; embedding — the expensive part — outside the lock; then
per batch: acquire the lock, re-resolve each **entry id** to its current rowid, **re-check the
selection predicate inside the batch** (`rowid NOT IN (SELECT rowid FROM entries_emb)`), write with
`INSERT OR IGNORE`, release.

`INSERT OR IGNORE` rather than `INSERT OR REPLACE` is the load-bearing change: the selection
predicate is "entries with no embedding row", so "never overwrite" is exactly the intended
semantics. `OR REPLACE` lets a batch clobber a **correct, fresh** embedding written by a concurrent
`kb_core::add` between selection and write — computed from text that no longer exists. Re-resolving
by id does not catch this, because `is_stale` means expired, not edited.

Batch size derives from a stated lock-hold budget of **≤ 50 ms**, not from a round number. 32 f16
blob upserts is sub-millisecond, so the acquire/release plus the re-resolution query dominates; the
constant is named, commented with the budget, and measured once in the task.

**Drivers.** A reembed run over a large corpus takes minutes; holding the lock throughout blocks
every writer for that window.

**Alternatives considered.** *Hold the lock for the entire run* — trivially correct, rejected on
availability. *Leave it unlocked* — rejected: writes land in a renamed-away inode and disappear.

**Consequences.** A future `reembed --force` (deliberate overwrite) needs a separate path, since
`OR IGNORE` forecloses overwriting by design.

## ADR-3 — Port protocol correlation and failure semantics

**Decision.**

1. **Make port death deliverable.** `init/1` calls `Process.flag(:trap_exit, true)` **and**
   `Port.open` gains `:exit_status`. Without both, the crash clauses cannot fire — the existing ones
   at `port_manager.ex:101,106` are dead code today, which is why F3's real symptom is a permanent
   invisible hang, not a 30 s wait. `:exit_status` is the primary mechanism (an ordinary message, no
   change to shutdown semantics); `trap_exit` covers owner-directed exits and obliges a `terminate/2`.
2. **Correlate.** `collect_response` compares `response["id"]` to the requested id; a non-matching
   final response is logged at warn with both ids, increments a `discarded` counter, and is
   discarded. Progress ticks reset nothing unless their id matches.
3. **Bound.** The inner deadline is **absolute** — a monotonic deadline computed once, with the
   remaining budget recomputed per loop iteration, so progress ticks *and* discards both consume it.
   r1's "reset the window on each tick" is what made the inner deadline unbounded while the outer
   stayed absolute; correlation without this turns a wrong answer into a hang.
4. **Layer.** The caller's `timeout` argument is the **inner** deadline; `GenServer.call` receives
   `:infinity`, so the inner deadline is genuinely authoritative and the two cannot race. The
   client-visible timeout is unchanged. Because a wedged GenServer then never times out at the
   caller, decision 1 is the sole liveness guarantee and **must land in the same task**, not after.
5. **Crash inside the request.** `collect_response` handles `{^port, {:exit_status, _}}`,
   `{^port, :closed}` and `{:EXIT, ^port, reason}`, returning a `port_closed` error;
   `handle_call` returns `{:stop, :port_closed, reply, state}` — a valid return that replies before
   terminating. Callers queued behind it get `exit(:noproc)`; `call_port` wraps `GenServer.call` so
   that surfaces as a `port_unavailable` **envelope**, not a raised exit, matching P1's own contract.
6. **Startup.** `await_ready` handles close/exit/exit_status immediately; Rust emits a final
   `{"type":"error", ...}` stdout envelope before exiting on fatal startup failure, so the cause
   travels on the protocol instead of dying on stderr.
7. **No restart on timeout.** Late replies are made harmless by rule 2, so a timeout does not
   discard the warmed embedder.

**Drivers.** Principle 6; the observed bug class is a *wrong answer*, and the naive fix converts it
into a hang.

**Alternatives considered.** *Restart the port on every timeout* — simplest guarantee of no stale
replies. **Rejected:** discards warm state and turns one slow request into a cold start for all
subsequent ones. *Multiplex requests* — out of scope; nothing here requires it.

**Consequences.** Trapping exits obliges a `terminate/2` and changes the manager's relationship to
the `System.cmd` child spawned in `handle_cast(:rebuild_async, ...)`; P1 states what happens to that
child on manager shutdown. There is **no unbounded-mailbox risk** from discarding: the selective
receive pattern `{^port, {:data, {:eol, line}}}` matches all port data regardless of id, so a
discard genuinely dequeues, and `handle_info` (`:96`) drains the same shape between calls.

## ADR-4 — Reject at the outermost layer: Elixir first, then typed Rust structs

**Decision.** Two layers, in this order of importance:

1. **Elixir rejects unknown args.** Tool schemas set `additionalProperties: false` and dispatch
   validates `args` keys against the schema before building the port request. This is where an
   agent's misspelled field currently vanishes without trace (`put_if_present`,
   `mcp_server.ex:432–440`), and no Rust change can reach it.
2. **Rust deserializes into per-method typed structs** with `#[serde(deny_unknown_fields)]`,
   non-`Option` required fields, `Vec<T>` array members (a mixed-type member is a deserialize error,
   not a filtered one), and explicit numeric range validation. Because the request object is flat
   (`id` and `method` are siblings of the params, `mcp.rs:129–131`), each struct declares `id` and
   `method` or dispatch strips them first — the plan picks *declare*, so the envelope stays one
   deserialization.

`deny_unknown_fields` is chosen because the Elixir adapter and the Rust port ship from this
repository and version in lockstep; the adapter's whitelist means a stale fleet pin **cannot**
forward an unknown field to Rust through the MCP path, so the Rust half is low-risk. Layer 1 is the
breaking one, and it breaks in the right direction: an argument that was silently ignored now
returns an error naming it.

**Alternatives considered.** *Lenient Rust structs.* Preserves forward compatibility if a
third-party MCP client appears. **Rejected on current facts**, recorded so it is revisitable.
*Runtime JSON-Schema validation in Rust.* Adds a dependency and a second source of truth; the struct
is the schema.

**Consequences.** Adding a request field requires touching the struct — a feature. Layer 1 changes
agent-visible behaviour for any caller sending an undocumented argument, so `B1` carries a
field-enumeration criterion against the deployed `kb-protocol.md` pin and a machines_conf
notification, per the standing cross-repo rule.

Caps for `search_entries` (lens 3 finding 4) are **not** in this ADR — see Cross-component ordering.

## ADR-5 — Audit-record atomicity

**Decision.** Per verdict: append the expire event to the log first, then `apply_event` +
`INSERT OR IGNORE INTO audit_runs` + the `source_weights` upsert inside **one SQLite transaction**,
opened with `conn.unchecked_transaction()` (the handler's `conn` at `mcp.rs:1520` is not `mut`; this
is the repo's existing precedent at `older_than.rs:315`). The `inserted > 0` gate then correctly
implies the weight was applied, because both happened or neither did.

**Transaction ownership: SETTLED — C1's `D3` owns the outer transaction and `A1` joins it via a
savepoint.** C1's planner confirmed this in `open-questions.md` (2026-09-04). The hazard it avoids:
`unchecked_transaction()` issues `BEGIN DEFERRED`, and SQLite rejects a transaction inside a
transaction, so a wrong guess is a runtime error on every audit record. `A1` therefore opens a
savepoint, not a transaction, when running inside `D3`'s scope.

**Drivers.** The current failure is permanent and silent — the retry path cannot repair it.

**Alternatives considered.** *A `weight_applied` ledger column with a repair pass.* Redundant once
the transaction exists. *One event applied on replay.* Cleaner in principle but expands the event
schema, which is C1's surface this cycle.

**Consequences.** A residual window remains between the log append and the transaction, closed only
by C1's fsync work; `A1` names it rather than claiming durability C2 does not deliver. Rows already
split by a past crash are not retroactively repaired. **The lead ruled this stays deferred, but as an
explicit decision checkpoint inside `A1`'s acceptance criteria rather than a silent omission** — `A1`
counts the affected rows and records the deferral with that count, so the drift is a known quantity.

## ADR-6 — Surface drift: expose audit and provenance, keep peers port-internal

**Decision.** Add Elixir tool schemas, dispatch clauses and renderers for `audit_run`,
`audit_record`, `audit_report` and `provenance`. Leave `kb_peers_*` implemented but not in `@tools`,
documented in `mcp/README.md` as CLI-parity internal port methods. Render `similar_existing`
(id, path, summary, score) in the `kb_add` response, and apply the same shape/renderer consistency
test **method-wide** — `handle_reembed` alone returns three shapes for one method.

**Drivers.** `AGENTS.md` §Agent knowledge base makes MCP the only sanctioned surface for agent KB
operations, and `/kb-review` performs audit operations, so leaving audit CLI-only forces a rule
violation. Peer graph setup is an operator/hook action, not an agent action — that asymmetry is the
whole argument, and r1's "four schemas cost context" reasoning is withdrawn: it cut equally against
the four tools being added.

**Security note.** Exposing `kb_audit_record` puts `source_weights` — the ranking signal — on the
agent-facing surface. Agent-triggered expiry is not new (`kb_expire` is already exposed), but weight
manipulation is. `S1` carries a `/threat-model` pass per `AGENTS.md`.

**Alternatives considered.** *Expose peers too.* Maximally consistent with the MCP-only rule;
**rejected** because peer mutation is not an agent workflow, revisitable the moment one needs it.
*Retire the unused Rust handlers.* **Rejected:** they are the CLI-parity surface and retiring them
would strand `/kb-review`.

## ADR-7 — Reads, recovery, and the write lock (meta-epic decision)

**The tension.** You can have at most two of: (a) reads never change content; (b) reads always see a
recovered, log-current DB; (c) reads never take the write lock. C2's principle 1 picks (a) and (c).
C1's `T2` makes `Open` a cursor-driven recovery point — "DB converges with no manual `kb rebuild`"
— which picks (b). Under `open_ro` + `query_only`, recovery on a read path is not merely misplaced,
it is impossible.

**Decision: SETTLED — C1 yields.** C1's planner confirmed in `open-questions.md` (2026-09-04):
"recovery fires at process entry and write paths only; read paths detect and warn, never taking the
write lock." Recovery lives in `open_or_init`, invoked at MCP startup and CLI dispatch, and in write
paths. A pure reader that finds the DB behind the log **serves what it has and emits a one-line
staleness note on stderr naming `kb rebuild`**; it does not take the write lock and does not silently
recover.
Sacrificing (b) for readers is the right trade because a read that blocks on the write lock turns
every `kb search` into a contention point, and a read that silently mutates is the defect this
component exists to remove.

**Consequence for `L1b`'s contract table.** C1 also records that rebuild's swap does **not** preserve
arbitrary `kb_meta` keys — the tmp DB receives only `schema_version` (`rebuild.rs:148`) and
`embed_text_mode` (`db.rs:633`), and C1's `T5b` now writes its cursor rows explicitly before the
swap. Any `kb_meta` key C2 relies on must ride that same mechanism rather than assume it survives a
rename. The contract table's rebuild row states this.

---

## Guardrails

**Must have**
- Every impl task carries a dependency edge to the spec task `T1` (repo TLA+ gate).
- TDD: the failing test is written and committed before the implementation.
- Property-based tests (`proptest`) where the property is the point: request-struct rejection over
  generated malformed JSON; reembed batch re-resolution over generated interleavings.
- `T0` lands before any change under `mcp/` is claimed complete. No Elixir landing may close on
  "looks right" — `mix compile --warnings-as-errors` and `mix test` evidence required.
- Cargo test evidence uses `tee` + `PIPESTATUS` (KB `procedures/verification-harness`).
- The lock-discipline contract table is a committed artifact under `docs/`, not a plan-only note.
- Every pre-mortem mitigation appears in a named task's acceptance criteria.

**Must NOT have**
- No change to the rebuild swap sequence, WAL/SHM handling, or event-log fsync — that is C1.
  C2 supplies and *writes down* the exclusion invariant the swap relies on (lens 4 finding 13);
  C1 rewrites the swap.
- No request multiplexing over the port.
- No new event-schema fields (C1 owns the event log this cycle).
- No `filter_map(|r| r.ok())` introduced on any request-parsing path.
- No clamping work inside `search_entries` — reassigned to C3's `S2`.
- No retroactive repair of already-split audit rows (Open questions Q3 records the deferral).

---

## Cross-component ordering (C1 / C2 / C3)

**1. `L1a` is the shared foundational task and lands on the aggregator first.** C1's applied cursor
is *a mutation performed at open time* — the class `L1a` exists to eliminate — and C1's `T2` makes
`Open` a recovery point, which `open_ro` + `query_only` makes impossible (ADR-7). If `L1a` lands
first, both are born in the right place. `L1a` is deliberately scoped to the small, self-contained
piece — the function split, the `Lock` path token, `add_locked`, the re-entrancy registry, the
`#[deprecated]` wrapper and the test-fixture helper — so the meta-epic serializes on the smallest
possible artifact. The peer-TTL policy change and the contract table move to `L1b`, off the critical
path. C3's plan independently reaches the same conclusion ("C2's lock-contract task lands on the
aggregator first, and V3 rebases onto it").

Mechanics: wire cross-epic beads dependency edges from C1's applied-cursor task and C3's `V3` to
`bd-21ef.2`'s `L1a`.

**2. `open_db` is deprecated in `L1a`, deleted in a later task** once C1 and C3 have rebased. This
is the concession that makes serialization survivable with three agents on one branch.

**3. ADR-7's read/recovery decision binds C1 and must be confirmed before `L1a` lands.**

**4. The WAL self-heal is a hard C1 dependency.** `open_db`'s unconditional `PRAGMA journal_mode=WAL`
currently repairs the `journal_mode=DELETE` mode that rebuild's rename leaves behind
(`rebuild.rs:379,425`). `open_ro` cannot. Either C1's swap sets WAL on the tmp DB before the rename,
or `open_or_init` becomes a mandatory post-swap step. C2's guardrails forbid C2 from fixing it.

**5. `A1` needs C1's fsync for full durability, and needs C1's `D3` transaction-ownership decision
before it can be written.** `A1` does **not** depend on `L1a` — `handle_audit_record` already
acquires the lock (`mcp.rs:1514`) — so that edge is dropped and `A1` runs in parallel.

**6. Lens 3 finding 4 is reassigned to C3 — CONFIRMED by the lead (2026-09-04).** This was a
deviation from the component mandate, made because C3's `S2` already claims the identical edits — "clamp `limit`, `inline_verify_k` and
`verify_pool_size` inside `search_entries` itself … correct the comment at `db.rs:2194`" — in the
same hunk (`db.rs:2194–2205`). Two agents implementing it concurrently on one aggregator produces a
conflict whose bad resolution silently keeps one agent's clamp and the other's pool calculation.
C3 also owns the third element of that finding (batching dynamically-sized `IN` queries) and the
whole surrounding code region. **No edit to C3's plan is needed** — it already has it. C2 carries
nothing in `search_entries`.

**7. `B1` is ordered before `L2`, `L3`, `A1`-Rust, `B3` and `S1`-Rust,** all of which edit
`mcp.rs` handlers that `B1` rewrites wholesale (47 of the 107 `open_db` sites are in that file).
They are not parallel siblings.

---

## Task flow

```
T0 elixir devShell ──────────────┬─► P1 port protocol ──────┐
                                 │                          │
T1 spec (PortProtocol.tla + waiver) ─────── (gates all) ─────┤
                                 │                          │
L1a open split + add_locked ──┬──► L1b TTL + contract table ─┤
 (foundational, lands first)  │                              │
                              └──► L2 mutations under lock ──┤
                              └──► L3 reembed exclusion ─────┤
                                                             ├─► docs ─► post-impl
B1 typed requests + Elixir rejection ──► (L2, L3, B3, S1) ────┤
B2 derived_from conditional ─────────────────────────────────┤
B3 repo-root derivation parity ──────────────────────────────┤
A1 audit-record atomicity (parallel; needs C1 tx decision) ───┤
S1 surface drift + threat model ─────────────────────────────┤
L1c delete deprecated open_db (after C1+C3 rebase) ──────────┘
```

`T0` before `P1` and `S1`. `T1` before every impl task. `L1a` before `L1b`, `L2`, `L3`. `B1` before
every other `mcp.rs` task. `L1c` last.

---

## Tasks

### T0 — add Elixir to the devShell (`chore`)

Add `beam27Packages.elixir` (1.18.4 on OTP 27 — `mcp/mix.exs` uses OTP 27's `:json`, a hard floor)
to `flake.nix`'s default devShell. Standing gap: no change under `mcp/` in three epics was
compile-checked. Pin the versioned attribute, not bare `elixir`, which tracks the default BEAM.

*Acceptance criteria*
- `nix develop -c mix --version` reports Elixir 1.18.x on OTP ≥ 27.
- `nix develop -c sh -c 'cd mcp && mix compile'` result on unmodified `master` is recorded verbatim
  in the task. If it fails, or emits warnings, **`T0` owns fixing them** — `P1` and `S1` require
  `--warnings-as-errors` and cannot inherit a dirty baseline.
- `nix develop -c sh -c 'cd mcp && mix test'` runs; the **test count** is recorded. `mcp/test/`
  currently holds one file — if the suite is effectively empty, `T0` records that fact explicitly so
  `P1` and `S1` are re-estimated as harness-build tasks rather than test-addition tasks.
- `mix format --check-formatted` baseline recorded (pass, or the diff).
- `.github/workflows/ci.yml` uses the devShell: the CI change is made and the closure-size delta is
  recorded. Not "unaffected or consistent" — the concrete change, or a stated reason there is none.
- `mcp/_build` stays git-ignored.

### T1 — spec: `PortProtocol.tla` + lock-contract waiver (`task`)

**Decision: model the port protocol; waive a spec for flock exclusion-per-se, with three surfaces
named and argued individually.**

*Model.* `.state/agent-kb/tla/PortProtocol.tla` — client, manager, port; actions `Send`, `Reply`,
`Progress`, `Timeout`, `PortCrash`, `Restart`, and a mailbox that may hold a reply whose request
already timed out. Invariants:
- `NoCrossTalk` — a client never receives a response whose id differs from its outstanding request
  id. This is the F2 bug; the model must produce the counterexample against the *current* design
  before the fix.
- `BoundedDeadline` — a request terminates within `D` **even under an unbounded stream of
  non-matching replies and progress ticks**. This invariant, not `NoLostCaller`, is what catches the
  ADR-3 rule-3 defect: `NoLostCaller` under weak fairness is satisfied by a design that resets its
  deadline forever, so a spec carrying only it would be TLC-green on the bug.
- `CrashIsPrompt` — after `PortCrash`, no client waits longer than one deadline period. (Meaningful
  only because ADR-3 rule 1 makes crash observable; the model states that as an assumption on the
  environment, and P1's conformance note maps it to `:exit_status`.)

*Waiver.* `.state/agent-kb/tla/decisions/lock-contract-no-spec.md`, following the per-task table
shape of `kb-write-traffic-akb-no-spec.md`. It must argue **three** surfaces separately, not one:

1. *Flock exclusion.* Genuinely waivable: a global exclusive `flock` excluding writers is already the
   assumption `AgentKb.tla` is written under. Modelling it restates an existing assumption.
2. *Re-entrancy and deadlock.* **Not** covered by a type token — the plan's own Scenario 1 is a
   liveness failure, and `&Lock` cannot express one. Covered instead by the `acquire_lock`
   re-entrancy registry, which converts the deadlock into an error at the point of the second
   acquire. The waiver must say this, and `L1a` must carry the registry's test.
3. *Two-phase peer TTL.* A genuinely new temporal surface: rows become observable in a
   logically-expired-but-physically-present state. The waiver must either argue it is trivially safe
   (the read filter is total, so no reader can observe an expired row) or a small model is added.
   **Recommendation: argue it**, on the grounds that the filter is applied at every peer read site
   and `L1b`'s test asserts exactly that — but the argument must appear, not be omitted.

*Acceptance criteria*
- `PortProtocol.tla` + `.cfg` committed; TLC green on the fixed design for all three invariants.
- The pre-fix `NoCrossTalk` counterexample is a **committed file** under `.state/agent-kb/tla/`
  (house precedent: `T0-counterexample.md`), not a note in the task.
- A TLC run matrix is recorded — config × constants × states explored × time — per the
  `T0-counterexample.md` standard. If the model does not close at useful constants, that is reported
  as a blocker rather than absorbed silently.
- The waiver record exists and argues all three surfaces above individually.
- **code-reviewer and analyst audit pass. The task closes only with all of these** (repo TLA+ gate;
  matching C3's `T0`).
- Every impl task in this epic has a beads dependency edge to `T1`.

### L1a — open split, `add_locked`, re-entrancy registry (`task`) — *foundational, lands first*

The critical-path half of ADR-1. Deliberately excludes the TTL policy and the contract table.

*Acceptance criteria*
- `db::open_ro`, `db::open_rw(&Paths, &Lock)`, `db::open_scratch`, `db::open_or_init(&Paths) -> Result<()>`
  exist with the signatures in ADR-1. `open_or_init` returns no connection.
- `open_db` remains as a `#[deprecated]` wrapper (`open_or_init` + `open_ro`) so C1 and C3 can
  rebase incrementally; `L1c` deletes it.
- `Lock` carries its canonicalized path; `open_rw` errors when the token's path is not `paths.lock`.
  A test asserts that.
- `open_scratch` errors when handed `paths.db`. A test asserts that.
- `open_ro` sets `PRAGMA query_only=ON`; a test asserts an `INSERT` on an `open_ro` connection fails.
- **A test crashes a writer leaving a hot WAL, then asserts an `open_ro` connection recovers and
  reads the committed data.** ADR-1's Option D rejection rests on this premise and ~42 non-test call
  sites depend on it.
- `open_ro` returns a distinct `DbUninitialized` on a missing DB or missing `entries` table.
  `kb search`, `kb context`, `kb cited-by`, `handle_kb_get` and `handle_provenance` map it to an
  **empty result plus a one-line stderr note** — tests assert first-run UX is unchanged for all five.
  This is a user-visible change: it is called out in the docs task and surfaced to the user at
  post-impl.
- `kb_core::add_locked(&Lock, &Connection, ..)` exists with the logic; `kb_core::add` is a thin
  acquiring wrapper. A test asserts a caller holding the lock can call `add_locked` without blocking.
- `acquire_lock` carries a process-local re-entrancy registry keyed on the canonicalized path; a
  second in-process acquire returns an error naming the first acquisition, and a test asserts it
  errors rather than hanging (with a 10 s test timeout so a regression fails CI, not production).
- A `test_db(root) -> (Paths, Connection)` fixture helper exists; test call sites migrate to it.
- Full-tier landing: `cargo nextest` full tier green with `tee` + `PIPESTATUS` evidence.
- **Option B checkpoint (lead ruling):** `L1a`'s post-impl review is the decision point for ADR-1's
  recorded fallback. The reviewer explicitly assesses whether the 107-site diff is a red flag; if it
  is, the component drops to Option B (one non-mutating `open_db`, `query_only`, the re-entrancy
  registry) **before** `L1c` deletes anything. `L1a` therefore deprecates rather than deletes so the
  fallback stays reachable.

### L1b — peer TTL policy and the lock-discipline contract table (`task`) — *after L1a*

*Acceptance criteria*
- `sweep_expired_peers` is no longer called from any open path; it runs under the lock from
  `compact`, `rebuild`, and the peer-mutating commands.
- Every peer read filters `expires_at`; a test asserts an expired peer is invisible to
  `kb peers list`, `kb peers show`, `peers edge-list` and graph traversal **with no delete having
  occurred**. A second test asserts the filter also applies to federated peer search
  (`search.rs:127–160`) — the boundary with C3's `S4` is that C2 owns the filter, C3 owns ranking.
- A committed contract table under `docs/` with **one row per file** for the 22 files containing
  `open_db` call sites (enumerated in the task, not left as "every entry point"), each row stating:
  read-only / write-under-lock / init / scratch, and the test or code reference backing it.
- The table carries four explicit rows beyond the call-site inventory:
  - **`query_hits` telemetry exemption** — `search.rs:233`, `context.rs:104`, `mcp.rs:325,354` write
    a separate database (`config.rs:249`) from read paths. Out of scope, stated with the reason.
  - **`check_embed_mode_vintage`** (`db.rs:632`) — writes `kb_meta`; moved under the lock by `L3`.
  - **rebuild's schema stamp** (`rebuild.rs:146–149`) — under the schema-upgrade lock, not
    `paths.lock`; no valid `&Lock` exists for it, so it uses `open_scratch`-class access. Documented.
  - **The swap precondition (lens 4 finding 13)** — the invariant stated in the form C1 can cite:
    *no connection is open for write against the old inode at the point of rename*. `rebuild.rs`'s
    WAL-deletion comment is rewritten to cite this invariant instead of "per-request connections",
    which the review falsified. This closes finding 13.
  - **Standing constraint:** `query_only` blocks temp-database writes, so no read path may use
    `CREATE TEMP TABLE`.

### L2 — every mutation under the lock (`task`) — *after L1a, after B1*

*Acceptance criteria*
- All six `peers.rs` mutating commands and both `handle_kb_peers_*` handlers acquire `paths.lock`
  before `open_rw`. That is **ten concrete sites** (the inventory table's eleventh row is the
  `open_db` sweep, which `L1b` removes) plus `handle_import` below.
- `handle_import`'s existence check and the add happen under one lock hold, using
  `kb_core::add_locked` — this is why `add_locked` is an `L1a` deliverable. A two-writer integration
  test covers the duplicate-skip race.
- The peer-DB open in federated search (`search.rs:133`) uses `open_ro` and no longer runs DDL or a
  sweep against another repository's database. A test asserts a peer DB is not modified by a search.
- CI gate: a grep check asserting `Connection::open` appears only inside `db.rs`, so the one escape
  hatch from the split is a reviewed exception rather than an open door. (r1's "the test documents
  intent" criterion was self-defeating and is replaced.)

### L3 — reembed exclusion and honest failure counting (`task`) — *after L1a, after B1*

Implements ADR-2 and lens 4 finding 6, in both `reembed.rs` and `handle_reembed`.

*Acceptance criteria*
- Selection unlocked; embedding outside the lock; writes in locked batches sized to a **≤ 50 ms
  lock-hold budget**, as a named constant carrying that comment, with the hold time measured once
  and recorded.
- Each batch re-resolves `entries.id` → rowid against the live DB, re-checks
  `rowid NOT IN (SELECT rowid FROM entries_emb)`, and writes `INSERT OR IGNORE` — never
  `OR REPLACE`. A test asserts a concurrent `kb_core::add` that wrote a fresh embedding between
  selection and the batch is **not** clobbered.
- `handle_reembed` propagates the `conn.execute` result: `done` increments only on `Ok`; the
  response reports `failed` with a cause. The `let _ = conn.execute(...)` at `mcp.rs:1126` is gone.
- **CLI behaviour is decided explicitly:** `reembed.rs:105–109` currently propagates with `?`,
  aborting the run on the first write failure and discarding accumulated counts. It changes to match
  the MCP path — count and continue, report `failed` with causes, non-zero exit if `failed > 0` — so
  the two agree. A test asserts CLI and MCP counts match on the same fixture.
- A test simulates a DB swap between two batches and asserts every embedding written by the run is
  present in the *live* DB afterwards.
- `check_embed_mode_vintage` (`db.rs:632`) is called under the lock, on a write connection.

### P1 — port protocol: correlation, crash detection, bounded deadline (`task`) — *after T0, T1*

Implements ADR-3.

*Acceptance criteria*
- `init/1` calls `Process.flag(:trap_exit, true)` and `Port.open` includes `:exit_status`. The task
  states what happens to the `rebuild_async` `System.cmd` child on manager shutdown, and whether the
  now-reachable `handle_info` clauses at `:101,106` are retained or folded into the new handling.
- **The crash test kills the OS process** (`kill` on the port's `os_pid`) — explicitly **not**
  `Port.close/1`, which delivers `{Port, :closed}` and would pass against the unfixed code. The test
  must fail on pre-fix code. The caller receives `port_closed` in well under the deadline.
- `collect_response` returns only on an id match; non-matching finals are logged at warn with both
  ids and counted. Progress ticks reset nothing unless their id matches.
- A test drives a stale reply into the mailbox before a fresh request and asserts the fresh request
  receives its own reply. This is the F2 regression test and must fail on pre-fix code.
- The inner deadline is absolute (monotonic, recomputed per iteration); a test asserts a request
  under a continuous stream of non-matching replies still terminates within the deadline.
- `GenServer.call` receives `:infinity`; the caller's `timeout` argument is the inner deadline. A
  test asserts a request that trips the inner deadline returns the `timeout` **envelope**, not a
  `GenServer` exit.
- `call_port` converts `exit(:noproc)` from a restarting manager into a `port_unavailable` envelope;
  a test asserts a caller queued behind a crashing request gets an envelope, not a raised exit.
- The timeout envelope carries `discarded_responses: N`, so a timeout that discarded replies is
  visibly different from one that received nothing.
- `gen_id` uniqueness is asserted.
- `await_ready` handles close/exit/exit_status immediately; Rust prints a final
  `{"type":"error", ...}` stdout line before exiting on fatal startup failure. A test with an
  unopenable DB asserts the real cause reaches the caller, not `handshake_timeout`.
- Conformance with `PortProtocol.tla` is stated: each spec action names its implementing clause, and
  `CrashIsPrompt`'s environment assumption maps to `:exit_status`.
- `mix compile --warnings-as-errors` and `mix test` green.

### B1 — reject at both layers: Elixir arg validation + typed Rust structs (`task`) — *before L2/L3/B3/S1*

Implements ADR-4.

*Acceptance criteria*
- **Elixir:** tool schemas set `additionalProperties: false` and dispatch rejects unknown `args`
  keys with the key named, before building the port request. A test asserts an unknown argument is
  **rejected**, not silently dropped by `put_if_present`.
- **Pre-landing, blocking:** the request fields actually sent by the *deployed* machines_conf
  `kb-protocol.md` pin are enumerated (not inferred from merged code) and asserted accepted. Per the
  standing cross-repo rule (`conventions/cross-repo/evidence-contract-notification`), machines_conf
  is notified before landing. This is a criterion, not a note — it is Scenario 2's mitigation.
- **Rust:** one `#[derive(Deserialize)] #[serde(deny_unknown_fields)]` struct per dispatch method,
  each declaring `id` and `method` (the request object is flat). Handlers consume the struct.
- Required fields are non-`Option`: a missing or wrong-typed `summary`/`content` is a rejection
  naming the field, never an empty string.
- Wrong-typed or out-of-range `limit`, `inline_verify_k`, `max_chars`, `max_hops` are rejected with
  the field name and the accepted range.
- `expand_ids` enforces `maxItems: 32`; a mixed-type array member is a rejection, not a filtered
  element. A `proptest` over generated malformed JSON asserts rejection and that no request ever
  produces a silently shortened array.
- Input-line size cap of 10 MiB (matching the Elixir `{:line, ...}` cap), implemented with
  `BufRead::read_until` or `Take` — **not** by measuring the `String` yielded by
  `stdin.lock().lines()` (`mcp.rs:125`), which has already allocated the bytes the cap exists to
  prevent. An over-long line yields `line_too_long` and the reader discards to the next newline. A
  test feeds an oversized line followed by a valid request and asserts the valid one is answered.
- A `query` field cap of **8 KiB** (a named constant) prevents an unbounded string reaching the
  embedder/FTS.
- The parse-error envelope carries a best-effort `id` from a shallow scan of the raw line; `null`
  only when none is recoverable.
- Nothing in `search_entries` is touched — those clamps belong to C3 (Cross-component ordering §6).

### B2 — `derived_from` required when `kind = "derived"` (`task`)

*Acceptance criteria*
- `add_validation.rs` rejects a `kind="derived"` evidence row whose `derived_from` is missing, null,
  non-string, empty, or over a documented length bound, in addition to the self-reference check.
- The Elixir schema expresses the condition with `if`/`then` (or `oneOf` variants).
- Tests cover missing, null, wrong-typed, empty, self-referential, and valid.
- The `kb_add` tool description states the requirement.

### B3 — repository-root derivation parity (`task`) — *after B1*

Fixes lens 4 finding 7.

*Acceptance criteria*
- **The legacy-layout precedence is decided in this task and recorded as a short decision note**, not
  left to the executor: it determines which repository the deployed fleet resolves path-only evidence
  against. Recommendation: `root_from_db`'s layout-aware derivation wins; `db_discovery.ex` is
  aligned to it.
- One layout-aware DB→root derivation is used by both `kb_cite` and `kb_core::add`; the three-parent
  walk (`kb_core.rs:145–149`) is gone.
- `Paths` gains a `root` field (`config.rs:181–194` has none) and the root is **passed into**
  `add_locked` rather than recomputed. Sequenced with `L1a` since both touch `Paths`.
- `Paths::discover` and `db_discovery.ex` agree on legacy-candidate precedence; a test asserts the
  same DB path yields the same root on both sides, and `db_discovery.ex`'s parity claim is made true.
- A legacy-layout fixture test asserts path-only evidence hashes a file from the intended repository.

### A1 — audit-record atomicity (`task`) — *parallel; needs the C1 transaction decision*

Implements ADR-5. **No dependency on `L1a`** — `handle_audit_record` already takes the lock.

*Acceptance criteria*
- The C1 `D3` transaction-ownership question is settled and recorded before implementation.
- Per verdict, `apply_event` + `INSERT OR IGNORE INTO audit_runs` + the `source_weights` upsert run
  inside one transaction via `conn.unchecked_transaction()`; a failure rolls all three back.
- A fault-injection test kills the operation between the row insert and the weight update and
  asserts that after retry both are present or neither is.
- Replaying the same `audit_record` request twice leaves weights unchanged.
- The residual window between the log append and the transaction is documented in code and named in
  the task closure, citing C1's fsync task. `A1` does not claim crash-atomicity across the append.
- **Q3 decision checkpoint (lead ruling):** count the `audit_runs` rows whose `source_weights` delta
  never applied — rows already split by a past crash — and record the deferral *with that count*.
  Deferring is the ruled outcome; deferring silently is not. If the count is large enough to distort
  the ranking signal, that is surfaced at post-impl rather than discovered later.

### S1 — surface drift, response-shape consistency, threat model (`task`) — *after T0, after B1*

Implements ADR-6.

*Acceptance criteria*
- `@tools` gains `kb_audit_run`, `kb_audit_record`, `kb_audit_report`, `kb_provenance` with schemas,
  dispatch clauses and renderers; each is exercised by a test.
- **`/kb-review` is exercised end-to-end** against the new tools — the workflow that justifies
  exposing them.
- A `/threat-model` pass covers `kb_audit_record` putting `source_weights` on the agent surface,
  cached per the standard path; ciso-risk-advisor writes the KB entry.
- `kb_peers_*` stay implemented and unexposed, documented in `mcp/README.md` with ADR-6's rationale
  and the revisit condition.
- The `kb_add` renderer prints `similar_existing` with id, path, summary and score; a test asserts a
  near-duplicate add surfaces them.
- **Method-wide shape consistency:** a test asserts, for every dispatched method, that each response
  shape the Rust handler can emit is matched by a renderer clause. `handle_reembed`'s three shapes
  (`mcp.rs:1061,1108,1136`) are the motivating case; `kb_add` alone is not sufficient.
- `mix compile --warnings-as-errors` and `mix test` green.

### L1c — delete the deprecated `open_db` (`task`) — *last, after C1 and C3 rebase*

*Acceptance criteria*
- `open_db` is removed; no call site remains in `src/`.
- C1's and C3's landed work on the aggregator compiles without it — verified on the aggregator
  branch, not on the component branch.

### Docs task (`docs`)

*Acceptance criteria*
- The lock-discipline contract table (`L1b`) is published under `docs/` and referenced from the KB.
- The first-run behaviour change (`open_ro` → empty result plus stderr note, from `L1a`) is
  documented for the five affected read commands.
- The MCP request-field reference per method (`B1`), including the new Elixir rejection of unknown
  arguments and the field enumeration sent to machines_conf.
- Port protocol semantics (`P1`), cross-referencing `PortProtocol.tla` and naming the
  `:exit_status`/`trap_exit` requirement.
- `mcp/README.md` note on internal port methods (`S1`).
- The `mcp` devShell entry and the CI change (`T0`).
- Blocked by the post-impl task.

### post-impl task (`task`)

Blocks the docs task. Closed by `/post-impl`. Carries the user sign-off for the two user-visible
changes: `L1a`'s first-run behaviour and `B1`'s Elixir argument rejection.

Also confirms at this gate: lens 4 finding 13's swap-precondition invariant is in the contract table
in the form C1 cites; the `T1` waiver carries code-reviewer + analyst sign-off; `A1` recorded the
Q3 deferral *with its row count*; and the `L1a` Option B checkpoint was actually exercised rather
than assumed (it happens at `L1a`'s own review, and this gate verifies it did).

---

## Pre-mortem (DELIBERATE mode)

**Scenario 1 — the read/write split deadlocks the MCP port.** A handler holding the lock calls a
helper that acquires it again; `flock` on the same file via a second file descriptor blocks forever
and `acquire_lock` has no timeout, so the port hangs. `P1`'s new deadline then reports `port_closed`
on a process that is merely stuck, and every subsequent request queues behind the wedged GenServer.
*Mitigations, in `L1a`'s acceptance criteria:* the `acquire_lock` re-entrancy registry, which turns
the second acquire into an error naming the first; `kb_core::add_locked`, which removes the one
call shape that forces re-acquisition; and a 10 s integration-test timeout so a regression fails CI.
r1 claimed the borrow checker would catch this — it cannot, and that claim is withdrawn.

**Scenario 2 — `B1` breaks the deployed fleet on landing day.** The Elixir layer starts rejecting
unknown arguments, and a deployed agent or hook on a stale `kb-protocol.md` pin sends one. KB writes
fail fleet-wide. *Mitigations, in `B1`'s acceptance criteria as blocking items:* enumerate the
fields the deployed pin actually sends and assert they are accepted, before landing; notify
machines_conf per the standing cross-repo rule. r1 proposed "one release of overlap where an unknown
field is logged and rejected" — that is not an overlap, it is the breaking change with better error
text, and it is withdrawn. If the enumeration turns up fields the schemas do not know, the fallback
is a real two-phase rollout: accept-and-warn with a counter, then reject one release later.

**Scenario 3 — the port fix hides a real hang behind a discard.** `collect_response` starts
discarding non-matching responses. If Rust echoes a wrong id, or a duplicate id is issued, the
manager discards forever and the caller times out with no sign that replies *were* arriving — the
symptom becomes indistinguishable from a dead port. *Mitigations, in `P1`'s criteria:* the discard
logs both ids and increments a counter surfaced as `discarded_responses` in the timeout envelope;
`gen_id` uniqueness is asserted; and `T1`'s `BoundedDeadline` invariant makes the unbounded-discard
design a TLC failure rather than a green spec.

**Scenario 4 — `mcp/` does not compile, and `T0` discovers it too late.** The Elixir half has never
been built. `mix compile` fails on master, or emits warnings no task owns, making `P1`'s and `S1`'s
`--warnings-as-errors` criteria unachievable; or `mix test` runs near-zero tests, making the eleven
"a test asserts…" criteria in `P1` and `S1` a from-scratch harness build with no budget.
*Mitigations, in `T0`'s criteria:* `T0` **owns** fixing whatever the baseline compile turns up, and
records the test count explicitly so `P1`/`S1` are re-estimated before they start rather than
discovered mid-task. If the repair is large, `T0` splits and the lead is told before `P1` begins.

**Scenario 5 — `L1a` lands and detonates two in-flight worktrees.** C1 and C3 are being written
concurrently against `open_db` right now; C3's `V3` edits `kb_core::add` (4 `open_db` references)
and C1's applied-cursor work edits the open path by definition. A hard removal of a function with
107 call sites mid-flight costs both siblings a day and risks a bad conflict resolution on the
aggregator. *Mitigations:* `L1a` leaves `open_db` as a `#[deprecated]` wrapper and `L1c` deletes it
only after both have rebased; `L1a` is scoped down so the serialization window is as short as
possible; the test-fixture helper absorbs roughly half the churn in one substitution.

## Test plan (DELIBERATE mode)

- **Unit:** `open_rw` lock-path mismatch; `open_scratch` refusing `paths.db`; `open_ro` write
  rejection under `query_only`; `DbUninitialized` on a missing `entries` table; re-entrancy registry
  erroring rather than blocking; `derived_from` across six input shapes; best-effort id recovery from
  an unparseable line.
- **Property (`proptest`):** generated malformed request JSON is always rejected with a named field,
  never coerced, never silently shortened; reembed batch re-resolution over generated interleavings
  of entry insert / upsert-with-embedding / expire / swap.
- **Integration:** hot-WAL recovery through an `open_ro` connection after a writer crash; two-writer
  flock contention on the peers commands and the import duplicate-skip race; reembed across a
  simulated rebuild swap and against a concurrent fresh-embedding write; audit-record fault
  injection between row insert and weight update; every locked MCP handler end-to-end under a 10 s
  timeout (Scenario 1 guard).
- **Elixir (`mix test`, requires `T0`):** stale reply in the mailbox before a fresh request; OS-level
  kill of the port mid-request (not `Port.close/1`); continuous non-matching replies still hitting
  the deadline; inner-deadline trip returning an envelope; a queued caller behind a crashing request
  getting `port_unavailable`; unopenable DB at startup surfacing the real cause; unknown argument
  rejected rather than dropped; `kb_add` renderer emitting `similar_existing`; each newly exposed
  audit/provenance tool round-tripping; `/kb-review` end-to-end.
- **E2E / CLI:** first-run `kb search` / `context` / `cited-by` on an uninitialized repo returning
  empty plus a stderr note; `kb peers add` blocked while another writer holds the lock; an over-long
  input line followed by a valid request on the raw port protocol; CLI and MCP reembed counts
  agreeing.
- **Formal:** TLC green on `PortProtocol.tla` for `NoCrossTalk`, `BoundedDeadline`, `CrashIsPrompt`,
  with the pre-fix `NoCrossTalk` counterexample committed and a run matrix recorded.
- **Observability:** `discarded_responses` in the timeout envelope; reembed `failed` counts carrying
  causes; rejected requests naming the offending field; reembed lock-hold time measured against the
  50 ms budget.

## Success criteria

1. Lens 4 findings 2–14 are each closed or explicitly deferred with recorded rationale — **including
   finding 13**, whose swap-precondition invariant is written into the contract table in the form C1
   cites. Lens 3 finding 4 is closed by C3 (Cross-component ordering §6), not by C2.
2. Every entry point in the committed contract table matches its landed obligation, with a test or
   code reference per row, and the four exemption/constraint rows are present.
3. No mutation of stored *content* occurs on any path the caller believes is a read. The
   `query_hits` telemetry database is the one stated exemption; WAL/`-shm` recovery state is not
   content.
4. `PortProtocol.tla` is TLC-green on all three invariants and `P1` is conformant; the lock-contract
   waiver is recorded, argues all three surfaces, and carries code-reviewer + analyst sign-off.
5. `nix develop -c sh -c 'cd mcp && mix compile --warnings-as-errors && mix test'` is green, and
   every `mcp/` change in this epic was compile-checked — the three-epic standing gap is closed.
6. No MCP request field is silently coerced or silently dropped, at either layer.
7. C1 and C3 have rebased onto `L1a` and `L1c` has deleted `open_db`.
8. Zero Critical findings at code review; post-impl gates pass; the user confirms merge, including
   the two user-visible changes.

## Open questions

Tracked in `.state/.omc/plans/open-questions.md`.
