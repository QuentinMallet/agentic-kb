# Lock Contract

This table records the current opener discipline in `src/` after C2/L1c landed.

`read-only` means `db::open_ro(...)` and therefore `PRAGMA query_only=ON`.
`write-under-lock` means `db::open_rw(paths, &lock)` with a live `paths.lock`
guard. `init` means `db::open_or_init(...)`. `scratch` means
`db::open_scratch(...)`. `checkpoint` means `db::open_live_for_checkpoint(...)`
— C1/D4's raw live-path opener, used only inside rebuild's swap while holding
`paths.lock`. `test-fixture` means `db::open_unchecked_for_test(...)` — a bare,
unlocked `Connection::open` with no DDL of its own, so it only works once the
schema already exists. `test-fixture-init` means `db::test_db(...)` — the
distinct opener class that runs `init_locked` (schema + stamp, under
`paths.lock`) before returning an unlocked writable connection, for fixtures
that need to create a fresh repository's database rather than merely inspect
an already-populated one.

References below name the enclosing function (qualified with its struct when
a file has more than one `run`/`execute_with`) instead of a line number, so
they survive a rebase that shifts lines. A line number appears only where a
function name would be ambiguous on its own.

| File | Opener class | Backing reference |
| --- | --- | --- |
| `src/bin/kb-bench-fixture.rs` | init + write-under-lock | `open_or_init` followed by `open_rw` in `fn main`. |
| `src/commands/add.rs` | delegates to `kb_core::add` + test-fixture + test-fixture-init | `execute_with` delegates event-writing and DB-apply work to `kb_core::add`; most unit-test inspection uses `open_unchecked_for_test` against a database `execute_with` already populated; one fixture that seeds a raw pre-`permanent`-field event with no prior write uses `test_db`. |
| `src/commands/cited_by.rs` | read-only | `db::open_ro` in `CitedBy::run`; first-run empty-result behavior is covered by `tests/first_run_ux.rs`. |
| `src/commands/compact.rs` | mixed: read-only (gate) + write-under-lock + init + test-fixture + test-fixture-init | `Compact::run` probes with `open_ro`; locked sweep and VACUUM paths use their existing locked openers; most unit fixtures use `open_unchecked_for_test`, one seeds a fresh repository with `test_db`. |
| `src/commands/compress.rs` | read-only + delegated write-under-lock + test-fixture + test-fixture-init | Both production read phases use `open_ro`; replacement delegates to `kb_core::add`; most unit fixtures inspect with `open_unchecked_for_test`, one seeds a fresh repository with `test_db`. |
| `src/commands/context.rs` | read-only | `db::open_ro` in `Context::run`; `tests/first_run_ux.rs` covers the uninitialized-read contract shared with other read paths. |
| `src/commands/digest.rs` | read-only + delegated write-under-lock + test-fixture | `read_digest_hash` uses `open_ro`; digest insertion delegates to `kb_core::add`; the unit fixture uses `open_unchecked_for_test`. |
| `src/commands/eval.rs` | read-only | `open_ro` in `Eval::execute_with`; it warns when behind and never repairs; an uninitialized repository substitutes a throwaway `open_db_memory` connection instead of erroring, since an empty golden-set evaluation is indistinguishable from one against an initialized-but-empty database. |
| `src/commands/expire.rs` | write-under-lock | `db::open_rw` in `Expire::run`. |
| `src/commands/mcp.rs` | mixed: read-only + write-under-lock + init + test-fixture + test-fixture-init | Read handlers, including `handle_audit_report`, use `open_ro` and map an uninitialized repository to an empty result; mutating handlers use `open_rw` or delegate to locked writers; the shared `setup()` fixture initializes through `test_db`, and most of its ~80 callers then inspect with `open_unchecked_for_test`; the four tests pinning the first-run "a read must never create the database" contract use a separate `setup_uninitialized()` that touches neither opener. |
| `src/commands/migrate_citations.rs` | init + read-only + write-under-lock + test-fixture-init | `execute` uses `open_or_init`, planning uses `open_ro`, and `apply_heals` uses `open_rw`; test setup (`setup_repo`) seeds a fresh repository with `test_db`. |
| `src/commands/older_than.rs` | read-only | `open_ro` in `OlderThan::execute`. |
| `src/commands/peers.rs` | mixed: read-only + write-under-lock + init | Read opens in `PeersList::run`, `PeersShow::run`, `PeersEdgeList::run`; locked peer mutations in `PeersAdd::run`, `PeersRemove::run`, `PeersImport::run`, `PeersEdgeAdd::run`, `PeersEdgeRemove::run`, `PeersEdgeCleanupEpic::run`; test setup uses `open_or_init` in `test_peers_read_helpers_filter_expired_rows_without_deleting_them` and `test_peers_import_skips_expired_but_present_duplicate`. |
| `src/commands/rebuild.rs` | mixed: read-only + write-under-lock + scratch + checkpoint + init + test-fixture + test-fixture-init | `recover_if_needed` gates with `open_ro` and writes with `open_rw`; `full_rebuild_for` holds both locks before `open_rw`; rebuild phases use `open_scratch` and swap uses `open_live_for_checkpoint`; most unit fixtures use `open_unchecked_for_test` against a database seeded by a prior write, but several seed/swap helpers that construct a repository with no prior write (the swap-crash child's seed role, and the concurrent-writer convergence tests) use `test_db`. |
| `src/commands/reembed.rs` | mixed: read-only + write-under-lock + init | Read opens in `run_reembed_with_hooks` and `confirm_embedded_ids_are_live`; locked write in `write_batches`; test setup uses `open_or_init` extensively; `check_embed_mode_vintage` stays a separate contract row below. |
| `src/commands/run.rs` | write-under-lock | `db::open_rw` in the top-level `run` function. |
| `src/commands/search.rs` | read-only + init + test-fixture + test-fixture-init | `Search::execute_with` uses the read-only federation openers; most unit fixtures use `open_unchecked_for_test`, one (the federated peer-merge fixture, which builds a second repository with no prior write) seeds with `test_db`. |
| `src/commands/stale_check.rs` | read-only + write-under-lock + test-fixture + test-fixture-init | `StaleCheck::execute_with` uses `open_ro` and maps an uninitialized repository to an empty report; `heal_relocations` bails out before touching the database when there is nothing to heal, and otherwise calls `open_or_init` itself (not the caller) before its own `open_rw`; most unit fixtures use `open_unchecked_for_test`, three that seed a fresh repository use `test_db`. |
| `src/commands/test_add.rs` | write-under-lock | `db::open_rw` in `TestAdd::run`. |
| `src/commands/tests.rs` | read-only | `Tests::execute` discovers paths and delegates to `Tests::execute_with`, which opens with `open_ro` and maps an uninitialized repository to an empty result. |
| `src/components/db.rs` | contract definitions: read-only + write-under-lock + scratch + checkpoint + init + test-fixture + test-fixture-init | Defines `open_live_for_checkpoint`, `open_ro`, `open_rw`, `open_scratch`, `open_or_init`, `open_unchecked_for_test`, and `test_db`; the ratchet permanently forbids the removed legacy opener and separately confines `open_unchecked_for_test` to test modules. |
| `src/components/kb_core.rs` | write-under-lock + test-fixture + test-fixture-init | Production add uses `open_rw` and `cursor::append_and_apply`; most unit-test inspection uses `open_unchecked_for_test`; the shared `setup()` fixture initializes a fresh repository with `test_db`. |
| `query_hits` telemetry exemption | out of scope: separate DB, not the KB DB | `src/commands/search.rs` and `src/commands/context.rs` (both via `query_hits::record_injection`), and `src/commands/mcp.rs` (via `query_hits::record_hits` and `query_hits::counts`) write `paths.query_hits`, which is a different SQLite file rooted by the `query_hits` field on `config::Paths` (`src/config.rs`). |
| `check_embed_mode_vintage` | write to `kb_meta`, contract deferred to L3 | `src/components/db.rs` writes via the caller's connection; the primary call site is inside `apply_event` itself, exercised by every non-noop write path (`cursor::append_and_apply`, `kb_core::add`, MCP handlers) that funnels through it; `reembed.rs`'s `write_batches` also calls it directly rather than by individual command call sites; L3 moves the write under the lock. |
| rebuild schema stamp | write-under-lock + schema-upgrade lock | `full_rebuild_for` acquires `schema-upgrade.lock` for single-flight and `paths.lock` before opening with `open_rw` and stamping. |
| rebuild swap precondition | invariant row | In `Rebuild::execute_with`, MCP and CLI reads use `open_ro` without `paths.lock`, while every mutation uses `open_rw` directly or through a locked writer, serializing on the lock rebuild holds across the swap. This guarantees the invariant the swap depends on: while `paths.lock` is held across `checkpoint_live_db`'s `open_live_for_checkpoint` connection, no other opener can write a WAL frame or take a second lock against the live DB, so the live database has no uncheckpointed frames at the rename step. |
| read-path temp DB constraint | standing constraint | `db::open_ro` sets `PRAGMA query_only=ON` inside its own body, so no read path may rely on `CREATE TEMP TABLE` or any temp-database write. |
| ADR-7 rebuild `kb_meta` survival | rebuild swap invariant | Rebuild's scratch DB becomes the live DB (the final rename) inside `Rebuild::execute_with`; per ADR-7 (now T5b), keys other than `schema_version` and `embed_text_mode` do not survive that swap, and C1/D3 writes the fresh cursor row itself in that same method immediately after catch-up so recovery does not loop on a cursorless post-rebuild database. |
| C1/D3 recovery vs. `kb compact` asymmetry | observation, not a defect | `kb add` and the other MCP mutating methods auto-recover first (`recover_if_needed`), so a database that is merely behind the log (D3 row 7, `ReplayTail`) is caught up and the write proceeds. `kb compact` (`Compact::run`) does not go through `open_or_init`/`recover_if_needed` at entry, so the same row 7 refuses with "the database is behind the event log" instead of self-healing — deliberate, since compact is destructive to the log itself. |
