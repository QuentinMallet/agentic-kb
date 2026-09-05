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

Under federation, `--limit` is a global cap across the local repository and all
peers, not a per-repository cap. `merge_federated_results` collides candidates
on `id` alone, so there is exactly one surviving row per id across all
origins: a local row and a peer row sharing an id collide, and the local row
always wins; two peer rows sharing an id collide too, resolved by the normal
deterministic ordering (`compare_federated_rows`). The surviving row is then
stored under the pair `(origin_repo, id)` — that pair is the storage key of
the row that won the collision, not the collision test itself. Exactly one
federated truncation follows. These cases are pinned by
`test_federated_global_limit_dedup_and_local_collision_contract`.

Cross-repository ordering is by within-repository rank position with a
deterministic `origin_repo`/entry-ID tie-break. It is not a comparison of
absolute relevance between repositories: repositories can have different corpus
sizes, and the scores produced by RRF encode within-corpus rank positions. Thus
a top-ranked result from a small peer can precede a middling local result; see
`compare_federated_rows` and
`test_federated_rank_position_top_tiny_peer_outranks_mid_local`. Peer traversal
order cannot change the output (`test_federated_order_is_byte_stable_under_peer_traversal_permutation`).

When `--peers` is set, `recency_lambda` is forced to `0.0` regardless of `kb.toml`. Each peer has its own clock; applying per-peer decay would make cross-peer scores incomparable. A warning is logged when `λ > 0` and peers are configured:

```
warning: recency_lambda forced to 0.0 for peer-federated search
```

To use recency bias with federation, coordinate `updated_at` across peers or disable federation for that query.

### Bounds and deterministic ranking

| Input | Accepted range | Enforcement |
|-------|----------------|-------------|
| CLI/MCP result limit | `1..=100` | `db::MAX_LIMIT`; `parse_limit` and `NumField::bounded` reject values outside the range |
| MCP `inline_verify_k` | `0..=100` | `db::MAX_INLINE_VERIFY_K`; `NumField::bounded` rejects values outside the range |

The bounds are re-applied inside `search_entries` by `clamp_search_caps`; the
public interfaces reject out-of-range requests rather than presenting a
silently clamped request as accepted. The MCP boundary behavior is pinned by
`test_search_rejects_limit_above_max_and_honours_the_maximum` and
`test_search_rejects_inline_verify_k_above_max_and_honours_the_maximum`.

All Rust ranking lanes use `compare_rank`: scores sort descending, equal scores
sort by entry ID ascending, `-0.0` is normalized to `0.0`, and `f32::total_cmp`
provides a total order. This makes ties and floating-point edge cases stable.

Non-finite values from the query embedder are rejected outright:
`validate_embedding` returns an error (`"embedder returned a non-finite
component"`) that propagates out of `search_entries`, so a corrupt query
embedding fails the whole search rather than merely scoring badly.

A stored (not query) corrupt or non-finite embedding is never dropped from the
result set. `decode_emb_blob` and `decode_f16_blob_into` detect the bad blob,
log a `kb: decode_emb_blob: unexpected blob length ...` or `kb:
cosine_similarity dimension mismatch` diagnostic to stderr, increment the
process-wide `models::corrupt_embedding_count` counter, and return an empty
vector; `cosine_similarity` then treats the resulting length mismatch as
similarity `0.0`, so the row stays in the ranked output and remains
deterministically sortable. The counter is exposed as
`db::SearchStats.corrupt_embeddings` by `search_entries_with_stats`, but no CLI
or MCP caller invokes that function today — `search_entries` (the function
every production caller uses) does not report the count anywhere. This is
pinned at the `db` layer by
`nan_blob_is_zero_scored_and_reported_in_search_stats`, which asserts the
corrupted row survives with score `0.0` and `stats.corrupt_embeddings > 0`;
the count itself is not yet surfaced in a CLI or MCP response.

Semantic and cue scoring may scan their indices, but full entry metadata is
materialized only for the first `2 * limit` ranked IDs in each lane. The bound
is implemented by `fetch_search_metadata` in `search_entries` and pinned by
`p1_metadata_materialization_is_bounded_to_twice_limit_per_lane`. See the
[S5 search caps decision packet](../decisions/s5-search-caps-packet.md) for the
resource-cap ruling and rationale.

Evidence metadata is fetched in bounded per-entry batches by
`fetch_evidence_for_entries`. A malformed database value is returned as a
search error rather than silently dropping its evidence row; this is pinned by
`test_fetch_evidence_for_entries_propagates_decode_errors`.

### Literal path prefixes

`--path-prefix` is a literal string prefix. `like_prefix_pattern` escapes the
SQL `LIKE` metacharacters `%` and `_` (and the escape character `\`) before all
FTS, semantic, and cue queries apply the filter.

| Query | Before | Now |
|-------|--------|-----|
| `--path-prefix 'src/%'` | `%` could match every suffix, effectively selecting `src/*` | Matches only paths beginning with the literal characters `src/%`; returns no rows when none exist |
| `--path-prefix 'src/_'` | `_` could match any one character | Matches only paths beginning with literal `src/_` |

The behavior is pinned by `test_search_path_prefix_percent_is_literal_and_can_return_empty`
and `test_search_path_prefix_matches_literal_underscore_prefix_only`.

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

## Deferred Follow-Ups

Two known gaps in the current search/federation contract are tracked as open
beads rather than fixed here:

- `bd-federated-verify-after-truncate-ayb8` — inline verification currently
  runs per repository (local and each peer) before `merge_federated_results`
  performs the global merge and truncation. A row that inline verification
  spent work on can still be discarded by the federated truncate, so
  verification effort is not scoped to the final global `--limit`.
- `bd-prenorm-embeddings-followup-te13` — a deferred change to persist
  pre-normalized embeddings in a new on-disk format, rather than normalizing
  at read time on every `cosine_similarity` call.
