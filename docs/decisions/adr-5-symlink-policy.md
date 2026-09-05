# ADR-5: Citation symlink policy

- Status: Accepted 2026-09-05
- Decision: Option A — fail closed

## Context

Citation verification resolves repository-relative paths at three sites: the
direct descriptor open, `safe_join` for cited-file relocation, and the
repository relocation walk. Their historical behavior differed: Linux
`openat2` permitted contained symlinks, its `O_NOFOLLOW` fallback rejected
them, `safe_join` followed them while canonicalizing, and the repository walk
rejected them.

The A0 audit reviewed 41 evidence rows. Zero traverse symlinks and zero would
flip verification status under a reject-all policy.

## Decision

Every citation path component must be a non-symlink on every platform. Linux
uses `RESOLVE_NO_SYMLINKS` with `openat2`; the descriptor-walk fallback uses
`O_NOFOLLOW`; `safe_join` checks each component with `symlink_metadata`; and
the repository relocation walk continues to skip symlinks. A direct citation
rejected for this reason reports `symlink_path_rejected`. It is neither
`file_missing` nor `path_escape`, is not eligible for relocation, and can
never produce an auto-heal event.

This is fail-closed because citation resolution is a containment path. A
uniform refusal is easier to audit and preserves the meaning of the stored
repository-relative path without depending on mutable link targets.

## Rejected alternative

Option B, allowing symlinks whose resolved targets remain in the repository,
was rejected. Matching that behavior across platforms would put hand-rolled
resolution code on the containment path, increasing race and semantic-parity
risk for no demonstrated compatibility benefit.
