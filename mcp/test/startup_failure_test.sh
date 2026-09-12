#!/usr/bin/env bash
set -euo pipefail

mcp_bin=${1:?usage: startup_failure_test.sh /path/to/agentic-kb-mcp}
db_path=$(mktemp)
trap 'rm -f "$db_path"' EXIT

set +e
output=$(KB_DB_PATH="$db_path" KB_BIN=/definitely/missing/kb "$mcp_bin" </dev/null 2>&1)
status=$?
set -e

if [[ $status -ne 1 ]]; then
  printf 'startup failure must exit 1, got %s:\n%s\n' "$status" "$output" >&2
  exit 1
fi

grep -F 'agentic-kb-mcp failed to start:' <<<"$output" >/dev/null
