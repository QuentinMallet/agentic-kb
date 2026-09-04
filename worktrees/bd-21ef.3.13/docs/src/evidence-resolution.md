# Evidence Resolution

The agent KB implements server-side resolution of evidence citations at write time. This page describes the resolution contract, the rationale for cheap-compliance citation authorship, and the retrieval guarantees that follow.

## The Resolution Contract

When an agent calls `kb_add` with an evidence row containing only a `citation_path`, the server resolves that path to a complete evidence record before appending the event to the log. The contract is:

- **Caller provides:** `citation_path` (file path or file path + byte range) and optionally `evidence.kind` and `evidence.derived_from`.
- **Server resolves:** `citation_hash` (SHA-256 of the byte range) and `citation_sha` (git HEAD commit SHA), computed at write time from the working repository.
- **Explicit values preserved:** If the caller supplies `citation_hash` or `citation_sha` explicitly, those values are never overwritten — the caller assertion is authoritative.
- **Before event append:** Resolution failures are loud write-time errors that reject the entire `kb_add` call, naming the problematic path and reason. A malformed citation or missing file causes the write to fail, never resulting in an unverifiable row in the database.
- **Replay invariant:** Resolved fields are persisted in the event as-is; the verifier on later rebuilds or replay never re-resolves them. This means the hash and SHA captured at write time are preserved exactly as computed, unaffected by later file changes or git history rewrites.

### Resolution Failure Cases

Resolution can fail at write time if:

| Condition | Reason | Behavior |
|-----------|--------|----------|
| Malformed `citation_path` | Path syntax invalid (e.g., range end before start) | Write rejected with parse error |
| File not found | `citation_path` refers to a path that does not exist in the working tree | Write rejected with FileMissing |
| Not a regular file | Path is a directory, symlink, or device file | Write rejected with FileMissing |
| Byte range outside bounds | Explicit range exceeds file size | Write rejected with RangeError |
| File too large | Whole-file citation exceeds `MAX_FILE_BYTES` (64 MiB); range citation exceeds `MAX_RANGE_BYTES` (4 MiB) | Write rejected with FileTooLarge |
| I/O errors | Filesystem errors during read | Write rejected with I/O error |

All failures surface as rejections to the caller. There is no silent fallback, no partial writes, and no stored rows that are unverifiable due to resolution failure.

## Cheap Compliance: The Rationale

Prior to this contract, evidence rows required manual citation authorship using the `kb cite` tool or equivalent MCP invocation. This created friction:

- The `kb_add` soft mandate on evidence (for observation, belief, procedure kinds) became a write deterrent.
- Agents had to compute SHA-256 hashes locally via `kb cite` before constructing the `kb_add` payload.
- This added a pre-requisite step and forced sequential authoring: first call `kb cite`, then call `kb_add`.

The server-side resolution contract removes this deterrent:

```
OLD FLOW:
  kb cite src/foo.rs
  → copy citation_hash, citation_sha, file_size from output
  → kb_add with evidence: [{kind: "code", citation_path: "...", citation_hash: "...", citation_sha: "..."}]

NEW FLOW:
  kb_add with evidence: [{kind: "code", citation_path: "src/foo.rs"}]
```

The entry-point call `kb_add` now succeeds or fails on its own, without requiring a separate tool invocation or pre-computed fields. The `kb cite` tool remains available for agents that want to inspect citations before writing, or to construct complete citations ahead of time; it is no longer a mandatory pre-step.

### Evidence Status and the Soft Mandate

The soft mandate on evidence (for observation, belief, and procedure kinds) does not block writes. Entries of these kinds are accepted with zero evidence rows, stored with `evidence_status="missing"`, and trigger a write-time warning to stderr. When evidence rows are provided, they need only include `citation_path`; the server resolves it to `citation_hash` and `citation_sha`. Evidence rows are capped at `MAX_EVIDENCE_ROWS_PER_ENTRY` (200), a limit enforced at both write and retrieval time.

Entries of kind convention or memory are not subject to the soft mandate and may carry zero evidence rows.

## Retrieval Trigger Guarantee

A key invariant pins the retrieval semantics: entries of **every kind** — observation, belief, procedure, convention, **and memory** — are FTS-indexed and retrievable by `kb_search`.

### No Kind Filter in Retrieval

The search indices (`entries_fts` and `entries_fts_v2`) do not include a kind-based filter. When `kb_search` is invoked with a query, it returns matching entries from all five kinds, subject only to:

- The query term's presence in the entry text (via FTS matching).
- The entry's `is_stale` flag (stale entries are excluded).
- Optional path prefix or tag filters supplied by the caller.

**Memory entries are NOT excluded from search results.** Previously, an unimplemented Phase 2 intent described excluding memory-class entries from retrieval; that intent is not enforced and represents a documentation error. Entries written to the KB with `kind: "memory"` are searchable and findable via `kb_search`, just like any other kind.

This guarantee is enforced by the regression test suite (`tests/kind_retrieval_regression.rs`), which asserts that FTS indexing and retrieval include all five kinds and pins the invariant against future regressions.

### Rationale

The cost of suppressing kind-based filters in retrieval is minimal: a single boolean predicate (`is_stale`) suffices to gate the retrieval results. Adding a kind exclusion filter would:

1. Suppress valid knowledge that agents have explicitly written to the KB.
2. Require a code change or configuration gate every time the filter's logic needed to change.
3. Create a divergence between the canonical event log and the retrieval surface: a stale-marked memory entry would exist in the database but be unreachable via search.

The design choice is to trust the entry's `kind` field as authoritative and let the caller filter results by kind if needed, rather than embedding kind-based suppression into the core retrieval machinery.

## See Also

- [Citation Semantics](./citation-semantics.md) — path forms, validation, and the `kb cite` tool
- [Evidence Storage](./evidence-storage.md) — how evidence rows are retained and recovered across operations
