#!/usr/bin/env bash
set -euo pipefail

: "${CAPTURE_FILE:?CAPTURE_FILE must name the request capture file}"

field() {
  printf '%s\n' "$2" | grep -o "\"$1\":\"[^\"]*\"" | head -1 | cut -d'"' -f4
}

printf '{"type":"ready"}\n'

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CAPTURE_FILE"
  id=$(field id "$line")
  printf '{"id":"%s","type":"result"}\n' "$id"
done
