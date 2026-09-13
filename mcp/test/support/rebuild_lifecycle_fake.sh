#!/usr/bin/env bash
# Controlled direct-child fixture for RebuildManager lifecycle tests. It records
# the real OS pid and argv, then either waits for inherited stdin EOF, fails, or
# emits more than the retained-log budget. It intentionally does not emulate
# the long-lived MCP JSON protocol.
set -euo pipefail

: "${REBUILD_PID_FILE:?REBUILD_PID_FILE must name the PID capture file}"
: "${REBUILD_ARGS_FILE:?REBUILD_ARGS_FILE must name the argv capture file}"

printf '%s\n' "$$" > "$REBUILD_PID_FILE"
printf '%s\n' "$*" > "$REBUILD_ARGS_FILE"

case "${REBUILD_FIXTURE_MODE:-hold}" in
  hold)
    cat >/dev/null
    ;;
  fail)
    printf 'controlled rebuild failure\n' >&2
    exit 42
    ;;
  flood)
    head -c 131072 /dev/zero | tr '\0' x
    cat >/dev/null
    ;;
  *)
    printf 'unknown REBUILD_FIXTURE_MODE: %s\n' "$REBUILD_FIXTURE_MODE" >&2
    exit 64
    ;;
esac
