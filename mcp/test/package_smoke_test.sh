#!/usr/bin/env bash
set -euo pipefail

# REG-6f32a0b-packaged-mcp-startup: The installed MCP package must include all
# executables needed for a clean-PATH launch, then answer the two mandatory
# protocol discovery requests.
mcp_bin=${1:?usage: package_smoke_test.sh /path/to/agentic-kb-mcp}
timeout_bin=$(command -v timeout)
request_initialize='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"package-smoke-\u00e9","version":"1"}}}'
request_tools='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
stdout_file=$(mktemp)
stderr_file=$(mktemp)
trap 'rm -f "$stdout_file" "$stderr_file"' EXIT

set +e
env -i PATH=/usr/bin:/bin \
  "$timeout_bin" 5s "$mcp_bin" \
  < <(printf '%s\n%s\n' "$request_initialize" "$request_tools"; sleep 1) \
  >"$stdout_file" 2>"$stderr_file"
status=$?
set -e
output=$(<"$stdout_file")
stderr=$(<"$stderr_file")

if [[ $status -ne 0 ]]; then
  printf 'MCP package failed to start (exit %s):\nstdout:\n%s\nstderr:\n%s\n' \
    "$status" "$output" "$stderr" >&2
  exit 1
fi

# Decode actual stdout rather than matching JSON-looking text. Under a clean
# package PATH, `IO.puts/1` used the standard device's Latin-1 encoding and
# emitted invalid `\\x{...}` sequences for Unicode in tool descriptions.
python3 -c '
import json
import sys

responses = [json.loads(line) for line in sys.stdin.buffer.read().splitlines()]
assert [response["id"] for response in responses] == [1, 2]
assert all(response["jsonrpc"] == "2.0" and "result" in response for response in responses)
assert responses[0]["result"]["protocolVersion"] == "2024-11-05"
tools = responses[1]["result"]["tools"]
assert [tool["name"] for tool in tools] == [
    "kb_search", "kb_add", "kb_cite", "kb_import", "kb_stale_check", "kb_expire",
    "kb_run", "kb_test_add", "kb_tests", "kb_reembed", "kb_compact", "kb_rebuild",
    "kb_audit_run", "kb_audit_record", "kb_audit_report", "kb_provenance", "kb_get",
]
assert any("—" in tool["description"] for tool in tools)
' <<<"$output"

if grep -Eqi 'authorization denied|policy_unavailable|opa eval' <<<"$output$stderr"; then
  printf 'MCP package emitted retired authorization runtime output:\nstdout:\n%s\nstderr:\n%s\n' \
    "$output" "$stderr" >&2
  exit 1
fi
