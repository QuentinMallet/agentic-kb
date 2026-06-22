# `kb hook`

Integrate the KB with session lifecycle hooks to auto-digest conversation transcripts.

## Synopsis

```
kb hook session-end --transcript <path> --session-id <id>
```

## What It Does

`kb hook session-end` reads unread bytes from a transcript file, synthesizes a digest (head, tail, tool calls), and writes it to `sessions/<id>/digest` in the KB. This is designed to run at session end to capture a concise summary of agent work.

The digest includes:
- First turn (context setting)
- Last N turns (outcome)
- All tool invocations with arguments and results
- Elapsed time

## Crash Safety

Byte-offset tracking ensures safety on crashes:

1. Read unread bytes from transcript
2. Synthesize digest
3. Write digest to KB with `replace_path=true`
4. **Advance byte-offset** (last step)

If a crash occurs before step 4, the next run re-digests the same turns. The `replace_path=true` flag makes the second write idempotent.

## Deduplication

The digest is hashed before writing. If the hash is identical to the previous run, no new events are written to the KB. This avoids creating duplicate entries on repeated hook invocations.

## Back-Pressure

Digestion is capped at 500 turns per run. Sessions longer than 500 turns are digested in multiple runs; the hook can be safely invoked multiple times.

## Installation

Add to Claude Code `settings.json` under `hooks`:

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "kb hook session-end --transcript $CLAUDE_TRANSCRIPT_PATH --session-id $CLAUDE_SESSION_ID"
          }
        ]
      }
    ]
  }
}
```

Supported environment variables (fallbacks for CLI flags):
- `KB_TRANSCRIPT_PATH` → `--transcript`
- `KB_SESSION_ID` → `--session-id`

## Behavior

| Condition | Action |
|-----------|--------|
| Transcript file missing | Error: "transcript not found at ..." |
| Session ID empty | Error: "session-id is required" |
| No new turns since last run | No-op; digest already captured |
| Digest is identical to previous | No-op; avoids duplicate KB entries |
| More than 500 turns | Digest first 500; byte-offset advances; next run continues |

## Source

- `src/commands/hook.rs` — CLI entry point
- `src/commands/digest.rs` — digest synthesis
- `src/components/transcript_state.rs` — byte-offset tracking

## Example

Manual invocation:

```bash
$ kb hook session-end \
  --transcript /tmp/claude_transcript_12345.json \
  --session-id sess-20260622-1445

Digesting session sess-20260622-1445 (1247 turns, 156 KB)
  Tools called: git (18), grep (3), read (42), edit (11)
  Elapsed: 34 min
Digest written to: sessions/sess-20260622-1445/digest
Byte-offset saved: 4294967296
```

On next invocation (if transcript grows):

```bash
$ kb hook session-end \
  --transcript /tmp/claude_transcript_12345.json \
  --session-id sess-20260622-1445

Digesting session sess-20260622-1445 (1263 turns, 164 KB)
  New turns since last run: 16
  Tools called: git (2)
  Elapsed: 2 min
Digest updated: sessions/sess-20260622-1445/digest
Byte-offset saved: 4300000000
```
