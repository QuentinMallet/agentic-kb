#!/usr/bin/env bash
# Test-only fake `kb mcp` port for AgenticKbMcp.PortManagerTest (bd-21ef.2.8).
#
# Speaks the line-delimited JSON port protocol well enough to drive the
# PortManager correlation/deadline/crash scenarios from ADR-3
# (.state/.omc/plans/c2-exclusion-boundary.md) without needing the real Rust
# binary. Behavior per request is selected by the "method" field the test
# puts on its request map; the "id" field is echoed back so the caller's
# correlation logic has something to check.
#
# stderr is severed immediately: Erlang's :use_stdio only redirects
# stdin/stdout, so this process would otherwise inherit BEAM's own stderr —
# which, under a test runner that pipes `mix test`'s output (e.g. `| tail`),
# is the *same pipe* the runner is reading from. A background loop below
# that outlives its GenServer would then hold that pipe open forever even
# after `mix test` itself exits, hanging the whole invocation.
exec 2>/dev/null

set -uo pipefail

field() { # field <name> <json-line> -> the string value of "<name>":"..."
  echo "$2" | grep -o "\"$1\":\"[^\"]*\"" | head -1 | cut -d'"' -f4
}

printf '{"type":"ready"}\n'

while IFS= read -r line; do
  id=$(field id "$line")
  method=$(field method "$line")

  case "$method" in
    stale_then_reply)
      # F2 regression: a leftover reply for an unrelated id arrives first;
      # the real reply for this request follows shortly after.
      printf '{"id":"stale-leftover-id","type":"result"}\n'
      sleep 0.05
      printf '{"id":"%s","type":"result"}\n' "$id"
      ;;
    progress_then_reply)
      printf '{"id":"%s","type":"progress","processed":1,"total":2}\n' "$id"
      sleep 0.05
      printf '{"id":"%s","type":"result"}\n' "$id"
      ;;
    progress_stream)
      # Never sends a final reply — only ticks, until the reader goes away
      # (printf then fails with EPIPE and the loop condition ends it).
      while printf '{"id":"%s","type":"progress","processed":1,"total":2}\n' "$id"; do
        sleep 0.03
      done
      ;;
    discard_stream)
      # Never sends a matching reply — only wrong-id finals, until the
      # reader goes away.
      while printf '{"id":"some-other-id","type":"result"}\n'; do
        sleep 0.03
      done
      ;;
    hang)
      # No output at all until killed externally, or until stdin closes
      # (the owning GenServer terminated and tore down the port).
      while IFS= read -r _unused; do :; done
      ;;
    audit_run)
      printf '{"id":"%s","type":"ok","run_id":"audit-1","samples":[]}\n' "$id"
      ;;
    audit_record)
      printf '{"id":"%s","type":"ok","recorded":1,"expired":0}\n' "$id"
      ;;
    audit_report)
      printf '{"id":"%s","type":"result","per_kind_session_precision":[],"last_run_at":null,"total_runs":0,"per_arm_precision":[]}\n' "$id"
      ;;
    provenance)
      printf '{"id":"%s","type":"result","roots":["root-1"],"graph":[],"truncated":false}\n' "$id"
      ;;
    reembed)
      printf '{"id":"%s","type":"ok","embedded":0,"failed":0,"failures":[],"skipped":0,"missing":1,"raced":0,"dry_run":false,"noop_embedder":true,"message":"KB_NO_EMBED is set — no embedder available"}\n' "$id"
      ;;
    *)
      printf '{"id":"%s","type":"result"}\n' "$id"
      ;;
  esac
done
