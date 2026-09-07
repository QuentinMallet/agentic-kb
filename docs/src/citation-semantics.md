# Citation Semantics

A citation is a reference to a specific byte range within a file in the repository. The KB uses citations to track the provenance of evidence rows: where in the source code a given finding was discovered and what the bytes were at that time.

## Citation Path Forms

Citations are specified as a `citation_path` string with two possible forms:

### Whole-File Citation

A bare file path without a colon specifies the entire file:

```
src/components/verification.rs
```

This form cites **all bytes** from offset 0 to the end of the file. The hash is computed over the entire file content.

### Range Citation

A file path followed by a colon and byte offsets specifies a range:

```
src/commands/cite.rs:1500-2300
```

This form cites bytes from offset `start` to `end` (exclusive, so bytes `[start, end)`). Both offsets are **byte counts**, not line numbers. The range is bounded: `start < end` is required; `start == end` (empty range) is rejected.

### Colon Parsing Rule

The discriminator is the **last** colon in the path. This allows filenames containing colons to be cited with an explicit range:

```
weird:name.rs:0-42
```

Here `rfind(':')` finds the rightmost colon, so the file is `weird:name.rs` and the range is `0-42`.

A file with a colon in its name **cannot** be cited in bare whole-file form — it can only be cited with an explicit range. If the parser encounters a colon and the suffix does not parse as a valid range, the citation is rejected as malformed (never silently degraded to whole-file).

### Malformed Citations

The following are hard rejections:

| Form | Reason |
|------|--------|
| Empty string | File part is required |
| `:0-100` | File part is empty |
| `src/foo.rs:` | Range part is missing |
| `src/foo.rs:42` | Range part missing the dash |
| `src/foo.rs:42-5B` | End offset is not a number |
| `src/foo.rs:50-40` | start >= end (requires start < end) |
| `src/foo.rs:100-100` | Empty range (start == end) |
| `src/` | Directories are not cited |

The intent is loud failure: a typo in a citation path must surface as a parse error, never as silent fallback to whole-file. If a citation was recorded and later appears malformed in the database, verification surfaces it as an `UnverifiedReason` rather than an error — this is the read-path (verification) behavior. The MCP write path is stricter: see "MCP write-time rejection" below.

### Special Cases

**Empty files.** A whole-file citation of an empty file is legal and meaningful. It asserts that the file is empty (0 bytes). The hash is `sha256("")` = `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`. If the file later gains bytes, the hash changes and the evidence row is flagged as unverified.

**Non-regular files.** Directories and device files are rejected as `FileMissing`. Any symbolic-link component, whether it points inside or outside the repository, is rejected as `SymlinkPathRejected` (`symlink_path_rejected` on machine-readable surfaces). This reason is not eligible for relocation or auto-heal. A file opened for verification must be a regular file (`metadata.is_file()` check required).

This reject-all policy is identical at every citation resolution site and on
every platform (`symlink_citations_are_rejected_by_openat2_and_fallback_resolvers`,
`safe_join_rejects_symlink_components_and_parent_components`, and
`relocation_scan_skips_symlinked_candidates_and_never_auto_heals_them`). The
reason reveals only that an in-repository component is a symlink, not where it
points. The compatibility audit found 41 evidence rows and zero affected; see
ADR-5: Citation symlink policy (`docs/decisions/adr-5-symlink-policy.md`)
rather than duplicating its security rationale here.

**Caller-supplied hashes.** Supplying `citation_hash` does not bypass
authorship validation. `kb_core::add` computes the file hash and
rejects a mismatch before append; this is pinned by
`test_kb_core_add_rejects_wrong_explicit_citation_hash`. `parse_cite_target`
and `parse_citation_path` both require `start < end`, so empty and reversed
ranges are rejected (`test_parse_cite_target_rejects_start_greater_than_end`
and `test_parse_cite_target_rejects_empty_range_exactly`).

### MCP write-time rejection

The MCP `add` handler validates caller-supplied hashes more broadly than
`kb_core::add` alone. Before `handle_add` appends any event or performs any
database write, `validate_explicit_citation_hashes` (`src/commands/mcp.rs`)
re-verifies every evidence row that carries both a `citation_path` and a
non-empty `citation_hash`, using the same `verify_evidence` path- and
range-hashing policy described above. Any non-verified outcome — a
malformed range, a missing file, an out-of-bounds range, or a path-escape
attempt — rejects the whole `kb_add` call with:

```
evidence[i] citation_hash failed verification for citation_path "...": <reason>
```

and error code `validation_error`. This is stricter than the read-path
behavior described above: a stored row that later drifts still surfaces as
`UnverifiedReason` on verification, but an incoming MCP write with a
mismatched explicit hash never reaches storage at all.

### Worktree citation warning

`kb add` and the `kb_add` MCP tool print a warning for every `citation_path`
beginning with `.state/worktrees/`:

```
warn: citation_path under .state/worktrees/ will go stale after the worktree is removed: <paths>
```

This is a non-blocking warning (`add_validation::warn_nested_worktree_citations`,
called from both `src/commands/add.rs` and `src/commands/mcp.rs`): the write
still succeeds. It exists because a citation rooted in a disposable worktree
directory becomes unresolvable once that worktree is removed — such
citations should be re-cited against the merged path once work lands.

## Verification Semantics

When an evidence row is verified, its `citation_path` is parsed and the byte range is hashed. The result has three possible statuses:

| Status | Meaning |
|--------|---------|
| `Verified` | Hash matches the recorded `citation_hash` at the same location |
| `Relocated` | Hash no longer matches at the original path, but the excerpt was found uniquely at a new path |
| `Unverified` | Hash does not match and no unique relocation candidate was found |

### Infallible Verification

The `verify_evidence()` function **never returns an error**. All conditions that would prevent verification instead surface as `UnverifiedReason` values carried in the outcome:

- Malformed citation path
- File not found or not a regular file
- Symbolic link in any citation path component
- Byte range outside file bounds
- I/O errors during hashing
- Hash mismatch
- Relocation search failures

This is a design boundary: a stored evidence row may contain invalid or stale data, and the verification machinery must report that data faithfully without propagating errors. Genuine programming bugs (invariant violations in the verifier logic) panic; runtime conditions (including malformed data) surface as `UnverifiedReason` strings.

### Relocation of Whole-File Citations

When a whole-file citation's hash no longer matches at its original path, but the excerpt is found uniquely elsewhere, it relocates to the new bare path:

```
# Original: src/foo.rs (hash mismatch)
# Found at: src/components/foo.rs (unique match)
# Relocates to: src/components/foo.rs (bare whole-file form)
```

This is the natural generalization of range relocation: a whole-file citation at a new location becomes a bare whole-file citation at that location.

### Relocation safety and scan bounds

Relocation succeeds only for exactly one excerpt match. If a second match is
found, `Verifier::search_for_excerpt` stops and reports `NonUnique`; it never
picks one candidate. `multiple_candidates_report_multiplicity` and
`prop_non_unique_is_never_relocated` pin this behavior.

Repository-wide relocation does not descend into `.git`, `target`,
`node_modules`, `.state`, or `agent-kb`, and `excluded_names` also adds plain
non-glob names from the repository-root `.gitignore`. This prevents build
outputs and the KB's own stored excerpts from becoming candidates; see
`excluded_directories_are_not_searched`, `gitignored_directory_is_not_searched`,
and `search_never_treats_the_kb_store_as_a_relocation_candidate`.

The scan charges each candidate file's size against
`MAX_RELOCATION_SCAN_BYTES`. If the next file exceeds the remaining budget,
`Verifier::scan_file` returns `CapExceeded`, which becomes the user-visible
unverified reason `scan_cap_exceeded` (`ScanCapExceeded`). This means the scan
could not establish repository-wide uniqueness within its bounded work; it
does not mean that no candidate exists, and a candidate found before exhaustion
is not accepted as unique.

## The `kb cite` Tool

The `kb cite` command generates evidence row fields ready for inclusion in a `kb_add` call. It is the recommended way to construct citations.

### Usage

```bash
kb cite src/components/verification.rs
kb cite src/commands/cite.rs:1500-2300
```

### Output

The tool outputs a JSON object:

```json
{
  "citation_path": "src/components/verification.rs",
  "citation_sha": "12f44d5abc...",
  "citation_hash": "sha256:abc123...",
  "file_size": 12345
}
```

`citation_sha` is the git HEAD commit SHA resolved at the cited file's own
parent directory, not the process's current working directory
(`compute_citation_fields` in `src/commands/cite.rs`). A file cited from
inside a nested worktree therefore records that worktree's HEAD, which can
differ from the outer repository's HEAD at the time of citation.

### MCP Usage

The `kb_cite` MCP tool is also available for use within agents:

```
Call kb_cite with citation_path="src/foo.rs:100-200"
```

### Self-Check Property

The tool computes the hash through the verifier's own code path and asserts a round-trip property: before emitting a citation, it verifies that the row it is about to emit comes back as `Verified` from `verify_evidence()`. This ensures that any disagreement between the parser and verifier is caught immediately on the author's machine, rather than silently entering the corpus to be discovered later by a stale-check run.

If the self-check fails, the tool exits with an error message showing the verification status and reason code. This makes citation authorship safe: a malformed citation is caught at cite time, not stored.

## Migration: `kb migrate-citations`

The command `kb migrate-citations` heals legacy whole-file workaround citations from the old `path:0-<size>` form into the new bare `path` form.

### Usage

```bash
# Show the migration plan without applying
kb migrate-citations --dry-run

# Apply the migration
kb migrate-citations
```

### Idempotency

The migration is idempotent. Each row is re-verified before healing:

1. Parse the legacy `path:0-<size>` form.
2. Compute the current whole-file hash of `path`.
3. Compare size and hash against the stored values.
4. If both match, emit a `citation_healed` event rewriting the path to bare form.
5. If either check fails, skip or report the row as failed.

Running the command twice produces the same result: already-healed rows are skipped; unhealable rows remain unhealable.

### Special Cases

**Self-referential log citations.** Rows citing `agent-kb-events.jsonl` are reported as failed, not healed. The event log grows on every `kb_add`, so its hash can never be stable; such citations must be manually re-cited or expired.

**Healing failures.** Rows that do not match the healed form are reported by reason:

- `size mismatch`: File size changed since the citation was recorded.
- `whole-file hash mismatch`: File content changed.
- File I/O errors during re-verification.

## Streaming Hashes and Size Bounds

The hashing machinery uses a fixed 64 KiB buffer and streams citation bytes into the hasher in chunks. This bounds memory usage to a constant regardless of file size.

Two size limits apply:

| Limit | Value | Applies To |
|-------|-------|-----------|
| `MAX_FILE_BYTES` | 64 MiB | Whole-file citations (only bound on this form) |
| `MAX_RANGE_BYTES` | 4 MiB | Explicit byte-range citations (only bound on this form) |

Whole-file citations use the larger bound because the streaming reader makes their memory cost O(1) in file size. Explicit ranges retain the smaller bound; unifying them to use streaming is optional.

If a citation exceeds its applicable size limit, verification fails with `FileTooLarge` or `RangeTooLarge` respectively.

## Formal Specification

The citation path grammar and relocation semantics are formally specified in TLA+ at `.state/agent-kb/tla/CitationRelocation.tla`. The specification models citation semantics independent of byte ranges — it cares about hash matching and relocation, not the concrete path syntax — so the addition of optional ranges and the form change from `path:0-size` to bare `path` do not require amendments to the specification.

## See Also

- [Evidence Storage](./evidence-storage.md) — how evidence rows are retained and recovered
- [MCP Surfaces](./mcp.md) — the `kb_cite` tool
