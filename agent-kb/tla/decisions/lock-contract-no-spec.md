# Decision: scoped TLA+ waiver for C2's lock contract

**Date:** 2026-09-04  
**Task:** `bd-21ef.2.2` (C2/T1)  
**Plan:** `.state/.omc/plans/c2-exclusion-boundary.md`, ADR-1 and T1  
**Decision:** model the port protocol in `PortProtocol.tla`; waive a new lock-contract module only for the three surfaces assessed below. This is a scoped waiver, not a claim that `&Lock` proves every lock property.

## Per-task surface assessment

| Surface | New temporal semantics? | TLA+ disposition | Implementation/test obligation |
|---|---|---|---|
| Global `flock` writer exclusion | No. The global exclusive flock is already an assumption of `AgentKb.tla`. | **Waived.** A second model would restate the existing assumption rather than test a new transition system. | `open_rw(&Paths, &Lock)` requires a live, path-matching token; normal exclusion tests remain required. |
| Process-local re-entrancy / self-deadlock | Yes: a second acquire can wait forever. This is a liveness failure. | **Not covered by the waiver's flock argument and not proved by `&Lock`.** A type token proves that a matching live guard exists at a mutating open; it cannot prove that the process did not attempt to acquire the same flock twice. | ADR-1's process-local canonical-path registry in `acquire_lock` converts a second in-process acquire from a deadlock into an error. **L1a (`bd-21ef.2.3`) must carry the registry test**, with its bounded test timeout, proving the second acquire errors rather than hangs. **The test must also cover two path spellings that canonicalize to one file** (e.g. a relative path and `std::fs::canonicalize`'s output for it) — `PeersShow::execute` (`peers.rs:264-271`) already treats "as-is" and canonicalized spellings of the same `repo_path` as two lookups that must be reconciled by id; the registry has the same obligation in the other direction, keying on the canonicalized path so a second acquire under a *different* spelling of the same file is still recognized as re-entrant rather than silently deadlocking. |
| Two-phase peer TTL | Yes: an expired row may be logically absent while still physically present until a locked sweep. | **Waived with an explicit total-read argument, corrected below.** Safety does not depend on sweep timing because every *consumer-visible* peer read site applies `AND (expires_at IS NULL OR expires_at >= datetime('now'))`. Therefore no peer consumer can observe the logically expired row even if physical deletion is arbitrarily delayed. **This is not total over every peer read site** — see the inventory below, which corrects the original version of this row (it claimed five surfaces and missed a sixth, internal one at `peers.rs:441-448`). | **L1b's test must assert exactly this:** expired peers are invisible to list, show, edge-list, graph traversal, and federated search while the database row remains physically present (no delete has occurred). The internal sixth site (`peers.rs:441-448`) is explicitly excluded from that filter — see below. |

## Peer read-site inventory (corrects the original TTL row)

The original TTL row said the read filter is total over five named surfaces. It is total over
five *consumer-visible* surfaces, but there are six peer read sites in `src/`, and the sixth is
internal and must **not** get the filter. Enumerated by file:line:

| # | Site | File:line | Consumer-visible? | Filter applies? |
|---|---|---|---|---|
| 1 | `kb peers list` (`PeersList::execute` → `query_peers_for_repo`) | `peers.rs:196`, SQL at `peers.rs:300` | Yes — printed directly to the caller. | **Yes.** An expired peer must not appear in the list. |
| 2 | `kb peers show` (`PeersShow::execute` → `query_peers_by_either_repo`, called twice for the as-is and canonicalized path) | `peers.rs:259-278`, SQL at `peers.rs:339` | Yes — printed directly to the caller. | **Yes.** |
| 3 | `kb peers edge-list` (`PeersEdgeList::execute`) | `peers.rs:669-678` | Yes — printed directly to the caller. | **Yes.** |
| 4 | Federated peer-graph traversal (`collect_peer_paths` → `bfs_peers`/`query_direct_peers`/`query_neighbors` → `query_target_repos`) | `search.rs:258-345` (bind sites at `:294`, `:297`, `:340`, `:345`) | Yes, indirectly — controls which peer repos' entries are federated into a `kb search` result. | **Yes.** An expired peer edge must not be traversable, or a repo it points at leaks into federated results through a link that should no longer exist. |
| 5 | MCP `kb_peers_list` (`handle_kb_peers_list`) | `mcp.rs:1916-1935`, SQL at `mcp.rs:1928` | Yes — returned directly in the MCP response. | **Yes.** Same obligation as site 1, over the MCP surface instead of the CLI. |
| 6 | `kb peers import` duplicate-suppression check (inline `SELECT id FROM peers WHERE source_repo=?1 AND target_repo=?2 AND edge_type='member' AND (epic_slug IS ?3 ...)`) | `peers.rs:441-448` | **No.** The result is never returned to the caller; it only decides whether the loop `continue`s past this entry instead of inserting a new one. | **No — must not apply.** |

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

- [ ] code-reviewer confirms the registry and the six-site TTL-filter inventory (filter on sites 1-5, no filter on site 6) match this record.
- [ ] analyst confirms no uncovered state machine was introduced on these three surfaces.

