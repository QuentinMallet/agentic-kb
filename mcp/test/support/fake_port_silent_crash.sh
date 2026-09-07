#!/usr/bin/env bash
# Test-only fake port simulating a startup crash with NO protocol output at
# all (e.g. the binary segfaults before printing anything). await_ready must
# still surface this promptly via :exit_status/:closed/:EXIT rather than
# waiting out the full handshake_timeout.
exec 2>/dev/null
exit 7
