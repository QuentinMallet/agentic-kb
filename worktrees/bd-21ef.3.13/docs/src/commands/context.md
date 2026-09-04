# `kb context`

Select a compact working set of KB entries for the current branch and dirty
tree.

## Synopsis

```bash
kb context --budget 1200 [--floor 0.05] [--json]
```

## What It Does

`kb context` scores live entries using two signals:

- Working-set overlap: evidence citations that point at files currently changed
  in the repo.
- Branch-token FTS: tokens extracted from the current branch name, searched
  against the KB.

The command blends those signals, sorts by score, and greedily packs whole
entries until the token budget is exhausted. Tokens use the simple heuristic
`ceil(UTF-8 bytes / 4)`, measured on the exact bytes that will be emitted for
the chosen output mode.

Text mode emits:

```text
<summary>

<first paragraph>
[kb#<id>]
```

Entries are never truncated further than the first paragraph. The `[kb#<id>]`
handle is the expansion key for later `kb_get` use in MCP flows.

## Budget and Floor

- `--budget` is required and applies to the exact emitted representation, not
  to search work.
- `--floor` is optional. When unset, the rule is "relevance or silence":
  include any entry with non-zero signal; if nothing has signal, print nothing.
- Entries are indivisible. A large entry that would exceed the remaining budget
  is skipped rather than clipped.
- The reported count is approximate and labeled as `approx. tokens`.

## JSON Mode

`--json` emits a JSON array of `{id, path, summary, approx_tokens, score}` rows.
In JSON mode, escaping and JSON punctuation count toward the same approximate
budget.

## Telemetry

If `KB_INJECTION_SOURCE` is set, selected ids are recorded into the separate
best-effort query-hits telemetry DB (`.state/agent-kb/query-hits.db`) under
that surface name. This does not affect selection and does not emit JSONL
events.
