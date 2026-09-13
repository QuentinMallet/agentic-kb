#!/usr/bin/env bash
# Controlled direct-child fixture for RebuildManager lifecycle tests. It records
# the real OS pid and argv, then either waits for inherited stdin EOF, fails, or
# emits more than the retained-log budget. It intentionally does not emulate
# the long-lived MCP JSON protocol.
set -euo pipefail

: "${REBUILD_PID_FILE:?REBUILD_PID_FILE must name the PID capture file}"
: "${REBUILD_ARGS_FILE:?REBUILD_ARGS_FILE must name the argv capture file}"
: "${REBUILD_LAUNCH_FILE:?REBUILD_LAUNCH_FILE must name the launch capture file}"
: "${REBUILD_COMPLETED_FILE:?REBUILD_COMPLETED_FILE must name the completion marker}"

printf '%s\n' "$$" > "$REBUILD_PID_FILE"
printf '%s\n' "$*" > "$REBUILD_ARGS_FILE"
start_time=$(awk '{print $22}' "/proc/$$/stat")
printf '%s %s\n' "$$" "$start_time" >> "$REBUILD_LAUNCH_FILE"

case "${REBUILD_FIXTURE_MODE:-hold}" in
  hold)
    # A lifecycle manager must close stdin and wait for this child to exit;
    # ignoring TERM prevents a test from mistaking a signal send for observed
    # process termination. Production's Rust EOF guard exits promptly.
    trap '' TERM
    cat >/dev/null
    sleep "${REBUILD_EOF_EXIT_DELAY:-0}"
    : > "$REBUILD_COMPLETED_FILE"
    ;;
  fail)
    printf 'controlled rebuild failure\n' >&2
    : > "$REBUILD_COMPLETED_FILE"
    exit 42
    ;;
  flood)
    printf 'flood-started\n'
    head -c 131072 /dev/zero | tr '\0' x
    : > "$REBUILD_COMPLETED_FILE"
    ;;
  *)
    printf 'unknown REBUILD_FIXTURE_MODE: %s\n' "$REBUILD_FIXTURE_MODE" >&2
    exit 64
    ;;
esac
