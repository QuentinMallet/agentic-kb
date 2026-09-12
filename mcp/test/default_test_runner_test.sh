#!/usr/bin/env bash
# Guards against the application consuming closed test stdin and halting the
# VM before ExUnit executes. Run from mcp/ inside the Nix development shell.
set -euo pipefail

output=$(mktemp "${TMPDIR:-/tmp}/mcp-default-test-run.XXXXXX")
trap 'rm -f "$output"' EXIT

timeout 30s mix test </dev/null >"$output" 2>&1
grep -q 'Running ExUnit' "$output"
grep -q 'Finished in' "$output"
grep -Eq '[0-9]+ tests, 0 failures' "$output"
