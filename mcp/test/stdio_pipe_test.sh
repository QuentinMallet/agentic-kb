#!/usr/bin/env bash
set -euo pipefail

# This is deliberately an OS-pipe test, not only a framer unit test. The
# writer keeps stdin open after a short request; the server must answer before
# that writer closes the pipe.
output_file=$(mktemp)
trap 'rm -f "$output_file"' EXIT

request='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'

{
  printf '%s\n' "$request"
  sleep 2
} | timeout 5s elixir -pa _build/test/lib/agentic_kb_mcp/ebin -e '
  Application.load(:agentic_kb_mcp)
  {:ok, _pid} = AgenticKbMcp.McpServer.start_link(db_path: nil)
  Process.sleep(500)
' >"$output_file"

grep -F '"id":1' "$output_file" >/dev/null
