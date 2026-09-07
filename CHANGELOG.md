# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.2.0] - 2026-09-07

This release lands the storage-correctness-2 program (bd-21ef): a four-lens
review of crash durability, exclusion discipline, and read-path integrity,
split into three components (C1 log durability, C2 exclusion/boundary
discipline, C3 read-path integrity) plus follow-up hardening and docs.

### Breaking

- Event-log appends are now wrapped in a `batch_begin`/`batch_commit` framing
  envelope so a crash mid-batch never durably exposes a partial prefix; an
  older binary silently applies whatever lines landed instead of treating the
  batch as atomic. Run `kb compact` under the new binary before downgrading to
  strip markers back to legacy-readable lines (`docs/src/downgrade-procedure.md`)
  (bd-21ef.1.6, bd-21ef.1.14).
- The Elixir MCP server now requires a host-injected `caller_id` on the
  `kb_expire`, `kb_audit_run`, and `kb_audit_record` port methods; a caller
  cannot influence it via `initialize.clientInfo` or a `caller_id`-shaped
  argument (bd-21ef.2.8, bd-21ef.2.11).
- Mutating audit and expiry MCP tools are now gated by an OPA/Rego policy
  (`mcp/priv/policies/agentic_kb.rego`, default-deny, one principal
  pre-trusted: `agentic-kb-host`) plus a per-caller rate limit, and the
  server must launch with `--caller-id`. This adds a runtime dependency on
  the `opa` binary (wired into `flake.nix`'s devShell and package outputs)
  (bd-21ef.2.11, `docs/src/security/mcp-authorization.md`).
- New embeddings are stored pre-normalized (finite, non-zero, L2-normalized
  f16) with a per-blob marker distinguishing normalized rows (dot product)
  from legacy rows (cosine); run `kb migrate-embeddings` to convert an
  existing store, which stages, validates, and atomically publishes a
  migrated copy and retains a `.pre-normalized-embeddings.bak` backup
  (bd-21ef.3.13).
- The MCP `kb_add` handler now rejects the whole call when a caller-supplied
  `citation_hash` fails re-verification, instead of accepting it and letting
  it drift into an unverified row later (bd-21ef.3.4).
- The raw event-writer entry points `cursor::append_and_apply` and
  `cursor::append_and_apply_with` are gated behind the `event-log-test-raw`
  Cargo feature and no longer compile into a production binary; out-of-tree
  callers must go through `cursor::append_and_apply_writer_events`
  (bd-21ef.1.6).

### Added

- `kb migrate-embeddings` CLI command to convert legacy embedding blobs to
  the pre-normalized format (bd-21ef.3.13).
- `kb_audit_run`, `kb_audit_record`, and provenance MCP tools, previously
  hidden Rust-only port methods, are now exposed and authorized
  (bd-21ef.2.12).
- `kb_add` renders `similar_existing` near-duplicate matches in its MCP
  response instead of only warning on the CLI (bd-21ef.2.12).
- Durable applied-cursor (`kb_meta` rows `generation`/`offset`/`tail_sha`)
  and automatic recovery so append-ok/apply-crash converges without a manual
  rebuild; nine classified recovery rows drive read/write/compact behavior
  (`docs/src/recovery-protocol.md`) (bd-21ef.1.9).
- Federated search contract: a global result limit across the local
  repository and all peers, id-based dedup with local-row precedence, and
  exactly one truncation (bd-21ef.3.11).
- Crash-simulation harness with named kill points and a `crash-sim` Cargo
  feature for exercising rebuild-swap and reembed interruption points
  (bd-21ef.1.1).
- `kb add` write-path benchmark lane with a recorded baseline
  (bd-21ef.1.2, bd-21ef.1.19).
- Six-step rebuild live-file swap (checkpoint, verify drained WAL, close,
  rename, unlink sidecars, directory fsync) that leaves a plain reader
  seeing only the complete pre- or post-swap state at every crash point
  (bd-21ef.1.10, bd-21ef.1.11).
- Port protocol response correlation by request id and observable port-crash
  detection in the Elixir `PortManager` (bd-21ef.2.8).
- Keyed, idempotent `run_history` insertion, replacing the positional
  retention cap that made compaction non-materialization-preserving
  (bd-21ef.1.8).
- `beam27Packages.elixir` (Elixir 1.18) added to the default devShell
  (bd-21ef.2.1).

### Changed

- Every MCP method now validates against a typed, `deny_unknown_fields`
  request struct on the Elixir side, in addition to Rust-side validation, so
  a wrong-typed optional or malformed array member is rejected rather than
  silently defaulted or filtered (bd-21ef.2.5).
- `reembed` batches now run under a re-opened, lock-scoped writer with
  re-checked row selection, count raced-away entries instead of dropping
  them silently, and no longer double-count failures (bd-21ef.2.7).
- Database access now goes through nine named openers
  (`open_ro`/`open_rw`/`open_rw_existing`/`open_scratch`/`open_or_init`/
  `open_live_for_checkpoint`/`defer_checkpoints`/`open_unchecked_for_test`/
  `test_db`) instead of one general-purpose `open_db`; every mutation,
  including peer writes and the import duplicate check, now runs under
  `paths.lock` (`docs/src/lock-contract.md`) (bd-21ef.2.3, bd-21ef.2.6,
  bd-21ef.2.13).
- Repository-root derivation is unified behind `config::Paths` for both the
  Rust CLI/MCP and the Elixir MCP server, with the canonical `.state` marker
  taking precedence over the legacy layout before its first write
  (bd-21ef.2.10).
- CLI/MCP result `limit` (1-100) and MCP `inline_verify_k` (0-100) are now
  enforced as hard bounds at every entry point instead of silently clamped
  (bd-21ef.3.17).
- `--path-prefix` now escapes SQL `LIKE` metacharacters (`%`, `_`, `\`)
  before matching, so a literal prefix containing them can no longer match
  more than intended (bd-21ef.3.8).
- Evidence rows with `kind: derived` now require `derived_from` (bd-21ef.2.9).

### Fixed

- Non-finite (NaN/corrupt) stored embeddings are scored `0.0` and kept in
  the ranked result set instead of being dropped or destabilizing sort
  order; a non-finite query embedding now fails the whole search instead of
  scoring badly (bd-21ef.3.7).
- Every citation resolution site (verification, `kb cite`, relocation scan)
  now rejects any symlink path component fail-closed, on every platform,
  instead of only some sites (bd-21ef.3.6, ADR-5).
- `kb cite` now hashes, self-checks, and emits a citation from one retained
  file descriptor, closing a TOCTOU window between hashing and emission
  (bd-21ef.3.4).
- Provenance reports dangling parent references separately from root
  entries instead of conflating the two (bd-21ef.3.9).
- A malformed evidence value at read time now surfaces as a search error
  instead of silently dropping the row's evidence (bd-21ef.3.8).
- `audit_record` is now applied as one atomic SQLite transaction (event,
  audit row, and weight update together) via a savepoint, instead of as
  separate writes that could partially apply (bd-21ef.2.11).
- Compacted audit evidence ordering, stale-audit sampled entries, and
  audit-authorization recovery are now preserved/atomic across compaction
  and replay instead of being reordered or dropped.
- `kb peers list`/`show`/`edge-list` now read through `open_ro` instead of
  taking the write lock and running recovery on every read (bd-21ef.2.17).
- CI's `l1c_opener_migration` production-source pin no longer truncates
  early on an aggregator branch (bd-21ef.2.21).

### Performance

- `reembed`'s per-batch lock hold is reduced to well under the 50 ms budget
  by deferring SQLite's own close-time and automatic checkpoints out of the
  lock window and draining the WAL periodically between batches instead
  (measured ~20 ms at `load1` 4.9) (bd-21ef.2.19).
- `kb add`'s write-path benchmark is re-baselined like-for-like against a
  fixture that includes an event log: p50 87.5 ms / p95 155 ms, a median
  +7.5 ms / mean +10.5 ms overhead over the pre-D2 write path (1.10x p50 /
  1.24x p95), attributable to the added `fdatasync` call per add. The
  acceptance gate is now a like-for-like ratio gate rather than an absolute
  threshold measured on a no-event-log fixture (bd-21ef.7).
- The event-log reader verifies a closing span by its own declared size
  instead of a fixed 64 KiB window, so a span larger than 64 KiB no longer
  forces a full-log scan on every write (bd-21ef.1.20).
- Semantic and cue search lanes materialize full entry metadata for only
  the first `2 * limit` ranked candidates per lane instead of the whole
  scanned set (bd-21ef.3.12).

### Documentation

- Added `docs/src/event-log-format.md`: batch framing envelope, reader
  rules, durability ordering, and measured write-path cost.
- Added `docs/src/recovery-protocol.md`: applied-cursor fields and the nine
  classified recovery rows.
- Added `docs/src/downgrade-procedure.md`: format boundary, old-binary read
  behavior, and the compact-before-downgrade procedure.
- Added `docs/src/security/mcp-authorization.md`: `--caller-id` launch
  requirement, the bundled OPA policy, rate limits, and denial reasons.
- Expanded `docs/src/lock-contract.md` with the full opener-class table
  across every CLI command and MCP handler.
- Expanded `docs/src/search-tuning.md` with the federation contract, search
  bound enforcement, non-finite embedding handling, and pre-normalized
  embedding migration.
- Expanded `docs/src/citation-semantics.md` with symlink rejection, MCP
  write-time hash rejection, worktree-citation warnings, and relocation
  scan bounds.

### Security

- MCP mutating audit/expiry tools are fail-closed by default: a missing
  `--caller-id`, a denied OPA decision, an exhausted rate-limit quota, or an
  unavailable policy evaluator (missing `opa` binary, timeout) all deny the
  call rather than allow it (bd-21ef.2.11,
  `docs/src/security/mcp-authorization.md`).
- Citation path resolution rejects any symlink component at every site,
  closing a path-escape avenue previously possible via a symlinked
  component pointing outside the repository (ADR-5, bd-21ef.3.1,
  bd-21ef.3.6).

[0.2.0]: https://github.com/QuentinMallet/agentic-kb/compare/2e2051d...v0.2.0
