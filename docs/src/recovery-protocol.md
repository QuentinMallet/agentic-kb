# Recovery Protocol

## Applied cursor

Three `kb_meta` rows form the applied cursor and commit in the same SQLite
transaction as the batch they describe. `write`, `append_and_apply_with`

| Field | Meaning |
|---|---|
| `generation` | Log-generation sidecar value; compaction bumps it under the log lock before replacement. `read_generation`, `bump_generation`, `Compact::run` |
| `offset` | `committed_len` immediately after the last committed log byte applied. `Cursor`, `test_add_writes_all_three_cursor_rows_in_the_apply_transaction` |
| `tail_sha` | Bounded fingerprint of `[0, offset)`, binding its length and windows anchored at the prefix's beginning and end. `tail_sha` |

## Nine recovery rows

`inspect` applies this ordered classification. Reads warn and serve the current
database without repairing it. `warn_if_behind`

| Row | Condition | Reads, recovery, writes, and compact |
|---:|---|---|
| 1 | Any cursor row is absent or unparsable. | `FullRebuild(CursorMissing)`: reads continue; recovery rebuilds; writes and compact block until recovery. `read`, `inspect`, `blocks_writes` |
| 2 | Cursor and log agree, but the schema stamp is obsolete. | `FullRebuild(SchemaObsolete)`: reads continue and recovery rebuilds when possible. This is the only rebuild decision that does not itself block writes or compact, and it is checked last. `inspect`, `blocks_writes`, `full_rebuild_for` |
| 3 | Cursor generation differs from log generation. | `FullRebuild(GenerationMismatch)`: reads continue; recovery rebuilds; writes and compact block until recovery. `inspect`, `blocks_writes` |
| 4 | The fingerprint at `offset` differs from `tail_sha`. | `FullRebuild(TailShaMismatch)`: reads continue; recovery rebuilds; writes and compact block until recovery. `inspect`, `blocks_writes` |
| 5 | `offset > committed_len`. | `FullRebuild(OffsetBeyondLog)`: reads continue; recovery rebuilds; writes and compact block until recovery. `inspect`, `blocks_writes` |
| 6 | Cursor read, committed-length read, prefix hash, or parsing past the cursor fails. | `Defer`: reads continue; recovery warns and declines rebuild; writes and compact refuse. `inspect`, `recover_if_needed`, `test_compact_refuses_unless_the_database_is_converged` |
| 7 | `committed_len > offset` and the tail is readable. | `ReplayTail`: reads continue; recovery applies the tail and advances the cursor; writes and compact block while behind. `kb add` first calls recovery and auto-recovers this row, while `kb compact` refuses it. `inspect`, `recover_if_needed`, `Add::run`, `Compact::run` |
| 8 | `committed_len == offset`, fingerprint agrees, and schema is current. | `NoOp`: reads, writes, recovery checks, and compact proceed. `inspect`, `blocks_writes` |
| 9 | Log is absent while a present cursor has nonzero offset or the database has entries. | `LogMissing`: reads continue; recovery warns and declines rebuild; writes and compact refuse, preventing creation of a one-batch replacement log. `inspect`, `recover_if_needed`, `test_compact_refuses_unless_the_database_is_converged` |

No database means recovery has nothing to converge. No log with an empty
database also does not meet row 9's condition. `recover_if_needed`, `inspect`

## Rebuild swap

Rebuild snapshots a committed prefix under the lock, replays it into a scratch
database without the lock, then reacquires the lock, verifies prefix identity,
and applies the committed tail. A changed prefix restarts the attempt.
`Rebuild::execute_with`

The live-file swap has six ordered steps:

1. Checkpoint the live database with bounded `wal_checkpoint(TRUNCATE)` retries; persistent busy state aborts before changing the live database. `checkpoint_live_db`, `test_swap_aborts_cleanly_when_checkpoint_stays_busy`
2. Require the live `-wal` to be absent or zero length. `verify_live_wal_drained`
3. Drop the checkpoint connection and finalize the scratch database. `finalize_tmp_db`, `Rebuild::execute_with`
4. Atomically rename scratch over the live database. `Rebuild::execute_with`
5. Remove stale `-wal` and `-shm` sidecars from the replaced inode. `Rebuild::execute_with`
6. Sync the containing directory to make the rename durable. `sync_parent_dir`, `Rebuild::execute_with`

At every instrumented crash point, a plain reader sees a structurally intact
database with exactly the complete pre-swap or complete post-swap state, never
a mixture. `assert_swap_kill_leaves_self_contained_db`,
`test_swap_kill_at_pre_checkpoint_leaves_self_contained_db`,
`test_swap_kill_at_post_dir_sync_leaves_self_contained_db`

## Compaction and fresh clones

Because compact rewrites the log and increments its generation, it first opens
an existing database read-only and applies `blocks_writes`. Unlike `kb add`, it
does not auto-replay a behind tail. `Compact::run`,
`test_compact_refuses_unless_the_database_is_converged`

When no database exists, compact rewrites the log without opening a read-write
database or creating the schema. It never materializes a fresh clone's
database. `Compact::run`,
`kb_compact_on_a_fresh_repo_stays_a_pure_log_rewrite`

## Materialization invariant

Replaying the committed log reproduces `entries`, `test_cases`, `evidence`,
`cues`, `entries_fts`, `entries_emb`, and `run_history`. `materialized`,
`test_every_apply_event_arm_is_idempotent_under_replay`

Database-native tables outside this invariant are `kb_meta`, `audit_runs`,
`source_weights`, `audit_run_candidates`, `graphs`, `peers`, and
`fts5_deprecation_gate`. `ensure_schema`

`entries_fts_v2` is maintained from `entries` by SQLite triggers rather than a
separate event arm. `ensure_schema`
