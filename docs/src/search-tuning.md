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
