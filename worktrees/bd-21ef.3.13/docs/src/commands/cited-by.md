# `kb cited-by`

List live KB entries whose evidence cites a given file.

## Synopsis

```bash
kb cited-by <repo-relative-file> [--json]
```

## Text Contract

Default output is one line per entry:

```text
GOVERNED <STATUS> [<path>] <summary> id=<entry_id>
```

`STATUS` is the strongest citation finding for that entry:

- `VERIFIED` — the cited byte range still hashes at the recorded location.
- `RELOCATED` — the cited bytes were found at a different location in the same file.
- `UNVERIFIED` — the citation no longer proves out.
- `DEFERRED` — the citation could not be verified in this mode, typically
  because the evidence kind or available repo context is insufficient.

The command is silent on empty results.

## JSON Mode

`--json` emits an array of:

```json
{
  "id": "entry-id",
  "path": "docs/topic.md",
  "summary": "Why this file matters",
  "status": "VERIFIED",
  "citation_path": "src/topic.rs:10-24"
}
```

Use this command in hooks or review scripts that want to discover which KB
entries are governed by a file before editing it.
