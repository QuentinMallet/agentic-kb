# `kb migrate-embeddings`

Convert legacy embedding blobs to the pre-normalized f16 format in place.

## Synopsis

```
kb migrate-embeddings
```

Takes no flags (`MigrateEmbeddings` in `src/commands/migrate_embeddings.rs`).

## Purpose

New embeddings are stored as finite, non-zero, L2-normalized f16 vectors,
each carrying its own normalization marker so a partially migrated store
does not silently change ranking (marked rows use a dot product; legacy rows
retain cosine scoring). `kb migrate-embeddings` converts every existing
legacy entry and cue embedding blob to that format in one pass, so the whole
store can eventually drop the legacy cosine-scoring path.

## What it does: staged swap with a retained backup

1. Creates and retains `agent-kb.db.pre-normalized-embeddings.bak`, a backup
   of the live database, before making any change.
2. Copies the live database into a staging file and migrates every legacy
   entry and cue blob there, leaving the live database untouched during
   this phase.
3. Validates the staged copy: publication proceeds only once every legacy
   blob in the stage is confirmed finite, non-zero, and exactly the
   configured embedding dimension. A corrupt blob aborts the migration
   without marking any row live — the live database is never touched by a
   failed migration.
4. Checkpoints and verifies the live database's WAL before publication, so
   already-committed pages are never discarded during the swap, then
   atomically publishes the staged database over the live one.

A durable staging-state file records progress, so an interruption before
publication is resumable: re-running the command publishes only when the
live source digest is unchanged since staging began, otherwise it discards
the stale stage and restarts from the live database.

## When to run it

Run `kb migrate-embeddings` once, after upgrading to a release that writes
pre-normalized f16 embeddings, to bring existing legacy blobs onto the same
format. It is not part of the ordinary write path and does not run
automatically. If an operator needs to roll back after a completed
migration, restore the retained `.bak` file.

Legacy migration accepts only the exact 384-element f32 wire format (1536
bytes); an unmarked 768-byte blob is rejected rather than guessed to be f16,
since it could also represent a malformed 192-element f32 vector.

## See Also

- [Search Tuning](../search-tuning.md) — "Pre-normalized embedding
  migration" and "Embedding Text Mode" for the broader embedding-format
  context.
