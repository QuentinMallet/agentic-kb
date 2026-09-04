# Cue Anchors

Cue anchors are agent-supplied semantic entry points attached to an entry —
short "[Main Entity] + [Key Aspect]" phrases embedded separately from the
entry and searched as a **third retrieval lane** alongside FTS and the entry
embedding. They exist so a vague future query can still reach the entry even
when it shares no tokens with the summary or content.

The design follows Memora's harmonic memory representation (index
abstractions, not content): the entry content is never what the cue lane
matches against — only the deliberately chosen anchor phrases.

## Writing Good Cues

Pattern: `[Main Entity] + [Key Aspect]`, each cue a *different* facet.

| Good | Why |
|------|-----|
| `recency bias decay` | entity (recency bias) + aspect (decay) |
| `kb rebuild three-phase` | names the mechanism a future query will grope for |
| `FTS5 injection quoting` | anchors the exact failure mode |

| Bad | Why |
|-----|-----|
| `performance` | generic single word — matches everything, anchors nothing |
| `config` | no entity |
| three cues restating the summary | same facet three times |

Limits: max 8 cues per entry, 120 chars each. 2–3 well-chosen cues beat 8
mechanical ones.

## Usage

CLI:

```bash
kb add --path arch/search/rrf --summary "..." --content "..." --tags search \
  --cue "RRF rank fusion" --cue "hybrid search scoring"
```

MCP: pass `cues: ["...", ...]` to `kb_add`.

## Storage and Atomicity

Cues ride the entry's upsert event — there are no separate cue events in the
JSONL. `apply_event` replaces the entry's cue rows wholesale inside a
savepoint; expire deletes them in the same transaction as the entry. The
design is verified by `.state/agent-kb/tla/CueBatch.tla` (no orphan cue rows, no
stale cue sets, crash + rebuild converges).

## Retrieval

In hybrid mode each live cue row is scored by cosine against the query; the
best cue score per entry forms a ranked list fused into RRF as a third source
(same `1/(k+rank)` contribution as the FTS and semantic lanes). Entries
reachable only via a cue get `source: "cue"` in results.

Cue embeddings are written at add time. Entries added with `KB_NO_EMBED=1`
store cue text without embeddings; those rows are invisible to the cue lane
until re-added.
