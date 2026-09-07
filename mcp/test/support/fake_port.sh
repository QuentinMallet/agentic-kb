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

audit_state=$(mktemp "${TMPDIR:-/tmp}/agentic-kb-audit.XXXXXX")
trap 'rm -f "$audit_state"' EXIT

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
      printf '{"id":"%s","type":"ok","run_id":"audit-1","samples":[{"id":"audit-e1","path":"audit/e1","summary":"first","kind":"belief","evidence_status":"present","arm":"uniform","evidence":[]},{"id":"audit-e2","path":"audit/e2","summary":"second","kind":"belief","evidence_status":"present","arm":"uniform","evidence":[]}]}\n' "$id"
      ;;
    audit_record)
      # `audit-perm` is a fixture-only id for the permanent-guard case: a
      # false verdict against it always refuses, before any counting below,
      # mirroring handle_audit_record's permanent check that runs before its
      # apply loop (src/commands/mcp.rs) — nothing is appended to
      # $audit_state, so it can never contribute to a later audit_report.
      #
      # Scanned per verdict item, not over the whole line: each item is a
      # leaf JSON object with no nested braces, so `grep -oE '\{[^{}]*\}'`
      # extracts them one at a time and entry_id/verdict are matched
      # together within the SAME object — a batch also carrying another
      # entry's unrelated false verdict must not be misread as this case.
      permanent_guard_hit=false
      while IFS= read -r verdict_obj; do
        if echo "$verdict_obj" | grep -Eq '"entry_id"[[:space:]]*:[[:space:]]*"audit-perm"' \
            && echo "$verdict_obj" | grep -Eq '"verdict"[[:space:]]*:[[:space:]]*false'; then
          permanent_guard_hit=true
        fi
      done < <(echo "$line" | grep -oE '\{[^{}]*\}')

      if [ "$permanent_guard_hit" = true ]; then
        printf '{"id":"%s","type":"error","code":"permanent_guard","message":"entry '\''audit-perm'\'' cannot be expired: permanent"}\n' "$id"
      else
        # Count every verdict in the line, not just the first: a mixed batch
        # of true and false verdicts records and expires each one, matching
        # the real handler's per-verdict apply loop. An empty batch counts
        # zero of each, which also gives the correct recorded:0,expired:0
        # for that case without a separate branch.
        true_count=$(echo "$line" | grep -oE '"verdict"[[:space:]]*:[[:space:]]*true' | wc -l | tr -d ' ')
        false_count=$(echo "$line" | grep -oE '"verdict"[[:space:]]*:[[:space:]]*false' | wc -l | tr -d ' ')
        i=0
        while [ "$i" -lt "$true_count" ]; do
          printf 'true\n' >> "$audit_state"
          i=$((i + 1))
        done
        i=0
        while [ "$i" -lt "$false_count" ]; do
          printf 'false\n' >> "$audit_state"
          i=$((i + 1))
        done
        recorded=$((true_count + false_count))
        printf '{"id":"%s","type":"ok","recorded":%s,"expired":%s}\n' "$id" "$recorded" "$false_count"
      fi
      ;;
    audit_report)
      total=$(wc -l < "$audit_state" | tr -d ' ')
      supported=$(grep -c '^true$' "$audit_state" || true)
      if [ "$total" -eq 0 ]; then
        printf '{"id":"%s","type":"result","per_kind_session_precision":[],"last_run_at":null,"total_runs":0,"per_arm_precision":[]}\n' "$id"
      else
        # %.10g, not %.1f: the real handler emits an unrounded f64 ratio, and
        # a fixed one-decimal-place round trips only for a 1/2 split (0.5) —
        # a future three-verdict case (2/3) would otherwise render 0.7, a
        # value the real handler never produces.
        precision=$(awk -v yes="$supported" -v all="$total" 'BEGIN { printf "%.10g", yes / all }')
        printf '{"id":"%s","type":"result","per_kind_session_precision":[{"kind":"belief","session_id":"__GLOBAL__","precision":%s,"n":%s}],"last_run_at":"2026-09-05T00:00:00Z","total_runs":%s,"per_arm_precision":[{"arm":"uniform","n":%s,"precision":%s}]}\n' "$id" "$precision" "$total" "$total" "$total" "$precision"
      fi
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
