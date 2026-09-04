# `kb compress`

Compress a KB entry by removing redundant paragraphs using semantic similarity.

## Synopsis

```
kb compress <path> [--threshold-chars N] [--dry-run]
```

## What It Does

`kb compress` loads the most recent non-stale entry at `<path>`, splits the body into paragraphs, embeds each one, removes semantically duplicate paragraphs (cosine similarity > 0.85), and writes the compressed entry back with `replace_path=true`.

The result is a leaner entry that retains all unique information while cutting down redundancy.

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--threshold-chars N` | 4000 | Skip compression if entry body is smaller than N chars |
| `--dry-run` | off | Print proposed changes without writing |

## Behavior

**Threshold check:** Entries smaller than `--threshold-chars` are skipped entirely. This avoids unnecessary work on small entries.

**Idempotency:** Running `kb compress` twice on the same entry produces no change on the second run (the body is already under the threshold, or no duplicates remain to remove).

**Header:** The compressed entry is prepended with `(compressed N→M paragraphs)` to indicate the reduction.

**Embeddings required:** `kb compress` fails with a clear error if embeddings are disabled (`KB_NO_EMBED=1`).

## Example

```bash
$ kb compress architecture/erlang/supervisor-trees --dry-run

Loaded entry: architecture/erlang/supervisor-trees (2800 chars, 12 paragraphs)
Proposing compression:
  Paragraph 1: "Supervisors form the backbone..." [0.95 duplicate of paragraph 3, removing]
  Paragraph 2: "Each supervisor has a restart strategy..." [KEEP]
  ...
Result: 12 → 10 paragraphs (2200 chars)

Use 'kb compress architecture/erlang/supervisor-trees' to apply.
```

Apply:
```bash
$ kb compress architecture/erlang/supervisor-trees
Compressed: 2800 → 2200 chars (12 → 10 paragraphs)
```

## Configuration

In `kb.toml`:

```toml
compress_threshold = 4000        # Min entry size to consider for compression
compress_cosine_cutoff = 0.85    # Similarity threshold (0.0 - 1.0)
```

Tuning:
- Raise `compress_cosine_cutoff` to 0.90 to keep more detail.
- Lower it to 0.80 to be more aggressive about removing redundancy.
- Increase `compress_threshold` to avoid compressing small entries.

## When to Use

- After `kb_add` with a large, multi-part summary or evidence
- Before publishing curated KB sections
- Periodic maintenance on high-churn documentation areas (conventions, patterns)

## See Also

- [`kb stale-check`](../commands/stale-check.md) — identify aged entries for review
- [Embeddings](../architecture/embeddings.md) — understand how similarity is computed
