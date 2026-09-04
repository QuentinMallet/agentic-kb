#!/usr/bin/env bash
# Test-only fake port simulating ADR-3 rule 6's fixed Rust startup behavior:
# a fatal startup failure (e.g. an unopenable DB) is announced as a
# {"type":"error", ...} JSON line on stdout before the process exits, so the
# real cause travels on the protocol instead of dying on stderr.
exec 2>/dev/null
printf '{"type":"error","code":"internal","message":"unable to open database: permission denied"}\n'
exit 1
