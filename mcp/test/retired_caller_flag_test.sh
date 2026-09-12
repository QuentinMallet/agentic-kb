#!/usr/bin/env bash
set -euo pipefail

# Option B removes launch-time identity.  This is intentionally a process
# boundary test: the CLI must reject the retired flag before application
# startup, so it does not rely on a test-only parser API.
mcp_bin=${1:?usage: retired_caller_flag_test.sh /path/to/agentic-kb-mcp}
timeout_bin=$(command -v timeout)

set +e
output=$(env -i PATH=/usr/bin:/bin "$timeout_bin" 5s "$mcp_bin" --caller-id retired </dev/null 2>&1)
status=$?
set -e

if [[ $status -eq 0 || $status -eq 124 ]]; then
  printf 'retired --caller-id was accepted or started the server (exit %s):\n%s\n' "$status" "$output" >&2
  exit 1
fi

grep -F -- '--caller-id is no longer supported' <<<"$output" >/dev/null
