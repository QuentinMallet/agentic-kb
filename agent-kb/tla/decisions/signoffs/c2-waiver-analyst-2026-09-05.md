# Analyst sign-off — C2 lock-contract waiver (bd-21ef.2.14 / T1 criterion)

Scope: the second sign-off box of
`/home/urist/Documents/perso/agentic-kb/.state/agent-kb/tla/decisions/lock-contract-no-spec.md:78`
— "analyst confirms no uncovered state machine was introduced on these three surfaces."

Read-only audit against the aggregator worktree
`/home/urist/Documents/perso/agentic-kb/.state/program-worktrees/storage-correctness-2` at `542d49d`.
No cargo, nextest, mix or TLC was run. TLC results for `PortProtocol` are taken from the recorded
run matrix in `.state/agent-kb/tla/PortProtocol-counterexample.md`.

---

## 1. PortProtocol.tla still corresponds to the landed Elixir port — PASS

`mcp/lib/agentic_kb_mcp/port_manager.ex` landed at `91d5bbb`
("feat(mcp): implement ADR-3 port protocol correlation and crash detection"); the file is present in
`HEAD` and the working tree is clean. Each `Next` disjunct of the `FixedDesign = TRUE` model maps to
a live clause:

| Spec action | Landed clause | Correspondence |
|---|---|---|
| `Send(id)` | `handle_call({:request, request, timeout}, ...)` | Builds the line, `Port.command/2`, then `deadline_ms = System.monotonic_time(:millisecond) + timeout`. The id is captured once and passed to `collect_response/4` as the correlation key. |
| `ConsumeMatching` | `{:ok, %{"id" => ^id} = response}` | Pin-matches the caller's own id; this is the only clause that returns a final answer. |
| `ConsumeReply` discard branch (`elapsed' = elapsed + 1`, guarded `elapsed < D`) | `{:ok, %{"id" => other_id}}` | Logs at `warning` with both ids, recurses with `discarded + 1` and the **same** `deadline_ms`. Budget is consumed, never reset — the model's discard charge. |
| `Progress` (guarded `elapsed < D` under `FixedDesign`) | `{:ok, %{"type" => "progress"} = prog}` | Recurses with the same `deadline_ms`; `remaining` is recomputed at the top of every recursion, so a tick consumes budget. |
| `Timeout` (guarded `elapsed >= D`) | `after remaining ->` | `remaining = max(deadline_ms - now, 0)`, so the `after` fires at the absolute deadline, not on a per-receive window. Envelope carries `discarded_responses`. |
| `PortCrash` under the observability assumption | `{^port, {:exit_status, status}}` in `collect_response/4` | `init/1` sets `Process.flag(:trap_exit, true)` and `Port.open` passes `:exit_status`. `handle_call` then returns `{:stop, :port_closed, response, state}` — replies before terminating. The `handle_info` `{port, :closed}` / `{:EXIT, port, reason}` clauses are the `trap_exit` complement for the idle case. |
| `Restart` | deliberately absent | Matches the model comment: restart is the OTP supervisor's, not this GenServer's, and rule 7 constrains what may *trigger* a restart, not whether a later crash may be restarted. |

`CrashIsPrompt`'s environment assumption (crash is observable) is discharged by the two `init/1`
flags, exactly as the module comment claims. The model's stated out-of-scope items (ADR-3 rules 4-6)
remain out of scope for the right reason and are each covered by a landed test:
`mcp/test/port_manager_test.exs` has "a request that trips the inner deadline returns the timeout
envelope, not a raised exit" (rule 4, `GenServer.call(..., :infinity)` plus the `catch :exit` in
`call_port/3`), "a caller queued behind a crashing request gets a port_unavailable envelope"
(rule 5), and two `await_ready/1` startup tests (rule 6). CE1, CE2 and CE3 each have a named
regression test with a "MUST FAIL pre-fix" comment.

No landed commit touches `PortProtocol.tla` after T1 (`git log -- .state/agent-kb/tla/PortProtocol.tla`
returns nothing since the recording), and no commit touches `port_manager.ex` after `91d5bbb`.

## 2. Residual model/implementation asymmetry on malformed lines — CONCERN (minor, no remedy required for this box)

`collect_response/4` has two clauses with no counterpart in the model: `{:ok, _response}` (a decoded
object with no `"id"` key) and `{:error, _}` (undecodable line). Both **terminate the call** with a
`parse_error` envelope stamped with the caller's own id. A well-formed reply carrying a *different*
id is discarded and the wait continues; a malformed line is not. Under the model's alphabet
(`mailbox \in Seq(RequestIds)`) this case cannot arise, so the spec neither permits nor forbids it.

Assessment: this is not a `NoCrossTalk` violation — the envelope is attributed to the caller's own
request, so no reply is ever delivered as the answer to a different one. It is a liveness
difference only: a stray malformed line belonging to another request's stream ends the current call
early instead of being charged to the deadline like a stale reply. No test covers either clause.
This does not introduce an *uncovered state machine* on any of the three waived surfaces (it is on
the port surface, which is modelled), so it does not affect the box. Worth a one-line note in the
model's out-of-scope block if the module is next touched.

## 3. Surface 1, global flock writer exclusion — waiver argument still correct — PASS

The waiver's argument is that a second module would restate `AgentKb.tla`'s existing environmental
assumption rather than test a new transition system. That still holds, and the implementation
obligation the row attaches to it is met and strengthened:

- `db::open_rw` (`src/components/db.rs:523`) canonicalizes `paths.lock` and rejects a guard whose
  canonical path differs — holding *a* lock is not enough, as the row requires.
- `cursor::append_and_apply` / `append_and_apply_with` (`src/components/cursor.rs:717,732`) take
  `&Lock` and call `verify_lock` rather than acquiring, so C1's cursor writer adds no new acquire.
- `tests/open_db_ratchet.rs` pins the legacy opener at zero call sites, and `Connection::open`
  appears nowhere in `src/` outside `src/components/db.rs` (the single hit at
  `src/commands/rebuild.rs:948` is inside a comment).

There is a second lock file — `paths.lock.with_extension("schema-upgrade.lock")` acquired at
`src/commands/rebuild.rs:197` before `paths.lock` at `:201` — i.e. a nested acquire and therefore a
lock-ordering discipline. It is not a post-T1 addition: `git log -S "schema-upgrade.lock"` dates it
to `e0fa9f2` (2026-07-06), well before the waiver, and the contract table carries a dedicated
"rebuild schema stamp" row for it. It is the only site that takes both, always outer-then-inner, so
no cycle exists and deadlock-freedom follows by inspection rather than by model. The registry keys
on `(thread_id, canonical_path)` so the two files never collide. The waiver's withdrawal condition
("any nested-acquire path outside the registry") is not tripped: both acquires go through
`acquire_lock`.

## 4. Surface 2, process-local re-entrancy — obligation met, not waived — PASS

`acquire_lock` (`src/commands/add.rs:250-296`) is `#[track_caller]`-style instrumented (it records
`std::panic::Location::caller()` as the first-acquire site), canonicalizes **after** creating the
file so a first-ever acquire resolves, keys the registry on `(thread::current().id(), canonical)`,
and `anyhow::bail!`s naming the first acquire site instead of blocking. The entry is removed if
`lock_exclusive()` itself fails, so a failed acquire does not poison the key.

Both tests the waiver demands exist in `tests/open_split.rs`:
`registry_canonicalizes_two_spellings_of_one_lock_file` (`:530`) constructs
`<root>/.state/agent-kb/../.lock`, asserts the two spellings differ textually, asserts both name one
file, and asserts the aliased second acquire is rejected as re-entrant; and
`open_scratch_refuses_the_live_db_through_an_equivalent_spelling` (`:370`) covers the sibling
aliasing hazard on the scratch opener. This is the exact "different spelling of the same file" case
the waiver's re-entrancy row called out.

## 5. Surface 3, two-phase peer TTL — six-site inventory matches the landed code exactly — PASS

The read filter is a single shared helper, `db::live_peer_predicate(alias)`
(`src/components/db.rs:206`), emitting `(<alias>.expires_at IS NULL OR <alias>.expires_at >=
datetime('now'))`. Its eight call sites cover the five consumer-visible surfaces and nothing else:

| Waiver site | Landed filter site |
|---|---|
| 1 `kb peers list` | `src/commands/peers.rs:308` (`query_peers_for_repo`) |
| 2 `kb peers show` | `src/commands/peers.rs:350` (`query_peers_by_either_repo`, still called twice from `PeersShow::execute` for the as-is and canonicalized spellings, deduplicated by id) |
| 3 `kb peers edge-list` | `src/commands/peers.rs:787` |
| 4 federated traversal | `src/commands/search.rs:391,399,447,455` (all four bind variants) |
| 5 MCP `kb_peers_list` | `src/commands/mcp.rs:2542` |
| 6 import duplicate-suppression | `src/commands/peers.rs:453` — **unfiltered**, carrying the comment "Waiver site 6: duplicate suppression must see expired-but-present rows too because the peers table has no UNIQUE constraint." |

I enumerated every `FROM peers` / `JOIN peers` occurrence in `src/`. Outside test modules the only
non-`DELETE` reads are the seven above; there is no seventh consumer-visible read path and no
unfiltered one. The waiver's withdrawal condition ("if implementation introduces a peer read path
without the predicate") is not tripped.

Tests exist for every row of the L1b list:
`test_peers_read_helpers_filter_expired_rows_without_deleting_them` (`peers.rs:991`),
`test_peers_import_skips_expired_but_present_duplicate` (`peers.rs:1048`, the directed negative
test for site 6), `test_collect_peer_paths_filters_expired_rows_without_deleting_them` and
`test_federated_global_limit_ignores_physically_present_expired_peer` (`search.rs:798,835`),
`test_handle_kb_peers_list_filters_expired_rows_without_deleting_them` (`mcp.rs:2853`), and
`open_or_init_does_not_sweep_expired_peers` (`tests/open_split.rs:273`).

## 6. Peer-sweep-only-in-locked-writers rule — holds — PASS

Every production `db::sweep_expired_peers` call sits behind an `open_rw` opened from a live guard:

| Call site | Lock evidence |
|---|---|
| `peers.rs:161,234,524,655,727,766` (via `maybe_sweep_expired_peers`, which fires only when `did_mutate`) | each enclosing command does `acquire_lock(&paths.lock)` then `db::open_rw(&paths, &lock)` |
| `mcp.rs:2522` (`handle_kb_peers_add`) | `acquire_lock` at `:2452`, `open_rw` at `:2458` |
| `mcp.rs:2605` (`handle_kb_peers_remove`) | `acquire_lock` / `open_rw` immediately above at `:2585-2592` |
| `compact.rs:126` | inside `Compact::run`'s locked section (`acquire_lock` at `:84`) |
| `rebuild.rs:421` | inside Phase 1's `{ let lock = acquire_lock(...); let conn = db::open_rw(paths, &lock)?; ... }` block |

No open path sweeps: `open_or_init` and `open_ro` contain no sweep, and the ratchet test pins that.
The waiver's "physical deletion is owned by the locked sweep path" premise is intact.

## 7. Consistency with C1's models where they overlap — PASS

**`RebuildProtocol.tla`.** Its only concurrent-writer action, `WriterAppend`, is guarded by
`phase = "p2"`; there is no action in `Next` that mutates the database during Phase 3
(`Phase3CatchUp`, `Checkpoint`, `VerifyAndClose`, `SetTmpWalMode`, `FirstNameOperation`,
`SecondNameOperation`, `DirSync`, `Finish`, `Kill`, `Reopen`). The model therefore *assumes* writer
exclusion across catch-up and swap and proves nothing about it. The contract table's swap-precondition
row is exactly the discharge of that assumption, which is why finding 13 asks for it in citable
form. The landed code satisfies the assumption: Phase 1's lock is scoped inside its own block and
released before Phase 3 re-acquires (`rebuild.rs:416` vs `:504`), so there is no self-deadlock, and
every mutating opener funnels through `open_rw`'s path-matching guard.

**`DurableBatch.tla`.** Its row-9 disposition ("serve reads, refuse writes" when the log is
unreadable) is a read/write asymmetry, not a lock protocol; it neither assumes nor constrains the
flock beyond what `AgentKb.tla` already assumes. The landed cursor writer takes `&Lock` and calls
`verify_lock`, so C1's writer introduces no acquire of its own and cannot conflict with the
re-entrancy registry.

## 8. No landed change after T1 reopened a waived surface — PASS

Checked, per component:

- **L2 (every mutation under the lock)** — moved writers behind ADR-1 locks (`46034ae`). It closes
  the flock surface further; it opens nothing. All ten peer mutation sites plus the MCP handlers now
  acquire before `open_rw`.
- **L1c (delete `open_db`)** — `eedf497` removed the shim; the ratchet is pinned at zero. No new
  opener class, no new lock protocol.
- **L3 (reembed exclusion)** — this is the one genuinely new interleaving on the flock surface:
  selection unlocked, embedding outside the lock, and writes in *repeated* short locked batches, so
  the lock is handed off between batches and another writer may interleave. It is not an *uncovered*
  state machine: the plan assigned it to test rather than model, and the tests landed —
  `test_reembed_does_not_clobber_fresh_embedding_added_after_selection` (`reembed.rs:480`),
  `test_reembed_skips_when_content_changed_after_selection` (`:521`),
  `test_reembed_database_swap_between_batches_reconciles_live_db` (`:569`),
  `test_reembed_swap_drops_ids_that_no_longer_resolve_in_the_replacement_db` (`:607`),
  `test_reembed_raced_count_from_pass_one_is_discarded_after_a_swap_reconcile` (`:655`),
  `test_reembed_batch_lock_hold_budget` (`:826`), plus a proptest over batch partitioning. The write
  itself re-resolves against the live DB and uses `INSERT OR IGNORE ... AND e.rowid NOT IN (SELECT
  rowid FROM entries_emb)` (`reembed.rs:388-391`) — never `OR REPLACE`.
- **B3 (root parity)** — touches repository identity (`detect_source_repo` at `peers.rs:47` returns
  `paths.root`). It changes which string a peer edge is keyed by, not the TTL predicate or the lock
  discipline. `PeersShow::execute`'s as-is/canonical double lookup with id-deduplication survives
  unchanged, which is the reconciliation the waiver's re-entrancy row cross-referenced.
- **S1 (audit/provenance controls)** — new MCP handlers; the mutating ones go through
  `acquire_lock` + `open_rw`, the read ones through `open_ro`. No new peer read site.
- **V4b (symlink policy)** — confined to citation verification and `safe_join`; no peer, lock or
  port surface.
- **C1's cursor writer** — see §7; takes `&Lock`, adds no acquire.

## 9. Finding 13: the contract table carries the mechanism, not the INVARIANT sentence — CONCERN

`docs/src/lock-contract.md`'s row reads:

> | rebuild swap precondition | invariant row | In `Rebuild::execute_with`, MCP and CLI reads use `open_ro` without `paths.lock`, while every mutation uses `open_rw` directly or through a locked writer, serializing on the lock rebuild holds across the swap. |

That is the *mechanism* (who takes which opener, and that mutations serialize). It never states the
proposition C1 needs to cite. The plan (`.state/.omc/plans/c2-exclusion-boundary.md:685`) asks for
"the invariant stated in the form C1 can cite: *no connection is open for write against the old
inode at the point of rename*", and the post-impl gate line at `:883` re-checks exactly that. As
written, a reader of `rebuild.rs` cannot point at a sentence in the table and say "this is the
precondition my rename relies on"; they can only reconstruct it.

**Exact sentence that would satisfy finding 13** — add as the row's leading sentence, before the
existing mechanism text:

> **Invariant: no connection is open for write against the old inode at the point of rename.** Every
> mutating opener (`open_rw`, `open_or_init`) acquires `paths.lock` first and rebuild holds
> `paths.lock` across the whole of Phase 3, so no writer can hold or begin a write transaction
> against the live inode while `fs::rename` executes; readers opened through `open_ro` are
> `PRAGMA query_only=ON` and therefore cannot commit content to it, and `open_live_for_checkpoint`'s
> connection — the one exception — is closed in step 3, before the rename.

The final clause is not decoration: `open_live_for_checkpoint` (`db.rs:328`) is a raw
`Connection::open` against the live path with no `query_only`, so an invariant phrased only as "no
writable connection exists" would be false as stated. `rebuild.rs:637-643` closes it before step 4,
which is what makes the invariant true.

## 10. `rebuild.rs` still carries the falsified justification alongside the corrected one — CONCERN

The plan says the WAL-deletion comment "is rewritten to cite this invariant instead of 'per-request
connections', which the review falsified". The falsified sentence is still present verbatim at
`src/commands/rebuild.rs:602-604`:

```
// Safety (Linux): the per-request connection model means no MCP handler
// holds a connection across the lock boundary, so no reader has the WAL
// open when we unlink it.
```

It was *supplemented*, not replaced: the corrected argument follows at `:608-621` under "Safety,
re-derived after C2/L1c". Both paragraphs now stand as active justifications for the same step, and
the first one is the argument the review rejected. Remedy: delete the three lines above and keep the
Linux inode-lifetime sentence that follows them, so the re-derived paragraph is the only standing
argument.

## 11. Peers and graphs are not event-sourced, so the rebuild swap drops them — CONCERN (pre-existing, but it makes one landed test vacuous)

`db::apply_event`'s arms are `("upsert","entries")`, `("expire","entries")`,
`("upsert","test_cases")`, `("insert","run_history")`, `("evidence_add","evidence")`,
`("citation_healed","evidence")`, `("evidence_expire","evidence")`. There is no peer or graph arm,
and `rebuild.rs` has no ATTACH or carry-over step that copies `peers`/`graphs` from the live DB into
the tmp DB. The tmp DB is built purely from the log and then renamed over the live file, so **every**
peer edge — live and expired alike — disappears at the swap. (The `VACUUM INTO` backup at
`rebuild.rs:326-352` preserves them in a `.pre-vN.bak` file, so this is recoverable, not
irretrievable.)

Two consequences that touch this waiver's surfaces:

1. `rebuild.rs:421`'s Phase-1 `sweep_expired_peers` runs against a live DB that is about to be
   discarded, so it has no effect on the post-swap state.
2. `test_rebuild_physically_removes_expired_peers` (`rebuild.rs:1241`) asserts the expired peer is
   absent after a rebuild. It passes vacuously — a live peer inserted alongside it would also be
   absent. The test does not distinguish "swept" from "everything was dropped", so it provides no
   evidence for the property it names.

This is **pre-existing** (peers were never event-sourced; C2 did not change it) and is therefore not
a state machine C2 introduced, which is why it does not block this box. But the contract table's
"ADR-7 rebuild `kb_meta` survival" row names only `schema_version` and `embed_text_mode` as
non-surviving keys and says nothing about whole tables that do not survive at all. Recommended
follow-up, as a separate task rather than a C2 blocker: state the table-level loss in that row, and
either strengthen `test_rebuild_physically_removes_expired_peers` with a live peer that must also be
gone (documenting the loss) or replace it with one that pins the intended behaviour once decided.

---

## Findings summary

| # | Finding | Verdict |
|---|---|---|
| 1 | `PortProtocol.tla` (`FixedDesign = TRUE`) corresponds action-for-action to the landed `port_manager.ex`; rules 4-6 out-of-scope claims each carry a landed test | PASS |
| 2 | Malformed / id-less reply terminates the call while a stale reply is discarded — outside the model's alphabet, not cross-talk, untested | CONCERN |
| 3 | Flock surface: `open_rw` path-matching guard, ratchet at zero, `Connection::open` confined to `db.rs`; the second lock file predates T1 and has a single fixed acquisition order | PASS |
| 4 | Re-entrancy surface: registry landed with both required tests, including the two-spellings alias case | PASS |
| 5 | TTL surface: filter on sites 1-5 via one shared helper, deliberately absent on site 6 with a comment; no seventh read path exists | PASS |
| 6 | Peer sweep runs only from locked writers; no open path sweeps | PASS |
| 7 | `RebuildProtocol.tla` assumes writer exclusion in Phase 3 and the contract row is its discharge; `DurableBatch.tla` overlaps only on read/write asymmetry | PASS |
| 8 | No post-T1 landing (L2, L3, B3, S1, L1c, C1 cursor writer, V4b) reopened a waived surface; L3's batch lock hand-off is covered by six named tests plus a proptest | PASS |
| 9 | Contract table's swap-precondition row states the mechanism, not the invariant sentence finding 13 requires | CONCERN |
| 10 | `rebuild.rs:602-604` still carries the falsified "per-request connection model" justification beside the corrected one | CONCERN |
| 11 | Peers/graphs are not event-sourced, so the swap drops all of them; `test_rebuild_physically_removes_expired_peers` is vacuous and the ADR-7 row is silent on table-level loss | CONCERN |

## Confirmation, in the box's terms

**No uncovered state machine was introduced on these three surfaces.** The flock surface remains the
environmental assumption `AgentKb.tla` already carries, now enforced in the type system by
`open_rw`'s path-matching guard rather than by convention. The re-entrancy surface's liveness hazard
is closed by the runtime registry, not left to a model, and both required tests landed. The TTL
surface's new temporal state is externally visible only as a read post-condition, and that predicate
is total over exactly the five consumer-visible sites and deliberately absent on the sixth. The one
new interleaving added after T1 — L3's batch-wise lock hand-off during reembed — was assigned to
test rather than model by the plan, and those tests landed, including the reembed-versus-swap
reconciliation cases. Findings 9, 10 and 11 are documentation and test-quality defects, not
uncovered transition systems.

**SIGN-OFF GRANTED.**

Findings 9 and 10 are *not* covered by this box, but they are separate criteria of the same
post-impl gate (`.state/.omc/plans/c2-exclusion-boundary.md:883` and success criterion 1) and remain
open. They need the two edits given verbatim in §9 and §10 before that gate can close. Finding 11 is
a recommended follow-up task, not a C2 blocker.
