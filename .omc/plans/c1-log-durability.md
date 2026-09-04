# C1 — Log durability & crash recovery (`bd-21ef.1`)

Component epic of meta-epic `bd-21ef` (storage-correctness-2). Branches from the
`storage-correctness-2` aggregator; post-impl cascades component → aggregator, **never master**.

Source of findings: 4-lens deep review of master `2e2051d` (2026-09-04) — lens-1 findings 1-6,
lens-4 finding 1. Five Criticals + two riders.

Revision 2, after architect and critic passes. Both rejected revision 1; the changes they forced are
listed in §11 so the reasoning is not lost. Most consequentially, **D5 is reversed** — `run_history`
is kept event-sourced with stable keying rather than de-event-sourced.

Revision 2.2 folds in the PM gate (task splits T2a/T2b and T5a/T5b, the `T8` inbound edges) and three
binding inputs from C2's planning: ADR-7 (reads never recover — recovery moves into `open_or_init`),
the WAL self-heal that `open_ro` removes (D4 now renames the tmp in WAL mode), and outer-transaction
ownership (D3 owns it, C2's `A1` joins via savepoint). See §6 cross-component items.

---

## 1. Context

`agentic-kb` is an event-sourced store: `.state/agent-kb/agent-kb-events.jsonl` is the log of record,
and `agent-kb.db` (SQLite) is a materialization of it. The system-wide invariant is

```
materialized_tables(DB) == Materialize(committed events.jsonl)
```

where `materialized_tables` = `entries`, `test_cases`, `evidence`, `cues`, `entries_fts`,
`entries_emb`, `run_history`. It is stated positively because several tables — `peers`, `graphs`,
`audit_runs`, `audit_run_candidates`, `source_weights` — have no `apply_event` arm at all
(`db.rs:720, 896, 927, 946, 960, 1038, 1061`, with `_ => {}` at `:1100`) and are already DB-native.
`peers` and `graphs` are additionally mutated by a wall-clock TTL sweep on **every** `open_db`
(`db.rs:115-121`, called at `:172`).

Every write path is "JSONL-first": append the event(s), then `db::apply_event` each one
(`kb_core.rs:385-389`). Rebuild reconstructs the DB in three phases (snapshot under lock → replay
unlocked into a tmp DB → catch-up + atomic swap under lock). Compact rewrites the log to an
equivalent shorter one.

The review's verdict: **the logical layer is proven, the durability layer is not.** Compaction
convergence, injection safety, and stale filtering all cleared. What did not clear is what happens
when a write is interrupted — by an I/O error, ENOSPC, SIGKILL, or power loss. Today:

- `append_events_batch` writes one `writeln!` per event with no framing, so a failure on line 2 of 3
  leaves line 1 permanently reader-accepted while `add` applies nothing (lens-1 #1).
- Neither append function calls `flush`/`sync_data`, so "JSONL-first" is an *ordering* property of
  two `write` syscalls, not a *durability* property. SQLite (WAL, its own fsync discipline) can reach
  disk first; power loss then leaves a DB ahead of the log, and the next rebuild deletes committed
  state (lens-1 #2).
- A fully appended batch is applied as N independent top-level savepoints, and nothing repairs the
  gap: `rebuild_if_schema_obsolete` only fires on a schema-version bump, so an append-ok/apply-crash
  silently persists until a human runs `kb rebuild` (lens-1 #3).
- Rebuild Phase 3 unlinks the live `-wal`/`-shm` *before* renaming the replacement in
  (`rebuild.rs:444-449`); a kill in that window destroys committed WAL transactions (lens-4 #1).
- Compact retains the last 500 `run_history` events but never deletes the corresponding DB rows, and
  replay renumbers `AUTOINCREMENT` ids — so compaction is not materialization-preserving (lens-1 #4).

These are not five independent bugs. Framing without fsync still loses the log tail; fsync without
framing still exposes prefixes; both without a cursor still leave the DB behind after a crash; all
three without a safe swap still lose committed WAL on a rebuild kill; and all four without a
`run_history` disposition still fail the invariant the moment 501 test runs exist. **They compose
into one design, presented below before the tasks.**

Deployment context, which turns out to be load-bearing (see D7): the log is written by the `kb` CLI
*and* by the MCP server, and `machines_conf` pins `agentic-kb` as a flake input
(`machines_conf/flake.nix:192-193`) with a **second, independent pin into a microVM closure**
(`systems/desktop.nix:395-396`). Two binary versions can therefore address one log across a host
generation rollback or a host/VM pin skew.

Corpus today: **116 events, 180,008 bytes**, never compacted.

---

## 2. Principles

1. **The log is the truth; the DB is a cache.** Any repair must be expressible as "recompute the DB
   from the log," never "patch the log to match the DB."
2. **Old logs replay forever.** The existing un-framed log is the corpus of record. Reader backward
   compatibility is a hard constraint.
3. **A byte prefix of the log must be self-interpreting.** Rebuild's snapshot/catch-up protocol and
   the applied cursor both treat "the first N bytes" as a complete, stable meaning. Any framing that
   makes the meaning of already-written bytes depend on bytes that arrive later breaks both. This
   principle is new in revision 2 and it is the one the first draft violated.
4. **Recovery is automatic, bounded, and cannot livelock.** A property requiring a human to run
   `kb rebuild` is not a property; neither is a recovery path that retries a deterministically
   failing record forever.
5. **Reuse the durability idiom already here.** `transcript_state.rs:72-95` does tmpfile →
   `sync_all` → rename → **directory fsync**. That is the pattern; do not write a second.

## 3. Decision drivers

1. **Correctness under crash at any instruction** — the invariant must hold at every kill point.
2. **Backward compatibility with the existing un-framed log**, and a defined posture for version skew
   against an older deployed binary.
3. **Per-add latency cost** — fsync is a real per-write cost on the hot MCP `kb_add` path and must be
   budgeted against a *measured write-path baseline*, which does not yet exist.

---

## 4. Composed design (ADR)

### D1 — Framing: a commit-marker envelope around **every** append

**Decision.** Every append — batch *and* single event — is wrapped in-band:

```jsonl
{"action":"batch_begin","batch_id":"<uuid>","n":3}
… the 3 event lines, unchanged …
{"action":"batch_commit","batch_id":"<uuid>","n":3}
```

A span counts as committed only when its `batch_commit` line is present **and newline-terminated**.
Reader rules:

| Log shape | Meaning |
|---|---|
| line outside any span (legacy log) | committed standalone event — exactly today's semantics |
| span with matching newline-terminated `batch_commit` | all its events committed |
| dangling `batch_begin` **at EOF** | uncommitted; dropped by every reader |
| dangling `batch_begin` **mid-log** | **hard error**, never a silent drop (see D7) |
| `n` disagreeing with the observed line count | hard error |

Marker lines are **not events**: they are consumed by the reader and never returned, never reach
`apply_event`, and never count toward `snapshot_len` (`rebuild.rs:377`) or `original_count`
(`compact.rs:90`). Because old logs contain no `batch_begin` lines, every legacy line is
standalone-committed — the new reader accepts the existing corpus unchanged, with no migration.

**Why single events are enveloped too** (reversed from revision 1). Revision 1 exempted them,
arguing a lone `writeln!` is self-framing. Mechanically that is true for a *crash* — `File` does not
override `write_fmt`, so body and newline are two separate `write_all` calls and a crash truncates to
a newline-less chunk. But it is false for a *write error*: the body write can succeed and the newline
write fail (ENOSPC, EIO, EDQUOT), `append_event` returns `Err` at `events.rs:147`, the caller applies
nothing — and the *next* append's `repair_torn_tail_before_append` classifies that complete-JSON tail
as reader-accepted and **appends the newline** (`events.rs:218-220`), promoting an event the caller
reported as failed into a committed one. With a cursor in place, recovery would then apply it. This
affects nine call sites (`expire.rs:77`, `run.rs:60`, `test_add.rs:68`, `stale_check.rs:257`,
`migrate_citations.rs:302`, `mcp.rs:922/990/1299/1589`). One uniform rule closes it and removes the
reader's two-mode state machine.

**Prefix self-interpretation (Principle 3).** Define `committed_len(log)` = byte offset of the end of
the last committed record, excluding any open span at the tail. `read_events` returns it alongside
the events. Every byte offset that crosses a process or phase boundary — rebuild's Phase-1 snapshot
(`rebuild.rs:348-350`, today `file_len - torn_tail`), Phase-3 catch-up (`rebuild.rs:412`), and the
applied cursor (D3) — must be a `committed_len` value, and `read_events_from_offset` asserts its
argument is a span boundary. This is what makes a prefix self-interpreting again: no span straddles a
committed boundary, so bytes before it can never be reinterpreted by bytes after it.

Without this, revision 1 was broken: a snapshot taken mid-span passes `prefix_matches` (those bytes
never change), Phase 2 drops the span as uncommitted, and Phase 3's offset reader — which structurally
cannot see a `batch_begin` before its offset — applies the span's *remaining* lines as standalone.
Half a batch applied, inside the recovery path, which is Critical 1 resurrected.

**Repair.** `repair_torn_tail_before_append` becomes `repair_uncommitted_tail_before_append`: today's
torn-tail work, then truncate any dangling tail span. Truncation is **sidecar-best-effort**: an
uncommitted span was never reader-accepted, so truncation alone is sufficient and safe, and the
sidecar write must never block it. Revision 1 had this backwards — `preserve_torn_tail`
(`events.rs:163`) writes a new file before truncating, so on ENOSPC (a motivating fault for this
whole epic) span repair would fail and the dangling span would persist.

The "a dangling begin is only ever at the tail" property holds today — all twelve log-writing call
sites take the flock — but it is enforced only by a doc comment (`events.rs:173-179`). Since the
repair is now destructive, the precondition becomes **structural**: the repair function takes the
lock guard, or asserts `LOCK_EX` on the fd.

**Alternatives rejected.** *Staging file + single `write_all`* — a short write on an `O_APPEND` fd
still leaves a prefix that, without markers, is indistinguishable from committed. *Length-prefixed
records or per-record checksums* — breaks the one-object-per-line format and Principle 2.

### D2 — fsync ordering: `sync_data` after the commit marker, before any DB write

**Decision.** After the `batch_commit` line's newline is written, call `File::sync_data()` **before**
the first `db::apply_event`. `fdatasync` is sufficient for a size-extending append — Linux flushes
`i_size`; `sync_all` buys only timestamps. Compact gains a directory fsync after its rename (it syncs
the tmp at `compact.rs:277` but not the directory, so a crash can currently lose the rename itself).

This makes "JSONL-first" a durability claim: the record is on stable storage before SQLite may make
anything durable, so a power loss can only leave the DB *behind* the log — the direction D3 repairs.

**Directory fsync.** Needed when the log file or its directory is newly created. It cannot be detected
at the append handle: `repair_torn_tail_before_append` already opens with `.create(true)`
(`events.rs:180-186`) before the append handle exists, so by then the file always exists. Creation
must be detected before the repair call, and `fs::create_dir_all` (`events.rs:121`, `:138`) can create
`.state/agent-kb/` itself, whose entry needs its *parent* fsynced on a first-ever write.

**Sync-failure policy.** On `sync_data` failure: fail the operation, apply nothing, and **do not
retry-and-trust** — on Linux a failed `fsync` may clear the error so a retry returns success while the
data is gone.

**Cost budget.** Revision 1 cited "hybrid-embed `kb_add` p95 ~357 ms" from
`.omc/benches/2026-08-15-profile-verdict.md`. That number is from `bench-interactive.sh:39`, which
runs `kb search` — a read path that never appends. **There is no write-path benchmark in this repo**:
all five hyperfine lanes are read surfaces and all four criterion targets (`Cargo.toml:37-51`) are
search/verification. The citation is withdrawn. The same verdict file also calls that measurement
contaminated ("an invalid cold/warm inversion"), which is conservative for a ceiling and exactly
backwards for a denominator.

Therefore: **T1b builds a `kb add` write-path lane first, and the budget is set against its measured
baseline before T2b can close.** The provisional gate is an **absolute** one — `kb add` p95 regression
≤ 5 ms — because a relative gate against an embedding-dominated number is unfailable by construction
(2 % of 357 ms is 7.14 ms, looser than the 5 ms cap, so the relative clause in revision 1 was
decorative). Note the honest denominator excludes embedding: every other lane runs `KB_NO_EMBED=1`,
and against a no-embed baseline 0.5-3 ms of `fdatasync` is a large relative cost, not noise. If the
measured baseline shows the absolute gate is wrong, T1b's output — not speculation — sets the number.

**Alternative rejected.** *A `durability` config knob* — invites an unsafe default, doubles the test
matrix, and makes the invariant conditional on configuration. Revisit only on measured breach (Q2).

### D3 — Applied cursor: `(generation, offset, tail_sha)` in the DB, transactional with the apply

**Decision.** Three `kb_meta` rows — `applied_log_generation`, `applied_log_offset`,
`applied_log_tail_sha` — written **inside the same SQLite transaction that applies the batch**, so
the cursor and the state it describes commit or roll back together.

`applied_log_offset` is a `committed_len` boundary (D1) and **excludes** the record's terminating
newline is *not* the rule — it is defined as the offset **immediately after** the last committed
byte, i.e. after the `batch_commit` line's newline. Pinning this matters: with the other choice, an
ordinary torn tail puts the cursor one byte past EOF and forces a full rebuild on every crash.

`applied_log_generation` is a monotonic counter bumped by compaction under the same lock as its
rename. Revision 1 used offset + tail-sha alone; that is **strictly weaker than the whole-prefix hash
rebuild already uses** (`rebuild.rs:548-550`) and misses the compaction case it claims to catch:
compaction only removes lines (`compact.rs:190-280`), so if every removed line lies *after* the
cursor, bytes `[0, offset)` and the line ending at `offset` are unchanged, the tail-sha validates,
and recovery replays the *compacted* tail onto a DB that already has the *original* tail applied.
Dropped orphan expires are never re-applied and entries that should be stale stay live — silent
divergence, no error. The generation counter makes that detection O(1) and total.

**Recovery. Reads never recover** (adopted from C2's ADR-7, `open-questions.md` C2-Q1). C2's principle
that a read never changes content and C1's original "recovery fires at all six open sites" cannot both
hold once C2's `open_ro` sets `PRAGMA query_only`. C1 yields, because C2 is right on the merits and
the coverage loss is nil in practice:

- `recover_if_needed(paths, embedder)` is called **from C2's `open_or_init(&Paths) -> Result<()>`**
  (`bd-21ef.2.3` / L1a), which runs at MCP startup and CLI dispatch, and **before every write path**
  (`add.rs:77`, `ingest.rs:126`, `migrate_citations.rs:76`). `open_or_init` returns no connection, so
  nothing escapes C2's open split.
- **Read-only paths do not recover and never take the write lock.** `search.rs:86` and `eval.rs:72`
  instead *detect* that the DB is behind the log and serve what they have plus a one-line stderr
  staleness note naming `kb rebuild`. Detection is the same cursor comparison; only the repair is
  withheld. Under `open_ro` + `PRAGMA query_only` recovery on a read path is not merely misplaced,
  it is impossible.

C2's ADR-7 frames this as "at most two of: (a) reads never change content, (b) reads always see a
log-current DB, (c) reads never take the write lock" and picks (a) and (c). C1 yields (b) for
readers, which is right: a read that blocks on the write lock makes every `kb search` a contention
point, and a read that silently mutates is the defect C2 exists to remove. **The task that carries
this is T4 (`bd-21ef.1.9`)** — C2's plan refers to it by its pre-split name `T2`.

There is no CLI-entry call site — revision 1 claimed seven; there are six, plus 12 references in
`tests/legacy_replay.rs` that the rename touches.

**Transaction ownership** (C2-Q2): **D3 owns the outer transaction and C2's `A1` joins it via a
savepoint.** `unchecked_transaction()` issues `BEGIN DEFERRED` and SQLite rejects a nested
transaction, so if `A1` opened its own the audit path would be a runtime error on every record. The
cursor update is the outermost writer because it is the thing that must commit with the apply.

Under the flock:

| Condition | Action |
|---|---|
| no cursor rows present (every DB today) | full rebuild — this is the migration path; `SCHEMA_VERSION` bumps 2 → 3 (`db.rs:129`) so row 1 fires |
| schema stamp obsolete | full rebuild (today's behaviour) |
| generation ≠ current log generation | full rebuild (log was compacted or rewritten) |
| tail_sha mismatches the log at that offset | full rebuild |
| offset > `committed_len` | full rebuild (legacy pre-fix state, external truncation, or a log restored from backup — **not**, after D2, a reachable power-loss state; revision 1 mislabelled it) |
| log unreadable (`read_events` hard-errors on a malformed *middle* line, `events.rs:307-318`) | defer with a warning, as `rebuild.rs:171-178` already does — do not take down all six entry points |
| `committed_len` > offset | replay the tail from the cursor, then advance it |
| `committed_len` == offset | no-op |

**The cursor does not survive a rebuild swap, so rebuild must write it.** Rebuild replays into a
*fresh* tmp DB whose `kb_meta` receives only `schema_version` (`rebuild.rs:148`) and
`embed_text_mode` (`db.rs:633`); arbitrary `kb_meta` keys are not carried across the rename. Rebuild
must therefore set all three cursor rows to the log's `committed_len` state **inside the Phase-3
lock, before the swap**, or the first `recover_if_needed` after any rebuild takes the cursorless
full-rebuild path and loops. This answers the question C3's planner raised in
`open-questions.md` ("Does `Rebuild`'s swap preserve arbitrary `kb_meta` keys?") — it does not, and
C1 now depends on that answer.

**Cost.** Revision 1 claimed the no-op path is "one `stat` plus one short read." That is withdrawn:
determining `committed_len` requires knowing whether the tail sits inside an open span, so the cost is
O(bytes appended since the last apply) — bounded by recent write volume, not by log size, which is the
honest and still-cheap characterization.

**Every writer must use the cursor.** Revision 1 scoped it to `kb_core::add` only. Six other
production sites append-and-apply — `expire.rs:77`, `stale_check.rs:257`, `test_add.rs:68`,
`run.rs:60`, `mcp.rs:921`, `migrate_citations.rs:302` — and after any of them the cursor is
permanently behind, so *every subsequent open* replays their events. That converts a rare crash gap
into a guaranteed corruption loop. All seven route through one helper that owns append + sync + apply
+ cursor as a unit.

**The embedder must not be inside the transaction.** `apply_event` calls `embedder.embed()` inside its
savepoint at `db.rs:864` (entry text) and `:881` (once per cue, up to 8 — `kb_core.rs:197`). Wrapping
the batch in one outer transaction would hold a SQLite write transaction across up to nine embed
calls — hundreds of ms, growing the WAL, and directly worsening D4's busy-checkpoint problem.
Embeddings are pre-resolved for the batch *before* `BEGIN`. (`with_apply_event_savepoint` itself
composes fine inside a caller-owned transaction — `db.rs:676-693` — that part of revision 1 was
sound.)

**Poison policy (Principle 4).** Without one, a deterministic apply failure — a down embedder, a
malformed record — rolls back the batch, leaves the cursor unadvanced while the log holds a committed
batch, and every one of the six entry points then replays it, fails identically, and fails again: a
single bad event bricks all reads and writes. Recovery therefore quarantines: after K failed attempts
on the same record, move it to a dead-letter sidecar, advance the cursor past it, and report loudly.

**Alternative rejected.** *Sidecar cursor file* — needs its own fsync and a two-phase protocol against
the DB commit, reintroducing the append/apply gap one level up.

### D4 — Rebuild swap: checkpoint, verify, close, rename, unlink

**Decision.** Replace `rebuild.rs:444-449` with the following, all under the Phase-3 flock. The kill
points are **named constants** shared by the code and the tests, so "every kill point" is enumerable
rather than prose:

| # | Step | Kill point |
|---|---|---|
| 1 | `PRAGMA wal_checkpoint(TRUNCATE)` on the live DB, bounded busy retry | `KP_PRE_CHECKPOINT` |
| 2 | Verify the live `-wal` is **zero-length** | `KP_POST_CHECKPOINT` |
| 3 | Drop the live connection; drop the tmp's last connection (clean close unlinks `tmp-wal`/`-shm`); assert no `tmp-wal`; `sync_all()` the tmp DB file | `KP_POST_TMP_SYNC` |
| 4 | `fs::rename(tmp, db)` | `KP_POST_RENAME` |
| 5 | Unlink the stale `-wal`/`-shm` | `KP_POST_UNLINK` |
| 6 | `sync_all()` the containing directory | `KP_POST_DIR_SYNC` |

Step 2 is the real gate, not step 1: `wal_checkpoint(TRUNCATE)` returns `(busy, log, checkpointed)`,
and after a successful truncation both `log` and `checkpointed` read 0 — indistinguishable from a
no-op. The zero-length `-wal` check is what proves the main DB file is self-contained.

Step 3's connection drop is mandatory and was missing from revision 1: SQLite's close path
checkpoints and unlinks `<db>-wal` **by name**, so a close after the rename would act on the new DB's
WAL name. The tmp file is likewise only "the whole database" after its last connection drops, so its
`sync_all` must follow that drop.

**The tmp DB is renamed in WAL mode, not DELETE mode** (settled against C2's ADR-1 residual). Today
rebuild forces `PRAGMA journal_mode=DELETE` on the tmp (`rebuild.rs:379`, `:425`) so the tmp is a
single self-contained file, and the DELETE-mode state that leaves behind is silently self-healed by
`open_db`'s unconditional `PRAGMA journal_mode=WAL` (`db.rs:163`). C2's `open_ro` drops that pragma,
so the self-heal disappears. C2 offered two repairs — set WAL on the tmp before the rename, or make
`open_or_init` a mandatory post-swap step. **C1 takes the first, in its cleanest form: stop forcing
DELETE at all.** Delete both `journal_mode=DELETE` lines, let the tmp stay in the WAL mode `open_db`
already gives it, and finalize by dropping the tmp's last connection — SQLite checkpoints and unlinks
the `-wal`/`-shm` on a clean final close — then assert no `tmp-wal` exists before renaming. WAL mode
is recorded in the database header, so the renamed file is WAL-headered *and* self-contained, and the
next connection creates a fresh WAL.

This is strictly better than the post-swap alternative: a mandatory `open_or_init` after the rename
is a step a crash can skip, leaving the live DB in DELETE mode indefinitely. It also removes the
dependency rather than relocating it — no C2 ordering constraint is created — and it is *less* code
than today.

**Consequence for the step 4 → 5 window, stated because it narrows.** With a WAL-headered DB the
header no longer protects against a stale `-wal`: under the old DELETE-mode scheme SQLite would have
ignored the sidecars outright. Safety now rests entirely on step 2's zero-length guarantee — a
zero-length `-wal` has no valid header, so recovery initialises an empty index and adopts no frames,
and the stale `-shm` is rebuilt by `walIndexRecover` when no live holder exists. **Steps 1-2 are
therefore load-bearing rather than belt-and-braces**, and step 5 is hygiene. Do not weaken the
zero-length check.

**Busy handling: bounded retry, then abort — and the liveness cost is stated, not hidden.** A TRUNCATE
checkpoint needs exclusive WAL access, and **every read path in this repo opens the live DB without
the flock** (`mcp.rs:233, 306, 517, 775, 1065, 1177, 1644, 1731, 1849, 1923, 1974`, plus
`rebuild.rs:118`). Worse, "reader" is a misnomer: every `open_db` runs `ensure_schema`'s `ALTER`s
(`db.rs:310-330`), `sweep_expired_peers` (a `DELETE`), and possibly `stamp_schema_version`. So on a
live MCP server `busy != 0` is the *common* case, and a naive fail-closed makes rebuild — which D3
names as the repair for five of its eight recovery rows — unrunnable exactly when it is needed. C1 is
therefore **safe standalone but not live standalone**: safety comes from never unlinking an undrained
WAL; liveness depends on **C2** closing the unlocked writers. That distinction is recorded in the code
comment, in T5a's acceptance criteria, and as a cross-component dependency edge from C2's
universal-flock task back to T5a. The existing safety comment at `rebuild.rs:434-443` is **amended, not
deleted**, so the assumption stays visible for C2 to discharge.

### D5 — `run_history`: stable keying + log-deterministic retention (**reversed from revision 1**)

**Decision.** Keep `run_history` event-sourced. Three changes:

1. **Idempotent keyed insertion.** Both emitters already carry a stable `run_id` (`run.rs:45`,
   `mcp.rs:915`). Make it the key with `ON CONFLICT DO NOTHING`, replacing the bare `INSERT` at
   `db.rs:946-954`. Legacy events without a `run_id` get a deterministic synthetic key derived from
   the event content plus its ordinal position in the log, so replay stays a pure function of the log.
   `AUTOINCREMENT` renumbering stops mattering because materialization equality is keyed on `run_id`,
   not `id`. Requires the `SCHEMA_VERSION` 2 → 3 bump D3 already needs.
2. **Remove the positional cap from compaction** (`compact.rs:16`, `:217-221`). A 500-record cap is
   the *sole* reason compaction is not materialization-preserving.
3. **If growth must be bounded, bound it log-deterministically** — drop runs older than N days by the
   event's own `ts` — which is a pure function of the log and therefore preserves materialization.

**Why this reverses revision 1.** Revision 1 proposed de-event-sourcing `run_history` and copying it
across rebuild via `ATTACH` + `INSERT … SELECT`. That design was self-contradictory and worse on three
counts:

- **It double-materializes.** It kept the legacy apply arm *and* copied the live table, so after
  replay `tmp.run_history` already holds the legacy rows and the copy inserts them again — a PK
  collision that aborts the rebuild, or a row set that doubles on every rebuild.
- **It breaks disaster recovery and violates Principle 1.** `rebuild.rs:638` documents the case
  rebuild exists for: "DB cleared (e.g. corrupted or missing)". In exactly that case the `ATTACH`
  source is absent and the table is silently, permanently gone. Making one table cache-only
  contradicts "the log is the truth."
- **It missed the dominant emitter.** It named only `run.rs:60`; `mcp.rs:915-921` emits the identical
  event, and §1 identifies the MCP server as the dominant writer. Critical 4 would have stayed open
  after the task closed.

The rejected-alternative reasoning in revision 1 was also a strawman: it argued stable keying "fixes
the id-renumbering half but not the cap half," which is true only if you *keep* the cap. Removing the
cap is the smaller change — no `ATTACH`, no cross-database copy, no rebuild Phase-3 edit, no restated
invariant, no DR regression — and its only cost is unbounded growth of a tiny telemetry table, which
point 3 addresses deterministically.

**Consequence for sequencing:** T3 no longer touches `rebuild.rs`, so it does not serialize behind
T5a. It becomes a **prerequisite of T4** instead, because the non-idempotent `run_history` arm is
precisely what makes cursor-driven replay destructive.

### D6 — Riders

- **R1 — replay timestamp determinism** (lens-1 #5; `db.rs:904`, `:1029`, `:1091`). The expire,
  evidence-add and evidence-expire arms write `updated_at=datetime('now')`, so replaying the same log
  on two days materializes different timestamps and different recency-weighted rankings. Derive
  `updated_at` from the event's `ts`; for legacy events with no `ts`, **leave the existing row value
  untouched** rather than stamping wall-clock. This is not cosmetic: `Materialize` being a *function*
  of the log is an assumption every spec in `.state/agent-kb/tla/` already makes.
- **R2 — the silently-failing ALTER** (lens-1 #6; the `source_weights` ALTER is at **`db.rs:330`** —
  revision 1's `:328, :359` was wrong). SQLite rejects `ADD COLUMN … DEFAULT (datetime('now'))` on an
  existing table; `let _ =` swallows the error, so `open_db` succeeds with a schema differing from a
  fresh DB while the stamp still reads current. Inspect `PRAGMA table_info`, add the column as plain
  nullable `TEXT`, backfill with a constant, propagate unexpected errors, and add a fresh-vs-upgraded
  schema comparison test.

### D7 — Version skew: the downgrade posture (new in revision 2)

`machines_conf` pins `agentic-kb` as a flake input (`flake.nix:192-193`) **and** separately into a
microVM closure (`systems/desktop.nix:395-396`), with three MCP front-ends. A host generation rollback
or a host/VM pin skew therefore puts an **old binary against a framed log**, where it does three
destructive things: its `apply_event` `_ => {}` arm ignores markers and applies every event of an
*uncommitted* span as committed; its repair function appends after a dangling `batch_begin`, cementing
a **mid-log** dangling begin; and its compact silently drops marker lines, promoting an uncommitted
span to committed in the log of record.

Revision 1 had a guardrail forbidding any format version field and no rollback story at all. Revised
posture:

1. **A mid-log dangling `batch_begin` is a hard error in the new reader**, never a silent drop. This
   converts the worst outcome — mass silent loss of everything after the marker — into a loud stop
   with a repairable state.
2. **The documented downgrade procedure is `kb compact` under the new binary, then downgrade.**
   Compact strips markers and emits a marker-free, legacy-readable log by construction. This is the
   rollback story, and it must be in the docs task, not folklore.
3. **`machines_conf` gets a notification** per the existing `conventions/cross-repo/
   evidence-contract-notification` convention, which requires verifying against the *deployed pin*
   rather than merged code.
4. **No `log_format` version line in this epic — RULED 2026-09-04 (Q5).** It protects nothing already
   deployed, since every binary in the field predates it, and it is itself a format change, so it
   buys nothing for this transition; (1) and (2) carry the actual weight. Deferred to P3 follow-up
   `bd-21ef.1.15`, so the *next* format change inherits the question at a point where a
   version-aware reader is already deployed and a version line would genuinely gate a skewed binary.
   The guardrail therefore stands as "no rewrite or migration of the existing log, and no version
   field in C1."

---

## 5. TLA+ gate (spec-first — blocks all implementation)

The bugs are interleavings, not logic errors. Per the repo mandate the spec tasks gate every
implementation task with explicit dependency edges, and TLC must produce **counterexamples against
the current design before any fix lands**.

`InnerGap.tla` is the spec that abstracts the bug away: `AppendBatch` is `jsonl' = jsonl \o
batch_events` in **one atomic step** (`InnerGap.tla:81-85`) — the assumption lens-1 #1 refutes — and
`Rebuild` (`:107-113`) is the only successor to `"crashed"`, so recovery is *forced* rather than
modelled. That is why the suite is green over broken code.

Revision 1 proposed one spec task; it is split into three, because two new modules plus an amendment
plus five counterexamples plus dual configs per CE plus a non-vacuity config per invariant is not one
bead. The invoked precedent (`T0-counterexample.md:3-5`) was **one** module and was itself revised
twice *after* implementation landed — so the "strict pre-implementation gate" claim needs honest
sizing, not a bigger single task.

**Modules.**

- **`DurableBatch.tla`** (T0a) — layer-1 refinement of `InnerGap.tla`. Variables: `log_written`,
  `log_durable`, `db`, `cursor`, `generation`, `phase`. Actions: `AppendLine` (per line, crashable
  between lines), `WriteCommitMarker`, `SyncLog`, `ApplyBatch` (atomic with the cursor update),
  `Crash`, `Open` (cursor-driven recovery), **`TruncateUncommittedTail`**, **`Compact`**,
  **`Rebuild`**. The last three were missing from revision 1 and without them most of the recovery
  table is asserted rather than verified — the truncation in particular is the only action that
  *shortens* the log, and "never removes reader-accepted bytes" is exactly what needs machine
  checking.
- **`RebuildProtocol.tla`** (T0b) — the full three-phase protocol (Phase-1 snapshot at
  `committed_len`, unlocked Phase-2 replay, Phase-3 verify + catch-up + swap) **and** the swap kill
  points. Revision 1 proposed a swap-only `SwapDurability.tla`, which could not express the
  snapshot-inside-a-span defect at all — the highest-severity finding against revision 1.
- **`CrossBatch.tla`** (T0c) — revision 1 said "amend so the invariant is over the durable committed
  log." As written that is a no-op or a rewrite: `CrossBatch.tla` has three variables (`:20`), no
  crash action, and updates log and db in one atomic step (`:53-54, 62-63, 73-74, 82-83`) — there is
  no written-vs-durable distinction to restate over. Honest disposition: **CrossBatch is superseded
  by `DurableBatch` for durability, and is retained unchanged as the coarse-grained boundary
  regression gate.** T0c instead owns the compaction/materialization model behind CE5 and CE7.

**Refinement mapping is a deliverable, not a word.** `InnerGap` has six variables; `DurableBatch` has
six different ones. "Refines" is only meaningful with an explicit `INSTANCE InnerGap WITH …` and a
`PROPERTY Spec_InnerGap` line in the `.cfg`. Without it, leaving `InnerGap` green is precisely the
false-coverage failure `gotchas/tla/compact-spec-fidelity-gap` records.

**Counterexamples.** Each must FAIL against a current-design config and PASS against the fixed-design
config, with the trace recorded in `T0-counterexample.md`.

| CE | Shape | Closes | Owner |
|---|---|---|---|
| CE1 | per-line append, crash after line 1 of 3 → durable log exposes `expire(A)`, DB still live A | Critical 1 | T0a |
| CE2 | apply and sync **unordered** — the config must permit *both* orderings and let TLC find the bad one; forcing the bad order proves only that a hand-picked trace is bad | Critical 2 | T0a |
| CE3 | append + sync succeed, crash before apply, `Open` recovers only on a schema bump → invariant violated permanently | Critical 3 | T0a |
| CE4 | unlink-before-rename, kill in the window → the `db` name resolves to a database missing committed WAL frames | Critical 5 | T0b |
| CE5 | 501 run_history events, compact to 500 → `DB ≠ Materialize(log)`; passes under D5's keyed insertion with no cap | Critical 4 | T0c |
| CE6 | Phase-1 snapshot boundary lands **inside** an open span; Phase-3 offset reader applies its tail lines as standalone → half a batch applied inside the recovery path | D1 / Principle 3 | T0b |
| CE7 | compaction removes only lines *after* the cursor offset → tail_sha still validates → recovery replays the compacted tail onto an already-applied DB | D3 generation counter | T0c |
| CE8 | apply fails deterministically → cursor never advances → unbounded replay. **Temporal property, not an invariant** | D3 poison policy | T0a |

**Non-vacuity is a first-class acceptance criterion.** `AgentKbEvidence.tla` passed over a live
data-loss bug because its invariant was vacuously true. Every new invariant ships with a
deliberately-violating `.cfg` that TLC reports as failing.

**Not restored:** `RebuildSwap.tla`. The `bd-w45u` audit retired it deliberately because it models the
pre-`bd-3mr.9` rebuild protocol; restoring a green spec over code that no longer exists asserts false
coverage.

**Mechanics:** new configs set `CHECK_DEADLOCK FALSE` (bounded models terminate legitimately — the
recovered `CrossBatch.cfg`/`InnerGap.cfg`/`CueBatch.cfg` omit it and exit 11 on a terminal state).
Each sub-task states its own `MaxLogLen` and id-set bounds up front: the precedent already took 3 min
for 159,545 distinct states, and `DurableBatch`'s per-line crash granularity is a materially larger
space. Tooling: `tlaps` + `tlaplus18` from `flake.nix:163-164`.

**Out of model scope, stated explicitly:** interior zero-fill damage (a `data=writeback` or
partially-written block producing a garbage *middle* line). `log_durable` as a prefix of `log_written`
is adequate here only because D2 makes the un-synced region exactly one record — that is a theorem
about D2, and it belongs in the module as a named `ASSUME` with its justification. The code-side
consequence still needs a position: `read_events` hard-errors on a malformed middle line
(`events.rs:307-318`), which would take down all six entry points, so recovery quarantines the
unparseable line to a sidecar and continues rather than failing every command (see Q4).

---

## 6. Guardrails

**Must have**
- Old un-framed logs replay identically, forever.
- TDD: the failing test precedes the implementation file in every task.
- Full-tier suite for landings, `PROPTEST_CASES=256`.
- Every new invariant proven non-vacuous by a deliberately-violating config.
- The `events.rs` regression fence — `test_append_event_never_removes_reader_accepted_tail`,
  `test_append_event_preserves_split_utf8_tail_in_sidecar` — stays green.
- Measured, not asserted, fsync cost against a **write-path** baseline that T1b creates.
- Named kill-point constants shared by code and tests.

**Must NOT have**
- No rewrite or migration of the existing log, and **no `log_format` version field in C1** (Q5 ruled
  defer, 2026-09-04; carried as `bd-21ef.1.15` for the next format change).
- No durability config knob.
- No restoration of `RebuildSwap.tla`.
- No embedder call inside a held write transaction.
- No recovery path that can retry a deterministically failing record without bound.
- No compaction mutating the live DB.
- No hard dependency on C2 landing first — state the liveness assumption instead.

**Cross-component items — three, all flagged rather than absorbed:**

1. **C2 `L1a` (`bd-21ef.2.3`) → T4 (`bd-21ef.1.9`)** — edge wired in beads. `L1a` lands on the
   aggregator first; T4 rebases onto it. T4 calls `recover_if_needed` *from* `open_or_init`, and takes
   C2's constraint as binding: **T4 does not touch `open_db`'s body; the cursor write is confined to
   `apply_event`, already under the lock on every locked path.** All three components serialize on
   `L1a`, which makes it the highest-leverage task in the meta-epic.
2. **C2-Q1 (reads never recover) — SETTLED, C1 yields.** Resolved in D3; recovery lives in
   `open_or_init` and write paths, readers detect and warn.
3. **C2-Q2 (outer transaction ownership) — SETTLED.** D3 owns the outer transaction; C2's `A1` joins
   via savepoint. Recorded in D3 and in T4's beads description.
4. **C2 ADR-1 residual (WAL self-heal) — SETTLED in C1's favour of removing the dependency.** C2's
   `open_ro` drops the `PRAGMA journal_mode=WAL` that currently self-heals the DELETE-mode DB
   rebuild's rename leaves behind. Rather than relocating the heal to a crash-skippable post-swap
   step, T5a renames the tmp in WAL mode (D4). No C2 ordering constraint is created.
5. **C2's universal flock → T5a liveness** (D4), as an edge from C2's flock task back to T5a.

**Withdrawn:** revision 2 flagged a collision with C3 on `read_events` / `read_events_up_to` /
`read_events_from_offset`. It does not exist — C3's decode-error sites are in `db.rs` (`:1354`,
`:1413`, `:1453`, `:1843`, `:1896`), not `events.rs`. Withdrawn before it cost a round trip.

---

## 7. Task flow

```
T0a DurableBatch spec ──┬──→ T2a framing/readers ──→ T2b fsync ordering ──┐
T1a crash harness ──────┘         ↑                        ↑              │
T1b kb-add bench lane ────────────┼────────────────────────┘              ├──→ T4 applied cursor
T0c compaction spec ──→ T3 run_history keying ────────────────────────────┘         │
T0b RebuildProtocol ──→ T5a swap sequence ──→ T5b Phase-3 cursor write ←─────────────┘
T0a ──────────────────→ T6 riders
                        (T2b, T3, T4, T5b, T6) ──→ T8 post-impl ──→ T7 docs
```

### T1a — Crash-simulation harness  · P1 · blocks T2a, T4, T5a
**There is none today**: `grep` for `fail_point|failpoint|KB_CRASH|fault_inject|simulate_crash`
across `src/` and `tests/` returns zero hits, and the dev-dependencies (`Cargo.toml:53-57`) are
`libc`, `criterion`, `proptest`, `tempfile`. Without this task, every crash-safety criterion below is
unfalsifiable and satisfiable by a unit test that calls the steps manually and stops early.
Build it as either `libc::fork()` + `_exit(1)` at a `#[cfg(test)]` seam (letting the parent inspect
on-disk state — `libc` is already a dev-dep) or a `KB_CRASH_AFTER=<label>` seam checked at each named
kill point.
**Acceptance:** kill points are named constants referenced by both code and tests; a demonstration
test kills at a chosen point and the parent observes the on-disk state; the harness is a no-op in
release builds.

### T1b — `kb add` write-path benchmark lane  · P1 · blocks T2b
Add a write-path lane to `scripts/bench-interactive.sh` and a criterion target for in-process
attribution, and record the pre-change baseline.
**Acceptance:** the lane measures `kb add` (not a read path), reports a p50/p95 against the
`kb-bench-fixture` corpus, runs with `KB_NO_EMBED=1` for the honest denominator, and the baseline
number is recorded in the task before T2b changes any code.

### T0a — Spec: `DurableBatch.tla` + CE1, CE2, CE3, CE8  · P1 · blocks T2a, T4, T6
**Acceptance:** each CE has a config TLC reports as **violating** against the current-design model and
**passing** against the fixed model, traces recorded; CE8 is a temporal property; every invariant has
a violating config proving non-vacuity; the `INSTANCE InnerGap WITH …` refinement mapping and its
`PROPERTY` line exist; model bounds stated; code-reviewer and analyst audits recorded.

### T0b — Spec: `RebuildProtocol.tla` + CE4, CE6  · P1 · blocks T5a
Models the full three-phase protocol including the Phase-1 snapshot boundary and the six named swap
kill points.
**Acceptance:** as T0a. CE6 must demonstrate the mid-span snapshot defect, which no swap-only model
can express.

### T0c — Spec: compaction/materialization + CE5, CE7  · P1 · blocks T3, T4
**Acceptance:** as T0a; plus an explicit statement of `CrossBatch.tla`'s disposition (retained
unchanged as the boundary regression gate, superseded for durability).

### T2a — Framing, span-aware readers, `committed_len`, repair  · P1 · deps T0a, T1a
`events.rs`: envelope every append; `committed_len` in `ReadEvents`; span-aware readers with the D1
rule table; mid-log dangling begin as a hard error; markers never returned as events;
`repair_uncommitted_tail_before_append` with best-effort sidecar and a structural flock precondition;
`compact.rs` strips markers.
**Acceptance:** failing tests first — an injected mid-batch write failure (T1a) leaves zero
reader-accepted events from that batch; a body-written/newline-failed single append is **not**
promoted on the next append; a mid-log dangling begin errors rather than dropping; the 116-event real
corpus plus a synthetic corpus covering every event kind both replay byte-identically; the two named
torn-tail tests pass; ENOSPC still permits span truncation.

### T2b — fsync ordering  · P1 · deps T2a, T1b
`sync_data` before any DB write, with the stated failure policy; directory fsync with creation
detected *before* the repair call; `compact.rs` gains its directory fsync.
**Acceptance:** failing test first — the sync precedes the first `apply_event` (ordering asserted);
a sync failure fails the operation, applies nothing, and does not retry-and-trust; `kb add` p95
within the T1b-derived budget, with the measured number recorded in the task.

### T3 — `run_history` stable keying  · P1 · deps T0c
Keyed idempotent insertion on `run_id` at `db.rs:946`; deterministic synthetic key for legacy events;
remove the compaction cap (`compact.rs:16`, `:217-221`); both emitters (`run.rs:45`, **`mcp.rs:915`**)
verified to carry `run_id`; `SCHEMA_VERSION` 2 → 3.
**Why this gates T4** — two independent reasons. (a) A non-idempotent `run_history` arm makes
cursor-driven replay destructive: one duplicated row per open, unbounded. (b) Version skew (D7): an
old binary appends-and-applies *without touching the cursor*, so the new binary's recovery replays
events already in the DB — and because the log itself is unchanged, neither the tail_sha nor the
generation counter rules that out. Idempotent replay is the only thing that does.
**Acceptance:** failing test first — 501 runs, compact, rebuild, and the invariant holds unrestated;
replaying the same log N times produces N-invariant `run_history`; a legacy log with `run_id`-less
events replays deterministically; the FK to `test_cases` holds.

### T4 — Applied cursor + automatic recovery  · P1 · deps T2b, T3, T1a
`(generation, offset, tail_sha)` in `kb_meta`, written in the same transaction as the apply;
embeddings pre-resolved before `BEGIN`; one helper owning append+sync+apply+cursor, routed through by
**all seven** writers; `recover_if_needed` with the eight-row table from D3, fired at process entry
and write paths only (reads detect and warn — D3/C2-Q1), with the 12 `tests/legacy_replay.rs`
references updated; poison/quarantine policy. **Constraint from C2:** does not touch `open_db`'s
body; the cursor write stays inside `apply_event`.
**Acceptance:** failing test first — kill after append+sync and before apply (T1a), reopen, DB
converges with no manual `kb rebuild`; the same for each of the seven writers, enumerated not sampled;
**every `apply_event` arm is shown idempotent under replay, enumerated** — not just `run_history`, so
a second non-idempotent arm cannot silently pass; a read-only path serves stale data with a warning
and never takes the write lock; a compacted log bumps the generation and triggers a full rebuild; a
cursorless DB takes the schema-bump rebuild path; an unreadable log defers with a warning instead of
failing every entry point; a deterministically failing record is quarantined after K attempts and the
cursor advances; no write transaction is held across an embedder call (asserted).

### T5a — Rebuild swap sequence  · P1 · deps T0b, T1a
The six-step D4 sequence with named kill points; bounded busy retry then abort; live connection
dropped before rename; **the tmp renamed in WAL mode** — remove `journal_mode=DELETE` at
`rebuild.rs:379` and `:425`, finalize by dropping the tmp's last connection and asserting no
`tmp-wal` remains; amend rather than delete the `rebuild.rs:434-443` safety comment with the
liveness-depends-on-C2 assumption explicit.
**Acceptance:** failing test first — a kill at each of the six named points leaves a self-contained DB
containing every committed transaction; a persistently busy checkpoint aborts with a clear error and
leaves the live DB untouched; **the renamed DB is WAL-headered with no `-wal` present, and a
subsequent `open_ro` (no `journal_mode` pragma) reads it correctly** — this is the regression test for
the self-heal C2's `open_ro` removes; the existing Phase-3 timing instrumentation still records a
bounded swap window; the C2 cross-component dependency edge exists in beads.

### T5b — Phase-3 cursor write  · P1 · deps T5a, T4
Rebuild writes the D3 cursor rows into the tmp DB inside the Phase-3 lock before the swap, since
`kb_meta` keys do not survive the rename.
**Acceptance:** failing test first — a rebuild followed by an immediate `recover_if_needed` is a
no-op rather than a second full rebuild.

### T6 — Riders: replay ts determinism + `source_weights` ALTER  · P2 · deps T0a
R1 and R2 from D6.
**Acceptance:** failing test first — replaying the same log twice, hours apart, produces identical
`updated_at` on every touched row; legacy events with no `ts` leave the row value unchanged; a fresh
DB and an upgraded DB produce identical `PRAGMA table_info` for `source_weights`; an unexpected
migration error propagates.

### T7 — docs  · P2 · blocked by T8
Log format and envelope semantics; the recovery protocol and its eight cases; the positively-stated
invariant; **the D7 downgrade procedure** (`kb compact`, then downgrade); the measured fsync cost.

### T8 — post-impl  · P1 · deps T2b, T3, T4, T5b, T6 · blocks T7
`/post-impl` gates: sync + rebase, spec compliance (`/verify` Covered), security review if triggered,
code review loop to zero Critical, user confirmation. Cascade component → `storage-correctness-2`
aggregator. Includes the `machines_conf` notification from D7 §3.

---

## 8. Success criteria

1. CE1-CE8 each fail against the current design in TLC and pass against the fixed design, traces in
   `T0-counterexample.md`, every invariant non-vacuous.
2. A kill at every **named** kill point in append, apply, and swap leaves a state from which the next
   open converges to `materialized_tables(DB) == Materialize(committed log)` with no human action and
   without unbounded retry.
3. The real 116-event corpus **and** a synthetic corpus covering every event kind replay
   byte-identically under the new reader.
4. `kb add` p95 regression measured against the T1b write-path baseline and within its budget.
5. Full-tier suite green at `PROPTEST_CASES=256`; zero Critical findings at post-impl code review.

## 9. Pre-mortem

1. **Framing breaks the torn-tail contract.** Span truncation and torn-tail repair both mutate the
   tail; a naive composition double-handles it or removes reader-accepted bytes. *Mitigation:* the two
   named regression tests are the fence; truncation only ever removes a span the reader already drops;
   `TruncateUncommittedTail` is modelled in T0a rather than argued.
2. **The fsync cost is worse than budgeted** — btrfs COW amplification is the plausible surprise, and
   against a no-embed denominator the relative cost is large. *Mitigation:* T1b establishes the
   baseline before T2b changes code, so the budget is derived, not assumed; breach reopens Q2 with data.
3. **The cursor and the swap disagree.** Rebuild replaces the DB wholesale; if the new DB's cursor is
   not set correctly under the same lock, the next `recover_if_needed` replays from zero or skips real
   events. *Mitigation (corrected — revision 1 offered "sequence T4 after T5", which is not a
   mitigation for a semantic disagreement):* the generation counter makes a post-swap cursor mismatch
   *detectable* rather than silent, and `Rebuild` is an explicit action in `DurableBatch.tla` so the
   post-swap cursor state is model-checked, not sequenced around.
4. **Version skew corrupts the log** — an old binary from a generation rollback or the VM pin writes
   into a framed log (D7). *Mitigation:* mid-log dangling begin is a hard error, the documented
   downgrade path is `kb compact` first, and `machines_conf` is notified per the cross-repo convention.
5. **T0's state space explodes.** Per-line crash granularity over a log-length-bounded model is much
   larger than the 159,545-state precedent. *Mitigation:* each spec sub-task states its bounds up
   front and the split into T0a/T0b/T0c keeps any one model tractable.
6. **The recovery path becomes the outage.** `recover_if_needed` runs at six entry points; any bug in
   it — poison loop, spurious full rebuild, hard error on an unreadable log — takes down every command
   at once. *Mitigation:* the defer-with-warning row, the quarantine policy, and CE8's temporal
   property all exist specifically for this; T4's acceptance enumerates them.

## 10. Open questions

Tracked in `.state/.omc/plans/open-questions.md` (C1 section). **Q1 and Q5 are both resolved**
(2026-09-04): the meta-epic aggregator was initialized, unblocking worktree creation, and the
`log_format` version line is deferred to `bd-21ef.1.15`. Q2 (fsync budget) and Q3 (`run_history`
growth bound) remain open by design — both are measurement-driven and `T1b` answers Q2.

## 11. What changed in revision 2, and why

Recorded so the reasoning survives; both reviews are the evidence.

| # | Revision 1 said | Revision 2 says | Why |
|---|---|---|---|
| 1 | Envelope batches only; single `writeln!` is self-framing | Envelope everything | Self-framing holds for a *crash*, not a *write error*: a failed newline write leaves complete JSON that the next append promotes to committed (`events.rs:218-220`), across nine call sites |
| 2 | Snapshot/cursor offsets unchanged | All boundary offsets are `committed_len`; offset readers assert span boundaries | Otherwise a snapshot inside an open span makes Phase 3 apply half a batch — Critical 1 inside the recovery path |
| 3 | Cursor = offset + tail_sha | + a generation counter bumped by compact | Compaction that only removes lines *after* the cursor leaves the tail_sha valid; recovery then replays a compacted tail onto an applied DB, silently |
| 4 | Cursor wired into `kb_core::add` | One helper, all seven writers | Six other writers would leave the cursor permanently behind, replaying on every open |
| 5 | Wrap the apply loop in one transaction | Same, but embeddings pre-resolved before `BEGIN`, plus a poison policy | The embedder is called inside the savepoint (`db.rs:864`, `:881`); a held txn across nine embeds worsens D4, and a deterministic failure would brick all six entry points |
| 6 | De-event-source `run_history`, copy across rebuild | Stable keying on the existing `run_id`, drop the cap | The copy double-materializes against the legacy apply arm, loses the table when the DB is missing (the case rebuild exists for), violates Principle 1, and missed the `mcp.rs:915` emitter |
| 7 | Fail closed on a busy checkpoint | Bounded retry, then abort, with the liveness cost stated | Every `open_db` writes and no read path takes the flock, so busy is the common case — fail-closed makes rebuild unrunnable exactly when needed |
| 8 | Budget vs. "357 ms `kb_add` p95" | Budget derived from a new write-path lane (T1b) | That number is `kb search`; no write-path benchmark exists, and the cited run is self-described as contaminated |
| 9 | Crash criteria assumed a harness | T1a builds one, with named kill points | Zero fault-injection hits in `src/` or `tests/` |
| 10 | "~21k-line corpus" | 116 events / 180,008 bytes, plus a synthetic corpus | The 21k was a byte range misread as lines |
| 11 | One spec task | T0a / T0b / T0c, plus `Truncate`/`Compact`/`Rebuild` actions and CE6-CE8 | Two modules + amendment + five CEs + dual configs is not one bead; and no proposed module could express the mid-span snapshot defect |
| 12 | No format version field, no rollback story | D7: hard error on mid-log dangling begin, `kb compact` as the downgrade path, machines_conf notified | `machines_conf` pins the flake twice; skew is a real, silent corruption path |
| 13 | T4 after T5 (avoid a merge conflict) | T3 before T4 (data dependency) | The regions are adjacent, not overlapping; the real dependency is that non-idempotent `run_history` makes replay destructive |
| 14 | `DB \ {run_history} == Materialize(log)` | Positively-stated materialized set | Several tables are already DB-native, so "DB minus one table" is false |
| 15 | `source_weights` ALTER at `db.rs:328`/`:359` | `db.rs:330` | Verified |
| 16 | 7 `rebuild_if_schema_obsolete` call sites | 6, plus 12 test references | Verified; there is no CLI-entry call |
