# Downgrade Procedure

## Format boundary

This transition adds no `log_format` record: the writer emits only begin,
event, and commit lines, while marker-free legacy lines remain standalone
committed events. `write_span`, `scan_events`

A mixed-version writer can turn an open tail into a mid-log framing violation.
The current reader hard-stops on a nested begin or more event lines than the
declared count; it never silently drops the middle region. `scan_events`,
`test_mid_log_dangling_begin_is_a_hard_error`,
`test_more_event_lines_than_declared_n_is_a_hard_error`

## Why no version line

A `log_format` version line was considered and rejected: every binary already
deployed predates it, so a version line protects nothing already in the
field, and adding one is itself a format change — it buys nothing for this
transition. The downgrade posture in this document carries the weight
instead. This ruling (2026-09-04, C1 question Q5) is tracked as bd-21ef.1.15,
which revisits adding a version line before the next format change.

## Old-binary read behavior

An old binary's `apply_event` matches on `(action, table)`. `batch_begin` and
`batch_commit` marker lines carry no `table` field, so they fall through to
its wildcard arm — `_ => {} // unknown event — skip silently` — and are
skipped, not rejected. The old reader has no concept of a span: it returns
every line, marker or not, as a candidate event, and the old binary applies
each recognized one as it is encountered, with no equivalent of
`committed_len` or span-completion checking.

A batch that crashes before its `batch_commit` line reaches disk — a torn
batch — is therefore not rejected by an old binary. Instead of the new
reader's hard span boundary, the old binary silently applies whatever event
lines already landed, with no atomicity: a torn batch becomes a silently
partial apply rather than a hard stop.

Run `kb compact` under the new binary before any downgrade so the log an old
binary reads carries no markers, and confirm with the new reader that none
remain (step 3 below).

## Procedure

1. Stop other writers; compact holds the event-log lock during its rewrite. `Compact::execute_with_paths`
2. Run `kb compact` with the new binary. It reads committed events with the span-aware reader, writes only event objects to a temporary JSONL file, syncs it, bumps generation, renames it over the log, and syncs the parent directory. `Compact::execute_with_paths`
3. Read the rewritten log with the new reader and confirm no raw line has action `batch_begin` or `batch_commit`. `test_compact_emits_a_marker_free_legacy_readable_log`
4. Only then replace the binary with the older release. The resulting log contains legacy standalone event lines. `test_compact_emits_a_marker_free_legacy_readable_log`

Do not delete markers in place: only the span-aware reader identifies which
event lines committed, and compact also supplies the durable replacement and
generation update. `scan_events`, `Compact::execute_with_paths`

## Deployment note: fleet pins

The deployment repository (machines_conf) pins this crate in two places: the
`agentic-kb` flake input in `flake.nix`, and the microVM closure in
`systems/desktop.nix`. Both pins must move together — it is the deployed
pin, not merged code, that determines which reader the fleet actually runs.
