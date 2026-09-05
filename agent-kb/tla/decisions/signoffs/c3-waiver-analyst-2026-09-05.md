# Analyst Review: C3 spec waiver sign-off (bd-21ef.3.14)

Read-only audit at aggregator HEAD `542d49d`, worktree
`/home/urist/Documents/perso/agentic-kb/.state/program-worktrees/storage-correctness-2`.
No files edited, no build or test tooling run, no TLC invocation. The recorded TLC matrix
in `.state/agent-kb/tla/CitationRelocation-planheal-trace.md` is taken as given.

Waiver under review: `.state/agent-kb/tla/decisions/c3-search-tasks-spec-waiver.md`,
second sign-off box: "analyst confirms `CitationRelocation.tla` (as amended by T0) still
covers every modified path, and that no invariant of it is reachable from the three waived
tasks."

---

## 1. The spec, and whether the landed code still refines it

### Variables

| Variable | Meaning |
|---|---|
| `rows` | evidence rows keyed by stable row id, each `[status, storedHash, contentHash, candidates, excerptStrong, path]` |
| `pass` | current verification pass, `{0,1}` |
| `plans` | outstanding relocation plan per row, `NoPlan` or `[kind, newPath, premisePath, premiseLive, premiseContent]` |
| `previousRows` | rows immediately before the last transition |
| `previousPass` | pass immediately before the last transition |
| `lastAction` | witness record `[kind, row, before, pathStale, liveStale, contentStale, verdictStale, committed]` |

### Actions and their implementation counterparts

| Action | Spec lines | Implementation |
|---|---|---|
| `Init` | 151 | rows already exist carrying a `storedHash`; creation is out of module scope by T0 part (b) |
| `Verify` | 188 | `verification::verify_evidence` re-hashing against the stored hash; also the read-only inline verification in `db::search_entries` |
| `ReVerify` | 203 | a later pass re-hashing after a content change and refreshing the search evidence and path |
| `ConcurrentHeal` | 225 | `db.rs:1055`, a path-only overwrite of `citation_path` that starts no pass |
| `PlanHeal` | 241 | `stale_check::run_stale_check`'s third pass, which computes statuses and proposed paths and never writes |
| `ApplyHeal` | 280 | `stale_check::heal_relocations`, `src/commands/stale_check.rs:255-328` |

### Invariants (nine in the main cfg, confirmed in the cfg files)

`TypeOK`, `NoHealOnVerified`, `NoStaleHealCommit`, `VerifiedImpliesHashMatch`,
`StoredHashImmutable`, `NoSilentPromotion`, `Monotonicity`, `NonUniqueUnverified`,
`WeakExcerptUnverified`. `NoStalePlanEverDiscarded` is the non-vacuity probe, listed only
in `CitationRelocation_NV_Discard.cfg` and expected violated there. The isolation cfg drops
`NoStaleHealCommit` and keeps the other eight, which matches the recorded eight-green run.

### Does the landed code still refine each modelled path?

**`ApplyHeal` maps to `heal_relocations`, and all four premise conjuncts are now present.**
The function takes the flock at `stale_check.rs:271` and opens the write connection under it
at `:272`. Under the lock it requeries the complete evidence row joined to `entries` with
`e.is_stale=0`, so a missing or dead row falls through `.optional()` to `continue`. That is
`LivePremiseHolds`. It then compares the current `citation_path` against the plan's
`old_path` at `:306-308` and discards on mismatch. That is `PathPremiseHolds`, the conjunct
`UnsafeApply` removes and the one `NoStaleHealCommit` exists to enforce. It re-runs
`verify_evidence` at `:309` and requires both `status == Relocated` and
`relocated_to == new_path`. Requiring `Relocated` subsumes `ContentPremiseHolds` (a content
that has reverted to matching yields `Verified`, not `Relocated`) and `VerdictPremiseHolds`
(`Relocated` presupposes a strong excerpt and a unique candidate). Requiring the same
destination is strictly stronger than the model, and is exactly the code-side obligation the
module header defers to V2 and V3 rather than modelling. Every failure arm is `continue`,
never a fallback or a weakened commit, which is `ApplyHeal`'s discard arm.

**The commit arm preserves `StoredHashImmutable`.** The healed event is built from
`evidence.citation_hash` read back under the lock (`:320`) and the applied write touches only
`citation_path`. Nothing on this path assigns a new stored hash.

**`PlanHeal` remains write-free.** The doc comment on `run_stale_check` states the third pass
is read-only and computes statuses and proposed paths only, matching the spec's
"`PlanHeal` writes no row".

**V1 makes the `candidates` refinement mapping sound.** `search_for_excerpt`
(`verification.rs:917-1024`) no longer short-circuits on an in-file hit: it accumulates
`total` across the repo walk and returns `NonUnique` as soon as `total > 1`.
`count_occurrences` (`:1394-1414`) advances by one byte after a hit, so overlapping
occurrences count separately. The cited file is excluded from the walk by
`FileIdentity` `(st_dev, st_ino)` comparison at `:993-996`, not by path string.
`CapExceeded` inside the walk returns `ExcerptSearch::CapExceeded` at `:1000` even when a
candidate is already held, and both callers fold it to `ScanCapExceeded` unverified
(`:1085`, `:1231`) rather than degrading to the in-file candidate. Those are the three
consequences plus the fourth obligation T0 part (c) pinned. `NonUniqueUnverified` and
`WeakExcerptUnverified` therefore now have a sound mapping, which they did not have at
`2e2051d`.

**V3 refines `ApplyHeal` as described above,** and the fabricated-`premisePath` hazard T0
called out is closed: an undecodable `citation_path` column now propagates an error instead
of substituting `rel_path`, pinned by
`heal_relocations_errors_on_undecodable_citation_path_column`.

**V4b sits below the spec's abstraction** and does not disturb any refinement. See section 3.

Conclusion for part 1: the amendment's modelled paths are still faithfully refined by the
landed code, and in two places (destination agreement, error on an undecodable path column)
the code is strictly stronger than the model.

---

## 2. Reachability from each waived task

### S3a — provenance dangling references (`bd-21ef.3.9`)

`handle_provenance`, `src/commands/mcp.rs:2294-2432`. It opens the database with
`db::open_ro`, which issues `PRAGMA query_only=ON` (`db.rs:391`), so the connection cannot
write at the SQLite level. It prepares exactly two statements: a `COUNT(*)` over `entries`,
and `SELECT DISTINCT derived_from FROM evidence WHERE entry_id=?1 AND kind='derived' AND
derived_from IS NOT NULL ORDER BY derived_from`.

It touches the `evidence` table, but only the `kind` and `derived_from` columns. Neither is
a field of the spec's `EvidenceRow`, whose fields are `status`, `storedHash`, `contentHash`,
`candidates`, `excerptStrong`, `path`. It never reads `citation_path`, `citation_hash` or
`citation_excerpt`, never calls `verify_evidence`, never appends an event, and never
acquires the flock. The whole change is a partition of an existing reply set plus an
`ORDER BY`. It cannot reach any state the spec constrains, in either direction.

### S3b — context token budget (`bd-21ef.3.10`)

`src/commands/context.rs`. The command drives `db::search_entries` through
`build_candidates` with `inline_verify_k: 0`, so `verify_count` at `db.rs:3098` is zero and
every returned evidence row carries `verified: None` and `verification_status: None`. The
verification pool is never entered. The task's diff is confined to rendering, byte
projection and the token estimate; the only `conn.execute` in the file is at `:557`, inside
the `#[cfg(test)]` module that begins at `:509`. No lock, no event, no write, and the task
does not even read the spec's derived verdict. Unreachable.

### S4 — federated search contract (`bd-21ef.3.11`)

`merge_federated_results`, `src/commands/search.rs:23-65`. It is a pure function over
`Vec<(Option<String>, Vec<db::SearchEntry>)>`. It takes no `Connection`, performs no I/O,
stamps `origin_repo`, deduplicates by entry id with local winning collisions, sorts with
`compare_federated_rows`, and truncates once. It moves whole `SearchEntry` values; it never
constructs or edits a `SearchEvidence`. The enclosing federation loop reads the local
database through `open_ro` and each peer through `open_ro_peer`, which uses
`SQLITE_OPEN_READ_ONLY` with `immutable=1` (`db.rs:473-480`). No production line in
`search.rs` mentions `expires_at`; the only occurrences are test fixtures above the
`#[cfg(test)]` boundary at `:463`, so C2's `L1b` boundary is intact and S4 added no second
expiry notion. No event, no flock, no evidence-row mutation. Unreachable.

### The inline-verification read, addressed directly

S4's enclosing CLI path does invoke verification: `build_search_options`
(`search.rs:166-181`) sets `inline_verify_k: self.limit`, so `search_entries` verifies the
returned rows for both the local batch and every peer batch. That is a read of
spec-modelled state. It evaluates `HashMatch(row)`, the module's derived predicate, and
reports the resulting verdict in the response.

A read cannot violate an invariant of this module, for three independent reasons.

1. **Every invariant is a predicate over `rows`, `plans`, `previousRows`, `previousPass`
   and `lastAction`.** A safety invariant is falsified only by a transition that produces a
   state where it does not hold. A read performs no transition: `rows`, `plans` and `pass`
   are all unchanged, so no invariant's truth value can change. This is a property of the
   invariant class, not of this particular code.
2. **The outcome is never persisted.** `db.rs:3260-3300` attaches `verified` and
   `verification_status` to the in-memory `SearchEntry` and stops there. The local
   connection is `query_only`; the peer connection is opened `immutable`.
3. **The search path does not even run the relocation search.**
   `SEARCH_PATH_RELOCATION_POLICY` is `RelocationPolicy::Never` (`db.rs:117`), so
   `search_for_excerpt` is not reached from search, `candidates` and `excerptStrong` are
   never sampled, and no `Relocated` verdict can be produced. The two invariants most
   sensitive to a wrong relocation verdict, `NonUniqueUnverified` and
   `WeakExcerptUnverified`, are not touched even as reads.

S4 did not change `inline_verify_k`; clamping it at the shared boundary is S5's work
(`56f6122`), which keeps its T0 edge.

One honest observation, non-blocking: S4 changes which rows survive to be displayed, and on
a local-versus-peer id collision drops the peer row in favour of the local one. That
discards the peer's verdict, which was computed against the peer's own root. This is a
display-selection change with no counterpart in the model, which has no notion of federation
or of two repositories. It is not a state change and no invariant ranges over it.

---

## 3. Coverage of paths modified after T0 (V4b)

V4b's two changes are below the spec's level of abstraction.

`PathIds` is an abstract finite set. The model gives a path no internal structure: no
components, no symlink status, no `(st_dev, st_ino)`. V4b changes which concrete pathnames
are admissible inputs and how a pathname is turned into a file. That is the domain of the
refinement map, not the state machine. V4b adds no action, no variable, and no status
transition, so no invariant of the module gains or loses reachable states because of it.

- **Symlink rejection.** `safe_join` now rejects any component that is a symlink
  (`verification.rs:209-215`) and rejects a component-free relative path. The repo walk skips
  symlinks outright. `search_for_excerpt` derives `cited_identity` with
  `FileIdentity::of(&abs)`, a symlink-following pathname stat, but only for an `abs` that
  `safe_join` has already vetted, so the follow cannot cross a symlink. On the heal path,
  `relocation_heal_target` refuses to heal a `SymlinkPathRejected` citation, which lands in
  `ApplyHeal`'s discard arm and is therefore consistent with the model rather than a new
  behaviour.
- **Descriptor-derived identity.** `kb_core::add_locked` opens the citation through
  `open_citation_descriptor` and derives identity with `FileIdentity::of_file`, an fstat on
  the open descriptor, rather than a pathname stat (`kb_core.rs:257-266`). This is on the
  **write path** that establishes `storedHash`.

That second item falls inside T0 part (b)'s explicitly recorded non-coverage. The decision
record states in terms that the write-side TOCTOU is out of this module's scope, names the
correct future artefact as a separate `EvidenceWritePath.tla`, and records the residual risk
that nothing in this module's proof chain establishes that `storedHash` was correct when
first written. So it is a declared gap, not a discovered one.

I therefore read the waiver's phrase "still covers every modified path" as "the spec's
modelled paths are still faithfully refined by the modified code". On that reading it holds.
It cannot mean "the spec models every path C3 modified", because T0 part (b) decided on the
record that it does not, and a waiver about three search tasks is not the instrument that
reopens that decision. V4b kept its T0 dependency edge, is not in the waived set, and had
its own code review and security pass; its coverage question belongs there and to T0's
residual-risk record, not here.

The amended spec is checked, not merely written: all nine cfgs were run to completion with
observed outcomes matching expectations, including the eight-green isolation run and the
deliberate probe violation that proves `NoStaleHealCommit` is non-vacuous.

---

## Missing Questions

1. **Does "covers every modified path" mean modelled-path refinement or exhaustive
   modelling?** — The two readings give opposite verdicts once V4b's write-path change is in
   scope. Section 3 states the reading I applied. If the lead intended the exhaustive
   reading, this box can never be ticked, because T0 part (b) already decided against it.
2. **Does the waiver's second box cover only the three waived tasks, or all of C3?** — Its
   text mixes both: the first clause says "every modified path", the second says "the three
   waived tasks". I audited both scopes and they agree, so the ambiguity is not load-bearing
   here, but it should be resolved before the same wording is reused.

## Undefined Guardrails

1. **No stated bound on what a future "pure read-path" task may read.** — Suggested bound:
   a task stays inside the waiver's terms only if it neither writes an event, nor mutates an
   evidence row, nor acquires the flock, **and** opens through `open_ro` or `open_ro_peer`.
   The opener is the mechanically checkable half and is currently unstated.
2. **`inline_verify_k` is not named as a waiver-relevant surface.** — A read path that
   raises it does more verification work but still cannot violate an invariant. Suggested
   definition: the waiver's terms are indifferent to `inline_verify_k` precisely because
   `SEARCH_PATH_RELOCATION_POLICY` is `Never`. If a future task changes that constant, the
   waiver's reasoning stops applying and the T0 edge must be restored.

## Scope Risks

1. **Argument by analogy.** — The waiver already forbids this in its "What this does not do"
   section. The failure mode to watch is a task that reads `citation_path` or
   `citation_hash` and is called read-only by analogy with S3a, which reads neither.
2. **The relocation-policy constant.** — Prevent drift by pinning
   `SEARCH_PATH_RELOCATION_POLICY == RelocationPolicy::Never` in a test with a comment
   naming this waiver, so a future change to it fails loudly rather than silently voiding
   the reasoning in section 2.

## Unvalidated Assumptions

1. **The recorded TLC matrix reflects the spec at `542d49d`.** — Validate by re-running the
   nine cfgs from the trace document's reproduction block after the benchmark finishes. I
   was instructed not to run TLC, so this rests on the recorded matrix.
2. **`db.rs:1055` overwrites only `citation_path`.** — Asserted in the spec's
   `ConcurrentHeal` comment and consistent with everything I read on the heal path, but I
   confirmed the heal write only through `heal_relocations` and the event constructor, not
   by reading the apply arm at `db.rs:1055` itself. Validate by reading that arm.
3. **No non-test caller reaches the waived functions with a writable connection.** —
   Validated for the three call sites I read. A grep for other callers of
   `merge_federated_results` and `handle_provenance` outside `#[cfg(test)]` would close it
   mechanically.

## Missing Acceptance Criteria

1. **The waiver has no evidence artefact requirement for either sign-off box.** — Suggested
   criterion: each box cites the file and line ranges inspected and the HEAD audited, so a
   later reader can re-run the check. This report supplies that for the analyst box.
2. **No criterion pins the read-only property of the waived paths in code.** — Suggested
   measurable criterion: a test asserting that `handle_provenance` and the federated search
   path both operate on a `query_only` connection, so a future change from `open_ro` to a
   writable opener fails a test rather than only a review.

## Edge Cases

1. **A peer whose row shares an id with a local row.** — S4 drops the peer row and keeps the
   local one. Handled by contract and asserted in the task's acceptance criteria.
2. **A logically expired but physically present peer row.** — C2's read-time filter removes
   the peer upstream of the merge. S4 correctly adds no second check; verified by the absence
   of `expires_at` in production code.
3. **A citation whose path becomes symlink-rejected between plan and apply.** — The re-run
   `verify_evidence` no longer returns `Relocated`, so `heal_relocations` discards. This is
   `ApplyHeal`'s discard arm and is the correct behaviour.
4. **A cited path that is a hard link to a walked file.** — Excluded by `(st_dev, st_ino)`,
   with a proptest asserting unique exactly when one copy is planted.
5. **A scan cap hit after one candidate is found.** — Reports cap-exceeded and never degrades
   to the in-file candidate, which is the false-`Unique` case V1 exists to remove.

## Recommendations

Ordered by priority.

1. Tick the analyst box with the reading of "covers" stated in section 3 recorded alongside
   it, so the ambiguity does not resurface at merge.
2. Pin `SEARCH_PATH_RELOCATION_POLICY == RelocationPolicy::Never` with a test comment naming
   this waiver. It is the single load-bearing fact behind "a read cannot violate an
   invariant" that a future edit could silently change.
3. Re-run the nine TLC cfgs once the machine is quiet, to convert assumption 1 into evidence
   at the merged HEAD.
4. Add the opener condition (`open_ro` / `open_ro_peer`) to the waiver's terms as the
   mechanically checkable half of "no write".
5. Carry T0 part (b)'s residual risk forward as a named follow-up, `EvidenceWritePath.tla`,
   so the one declared non-coverage stays visible after this epic closes. Non-blocking.

Two non-blocking observations outside the waiver's terms, recorded so they are not lost.
The collision lookup in `merge_federated_results` scans `by_origin_and_id.keys()` linearly
per candidate, which is quadratic in the merged set; the map's `origin_repo` key component
is consequently never load-bearing, since collision is decided on id alone. Separately,
`query_target_repos` (`search.rs:371-383`) uses `filter_map(|r| r.ok())` on peer-path rows,
the silent-default shape Principle 2 forbids; it is pre-existing and outside S4's diff.

## Open Questions

- [ ] Does "covers every modified path" mean modelled-path refinement, or exhaustive spec
  coverage of all C3-modified code? — The two readings give opposite verdicts on V4b's
  write-path change, and only the first is achievable given T0 part (b).
- [ ] Should the waiver's terms name the database opener (`open_ro` / `open_ro_peer`) as the
  checkable half of "no write"? — Without it, "read-only" is a review judgement rather than a
  mechanical property.
- [ ] Is `EvidenceWritePath.tla` a follow-up the program wants, or is T0 part (b)'s residual
  risk accepted permanently? — V4b's descriptor-derived identity now lands inside that
  declared gap, which raises its salience.

---

## Verdict

- S3a (`bd-21ef.3.9`, provenance dangling references): **COVERED-AND-UNREACHABLE**
- S3b (`bd-21ef.3.10`, context token budget): **COVERED-AND-UNREACHABLE**
- S4 (`bd-21ef.3.11`, federated search contract): **COVERED-AND-UNREACHABLE**

**SIGN-OFF GRANTED**
