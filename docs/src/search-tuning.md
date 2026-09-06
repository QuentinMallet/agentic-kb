# Search Tuning

Fine-tune KB search behavior to match your corpus and use case.

## Recency Bias

Recency bias is a post-RRF multiplier that applies decay to scores based on how recently an entry was updated. Entries touched recently rank higher than older ones, even if the semantic match is identical.

### How It Works

After the RRF hybrid scoring step (BM25 + semantic), each result's score is multiplied by:

```
score *= exp(-λ × days_since_updated)
```

Where:
- `λ` (lambda) is the decay rate, configured as `recency_lambda` in `kb.toml`
- `days_since_updated` is calculated from the `updated_at` column
- `updated_at` reflects the last touch: insert, evidence update, staleness change, or re-add — not original creation time

This rewards actively curated entries over stale documentation.

### Configuration

In `kb.toml`:

```toml
recency_lambda = 0.01    # Default: 0.0 (disabled)
```

### Tuning Guide

| Lambda | Half-Life | Behavior | Use Case |
|--------|-----------|----------|----------|
| 0.0 | ∞ | No recency bias (default) | Safe default; byte-identical to pre-recency behavior |
| 0.01 | ~70 days | Gentle decay | Large corpora (10k+ entries) with mixed age; favors recent without burying old |
| 0.05 | ~14 days | Aggressive | Fast-moving domains (changelog, incident logs); only very recent entries rank high |
| 0.10 | ~7 days | Very aggressive | Real-time systems with rapid updates |

**Example:** with `λ = 0.01`, an entry from 70 days ago scores 50% of an identical entry from today.

### Performance

Recency bias adds a single batch SELECT on ≤ `limit` IDs after RRF. Measured overhead: < 1% at 100k entries. No impact on index size or search latency.

### Federation (Multi-Peer Search)

When `--peers` is set, `recency_lambda` is forced to `0.0` regardless of `kb.toml`. Each peer has its own clock; applying per-peer decay would make cross-peer scores incomparable. A warning is logged when `λ > 0` and peers are configured:

```
warning: recency_lambda forced to 0.0 for peer-federated search
```

To use recency bias with federation, coordinate `updated_at` across peers or disable federation for that query.

### Example

Enable gentle recency bias:

```bash
echo 'recency_lambda = 0.01' >> kb.toml
```

Search results will now favor recently-updated entries while still considering semantic relevance.

Disable (reset to default):

```bash
sed -i '/^recency_lambda/d' kb.toml
# or
echo 'recency_lambda = 0.0' >> kb.toml
```

### Edge Cases

- **New entries:** `updated_at` is creation time; no penalty.
- **Imported entries:** `updated_at` is import time, not original creation time. Re-add with `kb_add --path <path>` to update `updated_at` to the current time.
- **Entries with evidence:** evidence updates do NOT change `updated_at` (evidence is immutable). Use `kb_add` to force an update.
- **Frozen entries:** staleness does not affect `updated_at`; it only affects eligibility for search results.

### See Also

- [Hybrid Search](./architecture/hybrid-search.md) — RRF scoring before recency decay
- [Stale Entries](./concepts/staleness.md) — how entries age and are filtered

## MMR Diversification

With `mmr_lambda > 0`, hybrid search re-ranks a 2×limit candidate pool after
RRF + recency, greedily selecting results that maximize:

```
λ × relevance − (1−λ) × max_cosine_to_already_selected
```

Relevance is the RRF score normalized to the pool maximum. The top-1 result is
never displaced; subsequent picks trade rank for diversity, so two
near-duplicate entries stop occupying two of the top-k slots.

```toml
mmr_lambda = 0.5    # Default: 0.0 (disabled, byte-identical ordering)
```

Entries without embeddings look maximally diverse to MMR (similarity 0) — the
pass errs toward inclusion. Validate any non-zero λ with `kb eval` before
adopting it.

## Near-Duplicate Probe on Add

`kb add` / MCP `kb_add` run a semantic-only search for live entries whose
embedding cosine is at or above the dedup cutoff and report them — the add is
never blocked. CLI prints `warn: similar existing entry ...`; MCP returns
`similar_existing: [{id, path, summary, score}]` so the calling agent can
merge or expire instead of accumulating near-duplicates.

```toml
dedup_cosine_cutoff = 0.85   # Default when unset: 0.85. Values > 1.0 disable.
```

Internal re-add paths (rebuild, digest, compress, import) skip the probe.

## Relocation Auto-Heal

`kb stale-check --relocate ...` can verify whether a cited byte range still
exists at the recorded path and, if not, whether it moved. This produces a
citation-level status lattice:

- `VERIFIED` — the recorded `citation_path` still hashes at HEAD.
- `RELOCATED` — the excerpt was found at exactly one new location.
- `UNVERIFIED` — the excerpt could not be re-proven.
- `DEFERRED` — verification was intentionally skipped elsewhere, such as
  `kb_search` beyond the inline verification budget.

CLI stale-check surfaces only the actionable relocation findings:

- `RELOCATED [old] -> [new]  id=<entry>  ev=<evidence>  (report-only|healed)`
- `UNVERIFIED [old]  id=<entry>  ev=<evidence>  reason=<reason>`

Configuration lives in `kb.toml`:

```toml
relocation_autoheal = false   # Default
```

Default `false` means relocation is compute-and-report only. Setting it to
`true` allows `kb stale-check --relocate file` or `--relocate file-then-repo`
to write a `citation_healed` event for each successful relocation. The heal
rewrites `evidence.citation_path` only; it never rewrites the stored hash.

The `citation_healed` event is the durable record. Rebuild replays it, and the
CLI marks healed lines with `(healed)` instead of `(report-only)`.

## Embedding Text Mode (`KB_EMBED_TEXT`)

What gets embedded per entry:

| Mode | Text | Notes |
|------|------|-------|
| `full` (default) | `path summary content` | Legacy behavior |
| `abstraction` | `path summary tags` | Memora principle: index abstractions, not content. Content stays FTS-indexed. |

Switching modes requires re-embedding the whole store (`kb reembed` after
deleting `entries_emb` rows, or `kb rebuild`) — mixed-vintage embeddings make
cosine scores incomparable. Measure with `kb eval` before and after.

## Pre-normalized embedding migration

New embeddings are stored as finite, non-zero, L2-normalized f16 vectors. Each
entry and cue blob carries its own normalization marker: marked rows use a dot
product while legacy rows retain cosine scoring, so a partially migrated store
does not silently change ranking.

Run `kb migrate-embeddings` to convert existing blobs. It creates and retains
`agent-kb.db.pre-normalized-embeddings.bak`, validates a staged copy, and
atomically publishes the staged database only after every legacy entry and cue
blob is finite and non-zero. A corrupt blob aborts the migration without
marking any live row; restore the retained backup if an operator needs to roll
back after publication.
