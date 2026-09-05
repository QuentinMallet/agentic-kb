# Code Review: C3 spec-waiver sign-off (bd-21ef.3.14)

**Scope:** confirm or refute the first sign-off box of
`.state/agent-kb/tla/decisions/c3-search-tasks-spec-waiver.md` — "code-reviewer
confirms S3a, S3b and S4 introduced no state-machine logic, no event write, and
no lock acquisition."

**Tree reviewed:** `/home/urist/Documents/perso/agentic-kb/.state/program-worktrees/storage-correctness-2`, HEAD `542d49d`.
**Commits reviewed:** `25f79cd` (S3a), `280d0dc` (S3b), `d405888` (S4). Full
diffs plus the code as it stands at HEAD.
**Files reviewed:** 3 production (`src/commands/mcp.rs`, `src/commands/context.rs`, `src/commands/search.rs`) plus `src/components/db.rs` and `src/components/query_hits.rs` as supporting evidence.
**Not run:** `lsp_diagnostics`. rust-analyzer would spawn a background `cargo
check`, and the task brief forbids cargo/nextest while a benchmark needs a quiet
machine. This is an evidence gap in the normal Stage 2 protocol, declared rather
than skipped silently. It does not affect the four waiver questions, which are
answered from diff and source evidence.

---

## S3a — provenance dangling references (bd-21ef.3.9) — CONFIRMED

**(a) State machine / persisted state:** none. The new `dangling` accumulator is
a function-local `Vec<String>` (`src/commands/mcp.rs:2325`) serialized into the
response at `src/commands/mcp.rs:2402`. No new table, no `CREATE TABLE`/`ALTER`,
no new column written, no `static`/`OnceLock`/`Mutex`, no file write. Both new
prepared statements are `SELECT`s (`src/commands/mcp.rs:2328` counts `entries`
rows; `src/commands/mcp.rs:2332` reads `evidence.derived_from`).

**(b) Event write / evidence mutation:** none in production. Grepping the diff's
added lines for `apply_event|append_event|append_and_apply|cursor::|INSERT|UPDATE|DELETE`
yields exactly three hits, all inside `mod tests` (which begins at
`src/commands/mcp.rs:2629`): `db::apply_event` at `src/commands/mcp.rs:5694` and
`INSERT INTO evidence(...)` at `src/commands/mcp.rs:5714`, both in the
`build_fixture` helper of `test_handle_provenance_is_deterministic_across_parent_insertion_order`.
Test setup writing state is permitted by the brief.

**(c) Lock acquisition:** none. `handle_provenance` opens with `db::open_ro`
(`src/commands/mcp.rs:2311`). `db::open_ro` (`src/components/db.rs:377`) takes no
flock; it sets `PRAGMA query_only=ON` and never touches `paths.lock`. No
`acquire_lock` or `open_rw` in the diff.

**(d) Opener change:** none. The opener line is identical in the commit's own
blob (`git show 25f79cd:src/commands/mcp.rs` → `db::open_ro`) and at HEAD.

**Note on the ratchet churn.** The commit bumped `OPEN_DB_CALLSITE_RATCHET`
69 → 70 because its new test fixture called `db::open_db`. That is now moot: the
C2/L1c merge at HEAD deleted `open_db` entirely, the ratchet reads `0`
(`tests/open_db_ratchet.rs:5`), and the S3a fixture uses the test-only
`db::open_unchecked_for_test` (`src/commands/mcp.rs:5687`). Test-convention
opener, production paths untouched.

**Verdict: CONFIRMED.**

---

## S3b — context token budget (bd-21ef.3.10) — CONFIRMED

**(a) State machine / persisted state:** none. `OutputMode`
(`src/commands/context.rs`, `#[derive(Clone, Copy, ...)] enum`) is threaded as a
by-value parameter through `build_candidates` and `greedy_select`; it is not
stored anywhere. `json_row_string`, `projected_bytes` and
`approx_tokens_from_bytes` are pure functions over their arguments. The
`Candidate` struct lost a field (`tokens`) rather than gaining one, and it is an
in-memory struct with no DB backing.

**(b) Event write / evidence mutation:** none. Grepping the diff's added and
removed lines for `open_ro|open_rw|open_db|open_or_init|acquire_lock|\.lock|append_event|append_and_apply|apply_event|cursor::|static |Mutex|RwLock|OnceLock|fs::write|File::create|INSERT|UPDATE|DELETE`
returns nothing at all.

**(c) Lock acquisition:** none. `Context::run` opens with `db::open_ro`
(`src/commands/context.rs:95`), under a doc comment at
`src/commands/context.rs:85` that states the contract explicitly. Unchanged by
this commit.

**(d) Opener change:** none.

**Accuracy note on the waiver's phrasing (not a refutation).** `kb context` does
write a file, and always did: `query_hits::record_injection`
(`src/commands/context.rs:139`) inserts rows into the auxiliary telemetry
database at `paths.query_hits`, gated on the `KB_INJECTION_SOURCE` environment
variable. That write is pre-existing — it appears as an unchanged context line in
the S3b diff — and it is not the event log and not an `entries`/`evidence` row.
`db::open_auxiliary` (`src/components/db.rs:304`) refuses live repository
database paths outright. The waiver document's own four terms (no event write,
no evidence-row mutation, no write flock, no cross-call state machine) all hold;
only the broader "writes no file" gloss would be imprecise for this command.

**Verdict: CONFIRMED.**

---

## S4 — federated search contract (bd-21ef.3.11) — CONFIRMED

**(a) State machine / persisted state:** none. `merge_federated_results`
(`src/commands/search.rs:23`) allocates a function-local `HashMap`
(`src/commands/search.rs:27`) and returns a `Vec`. No new table, no persisted
field, no static, no file write. `origin_repo` already existed on
`db::SearchEntry` (`src/components/db.rs:1959`); `git show d405888 -- src/components/db.rs`
is empty, so the schema was not touched.

**(b) Event write / evidence mutation:** none. The only openers the diff adds are
`db::open_or_init` and `db::open_unchecked_for_test`, both inside
`test_federated_global_limit_ignores_physically_present_expired_peer` in
`mod tests`. No `apply_event`, `append_event`, `append_and_apply` or `cursor::`
anywhere in the file.

**(c) Lock acquisition:** none in production. `acquire_lock` and `open_rw` appear
in `src/commands/search.rs` only at test lines 1356, 1410 and 1411, in a
pre-existing WAL-recovery test unrelated to this commit.

**(d) Opener change:** none. Local reads still use `db::open_ro`
(`src/commands/search.rs:197`); peer reads still use `db::open_ro_peer`
(`src/commands/search.rs:238`). `open_ro_peer` (`src/components/db.rs:473`) opens
with `SQLITE_OPEN_READ_ONLY` plus `immutable=1` and takes no flock. The diff
replaced only the body of the `Ok(...)` arm that consumes peer results; the
`match db::search_entries(&peer_conn, ...)` opener line above it is unchanged
context.

**Verdict: CONFIRMED.**

---

## Non-blocking findings (outside the waiver's scope, reported for completeness)

### [MEDIUM] Response-shape inconsistency: `dangling` missing on the uninitialized-db branch
File: `src/commands/mcp.rs:2317`
Confidence: HIGH
Issue: S3a documents the provenance result shape as carrying `dangling`
(`src/commands/mcp.rs:2296-2300`, plus `agentic-kb-defensibility.md` and
`skills/kb-audit/SKILL.md`, both updated in the same commit), but the
uninitialized-repository early return emits only
`{"roots": [], "graph": [], "truncated": false}`. A client that reads
`resp["dangling"]` unconditionally gets `null` on a first run instead of `[]`.
The covering test at `src/commands/mcp.rs:3557-3560` asserts `roots`, `graph`
and `truncated` but never `dangling`, so the gap is untested.
Fix: add `"dangling": []` to the early return and assert it in
`handle_provenance_on_uninitialized_db_returns_an_empty_graph`.

### [MEDIUM] Quadratic collision scan in `merge_federated_results`
File: `src/commands/search.rs:33-36`
Confidence: HIGH
Issue: for every candidate the code linearly scans every key already in the map
(`by_origin_and_id.keys().find(|(_, id)| id == &candidate.id)`), making the merge
O(n²) in total rows. With `db::MAX_LIMIT = 100` (`src/components/db.rs:101`) per
batch and an unbounded peer fan-out from `collect_peer_paths`'s BFS
(`src/commands/search.rs:354`), n is `100 * (1 + peers)`. At 20 peers that is
about 4.4M key comparisons per query. Not a production hazard at realistic peer
counts, but it is avoidable.
Fix: key the map on `id` alone (see next finding) so the collision lookup is a
single O(1) `get`.

### [LOW] Map key type does not match the dedup semantics it implements
File: `src/commands/search.rs:27` and `src/commands/search.rs:56`
Confidence: HIGH
Issue: the key is `(Option<String>, String)`, implying one row per
`(origin_repo, id)` pair, but the collision handling at
`src/commands/search.rs:33-54` guarantees at most one row per bare `id` across
all origins. The origin component of the key is therefore dead weight, and it
misleads a reader into thinking two repos may each contribute a row with the
same id. The behaviour is deliberate — the commit message says so and
`test_federated_global_limit_dedup_and_local_collision_contract` asserts it — but
the type should say what the code does.
Consequence worth recording: entry ids default to UUIDv4
(`src/commands/add.rs:109-112`), so accidental cross-repo collisions are
effectively impossible. Ids are caller-supplied when `--id` is passed, though, so
a deterministic id present in several repos collapses to one federated row.
Fix: change the key to `String` and drop the linear scan, or keep the tuple key
and document why the origin component is intentionally not part of the identity.

### [LOW] `entries.origin_repo` column is never read into search results
File: `src/components/db.rs:951` versus `src/components/db.rs:1959`
Confidence: MEDIUM
Issue: the column added by the `ALTER TABLE` is never selected into
`db::SearchEntry`; every construction site sets `origin_repo: None`
(`src/components/db.rs:2480, 2728, 2905, 2932, 3036`). Pre-existing, not
introduced by S4, and worth noting only because it means S4's
`candidate.origin_repo = batch_origin.clone()` (`src/commands/search.rs:31`)
cannot clobber a meaningful local value — it overwrites `None` with `None`.
Fix: none required for this waiver. Either wire the column through or remove it
in a separate task.

---

## Positive observations

- **S3a's start-node check is correctly scoped.** The `entry_not_found` branch
  fires only at `depth == 0`, and the DFS seeds exactly one zero-depth frame
  (`src/commands/mcp.rs:2349`), so a missing *ancestor* still degrades to
  `dangling` rather than failing the whole query. Traversal through
  stale-but-existing parents is preserved, and
  `test_handle_provenance_resolves_derived_edge_to_expired_entry` pins it.
- **S3a hoisted both prepared statements out of the DFS loop**
  (`src/commands/mcp.rs:2328, 2332`), removing a per-node `conn.prepare` while
  adding the existence check — a net reduction in per-node work.
- **The `ORDER BY derived_from` addition plus the insertion-order determinism
  test** is the right shape: the test builds the same graph under two evidence
  insertion orders and asserts byte-identical responses.
- **S3b's `json_row_string` fixed point is documented with its termination
  argument** (monotone growth, 20-digit `usize` bound) and carries an explicit
  iteration cap that fails loudly rather than looping, instead of relying on the
  invariant silently.
- **S3b's `every_skipped_relevant_entry_was_unaffordable_at_its_rank` property
  was updated to the new byte-accounting model** rather than deleted or
  weakened — the harder path.
- **S4 spells out the local-wins rule as an explicit match on
  `origin_repo.is_none()`** (`src/commands/search.rs:41-48`) with a comment
  saying why it does not lean on `Option`'s derived ordering. That is exactly the
  right instinct for a contract that a derive could silently invert.
- **S4's rank-position test is honest about a surprising behaviour**
  (`test_federated_rank_position_top_tiny_peer_outranks_mid_local`): it asserts
  that a tiny peer's top hit outranks a mid-ranked local hit and documents that
  this is by design, and the docs change says the same. Encoding a known-sharp
  edge as a passing test beats leaving it as folklore.
- **All three commits kept their production openers untouched**, which is what
  made this review answerable from the diffs at all.

---

## Recommendation

**COMMENT** on code quality (three non-blocking findings, none CRITICAL or HIGH).

**Waiver sign-off: SIGN-OFF GRANTED.**

S3a, S3b and S4 introduce no state-machine logic, no persisted state across
calls, no event-log write, no evidence-row mutation, no write-flock acquisition,
and no change to which opener any read path uses. The waiver's first sign-off box
is confirmed for all three tasks. The three findings above are ordinary read-path
quality issues and do not touch the waiver's terms; none of them requires
restoring the T0 dependency edge.

One caveat for the record: the waiver's own closing paragraph says a task that
writes an event, mutates an evidence row, or takes the write flock is outside its
terms by construction. That test is met here. If the gate is later read as the
stricter "writes no file at all", `kb context`'s pre-existing
`query_hits::record_injection` telemetry write would need calling out — but it
predates S3b and is not repository state.
