#!/usr/bin/env bash
# Production-process lifecycle coverage. Run from mcp/ inside the Nix dev shell:
#   MIX_ENV=prod bash test/application_process_test.sh
set -euo pipefail

test_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
fake_port="$test_dir/support/fake_port.sh"
silent_crash="$test_dir/support/fake_port_silent_crash.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/mcp-application-process.XXXXXX")

cleanup() {
  if [[ -n "${writer_pid:-}" ]]; then
    kill "$writer_pid" 2>/dev/null || true
  fi

  if [[ -n "${server_pid:-}" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi

  rm -rf "$work_dir"
}
trap cleanup EXIT

chmod +x "$fake_port" "$silent_crash"

start_server() {
  local kb_bin=$1
  local marker=$2

  mkfifo "$work_dir/stdin"
  tail -f /dev/null >"$work_dir/stdin" &
  writer_pid=$!

  KB_BIN="$kb_bin" KB_DB_PATH="$work_dir/test.db" MIX_ENV=prod \
    mix run --no-halt -e "IO.puts(\"$marker\")" <"$work_dir/stdin" \
    >"$work_dir/stdout" 2>"$work_dir/stderr" &
  server_pid=$!
}

wait_for_marker() {
  local marker=$1

  for _ in $(seq 1 100); do
    if grep -qx "$marker" "$work_dir/stdout" 2>/dev/null; then
      return 0
    fi

    if ! kill -0 "$server_pid" 2>/dev/null; then
      return 1
    fi

    sleep 0.05
  done

  return 1
}

touch "$work_dir/test.db"
start_server "$fake_port" "MCP_LIFECYCLE_READY"

if ! wait_for_marker "MCP_LIFECYCLE_READY"; then
  cat "$work_dir/stderr" >&2
  exit 1
fi

kill -0 "$server_pid"
kill "$writer_pid"
writer_pid=""

if ! wait "$server_pid"; then
  echo "MCP server did not exit cleanly after EOF" >&2
  cat "$work_dir/stderr" >&2
  exit 1
fi
server_pid=""

rm -f "$work_dir/stdin"
start_server "$silent_crash" "MCP_STARTUP_SHOULD_NOT_SUCCEED"

if wait_for_marker "MCP_STARTUP_SHOULD_NOT_SUCCEED"; then
  echo "MCP application reached user code after PortManager startup failure" >&2
  exit 1
fi

if wait "$server_pid"; then
  echo "MCP application exited zero after PortManager startup failure" >&2
  exit 1
fi
server_pid=""
