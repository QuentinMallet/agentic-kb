# Decision: scoped TLA+ waiver for C2's lock contract

**Date:** 2026-09-04  
**Task:** `bd-21ef.2.2` (C2/T1)  
**Plan:** `.state/.omc/plans/c2-exclusion-boundary.md`, ADR-1 and T1  
**Decision:** model the port protocol in `PortProtocol.tla`; waive a new lock-contract module only for the three surfaces assessed below. This is a scoped waiver, not a claim that `&Lock` proves every lock property.

## Per-task surface assessment

| Surface | New temporal semantics? | TLA+ disposition | Implementation/test obligation |
|---|---|---|---|
| Global `flock` writer exclusion | No. The global exclusive flock is already an assumption of `AgentKb.tla`. | **Waived.** A second model would restate the existing assumption rather than test a new transition system. | `open_rw(&Paths, &Lock)` requires a live, path-matching token; normal exclusion tests remain required. |
| Process-local re-entrancy / self-deadlock | Yes: a second acquire can wait forever. This is a liveness failure. | **Not covered by the waiver's flock argument and not proved by `&Lock`.** A type token proves that a matching live guard exists at a mutating open; it cannot prove that the process did not attempt to acquire the same flock twice. | ADR-1's process-local canonical-path registry in `acquire_lock` converts a second acquire **on the same thread** from a deadlock into an error. The registry is keyed per thread (`(ThreadId, PathBuf)`, `src/commands/add.rs:230`); a second acquire from a *different* thread of the same process deliberately blocks rather than erroring — that is ordinary mutual exclusion, and rebuild's schema-upgrade single-flight and its Phase 2 concurrent-writer guarantee both depend on same-process threads serializing on the flock rather than failing, pinned by `tests/open_split.rs:499` (`a_second_thread_blocks_on_the_flock_rather_than_being_rejected`). **L1a (`bd-21ef.2.3`) must carry the registry test**, with its bounded test timeout, proving the second acquire errors rather than hangs. **The test must also cover two path spellings that canonicalize to one file** (e.g. a relative path and `std::fs::canonicalize`'s output for it) — `PeersShow::execute` (`peers.rs:264-271`) already treats "as-is" and canonicalized spellings of the same `repo_path` as two lookups that must be reconciled by id; the registry has the same obligation in the other direction, keying on the canonicalized path so a second acquire under a *different* spelling of the same file is still recognized as re-entrant rather than silently deadlocking. |
| Two-phase peer TTL | Yes: an expired row may be logically absent while still physically present until a locked sweep. | **Waived with an explicit total-read argument, corrected below.** Safety does not depend on sweep timing because every *consumer-visible* peer read site applies `AND (expires_at IS NULL OR expires_at >= datetime('now'))`. Therefore no peer consumer can observe the logically expired row even if physical deletion is arbitrarily delayed. **This is not total over every peer read site** — see the inventory below, which corrects the original version of this row (it claimed five surfaces and missed a sixth, internal one at `peers.rs:441-448`). | **L1b's test must assert exactly this:** expired peers are invisible to list, show, edge-list, graph traversal, and federated search while the database row remains physically present (no delete has occurred). The internal sixth site (`peers.rs:441-448`) is explicitly excluded from that filter — see below. |

## Peer read-site inventory (corrects the original TTL row)

The original TTL row said the read filter is total over five named surfaces. It is total over
five *consumer-visible* surfaces, but there are six peer read sites in `src/`, and the sixth is
internal and must **not** get the filter. Enumerated by file:line:

**File:line anchoring note (amended 2026-09-06):** the line numbers originally recorded here had
drifted from HEAD within weeks (verified by the C2 waiver code-reviewer pass, see the "Sign-off
record" below). Line numbers are dropped from this table in favor of function-name anchors, which
survive reflow; the file names are kept.

| # | Site | Function anchor (file) | Consumer-visible? | Filter applies? |
|---|---|---|---|---|
| 1 | `kb peers list` (`PeersList::execute` → `query_peers_for_repo`) | `query_peers_for_repo` (`peers.rs`) — predicate injection site inside the function | Yes — printed directly to the caller. | **Yes.** An expired peer must not appear in the list. |
| 2 | `kb peers show` (`PeersShow::execute` → `query_peers_by_either_repo`, called twice for the as-is and canonicalized path) | `query_peers_by_either_repo` (`peers.rs`) — predicate injection site inside the function | Yes — printed directly to the caller. | **Yes.** |
| 3 | `kb peers edge-list` (`PeersEdgeList::execute` → `query_peer_edges`) | `query_peer_edges` (`peers.rs`) — predicate injection site inside the function | Yes — printed directly to the caller. | **Yes.** |
| 4 | Federated peer-graph traversal (`collect_peer_paths` → `bfs_peers`/`query_direct_peers`/`query_neighbors` → `query_target_repos`) | `query_direct_peers` and `query_neighbors` (`search.rs`) — predicate injected at both bind sites in each function | Yes, indirectly — controls which peer repos' entries are federated into a `kb search` result. | **Yes.** An expired peer edge must not be traversable, or a repo it points at leaks into federated results through a link that should no longer exist. |
| 5 | MCP `kb_peers_list` (`handle_kb_peers_list`) | `handle_kb_peers_list` (`mcp.rs`) — predicate injection site inside the function | Yes — returned directly in the MCP response. | **Yes.** Same obligation as site 1, over the MCP surface instead of the CLI. |
| 6 | `kb peers import` duplicate-suppression check (inline `SELECT id FROM peers WHERE source_repo=?1 AND target_repo=?2 AND edge_type='member' AND (epic_slug IS ?3 ...)`) | `PeersImport::execute` (`peers.rs`) — inline check, no separate query helper | **No.** The result is never returned to the caller; it only decides whether the loop `continue`s past this entry instead of inserting a new one. | **No — must not apply.** |

**Why site 6 is different, and why the recommendation is to leave it unfiltered rather than add the
filter for uniformity:** the `peers` table has no `UNIQUE` constraint on `(source_repo,
target_repo, edge_type, epic_slug)` (`db.rs:388-397` — the four `CREATE INDEX` statements are
non-unique). Deduplication is therefore *entirely* the responsibility of this `SELECT`-before-`INSERT`
check; there is no database-level backstop. If the check were changed to add the TTL filter, an
expired-but-still-present row would stop counting as "already exists", and a repeat `kb peers
import` run against the same seed file would insert a second, functionally-duplicate edge for a
pair whose original edge merely hasn't been swept yet. That is a duplication bug, not a
correctness fix — the row still physically exists, sweeping it is a separate concern owned by the
locked `sweep_expired_peers` path, and import's job is only to avoid creating redundant edges. So
site 6 must keep querying the *unfiltered* existence, and L1b's test list (below) must assert the
absence of the filter here as a directed negative test, not just assert its presence everywhere
else.

**L1b test list derived from this table** (`bd-21ef.2.4`):
- Sites 1-3, 5: seed one expired peer (physically present, `expires_at` in the past) and one live
  peer; assert the expired one is absent from `kb peers list`, `kb peers show` (both path
  spellings), `kb peers edge-list`, and the MCP `kb_peers_list` response, while a direct row count
  against the table shows both rows still present (no delete has occurred).
- Site 4: seed an expired peer edge as the only path from repo A to repo B; assert `kb search
  --reachable-from A` (or the equivalent federation option) does not federate B's entries, while
  the edge row still physically exists.
- Site 6 (negative test): seed an expired peer matching the exact `(source_repo, target_repo,
  edge_type, epic_slug)` tuple of a seed-file entry; run `kb peers import` with that seed file and
  assert it is **skipped** (added count does not increase, no second row is inserted) — i.e. that
  the duplicate-suppression check found the expired row despite its expiry, proving the filter was
  correctly *not* applied here.

## Why these are three different judgments

The flock row is an already-modeled environmental assumption. The re-entrancy row is a real liveness hazard whose mitigation is the runtime registry, not the signature and not the existing model. The TTL row is new temporal state, but its externally visible result is a simple read post-condition because the expiry predicate is total at every *consumer-visible* read boundary — total over five surfaces, deliberately not over the sixth, internal one. Combining the rows into “the lock is covered” would hide both the deadlock limitation and the TTL proof obligation, and combining “total over every read site” without the site-6 exception would misstate the TTL argument itself.

## Scope and withdrawal condition

This record waives only a new TLA+ module for these ADR-1 surfaces. It does not waive `PortProtocol.tla`, L1a's re-entrancy test, L1b's physically-present/logically-invisible TTL test, or post-implementation reviewer and analyst audit. If implementation introduces a peer read path without the predicate, or any nested-acquire path outside the registry, the corresponding waiver argument is invalid and must be revisited before merge.

## Related

- `.state/agent-kb/tla/AgentKb.tla` — existing global-exclusive-flock assumption.
- `.state/agent-kb/tla/PortProtocol.tla` — the temporal protocol surface modeled by T1.
- `.state/agent-kb/tla/PortProtocol-counterexample.md` — T1's TLC run matrix and counterexample traces.
- `.state/agent-kb/tla/decisions/kb-write-traffic-akb-no-spec.md` — per-task waiver-table precedent.
- `.state/agent-kb/tla/decisions/c3-search-tasks-spec-waiver.md` — scoped-waiver precedent.

## Reviewer + analyst sign-off

- [x] code-reviewer confirms the registry and the six-site TTL-filter inventory (filter on sites 1-5, no filter on site 6) match this record.
- [x] analyst confirms no uncovered state machine was introduced on these three surfaces.

## Sign-off record (2026-09-05)

**Code-reviewer pass** (opus code-reviewer agent, `rev-waiver-c2`): **SIGN-OFF GRANTED.** The
registry matches the record in substance, and the six-site TTL-filter inventory matches exactly
— filter on sites 1-5, no filter on site 6, and the six-site enumeration is total at HEAD
(`542d49d`). Full report: `signoffs/c2-waiver-reviewer-2026-09-05.md`.

**Analyst pass** (opus analyst agent, `audit-waiver-c2`): **SIGN-OFF GRANTED.** No uncovered
state machine was introduced on these three surfaces. Recommended non-blocking follow-up, quoted
verbatim from the report's closing section: "Finding 11 is a recommended follow-up task, not a C2
blocker." Full report: `signoffs/c2-waiver-analyst-2026-09-05.md`.

## Amendment (2026-09-06)

Two corrections from the code-reviewer sign-off pass (`signoffs/c2-waiver-reviewer-2026-09-05.md`,
findings F3/MEDIUM and F4/LOW), applied above:

1. **Re-entrancy row wording (was imprecise, not wrong in substance).** The row previously said the
   registry "converts a second in-process acquire from a deadlock into an error." The registry is
   keyed per thread, not per process (`(ThreadId, PathBuf)`, `src/commands/add.rs:230`), so a second
   acquire from a *different* thread of the same process deliberately blocks rather than erroring —
   `tests/open_split.rs:499` (`a_second_thread_blocks_on_the_flock_rather_than_being_rejected`) pins
   this as intended behavior, and rebuild's schema-upgrade single-flight plus its Phase 2
   concurrent-writer guarantee both depend on same-process threads serializing on the flock rather
   than failing. The row now reads "a second acquire **on the same thread**," with that
   cross-thread-contention note inline.
2. **Inventory table file:line drift.** Every file:line citation in the peer read-site inventory
   table had drifted from HEAD within weeks of being recorded. Replaced with function-name anchors
   (`query_peers_for_repo`, `query_peers_by_either_repo`, `query_peer_edges`, `query_direct_peers`,
   `query_neighbors`, `handle_kb_peers_list`, and the inline check in `PeersImport::execute`),
   keeping the file names (`peers.rs`, `search.rs`, `mcp.rs`). Substance was intact throughout —
   every cited surface still existed and behaved as described; only the line numbers were stale.

Not amended (out of scope for this pass, left for a separate follow-up if pursued): the `db.rs`
non-unique-index citation in the paragraph following the inventory table has the same drift and
was not touched here.

