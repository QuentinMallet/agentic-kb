#!/usr/bin/env bash
set -euo pipefail

# This is deliberately an OS-pipe test, not only a framer unit test. The
# writer keeps stdin open after a short request; the server must answer before
# that writer closes the pipe.
output_file=$(mktemp)
partial_file=$(mktemp)
recovery_file=$(mktemp)
restart_file=$(mktemp)
exact_file=$(mktemp)
oversize_eof_file=$(mktemp)
trap 'rm -f "$output_file" "$partial_file" "$recovery_file" "$restart_file" "$exact_file" "$oversize_eof_file"' EXIT

request='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'

run_held_open_server() {
  timeout 30s elixir --erl "-noinput" -pa _build/test/lib/agentic_kb_mcp/ebin -e '
    Application.load(:agentic_kb_mcp)
    Process.flag(:trap_exit, true)
    {:ok, _pid} = AgenticKbMcp.McpServer.start_link(db_path: nil)
    Process.sleep(500)
  '
}

run_eof_server() {
  timeout 30s elixir --erl "-noinput" -pa _build/test/lib/agentic_kb_mcp/ebin -e '
    Application.load(:agentic_kb_mcp)
    {:ok, _pid} = AgenticKbMcp.McpServer.start_link(db_path: nil)
    Process.sleep(:infinity)
  '
}

run_restarted_server() {
  timeout 30s elixir --erl "-noinput" -pa _build/test/lib/agentic_kb_mcp/ebin -e '
    Application.load(:agentic_kb_mcp)
    Process.flag(:trap_exit, true)
    {:ok, _pid} = AgenticKbMcp.McpServer.start_link(db_path: nil)
    GenServer.stop(AgenticKbMcp.McpServer, :shutdown)
    {:ok, _pid} = AgenticKbMcp.McpServer.start_link(db_path: nil)
    Process.sleep(500)
  '
}

{
  printf '%s\n' "$request"
  sleep 2
} | run_held_open_server >"$output_file"

grep -F '"id":1' "$output_file" >/dev/null

# A valid partial frame at EOF dispatches once, then the reader exits without
# waiting for another input byte.
printf '%s' "$request" | run_eof_server >"$partial_file"
test "$(grep -c '"id":1' "$partial_file")" -eq 1

# A 10 MiB + 1-byte frame is discarded through its newline. The following
# frame remains aligned, is answered once, and EOF terminates the server.
{
  dd if=/dev/zero bs=1048576 count=10 status=none | tr '\0' x
  printf 'x\n%s\n' "$request"
} | run_eof_server >"$recovery_file"

test "$(grep -c 'Frame exceeds 10 MiB limit' "$recovery_file")" -eq 1
test "$(grep -c '"id":1' "$recovery_file")" -eq 1
test "$(grep -n 'Frame exceeds 10 MiB limit' "$recovery_file" | cut -d: -f1)" -lt "$(grep -n '"id":1' "$recovery_file" | cut -d: -f1)"

# The native line port reports an exact-limit line as a bounded chunk followed
# by an empty newline segment. It remains a valid frame, so invalid JSON gets
# the normal parse error rather than the oversize error.
{
  dd if=/dev/zero bs=1048576 count=10 status=none | tr '\0' x
  printf '\n'
} | run_eof_server >"$exact_file"

test "$(grep -c '"message":"Parse error"' "$exact_file")" -eq 1
test "$(grep -c 'Frame exceeds 10 MiB limit' "$exact_file")" -eq 0

# A line one byte over the limit without a newline is discarded once at EOF.
{
  dd if=/dev/zero bs=1048576 count=10 status=none | tr '\0' x
  printf x
} | run_eof_server >"$oversize_eof_file"

test "$(grep -c 'Frame exceeds 10 MiB limit' "$oversize_eof_file")" -eq 1
test "$(grep -c '"message":"Parse error"' "$oversize_eof_file")" -eq 0

# A normal server shutdown must release the old reader port before the
# replacement starts. The replacement receives this one request exactly once.
{
  sleep 0.2
  printf '%s\n' "$request"
  sleep 2
} | run_restarted_server >"$restart_file"

test "$(grep -c '"id":1' "$restart_file")" -eq 1
