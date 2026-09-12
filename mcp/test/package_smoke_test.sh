#!/usr/bin/env bash
set -euo pipefail

# REG-6f32a0b-packaged-mcp-startup: The installed MCP package must include all
# executables needed for a clean-PATH launch, then answer the two mandatory
# protocol discovery requests.
mcp_bin=${1:?usage: package_smoke_test.sh /path/to/agentic-kb-mcp}
timeout_bin=$(command -v timeout)
request_initialize='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"package-smoke","version":"1"}}}'
request_tools='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'

set +e
output=$(env -i PATH=/usr/bin:/bin \
  "$timeout_bin" 5s "$mcp_bin" \
  < <(printf '%s\n%s\n' "$request_initialize" "$request_tools"; sleep 1) 2>&1)
status=$?
set -e

if [[ $status -ne 0 ]]; then
  printf 'MCP package failed to start (exit %s):\n%s\n' "$status" "$output" >&2
  exit 1
fi

# Each response must correlate to the request id and carry the expected MCP
# result surface. This rejects unrelated log lines or a response for only one
# request.
grep -E '^\{"id":1,"jsonrpc":"2\.0","result":.*"protocolVersion"' <<<"$output" >/dev/null
grep -E '^\{"id":2,"jsonrpc":"2\.0","result":\{"tools":' <<<"$output" >/dev/null

if grep -Eqi 'authorization denied|policy_unavailable|opa eval' <<<"$output"; then
  printf 'MCP package emitted retired authorization runtime output:\n%s\n' "$output" >&2
  exit 1
fi
