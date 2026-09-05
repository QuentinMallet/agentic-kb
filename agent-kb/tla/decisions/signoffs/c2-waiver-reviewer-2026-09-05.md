# Code-reviewer sign-off: C2 lock-contract waiver

**Waiver:** `/home/urist/Documents/perso/agentic-kb/.state/agent-kb/tla/decisions/lock-contract-no-spec.md`
**Box (line 77):** "code-reviewer confirms the registry and the six-site TTL-filter inventory (filter on sites 1-5, no filter on site 6) match this record."
**Worktree:** `/home/urist/Documents/perso/agentic-kb/.state/program-worktrees/storage-correctness-2`
**HEAD:** `542d49d` (clean tree; C2 at `f8d0076`, L1c at `542d49d`)
**Mode:** read-only. No `cargo`, `nextest`, `mix` or TLC was run.

---

## Part 1 — the registry (waiver row 2)

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| R1 | A process-local registry exists inside `acquire_lock` | CONFIRMED | `src/commands/add.rs:231-238` declares `HELD_LOCKS: Lazy<Mutex<HashMap<LockRegistryKey, String>>>`; consulted at `src/commands/add.rs:273-284` inside `acquire_lock` (`src/commands/add.rs:250`). |
| R2 | It converts a second acquire from a deadlock into an **error** naming the first site | CONFIRMED | `src/commands/add.rs:274-283`: `anyhow::bail!("re-entrant acquire of {}: this thread already holds it (acquired at {first})...")`. The `#[track_caller]` attribute at `src/commands/add.rs:249` plus `Location::caller()` at `src/commands/add.rs:255-256` supply the first-acquisition site. |
| R3 | It keys on the **canonicalized** path, so a second acquire under a different spelling of the same file is recognized as re-entrant | CONFIRMED | `src/commands/add.rs:268-269` canonicalizes after create; `src/commands/add.rs:271-272` builds the key from the canonical path. Rationale documented at `src/commands/add.rs:226-229`. |
| R4 | Canonicalization happens **after** the file is created, so a first-ever acquire resolves | CONFIRMED | `src/commands/add.rs:261-269` — `OpenOptions::create(true)` precedes `fs::canonicalize`. |
| R5 | The registry entry is released when the guard drops | CONFIRMED | `src/commands/add.rs:321-326` `impl Drop for Lock` removes `(self.owner, self.path)`. The `owner` field (`src/commands/add.rs:311`) makes the removal correct even when the guard is dropped on a different thread. |
| R6 | A failed `flock` does not leave a stale registry entry | CONFIRMED | `src/commands/add.rs:286-289` removes the key before returning the error. |
| R7 | L1a carries the registry test, with a bounded timeout, proving the second acquire errors rather than hangs | CONFIRMED | `tests/open_split.rs:466` `second_acquire_on_the_same_thread_errors_instead_of_blocking`. |
| R8 | The test covers two path spellings that canonicalize to one file | CONFIRMED | `tests/open_split.rs:530` `registry_canonicalizes_two_spellings_of_one_lock_file`, asserting at `tests/open_split.rs:567` "an aliased second acquire must be recognized as re-entrant". |
| R9 | The record's wording "**process-local** … a second **in-process** acquire" describes the implementation | **REFUTED** | The registry key is `(ThreadId, PathBuf)` — `src/commands/add.rs:230`. A second acquire from a *different thread in the same process* is **not** rejected; it blocks on the flock. `tests/open_split.rs:499` `a_second_thread_blocks_on_the_flock_rather_than_being_rejected` pins that as intended behavior, and `src/commands/add.rs:217-224` documents why: rebuild's schema-upgrade single-flight and its Phase 2 concurrent-writer guarantee both depend on in-process threads serializing on the flock rather than failing. |

**Disposition of R9.** The code is narrower than the record's wording, and deliberately so. The hazard the record names — "the process did not attempt to acquire the same flock twice" in one call chain — is a self-deadlock, which is by construction same-thread. Every self-deadlock ADR-1 names (`rebuild.rs`'s documented case, `handle_import` under L2) is one thread re-entering its own lock. So the substantive obligation is met and the implementation is more correct than the record's prose. This is a defect in the waiver text, not in the code. Recorded below as F3.

**Registry verdict: the registry matches this record in substance.** R1-R8 CONFIRMED; R9 is a wording defect in the record.

---

## Part 2 — the six-site TTL-filter inventory

The filter is centralized in one helper rather than hand-written per site:

`src/components/db.rs:205-207`
```rust
pub fn live_peer_predicate(alias: &str) -> String {
    format!("({alias}.expires_at IS NULL OR {alias}.expires_at >= datetime('now'))")
}
```

This is textually the predicate the record specifies (`AND (expires_at IS NULL OR expires_at >= datetime('now'))`).

| Site | Surface | Record says | HEAD | Verdict |
|---|---|---|---|---|
| 1 | `kb peers list` → `query_peers_for_repo` | filter | `src/commands/peers.rs:296-309`, predicate injected at `src/commands/peers.rs:308` | CONFIRMED |
| 2 | `kb peers show` → `query_peers_by_either_repo` | filter | `src/commands/peers.rs:340-351`, predicate at `src/commands/peers.rs:350` | CONFIRMED |
| 3 | `kb peers edge-list` → `query_peer_edges` | filter | `src/commands/peers.rs:778-788`, predicate at `src/commands/peers.rs:787` | CONFIRMED |
| 4 | Federated peer-graph traversal | filter, all bind sites | `query_direct_peers` at `src/commands/search.rs:392` and `src/commands/search.rs:400`; `query_neighbors` at `src/commands/search.rs:448` and `src/commands/search.rs:456`. All four `SELECT DISTINCT p.target_repo FROM peers p` variants carry it; `bfs_peers` (`src/commands/search.rs:407-434`) reaches the table only through `query_neighbors`. | CONFIRMED |
| 5 | MCP `kb_peers_list` → `handle_kb_peers_list` | filter | `src/commands/mcp.rs:2539-2543`, predicate at `src/commands/mcp.rs:2542` | CONFIRMED |
| 6 | `kb peers import` duplicate-suppression | **no** filter | `src/commands/peers.rs:451-459` — bare `SELECT id FROM peers WHERE source_repo=?1 AND target_repo=?2 AND edge_type='member' AND (epic_slug IS ?3 ...)`, no predicate. Carries an inline comment at `src/commands/peers.rs:449-450` naming the waiver and the reason (no `UNIQUE` constraint on the tuple). | CONFIRMED |

### The inventory is total

Every `FROM peers` occurrence in `src/` production code is accounted for:

- Sites 1-6 above.
- `DELETE FROM peers WHERE expires_at IS NOT NULL AND expires_at < datetime('now')` — the sweep, `src/components/db.rs:194`.
- `DELETE FROM peers WHERE id=?1` / `WHERE epic_slug=?1` — mutations, `src/commands/peers.rs:227`, `:720`, `:759`, `src/commands/mcp.rs:2594`.
- `SELECT DISTINCT graph_id FROM peers` orphan-graph subqueries — read only `graph_id` to decide graph deletion, never surface a peer row: `src/commands/peers.rs:231`, `:724`, `:763`, `src/commands/mcp.rs:2600`.
- Everything else is inside `#[cfg(test)]` modules.

No `JOIN peers` or aliased read exists outside these. **The record's claim that there are exactly six peer read sites in `src/` holds at HEAD.** CONFIRMED.

### Backing tests

| Sites | Test | Location |
|---|---|---|
| 1, 2, 3 + physical presence | `test_peers_read_helpers_filter_expired_rows_without_deleting_them` | `src/commands/peers.rs:991`; asserts 1 visible row per helper and `SELECT COUNT(*) FROM peers == 2` at `src/commands/peers.rs:1036-1042` |
| 4 | `test_collect_peer_paths_filters_expired_rows_without_deleting_them`, `test_federated_global_limit_ignores_physically_present_expired_peer` | `src/commands/search.rs:798`, `:835` |
| 5 | `test_handle_kb_peers_list_filters_expired_rows_without_deleting_them` | `src/commands/mcp.rs:2853` |
| 6 (negative) | `test_peers_import_skips_expired_but_present_duplicate` | `src/commands/peers.rs:1048`; asserts count stays 1 at `src/commands/peers.rs:1108-1111` with the message "duplicate suppression must treat an expired-but-present peer as already existing" |
| init must not sweep | `open_or_init_does_not_sweep_expired_peers` | `tests/open_split.rs:273` |

**Inventory verdict: CONFIRMED. Filter present on sites 1-5, absent on site 6, and the six-site enumeration is total at HEAD.**

---

## Part 3 — the broader lock-contract claims requested by the lead

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| M1 | Every peer mutation goes through a locked opener | CONFIRMED | CLI: `PeersAdd` `src/commands/peers.rs:97-98`, `PeersRemove` `:224-225`, `PeersImport` `:439-440`, `PeersEdgeAdd` `:593-594`, `PeersEdgeRemove` `:717-718`, `PeersEdgeCleanupEpic` `:756-757` — each `acquire_lock(&paths.lock)` immediately followed by `db::open_rw(&paths, &lock)`. MCP: `handle_kb_peers_add` `src/commands/mcp.rs:2452-2458`, `handle_kb_peers_remove` `src/commands/mcp.rs:2583-2589`. |
| M2 | `open_rw` requires a live, **path-matching** guard, not merely any lock | CONFIRMED | `src/components/db.rs:523-541` canonicalizes `paths.lock` and bails when `lock.path() != expected`. Pinned by `tests/open_split.rs:304` `open_rw_rejects_a_lock_on_the_wrong_path`. |
| M3 | Reader opens are `open_ro` with `PRAGMA query_only=ON` | CONFIRMED | `src/components/db.rs:390` sets `query_only=ON`; peer readers at `src/commands/peers.rs:192`, `:263`, `:686`, and `src/commands/mcp.rs:2533`. Pinned by `tests/open_split.rs:63` `open_ro_rejects_writes`. |
| M4 | Physical deletion happens only in locked writers; init/recovery leaves expired rows present | CONFIRMED | `init_locked` (`src/components/db.rs:632-638`) runs schema + stamp only; documented at `src/components/db.rs:626-631`. Every `sweep_expired_peers` call site holds a lock: `src/commands/peers.rs:161`, `:234`, `:524`, `:655`, `:727`, `:766` (all after the `acquire_lock`/`open_rw` pairs in M1), `src/commands/mcp.rs:2522`, `:2605`, `src/commands/rebuild.rs:421`, `src/commands/compact.rs:126`. |
| M5 | Ratchet gates enforce the opener discipline | CONFIRMED | `tests/open_db_ratchet.rs` carries three gates: `open_db_callsites_do_not_increase` (line 88, ratchet pinned at 0, line 5), `open_unchecked_for_test_is_confined_to_test_modules` (line 113), `connection_open_is_confined_to_the_db_component` (line 141, forbidding `Connection::open`, `open_with_flags`, `open_in_memory` outside `components/db.rs`). |
| M6 | L1c gates pin the migrated surfaces | CONFIRMED | `tests/l1c_opener_migration.rs:28` pins nine readers to `open_ro` and forbids the legacy opener; `:101` drives real command entry points while the write lock is held, with a 10 s timeout (`:15`) so a reader that blocks fails instead of hanging, and asserts no mutation occurred at `:196-203`; `:207` pins that a writer entry genuinely waits. |
| M7 | The three waived surfaces are the only lock surfaces outside `PortProtocol.tla`, and match the contract table | CONFIRMED with omissions | The contract table (`docs/src/lock-contract.md`) is function-name anchored as claimed and its peer row lists exactly the six mutators and three readers found in `src/commands/peers.rs`. Five `src/` files that call an opener have **no row**: see F2 and F6. |
| M8 | **Readers never take the write lock** | **REFUTED** | See F1. Three consumer-visible peer read commands acquire `paths.lock` at CLI dispatch. |

---

## Issues

### [HIGH] `kb peers list`, `peers show`, `peers edge-list` take the write lock at CLI dispatch
**File:** `src/commands.rs:104-117` (classification), `src/commands.rs:127-133` (dispatch), `src/components/db.rs:632-637` (`init_locked`)
**Confidence:** HIGH

`KbCmd::Peers` is a single subcommand variant (`src/commands.rs:76-77`) covering all nine peer subcommands. `mutates()` (`src/commands.rs:104-117`) is a `!matches!` against an allow-list of read commands, and `Peers` is not in it, so `mutates()` returns `true` for `kb peers list` as much as for `kb peers add`. Dispatch (`src/commands.rs:127-133`) therefore calls `db::open_or_init(&paths)`, whose `init_locked` (`src/components/db.rs:632-637`) **unconditionally** calls `acquire_lock(&paths.lock)`.

The comment immediately above that call states the invariant it breaks, verbatim (`src/commands.rs:121-125`):

```
// `open_or_init` takes the write lock, which a read must never do;
// reads detect the same condition on their own read-only connection and warn.
```

Consequences: `kb peers list` blocks for the full duration of any concurrent writer, and it runs event-log recovery, so a command the user invoked as a read can mutate the database. Sites 1-3 of this waiver's own inventory are exactly these three commands.

This does **not** invalidate the waiver. The TTL argument is unaffected: the predicate still applies at all three sites, and the read itself still runs on an `open_ro` connection (`src/commands/peers.rs:192`, `:263`, `:686`). It is also not a nested acquire — `init_locked` releases before `self.cmd.run()` (`src/components/db.rs:635-637`), so the registry is not tripped and the withdrawal condition at waiver line 65 is not triggered. `recover_if_needed` is separately lock-free in the no-op case (`src/commands/rebuild.rs:127-136`); the unconditional acquisition comes from `init_locked` alone.

**Fix:** split `KbCmd::Peers` in `mutates()` on the inner `peers::Peers` variant so `List`, `Show`, and `EdgeList` fall on the read side, matching the classification the contract table and `src/commands.rs:121-125` already assert. Add a test pinning the read/write classification per subcommand — there is currently none (`mutates` has no test anywhere in `src/` or `tests/`).

### [MEDIUM] Contract table omits the production `open_or_init` at CLI dispatch
**File:** `docs/src/lock-contract.md` (no row for `src/commands.rs`); call site `src/commands.rs:129`
**Confidence:** HIGH

The table opens with "This table records the current opener discipline in `src/` after C2/L1c landed" and carries a row for every other file with a production opener, including `src/bin/kb-bench-fixture.rs`. `src/commands.rs` holds the process-entry `open_or_init` that decides, for every CLI invocation, whether the write lock is taken before the subcommand runs. That is the load-bearing row for the read/write split, and F1 is exactly the drift it would have caught.

**Fix:** add a row — `src/commands.rs` | init | `EntryPoint::run` calls `open_or_init` when `KbCmd::mutates()` is true, and the per-subcommand classification lives in `KbCmd::mutates()`.

### [MEDIUM] The waiver's re-entrancy row overstates the registry's scope
**File:** `.state/agent-kb/tla/decisions/lock-contract-no-spec.md:13`
**Confidence:** HIGH

The row says the registry "converts a second **in-process** acquire from a deadlock into an error". The registry is keyed per-thread (`src/commands/add.rs:230`), and a second acquire on a different thread of the same process deliberately blocks (`src/commands/add.rs:217-224`, pinned by `tests/open_split.rs:499`). A future reader taking the row at face value would treat a cross-thread block as a regression.

**Fix:** amend the row to "a second acquire **on the same thread**", and note that cross-thread contention is ordinary mutual exclusion that rebuild's single-flight depends on.

### [LOW] Every file:line reference in the waiver's inventory table has drifted
**File:** `.state/agent-kb/tla/decisions/lock-contract-no-spec.md:13`, `:24-29`
**Confidence:** HIGH

| Record cites | Actual at HEAD |
|---|---|
| `peers.rs:264-271` (row 2, `PeersShow`) | `src/commands/peers.rs:266-285` |
| `peers.rs:196`, SQL `peers.rs:300` (site 1) | `src/commands/peers.rs:192`, `:296-309` |
| `peers.rs:259-278`, SQL `peers.rs:339` (site 2) | `src/commands/peers.rs:261-289`, `:340-351` |
| `peers.rs:669-678` (site 3) | `src/commands/peers.rs:683-690`, helper `:778-788` |
| `search.rs:258-345`, binds `:294`, `:297`, `:340`, `:345` (site 4) | `src/commands/search.rs:385-459`, binds `:392`, `:400`, `:448`, `:456` |
| `mcp.rs:1916-1935`, SQL `mcp.rs:1928` (site 5) | `src/commands/mcp.rs:2529-2578`, SQL `:2539-2543` |
| `peers.rs:441-448` (site 6) | `src/commands/peers.rs:451-459` |
| `db.rs:388-397` (non-unique indexes) | shifted; the four `CREATE INDEX` statements on `peers` remain non-unique |

Substance is intact — every cited surface still exists and still behaves as described. `docs/src/lock-contract.md:19-22` already adopted function-name anchoring for exactly this reason; the waiver did not.

**Fix:** re-anchor the waiver's table on function names, or restamp the line numbers against `542d49d`.

### [LOW] Site 2's two-spelling path is not exercised end to end
**File:** `src/commands/peers.rs:991-1042` (test), `src/commands/peers.rs:266-285` (untested reconciliation)
**Confidence:** HIGH

The waiver's L1b test list (line 47-49) asks that the expired peer be absent from `kb peers show` "(both path spellings)". `test_peers_read_helpers_filter_expired_rows_without_deleting_them` calls `query_peers_by_either_repo` once, with the canonical spelling (`src/commands/peers.rs:1027`). `PeersShow::execute`'s as-is/canonical double query and dedup-by-id (`src/commands/peers.rs:266-285`) is not driven by any test.

This is not a filter gap — the predicate lives inside the helper, so both spellings are filtered by construction — but the dedup-by-id logic and the two-call structure are unpinned. This is L1b's obligation (`bd-21ef.2.4`), not this box's.

**Fix:** extend the test to drive `PeersShow::execute` under a symlinked or relative repo root and assert one row, not two.

### [LOW] Four more opener-using files have no contract-table row
**File:** `docs/src/lock-contract.md`
**Confidence:** MEDIUM

All test-fixture-only, so lower stakes than F2: `src/bench_fixture.rs:130-131` (`open_db_memory`), `src/commands/import_cmd.rs:239`, `src/commands/ingest.rs:373`/`:421`/`:504`/`:535`, `src/components/cursor.rs:1160`/`:1178`/`:1268`/`:1297`/`:1325` (all after the `#[cfg(test)]` at `src/components/cursor.rs:1098`, so none trip the ratchet). The legend also names seven opener classes but not `open_ro_peer` (`src/components/db.rs:473`) or `open_db_memory` (`src/components/db.rs:707`), both `pub`.

---

## Open Questions (low-confidence, non-blocking)

### [MEDIUM] `test_db` returns an unlocked writable connection to the live DB path
**File:** `src/components/db.rs:647-652`
**Confidence:** LOW

`test_db` runs `init_locked` under the lock, then opens with `open_conn_rw(&paths.db)` (`src/components/db.rs:650`) and returns that connection with no guard. It is `#[doc(hidden)]`, documented as intentional at `src/components/db.rs:640-646` ("a fixture is the single writer in its own tempdir"), and the ratchet's raw-constructor gate cannot see it because `open_conn_rw` is a private helper inside `components/db.rs`. I found no production call site — the contract table classes every `test_db` row as test-fixture-init and L1c's review counted the nine sites. Flagged only because it is the one writable live-path opener with no lock in its signature and no gate confining it to test modules, unlike `open_unchecked_for_test` (`tests/open_db_ratchet.rs:113`). Confirming it stays fixture-only needs a call-site sweep I did not run to completion.

---

## Positive observations

- **The TTL filter is a single shared function, not six copies.** `db::live_peer_predicate` (`src/components/db.rs:205-207`) is used by all five filtered sites. Sites cannot drift apart, and a change to the expiry semantics is a one-line edit rather than a five-site audit. This is the strongest single reason the waiver's totality argument still holds after C2 and L1c both moved this code.
- **Site 6's deliberate omission is self-documenting.** `src/commands/peers.rs:449-450` carries an inline comment naming the waiver site number and the reason (no `UNIQUE` constraint), so the next reader who "fixes the inconsistency" is warned in place rather than in a document they would have to already know exists.
- **`open_rw` proves the *right* lock, not merely a lock.** The canonical-path comparison at `src/components/db.rs:524-540` closes the gap a bare `&Lock` marker type would have left, and the rationale is in the doc comment rather than only in the ADR.
- **The ratchet has three independent gates at different granularities** (`tests/open_db_ratchet.rs:88`, `:113`, `:141`), and the raw-constructor gate is the one that makes the other two hard to route around. The `strip_line_comment` helper documents its own known limitation at `tests/open_db_ratchet.rs:26-30` instead of pretending to be a lexer.
- **`tests/l1c_opener_migration.rs` tests the property, not the function.** It drives real command entry points on spawned threads with a 10 s timeout while the write lock is held (`:101-204`), so a reader that regresses to a locked opener fails an assertion rather than hanging the suite. The comment at `:96-100` is explicit that this is deliberately different from what `tests/open_split.rs` covers.
- **`recover_if_needed` detects lock-free.** `src/commands/rebuild.rs:127-136` opens read-only and returns early on `NoOp`, so the steady-state cost is one cursor comparison and no contention.

---

## Recommendation

**REQUEST CHANGES** on the C2 lock contract overall, driven by F1 (`kb peers list` takes the write lock at dispatch) and F2 (the contract table has no row for the dispatch opener that would have caught it). Neither is inside this box's scope.

On the box itself, both halves hold: the registry matches the record in substance (R1-R8 CONFIRMED, R9 a defect in the record's prose, not the code), and the six-site TTL-filter inventory matches exactly — filter on sites 1-5, no filter on site 6, and the six-site enumeration is total at `542d49d`.

**SIGN-OFF GRANTED**
