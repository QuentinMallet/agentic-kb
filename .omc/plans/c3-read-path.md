# Plan: C3 — read-path integrity & performance

Component epic `bd-21ef.3` of meta-epic `bd-21ef` (storage-correctness-2). Source findings: the
4-lens deep review of master `2e2051d` (2026-09-04), lens 2 (verification, findings 1–10) and
lens 3 (search, findings 1–10 — finding 4 was excluded by the epic description and reassigned back
to C3 by lead ruling). Zero Criticals in both lenses. In scope for C3: **15 Importants and 5 Minors** — lens3 #4 was excluded by the epic description and restored by lead ruling (see §Cross-component ordering).

Mode: **DELIBERATE** consensus (RALPLAN-DR). Risk class: **high** — this component changes the
verification substrate that every KB claim rests on and changes user-visible search result
contracts. Blast radius is system-wide even though most individual edits are local.

Branch: from the `storage-correctness-2` aggregator. Post-impl merges component → aggregator
only; the aggregator reaches master when C1, C2 and C3 all close.

**Revision 2.** Revision 1 was REJECTed by the critic pass, with the architect pass independently
reaching three of the same conclusions. §"Review history" records what changed. The three
blocking defects were: ADR-1 chose a versioning vehicle that fires the exact re-embedding
rebuild ADR-1 rejects; P1's headline acceptance criterion was unachievable against the RRF
fusion code; and S2 implemented a finding the epic assigns to `bd-21ef.2`.

---

## Principles

1. **Correctness lands before performance.** Every perf task sits downstream of the correctness
   task whose decision it consumes. No correctness task's *closure* may be gated on a
   measurement — measurements attached to correctness tasks are reporting obligations discharged
   at post-impl, not gates.
2. **A guess is never a result.** Relocation, decoding and ranking must each either produce an
   answer they can justify or report that they could not — never a plausible-looking default.
   `filter_map(|r| r.ok())` and `unwrap_or(Equal)` are the two shapes this principle forbids.
3. **One policy per behaviour, chosen explicitly, at every site that implements it.** Where Linux
   and the fallback disagree, or where local and peer repos are scored differently, the fix is a
   written-down decision applied to *all* the code paths that implement the behaviour — the
   review found three symlink policies where the plan had assumed two.
4. **A write-format change is versioned, migrated, and measured** — never inferred from the bytes
   it produced, and never versioned through a mechanism whose real effect is something else.

5. **An invariant enforced at one adapter is not enforced.** A cap the MCP layer applies and the
   shared function does not is a cap only for MCP callers. *(Withdrawn in Revision 2 when lens3 #4
   went to C2; reinstated when the lead ruled it back to C3.)*

## Decision drivers (top 3)

1. **Soundness of the evidence substrate.** The KB's entire value proposition is that a citation
   means something. V1/V2/V3 each let an unsound citation be written or blessed.
2. **Determinism as a testability precondition.** Nondeterministic ranking makes every retrieval
   regression test flaky-by-construction, and constrains the top-K perf work, whose comparator
   *is* the total order.
3. **Migration cost of the embedding format.** Pre-normalization is the only item here that
   rewrites persisted data. Its cost and reversibility drive whether it lands at all — and after
   the review pass measured that cost, the answer is that it does not land in this component as
   an implementation. See ADR-1.

---

## Context

Two code regions, one component, because they share files and reviewers:

**Verification** (`src/components/verification.rs`, `src/commands/cite.rs`,
`src/commands/migrate_citations.rs`, `src/commands/stale_check.rs`,
`src/components/kb_core.rs`) decides whether a stored citation still describes the bytes it
claims to. Governed by `.state/agent-kb/tla/CitationRelocation.tla`.

**Search** (`src/components/db.rs` `search_entries` and its lanes, `src/commands/search.rs`,
`src/commands/context.rs`, `src/models.rs`) decides which entries come back and in what order.

The findings cluster into four failure shapes:

| Shape | Findings |
|---|---|
| Time-of-check/time-of-use across an unheld lock | lens2 #3, #4, #8 |
| A uniqueness test that does not establish uniqueness | lens2 #1, #2, #7 |
| Platform/adapter divergence presented as one behaviour | lens2 #5, #6 |
| Silent substitution of a default for an unknown | lens3 #1, #2, #3, #5, #6, #7, #8 |
| An invariant enforced at one adapter instead of the shared boundary | lens3 #4 |
| Full materialization where a bounded one would do | lens3 #9, #10 |

### What the code actually does (verified at HEAD `2e2051d`)

All line references below were independently re-verified by the architect and critic passes.

- `search_for_excerpt` (`verification.rs:645`) scans the cited file first and **returns `Unique`
  on a single in-file hit at `:662`**, before the repo walk at `:686`. Under `FileThenRepo` the
  repo is never consulted, so `NonUniqueUnverified` is satisfied against a candidate count that
  was never computed. The repo walk would also re-scan the cited file, so the fix needs an
  explicit exclusion.
- `count_occurrences` (`verification.rs:809`) advances `i += needle.len()` after a hit (`:823`),
  so two overlapping occurrences count as one.
- **Three symlink policies, not two** (this corrects Revision 1):
  1. `open_citation_file` (`verification.rs:366`) — Linux `openat2` with
     `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` **permits** contained symlinks; the fallback
     (`:403`) sets `O_NOFOLLOW` per component and **rejects** all. This is lens2 #6.
  2. The relocation repo walk (`verification.rs:704–710`) — **rejects** all symlinks.
  3. The cited-file relocation branch (`verification.rs:655`) — uses `safe_join`, which
     **canonicalizes** (`verification.rs:194–195`) and therefore fully resolves symlinks before
     the containment prefix check. `scan_file`'s own `symlink_metadata` rejection at
     `verification.rs:768` is **dead code on this path**, because the path it receives is already
     canonical and by construction never a symlink. `safe_join` has a third caller at
     `mcp.rs:464`.
- `compute_citation_fields` (`cite.rs:77`) hashes by pathname, then `self_check_citation_fields`
  (`cite.rs:87`) re-opens the same pathname through `verify_evidence`. Two independent opens.
  `parse_cite_target` rejects `start > end` at `cite.rs:147`.
- `kb_core::add` resolves missing hashes at `:154–193`, memoized by path **string**, and acquires
  the flock only at `:238`. Resolved hashes are appended without re-verification.
- `stale_check::heal_relocations` (`:220`) takes the flock at `:230`, but the relocation plan was
  computed by `run_stale_check` at `:194`. Under the lock it requeries only `citation_hash`
  (`:239`) — never `citation_path`.
- `citation_targets_events_log` (`migrate_citations.rs:230`) compares normalized path **strings**.
- `opened_file_within_repo` (`verification.rs:486`) emits its degraded-containment warning only
  after a successful open and `is_file()` check — an existence oracle. **It is declared and
  emitted under `#[cfg(not(target_os = "linux"))]`** (`verification.rs:485`, `:503–508`), so it
  does not exist on this repo's platform.
- Every ranking lane sorts with `partial_cmp(...).unwrap_or(Equal)` and no id tie-breaker:
  semantic `db.rs:1882`, cue `db.rs:1939`, RRF `db.rs:2043`, post-recency `db.rs:2099`, MMR
  argmax `db.rs:1730`. Cue best-per-entry (`db.rs:1926`) uses `prev >= sim`, false for NaN, over
  a `HashMap`. The cue query selects `c.cue` at `db.rs:1891` but **never reads column index 1**
  (`:1899–1907`), and the `best` value tuple has no cue field.
- Both FTS lanes are `ORDER BY rank LIMIT ?` with no secondary key (`db.rs:1415`, `:1455`) — and
  **these are the same two string literals** the `LIKE` fix must edit (`db.rs:1413`, `:1453`).
  The parity gate at `:1762` already compares `BTreeSet`s of ids (`:1770–1780`), but of sets each
  truncated by the query's own `LIMIT`.
- **RRF consumes the full candidate vector.** `db.rs:1964` is
  `for (sem_rank, (_, id, ..)) in candidates.iter().enumerate()` — every semantically matched
  entry in the corpus contributes `1/(k+rank)`, and truncation to `limit * 2` happens only later
  at `db.rs:1988`. The cue lane *is* pre-truncated to `limit * 2` (`db.rs:1941`) and the
  semantic-only branch takes `opts.limit` (`db.rs:2119`). This single fact determines P1's shape.
- `decode_emb_blob` (`models.rs:53`) dispatches on **blob length only**. There is no version
  field in the blob.
- Embeddings are **not** in the event log. Written at `db.rs:865`, `db.rs:881`, `reembed.rs:106`,
  `mcp.rs:1124`. Read for similarity at `db.rs:1875`/`:1877` (semantic), `:1921`/`:1923` (cue),
  and **`db.rs:1724` (MMR diversity term)** — the last inside an O(selected × remaining) loop,
  the densest similarity site in the file.
- **`SCHEMA_VERSION = 2` is not a passive stamp.** Its documented contract (`db.rs:120–124`) is
  "bump when derived state requires **replaying the event log**". `schema_is_current`
  (`db.rs:137–146`) goes false for every existing DB on a bump, and
  `rebuild_if_schema_obsolete` (`rebuild.rs:112`) is called from six entry points (`search.rs:86`,
  `add.rs:77`, `mcp.rs:112`, `eval.rs:72`, `ingest.rs:126`, `migrate_citations.rs:76`). With a
  real embedder it performs a mandatory `VACUUM INTO` backup (`rebuild.rs:243–262`) and a full
  replay that recomputes every vector. Under `KB_NO_EMBED` it defers and **never stamps**
  (`rebuild.rs:225–233`). Rebuild also holds a two-lock surface (`rebuild.rs:129`, `:253`,
  `:332`, `:397`) with a documented self-deadlock hazard at `:125–128`.
- There are **eleven** `impl Embedder`, not two: `embedder.rs:21` (Noop, returns `vec![]` at
  `:22–24`), `:127` (Candle), `bench_fixture.rs:68` (divides by norm unguarded at `:59–62`), five
  in-crate fakes in `db.rs`, `compress.rs:257`, and six in `tests/`.
- `compress.rs:231` is a **private duplicate of `cosine_similarity`**, used at `compress.rs:123`.
- Federated search (`search.rs:127–160`) forces `recency_lambda = 0.0` and `mmr_lambda = 0.0` for
  peers while local keeps both; `local_ids` is never updated after construction; each repo
  independently returns up to `limit`. `search.rs:108` sets `inline_verify_k: self.limit`, and
  inline verification runs **inside** `search_entries` (`db.rs:2194`), once per peer.

---

## ADR-1 — Pre-normalized embeddings: **measure, then decide** (revised)

**Decision.** C3 does **not** implement the persisted-format change. P2 is a measurement-and-
decision task that produces a recorded verdict on whether pre-normalization is worth landing, and
lands only the parts that carry no format risk. The format change, if justified, becomes a
follow-up epic.

**Why this reverses Revision 1.** Revision 1 proposed `SCHEMA_VERSION` 2 → 3 plus a `kb_meta`
marker, asserting the migration was "a pure in-place pass… needs no embedder, so it runs under
`KB_NO_EMBED`." Both review passes independently established that this is false:

- Bumping `SCHEMA_VERSION` arms `rebuild_if_schema_obsolete` on six entry points. On the next
  `kb search`, every existing DB takes a mandatory backup and a **full event-log replay that
  re-embeds every vector through the model** — verbatim the alternative ADR-1's own table
  rejected, and verbatim this plan's own "Must NOT have".
- Under `KB_NO_EMBED` — the mode Revision 1 claimed as the migration's home — rebuild *defers and
  never stamps*, so the migration can never complete and the DB warns forever.

The marker could be decoupled from `SCHEMA_VERSION` (use `kb_meta.emb_normalized` alone, driven
by an explicit `kb migrate-embeddings`). That repairs the mechanism. But it makes the migration
**opt-in**, which changes the economics: the perf win reaches only users who run the command,
while the code must carry *both* read paths — marker-gated dot product and legacy cosine — at
three similarity sites (`db.rs:1875/:1877`, `:1921/:1923`, `:1724`) permanently, in the hottest
loops in the codebase. That is a permanent branch in the hot loop bought for roughly a third of
the per-row arithmetic, for an unmeasured fraction of users. Revision 1's own decision driver 3
says migration cost "drives whether it lands at all"; having now measured the cost, the honest
application of that driver is a measure-first gate.

**What P2 lands unconditionally:** the ADR-3 finiteness guards (they are correctness, and they
are a hard precondition for any future normalization — normalizing a non-finite vector yields an
all-NaN blob that is byte-legal, length-legal, and permanently corrupt), and the
`compress.rs:231` duplicate-cosine consolidation.

**What P2 produces:** a measurement of the actual cost of recomputing `norm_b` per stored vector
at the `bd-3mr` baseline corpus sizes, isolated at all three read sites including MMR, plus a
written verdict against a threshold stated before measuring.

**If the verdict is "land it", the follow-up epic must decide** (recorded now so the follow-up
does not re-derive them): marker-only versioning with no `SCHEMA_VERSION` bump; the marker's
lifecycle across `rebuild`'s fresh-DB swap, `reembed`, and fresh-DB creation — a `rebuild` that
silently drops the marker reverts every read to cosine with no signal; per-blob rather than
per-DB gating, since one unmigrated legacy blob in a marked DB would be scored by dot product
without normalization and silently mis-ranked; a release-build write guard, not a
`debug_assert`, at the normalization choke point; conformance of all eleven `Embedder` impls,
with `NoopEmbedder`'s `vec![]` and `bench_fixture.rs`'s unguarded norm division exempted by
`is_noop()` rather than by length; a zero-norm policy, since normalization *itself* can
manufacture the NaN that the pre-normalization finiteness check just cleared; and a backup/abort
path for an in-place rewrite of every embedding blob, which Revision 1 specified nowhere.

**Material fact from C1, added at the PM gate — it changes the follow-up's cost basis, not this
decision.** `c1-log-durability.md:231`, `:330`, `:577`: C1's applied-cursor migration **itself bumps
`SCHEMA_VERSION` 2 → 3**. If C1 lands on the aggregator, the mandatory backup and full
replay-with-re-embed that this ADR rejects as prohibitive **is a cost the aggregator already pays**,
and a pre-normalization migration riding that same rebuild costs approximately nothing extra: no
marker, no opt-in problem, no permanent dual read path, no per-blob gating hazard. The entire
follow-up decision list below would collapse.

C3 must **not** manufacture that coupling — C1's landing is not guaranteed, and creating a
cross-component dependency on it would be exactly the kind of hidden ordering constraint this
component is fixing elsewhere. The deferral stands. But P2's pre-registered threshold must be
stated against the *marginal* migration cost on an aggregator that may already be paying for a
rebuild, not against a standalone-component cost. P2's task text carries this.

Relatedly, `c1-log-durability.md:238-246` answers a question Revision 2 had left open: rebuild
replays into a fresh tmp DB whose `kb_meta` receives only `schema_version` and `embed_text_mode`,
so **arbitrary `kb_meta` keys are not carried across the rename**. A marker-only design would
therefore be silently dropped by every rebuild unless rebuild explicitly writes it — C1 has to
solve the identical problem for its cursor rows, inside the Phase-3 lock before the swap, and the
follow-up should ride that mechanism rather than invent a second one.

**Alternatives considered.**

| Alternative | Disposition |
|---|---|
| `SCHEMA_VERSION` bump + `kb_meta` marker (Revision 1) | **Rejected for C3** — fires a full re-embed rebuild on every DB, or never migrates at all under `KB_NO_EMBED`. Becomes materially cheaper if C1's own 2 → 3 bump lands first; the follow-up must re-evaluate against that. |
| Marker-only + explicit `kb migrate-embeddings` | **Deferred to the follow-up** — mechanically sound, but opt-in, and the permanent dual read path is unjustified until measured. |
| Store the norm in a side column, keep cosine | **Still rejected** — saves ~⅓ of the arithmetic and adds a derived column that every write path must keep in sync. |
| Version byte in the blob | **Still rejected** — changes `EMB_BLOB_BYTES` from 768, breaking length dispatch for every existing DB and every f16 constant in `models.rs`. |

**Consequences.** C3 delivers one of the two staged perf items (P1) as an implementation and the
other (P2) as an evidenced decision. This satisfies success criterion 1's "closed or explicitly
deferred with recorded rationale" and is flagged at the PM gate as a deliberate scope call.

## ADR-2 — Total order for ranking

**Decision.** One shared comparator, `(score.total_cmp() descending, id ascending)`, used by
every lane: FTS (in SQL, `ORDER BY rank, e.id`), semantic, cue, RRF fusion, post-recency, and the
MMR argmax. Cue best-per-entry breaks ties on the **cue text**, which requires re-plumbing
`c.cue` (selected at `db.rs:1891`, currently never read) through the row mapper and the `best`
value tuple.

**Drivers.** `total_cmp` is a total order on `f32` including NaN, so `unwrap_or(Equal)`
disappears; the id key removes dependence on SQLite row order and `HashMap` iteration order. A
single shared function makes inter-lane drift impossible to introduce silently.

**Consequences.** Result orderings change where scores tie. This establishes the **post-S1,
pre-P1 baseline** against which P1's no-regression criterion is measured. The FTS parity gate
must move from limit-truncated set comparison to a comparison that is not limit-sensitive; note
that an unlimited debug-build comparison on every search is a materially different cost at 100k
entries, so S1 must choose between accepting that cost and extending the limit past the tie
boundary, and record which.

## ADR-3 — Non-finite embedding policy

**Decision.** Three layers. (1) **Write:** validate the embedder's output is all-finite before
encoding; a non-finite vector is an error, not a silent zero. (2) **Decode:** a decoded blob
containing any non-finite component is corrupt in full — similarity 0.0 — and counted in a
reported corrupt-embedding counter. (3) **Compute:** `cosine_similarity` returns 0.0 unless both
inputs *and the result* are finite.

**Applies at every implementation of the behaviour** (Principle 3): layer 3 must cover
`compress.rs:231`, the private duplicate used by the near-duplicate cutoff at `compress.rs:123`,
where a NaN currently makes `NaN > cutoff` false so a corrupt entry silently never dedups. Route
`compress.rs` through `models::cosine_similarity` rather than extending the duplicate.

**Drivers.** The write guard cannot repair blobs already on disk; the decode guard cannot stop a
bad embedder writing more; the compute guard is the last line for the legacy f32 path. Counting
rather than silently zeroing is what distinguishes corruption from a genuinely orthogonal vector.

## ADR-4 — Federated search contract (revised)

**Decision.** `--limit` means a **global** limit. Accumulate into a map keyed by
`(origin_repo, id)`, with local (`origin_repo = None`, which sorts first under `Option`'s derived
ordering — state this explicitly rather than relying on it) winning an id collision; sort by the
ADR-2 comparator extended to `(score, origin_repo, id)`; truncate **once** to `limit`.

**Explicitly stated, because Revision 1 got this wrong:** the merged scores are **not
comparable across repositories**. In the default hybrid path `entry.score` is an RRF score
(`db.rs:1956–1975`), a pure function of *within-repo rank*: rank 1 in a three-entry peer scores
exactly as high as rank 1 in a 30,000-entry local corpus. Revision 1 claimed that forcing the
lambdas to zero for local as well as peers would make scores comparable and "make the asymmetry
symmetric instead of hidden". It does not — it equalizes *parameters*, not *scores*, and the
dominant asymmetry is corpus size, which no lambda touches. Cross-repo ordering under this
decision is **rank-position round-robin with a deterministic tie-break**, not relevance ordering.

**Given that, the lambda question is separable and is decided as follows:** leave the existing
local recency/MMR behaviour alone. Revision 1 proposed zeroing it for local too, justified
entirely by a comparability claim that is false; zeroing it would be a silent fidelity
regression for federated local results bought for nothing. Federation therefore keeps local
results scored as they are today, and the docs task states plainly that cross-repo ordering is
by rank position.

**Drivers.** `--limit 10` with ten peers can print 110 rows and duplicate an id across two peers.
A global limit and a real dedup key are both unambiguous improvements and are what the finding
asks for; the ranking honesty is a documentation obligation, not a solvable ranking problem
within this component.

**Alternatives considered.** (i) Rename `limit` to per-repo and document it — keeps the 110-row
output and the duplicate ids. (ii) Two-tier: local first as today, peers ranked among themselves,
one truncation — arguably *more* honest about what RRF supports across corpora, and worth
revisiting if the round-robin behaviour proves bad in practice; rejected here because it makes
`--limit` mean two different things depending on where a result came from.

**Accepted debt, named:** the global limit is presentation-only. `search.rs:108` sets
`inline_verify_k = limit` and verification runs inside `search_entries` per peer, so
`--limit 10 --peers` against ten peers performs eleven verification passes over 110 rows and then
discards 100 *after* verifying them. Deferring verification until after merge-and-truncate is a
real improvement and is **not** in scope here; it is recorded as a follow-up.

## ADR-5 — Symlink policy parity — **REQUIRES USER SIGN-OFF**

Linux `openat2` (`RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`) accepts a contained symlink; the
fallback (`O_NOFOLLOW` per component) rejects every symlink. The same stored citation is
`Verified` on a modern Linux kernel and rejected on an older one or another Unix.

**Option A — add `RESOLVE_NO_SYMLINKS` on Linux (recommended).** Parity by making Linux strict.
One flag. Fails closed.

**Option B — bounded descriptor-relative symlink resolver in the fallback.** Parity by making the
fallback permissive-but-contained. Reimplements in userspace what `openat2` does in the kernel
(hop limits, loop detection, per-hop containment) — new security surface on the exact path whose
job is containment.

**Recommendation: Option A.** Note that Revision 1's supporting argument was wrong and is
withdrawn: it claimed the relocation repo walk's unconditional symlink rejection made Option A an
internal-consistency fix. There are **three** policies, not two (see Context). The verify path and
the *in-file relocation* path currently agree on Linux — both permit contained symlinks, the
latter via `safe_join`'s canonicalize — and it is the repo-walk half that is the outlier. Option
A therefore *creates* a new divergence unless V4's scope extends to `safe_join` / the
`search_for_excerpt` cited-file branch. **That extension is now part of V4.** Option A remains the
recommendation because failing closed on a containment path is the right default and Option B
puts new hand-rolled resolution code on the containment path itself — but the recommendation now
rests on that reason alone, not on a consistency argument that does not hold.

**Guardrails, converting a silent regression into a measured one:** (1) a distinct
`SymlinkPathRejected` reason, never the generic `FileMissing`; (2) a one-shot corpus audit
counting affected rows, **owned by task A0, not V4**, so the sign-off is not blocked on the task
it gates; (3) an assertion that auto-heal never fires on the new reason.

**Honest cost statement.** Any existing citation whose path traverses an in-repo symlink flips
from `Verified` to rejected. A0's count is the size of that regression. Additionally, because
`open_citation_file_fallback` is reached on Linux only when `openat2` returns `NOSYS`
(`verification.rs:383`), and the existence-oracle warning is compiled out on Linux entirely
(`verification.rs:485`), **any parity or oracle assertion needs an injection seam** — a
`#[cfg]`-conditional test passes trivially on this repo's platform while exercising nothing. V4
must build that seam.

**Flagged for explicit user sign-off at the PM gate. A0's audit count is the input the user
needs.**

---

## Guardrails

**Must have**
- Every *implementation* task carries a dependency edge to the spec task `T0` (repo TLA+ gate),
  **except S3a, S3b and S4 under a scoped lead-granted waiver** — see
  `.state/agent-kb/tla/decisions/c3-search-tasks-spec-waiver.md`. A0 is a `chore` and is exempt.
- TDD: the failing test is written and committed before the implementation.
- Property-based tests where the property is the point: shuffle-invariance of ranking, relocation
  uniqueness under a generated tree, federated dedup under peer-order permutation. `proptest` is
  the repo default.
- Any acceptance criterion targeting `#[cfg]`-gated code names the seam that makes it meaningful
  on the CI platform.
- Perf measurement uses the `verify_matrix` methodology precedent from `bd-tx0` (explicit cell
  matrix, in-process via `db::search_entries`, real fixtures via `kb::bench_fixture::seed_db`).
- Cargo test evidence uses `tee` + `PIPESTATUS` (KB `procedures/verification-harness`).

**Must NOT have**
- No ANN index (deferred by the component description; the `TODO` at `db.rs:1847` parks it).
- No `SCHEMA_VERSION` bump in this component.
- No re-embedding as a migration path.
- No *silent* cap-clamping inside `search_entries` — the `inline_verify_k` fidelity loss is a
  user-visible regression and must be an explicit decision, not a side effect (S5).
- No changes to `acquire_lock`'s call shape — that surface belongs to C2.
- No perf task may gate a correctness task's closure.

---

## Cross-component ordering

**C2 (`bd-21ef.2`) overlaps C3 in two places, not one.** Revision 1 asserted "everything else in
C3 is independent of C1 and C2", which was false.

1. **The flock surface.** V3 moves path resolution inside the write flock in `kb_core::add` and
   tightens lock-scoped requery in `stale_check::heal_relocations` and
   `migrate_citations::apply_heals` — all three call `add::acquire_lock`, the surface C2 is
   reworking. **Resolved: `bd-21ef.3.5` (V3) now depends on `bd-21ef.2.3` (C2/`L1a`)**, C2's
   FOUNDATIONAL task, which lands on the aggregator first and leaves `open_db` as a
   `#[deprecated]` wrapper so V3 rebases incrementally rather than waiting for all of C2.
   **V3 builds on `add_locked` rather than reordering `add`'s own acquire** — see the task. The
   reverse edge is also wired: C2's `L1c` (`bd-21ef.2.13`, "delete the deprecated `open_db` after
   C1 and C3 rebase") now depends on V3, so `open_db` cannot be deleted out from under a
   half-rebased C3.

3. **The federation hunk.** C2's `L1b` (`bd-21ef.2.4`) inserts the peer-TTL `expires_at` filter at
   `search.rs:127–160`; S4 rewrites that same loop. Same hunk, not an adjacent boundary — neither
   plan had flagged it as a textual collision. **S4 now depends on `bd-21ef.2.4`** and rebases onto
   the filtered loop. Mechanism division adopted verbatim from C2: C2 owns the filter, C3 owns
   ranking, and S4 adds no expiry notion of its own.
2. **The cap surface — resolved by lead ruling, and C3 owns it.** `br show bd-21ef.2` assigns
   lens3 #4 ("caps into `search_entries` boundary") to C2, which is why Revision 2 removed it. C2's
   planner argued the opposite, and **the lead ruled for C2's reading** (open-questions.md, C2-Q4:
   "reassignment to C3 — CONFIRMED"). C3 accepts the ruling; the work is restored as **S5**
   (`bd-21ef.3.17`), a task of its own rather than folded back into S2, since S2 is now scoped and
   ordered around it.

   One caveat the lead should know: **the ruling was made against Revision 1's plan text**, whose
   argument was "C3's `S2` already claims the identical edits in the same hunk". Revision 2 had
   already removed those edits, so that premise no longer held when the ruling landed. The outcome
   is still right — the ruling is now the only thing keeping this finding assigned to anyone, and
   without it both plans disclaimed it, which is the one arrangement guaranteed to drop it. Acting
   on it rather than re-litigating.

**C1 (`bd-21ef.1`) — one flag answered, one fact absorbed.** `c1-log-durability.md:508-510`
records a "known scope collision", flagged to C3's planner: C1's T2 edits `read_events` /
`read_events_up_to` / `read_events_from_offset`, and C3 "claims decode-error propagation at the
3 read sites in the same functions". **This is a false positive.** C1's functions live in
`src/components/events.rs:234`, `:242`, `:266`; C3's three sites are `db.rs:1354`, `:1863`,
`:1911`, in the search lanes, and the epic lists the finding under SEARCH. No overlap. Recorded
here rather than left unanswered, because a bilateral flag nobody replies to is what produces
duplicated or dropped work at aggregation.

Separately, C1's applied-cursor migration bumps `SCHEMA_VERSION` 2 → 3
(`c1-log-durability.md:231`), which materially changes the cost basis of the pre-normalization
follow-up — see ADR-1 and P2. C3 creates no dependency on it.

---

## Task flow

```
A0 audit (chore, no deps) ─────────────► [user sign-off ADR-5] ──► V4b
S3a provenance ────────────────────────► (T0 edge waived, ready at start)
S3b context budget ────────────────────► (T0 edge waived, ready at start)
S4 federation ──► after C2/L1b only ───► (T0 edge waived, same hunk as L1b)
T0 spec ──┬─► V1 relocation soundness
          ├─► V2 emission boundary
          ├─► V3 write-path resolution        (after C2 lock contract)
          ├─► V4a existence oracle            (NOT gated on sign-off)
          ├─► V4b symlink parity              (after A0 + sign-off)
          ├─► S2 filter & decode correctness ─► S1 determinism ─┐
          │                                                     ├─► P1 ─► P2
          └─► S5 caps at the search_entries boundary ───────────┘
                                     all ──► post-impl ──► docs
```

Ordering rationale, each edge justified by a diff-surface or semantic fact:

- **S2 → S1**: both rewrite the *same two SQL string literals* (`db.rs:1407–1417` and
  `:1447–1457`) two lines apart. S2 adds `ESCAPE '\'`, S1 adds `e.id` to `ORDER BY`. Presenting
  them as parallel siblings guarantees a conflict a bad merge can resolve by silently dropping
  either change, with neither task's test catching it (S1's shuffle test does not exercise
  `path_prefix`; S2's prefix test does not exercise ties). S2 is the smaller diff, so it goes
  first.
- **S1 → P1**: P1's heap comparator *is* the total order; building it before the order is decided
  means rewriting it.
- **S2 → P1**: S2 edits `db.rs:1843`/`:1896`, and `:1896` is inside the block P1 rewrites.
- **S5 → P1**: S5's clamped `limit` / `inline_verify_k` feed the pool and truncation calculations
  P1 relies on. S5 is otherwise independent of S1 and S2 — different regions of `db.rs`.
- **P1 → P2**: P2 measures the read sites P1 restructures.
- **A0 → V4**: breaks Revision 1's cycle, where the audit that gates sign-off lived inside the
  task the sign-off gated.
- **P1 owns `db.rs:1863`/`:1911`**: Revision 1 assigned these `filter_map(|r| r.ok())` sites to
  S3, and P1 to rewriting the same loops — P1 would have silently reverted S3's fix. The error
  contract now lives in the task that owns the loop.

## Tasks

Beads ids under `bd-21ef.3`:

| Task | Bead | Task | Bead |
|---|---|---|---|
| A0 audit | `bd-21ef.3.1` | S1 determinism | `bd-21ef.3.7` |
| T0 spec | `bd-21ef.3.2` | S2 filter/decode | `bd-21ef.3.8` |
| V1 relocation | `bd-21ef.3.3` | S3a provenance | `bd-21ef.3.9` |
| V2 emission | `bd-21ef.3.4` | S3b context budget | `bd-21ef.3.10` |
| V3 write-path | `bd-21ef.3.5` | S4 federation | `bd-21ef.3.11` |
| V4a existence oracle | `bd-21ef.3.16` | S5 caps boundary | `bd-21ef.3.17` |
| V4b symlink parity | `bd-21ef.3.6` | P1 materialization | `bd-21ef.3.12` |
| post-impl | `bd-21ef.3.14` | P2 pre-norm decision | `bd-21ef.3.13` |
| docs | `bd-21ef.3.15` | | |

`br dep cycles` clean. Only `T0` and `A0` are ready at start.

Deferrals carry beads so they are not lost as prose: `bd-prenorm-embeddings-followup-te13`
(ADR-1's persisted-format change) and `bd-federated-verify-after-truncate-ayb8` (ADR-4's accepted
debt).

### A0 — ADR-5 corpus symlink audit (`chore`, no dependencies)

Count, over the current live corpus, the citations whose `citation_path` traverses an in-repo
symlink and would flip from `Verified` to rejected under ADR-5 Option A. Report the count and a
sample of affected entry paths.

*Acceptance:* count recorded in this plan file and reported to the user. Throwaway script; no
production code. Exempt from the T0 edge (not an implementation task).

### T0 — spec: `CitationRelocation.tla` amendment (`task`)

Three parts.

**(a) Add a `path` field to `EvidenceRow` and split `Heal`.** The current `EvidenceRow` is
`[status, storedHash, contentHash, candidates, excerptStrong]` — **there is no path**, and `Heal`
models relocation by changing `contentHash`. Modelling the lens2 #8 race requires the variable
first. This is an add-a-variable change (`TypeOK`, `Init`, `ReVerify`'s full-record `EXCEPT`, and
`StoredHashImmutable`'s witness all need updating); the repo's `tlaplus-add-variable` skill
applies. Then split `Heal` into `PlanHeal` (records a plan against a row snapshot) and
`ApplyHeal` (commits only if the row's current path and liveness still match the plan's premise;
otherwise discards). Because the spec has no process model, this split is precisely what lets TLC
explore `PlanHeal(r) ; ReVerify(r) ; ApplyHeal(r)` — the A→B/A→C race. Add an invariant that no
`ApplyHeal` commits over a row whose path changed since its plan was recorded.

**(b) Cover the write path, or record why not.** V2 (`cite.rs:77` hash → `:87` self-check → emit)
and V3 (`kb_core.rs:154–193` resolve → `:238` flock → append) are the *same* plan-then-commit
shape, and V3's risk is that a hash resolved outside the lock is appended as `storedHash` — the
spec's write-once, `StoredHashImmutable`-protected value. Either add an
`Acquire`/`ResolveHash`/`Append` split with an invariant that an appended `storedHash`
corresponds to content observed under the lock, or record explicitly that the write-side TOCTOU
is out of spec scope and justify why V2/V3 still carry the required edge.

**(c) Refinement note and determinism decision.** Record that lens2 #1 and #2 are
**refinement-mapping** failures, not spec failures: `NonUniqueUnverified` holds in the model, but
`search_for_excerpt` returns `Unique` (`verification.rs:662`) on a count computed over one file.
Pin the *unit* of `candidates` so the mapping is checkable: match locations repo-wide, including
the cited file, counted with overlap after V1's `count_occurrences` change. Separately, decide and
record whether ADR-2's total order warrants its own minimal TLA module or a documented no-change.

*Acceptance:* `.tla` under `.state/agent-kb/tla/`; TLC green on every modified spec; the
`PlanHeal ; ReVerify ; ApplyHeal` trace is reachable and the new invariant catches it; (b)'s
decision written down either way; refinement note pins the `candidates` unit; determinism
decision recorded; code-reviewer and analyst audit pass.

### V1 — relocation soundness: repo-wide uniqueness and overlapping occurrences (`task`)

`search_for_excerpt` continues the repo walk after an in-file hit and returns `NonUnique` if any
further match exists. `count_occurrences` advances by one byte after a match.

**The cited-file exclusion must be by `(st_dev, st_ino)` identity, not by path string.** If the
cited path is a symlink or hard link, `safe_join` scans its *target* while the walk scans that
same real file again, so a string-keyed exclusion yields `total == 2` and a false `NonUnique`.
This is the same file-identity mechanism V3 needs for `migrate_citations`; share it.

**Budget exhaustion after an in-file hit must be decided explicitly.** `scan_file` returns
`CapExceeded` when a single file exceeds the *remaining* budget (`verification.rs:786–788`), and
both callers propagate it as a whole-search abort (`:657`, `:720`), discarding candidates already
found. Today the in-file short-circuit means this rarely fires; V1 makes the walk unconditional
with the budget already partly spent, and `entries.sort()` (`:693`) makes the failure
deterministic per repo. The decision must distinguish *cap hit before any candidate* from *cap
hit with one candidate already found* — the latter must report cap-exceeded, never degrade to the
in-file candidate, which is exactly the false `Unique` this task exists to remove.

*Acceptance:* a failing test first for each half — (a) the same strong excerpt in the cited file
and one other file yields `NonUnique`, not `Relocated`; (b) a periodic ≥64-byte multiline excerpt
with two overlapping copies yields count 2. A property test over a generated tree with *k* planted
copies asserts `Unique` iff *k* = 1, including a case where the cited path is a hard link to
another scanned file. The budget decision is recorded in this plan file with both branches named.
Relocation cost before/after is measured on a realistic tree and **reported at post-impl** — per
Principle 1, this is a reporting obligation, not a closure gate.

### V2 — emission-boundary integrity: cite TOCTOU and front-end parsing (`task`)

Hash, self-check and emit from **one retained descriptor** in `cite.rs` and the MCP cite handler,
with a pathname-to-descriptor identity check immediately before emission.

**State the contract, because this narrows the window rather than closing it.** The retained
descriptor buys *snapshot consistency*: the emitted hash describes bytes the process actually
read. The artifact stored is the pair `(citation_path, citation_hash)`, and no check-before-emit
can make that pair atomic against a rename after the check. C3 chooses snapshot consistency; the
residual window (identity-check → emission) is unclosable by construction and is recorded here as
accepted.

Reject `start == end` at both front ends (`cite.rs:147`, `mcp.rs:581`) with a "start must be less
than end" message.

`usize::try_from` on wire offsets (`mcp.rs:560`, `:569`): **this is a 32-bit-only finding.** On a
64-bit host `usize::MAX == u64::MAX`, so `usize::try_from(u64)` cannot fail and there is no value
to test. Either gate the test `#[cfg(target_pointer_width = "32")]` or, better, add the behaviour
that is meaningful on the target platform: bound `end` against file size before use.

**Named blast radius:** self-checking from a retained descriptor requires a descriptor-taking
variant of `verify_evidence`, which is also called from `db.rs`'s search path and
`stale_check.rs`. The API change is part of this task; the existing pathname-based callers keep
working.

*Acceptance:* an integration test asserts the two-open window is gone by construction (one
descriptor, one read). A **separate** test exercises the identity check itself: replace the
pathname binding (rename/replace) after self-check and assert the check fires — Revision 1's
single test exercised only the window the descriptor closes structurally, leaving the actual
residual mitigation untested. `kb cite f.rs:4-4` and the MCP equivalent are rejected at parse.

### V3 — write-path resolution under the lock (`task`) — *after C2's `L1a` (`bd-21ef.2.3`)*

**Build on C2's `add_locked`, not on today's self-acquiring `add`.** C2's `L1a` (`bd-21ef.2.3`)
splits the function into `kb_core::add_locked(&Lock, &Connection, ..)` carrying the real logic,
with `kb_core::add(..)` as a thin acquiring wrapper. V3 therefore does **not** reorder `add`'s own
`acquire_lock` call — that surface belongs to C2. Instead, path-only hash resolution and the
pre-append re-verify move **inside `add_locked`**, where the lock is held by construction and the
`&Lock` token proves it at the type level. Today's bug (resolution at `kb_core.rs:154–193`, flock
at `:238`) then disappears structurally rather than by careful ordering — which is the whole point
of C2's split, and is why this task waits on `L1a` rather than racing it.

Re-verify every newly resolved citation immediately before event construction; memoize on
`(st_dev, st_ino)` file identity, not the path string. `stale_check::heal_relocations`: under the flock, requery the
complete evidence row and live-parent state, require the current path to equal the planned old
path, re-run relocation, and emit only if it still yields the same destination.
`migrate_citations`: compare `(st_dev, st_ino)` against the configured event log under the flock,
rejecting hard-link and symlink aliases before hashing, and recheck before append.

**Added at the PM gate — two silent-default sites in a file this task already opens.**
`stale_check.rs:353` is an uncovered evidence-read `filter_map(|r| r.ok())`, and two lines above it
`r.get::<_, String>(3).unwrap_or_else(|_| rel_path.clone())` substitutes a **default for an
undecodable citation-path column, on the heal path** — a third instance of the shape Principle 2
forbids, and the one with the worst consequence, since the substituted value then feeds relocation.
Both are in V3's scope.

*Acceptance:* a concurrent test with two writers contending the flock shows the second heal
discarded rather than overwriting the first (the A→B / A→C race). A hard link to the events JSONL
is rejected as self-referential. A file mutated between resolution and append is caught by the
pre-append re-verify. An undecodable citation-path column produces an error, never a substituted
path. Conformant with T0's `PlanHeal`/`ApplyHeal`.

**Cross-epic edge wired.** V3 depends on **`bd-21ef.2.3` (C2/`L1a`: db open split,
`kb_core::add_locked`, `acquire_lock` re-entrancy registry)**, which C2 marks FOUNDATIONAL and
lands on the aggregator first. `L1a` leaves `open_db` as a `#[deprecated]` wrapper, so V3 rebases
incrementally rather than waiting for all of C2. Cycles clean. C2 was undecomposed when this
plan's PM gate ran, which is why the plan text above still frames it as a recommendation; the
beads edge now enforces it.

### V4a — existence oracle in the capability warning (`task`, not gated on sign-off)

lens2 #5. Emit the platform capability warning unconditionally at verifier initialization rather
than from request-dependent execution. Today it fires only *after* a successful open and
`is_file()` check (`verification.rs:486`, `:505`), so in a fresh process the presence of that line
on stderr distinguishes an existing regular file from a missing path.

**Build the test seam.** The warning is declared and emitted under
`#[cfg(not(target_os = "linux"))]` (`verification.rs:485`, `:503–508`), so it does not exist on
this repo's platform and a `#[cfg]`-conditional test passes trivially while exercising nothing.
Extract the emission behind a testable seam (trait or runtime flag) so it is exercised on the CI
platform.

*Split from V4 at the PM gate:* this finding needs no decision from anyone. Bundling it with the
sign-off-gated parity work meant a "no decision yet" answer would have deferred a fix that was
never in question.

*Acceptance:* two fresh processes verifying an existing path and a missing path produce identical
stderr, asserted **through the seam** so the assertion is meaningful on the CI platform. The
warning is emitted exactly once, at init, never request-dependently.

### V4b — symlink policy parity (`task`) — *after A0 + user sign-off*

lens2 #6. Implement the signed-off ADR-5 option **at all three sites**: `open_citation_file`, the
repo walk, and the `safe_join` / cited-file relocation branch. Option A applied only to
`open_citation_file` leaves verify strict while the in-file relocation scan still follows symlinks
through `safe_join`'s canonicalize. Note `safe_join`'s third caller at `mcp.rs:464`.

**Test seam required here too:** `open_citation_file_fallback` is reached on Linux only when
`openat2` returns `NOSYS` (`verification.rs:383`), so resolver selection must go behind a seam or
the parity assertion exercises one branch.

*Acceptance:* A0's audit count is on the record before this lands. Both resolver branches are
exercised on the CI platform through the seam and agree. Under Option A: a citation through an
in-repo symlink yields `SymlinkPathRejected` (never generic `FileMissing`), auto-heal is asserted
not to fire on it, and the relocation scan agrees with the verify path. **Fallback if sign-off
does not arrive:** V4b alone closes as deferred with recorded rationale (permitted by success
criterion 1) and the divergence is carried as known debt; V4a is unaffected and the epic is not
blocked.

### S1 — ranking determinism (`task`) — *after S2*

Implement ADR-2 and ADR-3. The shared comparator at `db.rs:1730`, `:1882`, `:1926`, `:1939`,
`:2043`, `:2099`; `ORDER BY rank, e.id` in both FTS lanes; `c.cue` re-plumbed through the cue row
mapper and `best` tuple for the tie-break; the parity gate's limit-sensitivity resolved and the
choice recorded (see ADR-2 consequences). Route `compress.rs:123` through
`models::cosine_similarity` and delete the duplicate at `compress.rs:231`.

*Acceptance:* a property test shuffles input row order and asserts byte-identical output ordering.
A second injects a NaN-bearing blob and asserts a deterministic, non-promoted result plus a
non-zero corrupt-embedding count — **including through `compress.rs`'s near-duplicate cutoff**,
where a NaN currently makes `NaN > cutoff` false so the corrupt entry silently never dedups. A
test asserts the parity gate no longer fires on a tie crossing the limit boundary. No
`unwrap_or(Ordering::Equal)` remains in a ranking path. The resulting ordering is captured as the
**post-S1 baseline** for P1.

### S2 — read-path filter and decode correctness (`task`)

Escape `\`, `%` and `_` in `path_prefix` and use `LIKE (? || '%') ESCAPE '\'` at all four sites
(`db.rs:1413`, `:1453`, `:1843`, `:1896`), or switch to a literal prefix comparison. Propagate
row-decode errors at `db.rs:1354` (the evidence-fetch lane) instead of `filter_map(|r| r.ok())`.
Correct the false comment claiming an external `MAX_INLINE_VERIFY_K` bound — it is at
**`db.rs:2293`**, not `:2194`.

*Acceptance:* `--path-prefix 'src/_'` matches only the literal prefix; `--path-prefix 'src/%'`
returns nothing rather than everything. A corrupt evidence row surfaces as an error, never as
absence — and the criterion is *propagate*, not "or count it if you decide best-effort is
intentional", which Revision 1 left as an implementer's choice that could have satisfied the
criterion while shipping the finding. The `db.rs:1863`/`:1911` sites are **not** in this task;
P1 owns them.

*Note:* `--path-prefix 'src/%'` changing from "everything" to "nothing" is itself a user-visible
behaviour change; it goes in the docs task.

### S3a — provenance dangling references (`task`) — *T0 edge waived*

`mcp.rs:1777`, `:1799`: return `entry_not_found` for a missing start node; report missing parents
in a separate `dangling` bucket rather than as `roots`; preserve the existing intentional
traversal through stale-but-existing parents; add `ORDER BY derived_from` for stable
serialization.

*Acceptance:* a dangling parent appears under `dangling`, not `roots`; a missing start node
returns `entry_not_found`; a diamond and a cycle are still distinguished (existing behaviour is
correct — do not regress it).

### S3b — context token budget (`task`) — *T0 edge waived*

`context.rs:122`, `:226`, `:317`: build the exact representation for the selected output mode
before packing, estimate that serialized byte sequence, and label the figure an approximation
rather than "tokens emitted".

*Acceptance:* JSON-mode context output is asserted to fit the reported budget for a case that
exceeds it today.

### S4 — federated search contract (`task`) — *after C2's `L1b` (`bd-21ef.2.4`); T0 edge waived*

Implement ADR-4: global limit, `(origin_repo, id)` dedup with local winning collisions, one
truncation, local recency/MMR unchanged.

**Expired peers under federation — the contract, stated explicitly, adopting C2's mechanism.**

*Are expired peers excluded at federation time?* **Yes.** *By which mechanism?* C2's `L1b`
(`bd-21ef.2.4`) read-time filter — `AND (expires_at IS NULL OR expires_at >= datetime('now'))`
applied to peer reads, paired with a locked physical `sweep_expired_peers` that no longer runs
from any open path. C2's `L1b` acceptance already carries a test asserting the filter reaches
federated peer search at `search.rs:127–160`. The division is **C2 owns the filter, C3 owns
ranking**, and S4 adopts that verbatim: it **must not** add a second expiry check, a second
`expires_at` predicate, or a TTL notion of its own. If S4's author finds themselves writing
`expires_at` anywhere, the boundary has been crossed — raise it instead.

Because the filter lands at peer-selection time it is cleanly upstream of S4's merge, so an
expired peer contributes no rows at all and cannot consume a truncation slot. S4's obligation is
only to not defeat that: no caching of peer paths across the filter, and no re-adding a peer from
a stale list.

**Textual collision — this is the part neither plan had flagged.** C2's `L1b` edits
`search.rs:127–160` to insert the filter; S4 rewrites that same loop for global limit, dedup and
truncation. It is not merely a semantic boundary, it is the same hunk. **S4 therefore lands after
`bd-21ef.2.4`** (edge wired), and rebases onto the filtered loop rather than merging against it.
Note the second-order consequence of C2's read-time-filter design: a peer can now be *logically
expired but physically present*, which is a new observable state C2's spec waiver has to cover —
S4 inherits it and must not assume peer rows in the table are live.

*Acceptance:* `kb search --limit 10 --peers` against ≥2 peers returns exactly 10 results; an id
present in two peers appears once; an id present locally and in a peer resolves to the local row,
**asserted explicitly** rather than relying on `Option`'s derived ordering. Ordering is stable
under peer traversal-order permutation. With an expired peer present, `--limit N` still returns N
live results. A test documents the rank-position (not relevance) character of cross-repo ordering
by asserting that a top hit from a tiny peer outranks a mid-ranked local hit — this is the
accepted behaviour, and pinning it prevents a later reader mistaking it for a bug.

### S5 — resource caps at the `search_entries` boundary (`task`) — *lead-assigned*

lens3 #4. Restored to C3 by lead ruling (see §Cross-component ordering). MCP clamps `limit` to 100
and `inline_verify_k` to 20 (`mcp.rs:284`); `search_entries` (`db.rs:2048`) does not, and the CLI
sets `inline_verify_k = limit` (`search.rs:108`) with an unrestricted `usize`. Verification may
schedule `limit × 200` filesystem tasks, and `verify_pool_size` is only lower-bounded
(`db.rs:2201–2204`), so a large configured value spawns that many OS threads.

Clamp `limit`, `inline_verify_k` and `verify_pool_size` inside `search_entries`, and use the
clamped values for every downstream pool and truncation calculation. Add an explicit CLI
validation range. The MCP clamps become redundant rather than load-bearing.

**Two things make the naive fix wrong, and the lead has fixed the posture while deferring the
decision itself to implementation time.**

*Posture, binding, decided before any measurement:* **no silent user-visible CLI regression** —
whatever `kb search --limit 50` ends up doing about verification is an explicit documented choice,
never a side effect of a clamp landing; and **both ceilings must be named constants** — no magic
numbers, and no clamping against a bound that exists only in a comment.

*The two problems.* (1) `search.rs:108` deliberately sets `inline_verify_k = limit` ("verify all
results by default"), so a naive clamp takes `--limit 50` from 50 verified rows to 20 with the
rest `verified=null`. (2) `verify_pool_size` has **no ceiling constant anywhere in the tree**
(`db.rs:2201–2204` is a floor), so "clamp it" is unmeasurable until one is named.

*What the implementer owes the lead before deciding:* a decision packet with (a) the **measured**
worst-case fan-out, `inline_verify_k × MAX_EVIDENCE_ROWS_PER_ENTRY` — measured, not estimated,
since it determines whether the cap protects against anything real; (b) the **pre-existing CLI/MCP
asymmetry** stated plainly: MCP *already* clamps `inline_verify_k` to 20 (`mcp.rs:284`) while the
CLI sets it to `limit`, so the two front ends already disagree today and this predates S5. The
question is therefore not "should we introduce a regression" but "which of two existing behaviours
becomes the contract" — present it that way; (c) the proposed `verify_pool_size` ceiling and its
reasoning. **The lead rules on the packet.** Do not resolve it unilaterally and do not land the
clamp before the ruling.

**Do not add IN-query batching as dead code.** Once `limit` is bounded, the recency `IN` query
(`db.rs:2062–2066`) can never exceed 200 parameters. Batch only where an unbounded fan-out
actually survives clamping; if none does, record that and skip it.

*Ordering:* independent of S1 and S2 (different regions of `db.rs`), but the clamped values feed
the pool and truncation calculations P1 relies on, so S5 lands before P1.

*Acceptance:* a direct `search_entries` call with an absurd `limit` and `inline_verify_k` is
clamped, evidenced by **the number of scheduled verification tasks and spawned threads**, not just
the result count. A named `verify_pool_size` ceiling constant exists and is enforced. The CLI
rejects out-of-range values explicitly. The `inline_verify_k` fidelity decision is recorded and,
if the regression is accepted, appears in the docs task.

### P1 — bounded materialization for semantic and cue scans (`task`) — *after S1, S2, S5*

**Revision 1's design was unachievable and is replaced.** It called for a bounded top-K heap over
the semantic lane plus a "results byte-identical to pre-change ordering" criterion. Those are
mutually exclusive: `db.rs:1964` enumerates the **entire** candidate vector to compute each
entry's semantic RRF rank contribution, and truncation to `limit * 2` happens only afterwards at
`:1988`. Any heap with K < N discards tail ranks, changing fused scores, membership and order for
every FTS- or cue-matched entry outside the heap. An implementer could only satisfy the criterion
by setting K = N — zero win — or by shipping changed results.

**What this task actually does.** Bound the *materialization*, not the rank vector. The semantic
lane currently collects `Vec<(f32, String, String, String, String, String, String)>` — score, id,
path, summary, content, tags, updated_at — for every matching row (`db.rs:1852`, `:1868`). That
duplicated string and blob traffic is where lens3 #9's memory complaint actually lives. Stream
rows from `query_map` retaining only `(score, id)`, sort that, then batch-fetch metadata for the
`limit * 2` winners. Ranks are preserved **exactly**, so byte-identity against the post-S1
baseline is achievable and is the right criterion.

Apply the same shape to the cue lane, which additionally clones full entry metadata once per cue
before consolidating (`db.rs:1911`, `:1932`): query `(entry_id, cue, embedding)`, maintain a
`(score, cue)` best-per-entry map, truncate to `limit * 2` as today (`db.rs:1941`), then batch-
fetch metadata. Cue ranks beyond `limit * 2` already do not contribute, so a bounded structure is
safe here in a way it is not in the semantic lane.

**This task owns the decode-error contract for `db.rs:1863` and `:1911`** — the two
`filter_map(|r| r.ok())` sites inside the loops it rewrites. Errors propagate, or are counted and
reported in result metadata.

*Acceptance:* results are byte-identical to the **post-S1, pre-P1 baseline** on a fixture corpus
in both hybrid and semantic-only modes — this is now achievable because ranks are preserved. Peak
ranking memory drops from O(N × full row) to O(N × (f32 + id)) plus O(K × row), evidenced by a
before/after `verify_matrix`-style matrix at the `bd-3mr` corpus sizes. lens3 #9 and lens3 #10 are
separately evidenced: the matrix has a cell for the semantic lane and a cell for the cue lane, so
"the cue lane was addressed" is not inferred from an aggregate.

### P2 — pre-normalization: measure and decide (`task`) — *after S1, P1*

Per ADR-1, this task does **not** change the persisted format.

Land unconditionally: ADR-3's finiteness guards at all three layers (write, decode, compute), and
the `compress.rs` duplicate-cosine consolidation if S1 did not already carry it.

Then measure and decide: isolate the cost of recomputing `norm_b` per stored vector at the
`bd-3mr` baseline corpus sizes, at **all three** similarity read sites — semantic
(`db.rs:1875`/`:1877`), cue (`:1921`/`:1923`), and **MMR (`db.rs:1724`)**, the last being the
densest similarity loop in the file and the one P1 does not touch. State the threshold that would
justify a persisted-format change **before** measuring. Record the verdict.

**State the threshold against the marginal cost, not a standalone one.** C1's applied-cursor
migration bumps `SCHEMA_VERSION` 2 → 3 (`c1-log-durability.md:231`). If C1 lands on the aggregator
first, the full backup-and-replay-with-re-embed that ADR-1 treats as prohibitive is already being
paid, and pre-normalization riding that rebuild is close to free — no marker, no opt-in gap, no
permanent dual read path. Check C1's status when writing the threshold and say which basis it
assumes.

*Acceptance:* the finiteness guards are covered by S1's NaN property test and a write-path
rejection test. The threshold is recorded before the numbers are. The measurement matrix follows
the `bd-tx0` methodology and reports per-site cost. A written verdict — land, or defer with
rationale — is recorded in this plan file, and if "land", the follow-up epic's open decisions are
the list already enumerated in ADR-1 so the follow-up does not re-derive them. Closing this task
as a deferral is a valid outcome under success criterion 1.

### post-impl task (`task`)

Blocks the docs task. Closed by `/post-impl`. Collects V1's relocation-cost measurement as a
reporting obligation.

**Two lines that must not be skipped:**
- **An explicit `security-reviewer` pass on V4b.** ADR-5 changes a path-containment control, but
  `verification.rs` does not match post-impl's auto-trigger pattern
  (`secrets/|auth/|oidc|policies/|pki/|apparmor`), so the gate will not fire on pattern match. If
  V4b deferred, record that the control was not changed — do not silently drop the line.
- **The TLA+ waiver's two sign-off checkboxes** from
  `.state/agent-kb/tla/decisions/c3-search-tasks-spec-waiver.md`: code-reviewer confirms S3a/S3b/S4
  introduced no state-machine logic, event write or lock acquisition; analyst confirms the amended
  `CitationRelocation.tla` still covers every modified path.

### Docs task (`docs`)

Document the user-visible changes: the ADR-4 federation contract, the global `--limit`, and the
rank-position (not relevance) character of cross-repo ordering; the ADR-5 symlink policy and its
new reason code, or its deferral; `--path-prefix` metacharacters no longer acting as wildcards;
the new corrupt-embedding and dropped-row reporting; the corrected `inline_verify_k` bound
comment; and the deferred-verification and pre-normalization follow-ups.

---

## Pre-mortem (DELIBERATE mode)

Revision 1's three scenarios are retained, corrected where the review pass changed the mechanism,
and three added — the review noted that Revision 1's pre-mortem missed the three highest-
probability failures, all mechanically derivable from its own file references.

**1 — the symlink flip quietly invalidates the live corpus.** Option A lands, existing citations
flip to rejected, nobody notices until a `stale_check` run reports mass failures and auto-heal
relocates those rows to guessed paths. *Mitigations:* the `SymlinkPathRejected` reason, A0's
pre-landing count, and the assertion that auto-heal never fires on that reason.

**2 — the uniqueness fix blows the scan budget.** V1 makes the walk unconditional with the budget
already partly spent; `CapExceeded` aborts the whole search on a single oversized file, and
deterministic traversal makes the failure reproducible per repo. *Mitigations:* V1's required
explicit decision distinguishing "cap hit before any candidate" from "cap hit with one candidate
found", with the latter reporting cap-exceeded rather than degrading to the in-file candidate.

**3 — a corrupt vector is blessed rather than caught.** *Superseded in mechanism:* Revision 1
placed this in a migration that no longer exists. It survives as a live risk without any format
change, because a NaN-bearing vector today ranks nondeterministically and silently never dedups
through `compress.rs:123`. *Mitigations:* ADR-3's three layers, applied at `compress.rs` too.

**4 (new) — a merge silently drops half a fix.** S1 and S2 rewrite the same two SQL literals two
lines apart, and neither task's test exercises the other's change. A conflict resolved by taking
one side ships either an unescaped `LIKE` or a tie-less `ORDER BY`, with a green test suite.
*Mitigation:* the S2 → S1 edge, plus a review checklist item that both changes are present in the
final literal.

**5 (new) — P1 reverts a landed fix.** Revision 1 assigned `db.rs:1863`/`:1911` to S3 and the
rewrite of the same loops to P1, with no edge between them. *Mitigation:* P1 owns those sites; S2
keeps only `db.rs:1354`.

**6 (new) — the platform-conditional tests pass without testing anything.** V4's oracle and
parity criteria target code compiled out on Linux. A green suite would mean nothing. *Mitigation:*
V4's seam requirement, and the standing guardrail that any `#[cfg]`-targeting criterion names its
seam.

## Test plan (DELIBERATE mode)

- **Unit:** overlapping-occurrence counting; LIKE metacharacter escaping; `start == end` at both
  front ends; `end` bounded against file size; finiteness guards in both cosine implementations.
- **Property:** shuffle-invariance of ranking (S1); relocation uniqueness over a generated tree
  with *k* planted copies including a hard-link case (V1); federated dedup under peer-order
  permutation (S4).
- **Integration:** the retained-descriptor structural change and, separately, the residual
  identity check (V2); two-writer flock contention on the heal race (V3); hard-link alias to the
  events log (V3); both resolver branches through V4's seam.
- **E2E / CLI:** `kb search --limit N --peers` emits exactly N; `kb cite f:4-4` rejected at parse;
  `--path-prefix 'src/%'` returns nothing.
- **Observability:** corrupt-embedding counter and dropped-row counts in result metadata; the
  capability warning emitted exactly once at init, asserted through the seam.
- **Perf:** `verify_matrix`-style matrices for P1 (separate semantic and cue cells) and P2
  (separate semantic, cue and MMR cells), at the `bd-3mr` corpus sizes, in-process via
  `db::search_entries` with `kb::bench_fixture::seed_db` fixtures.

## Success criteria

1. All 15 Importants and 5 Minors in C3's scope are closed or explicitly deferred with recorded
   rationale, lens3 #4 included per the lead ruling.
2. T0's spec is TLC-green, the `PlanHeal ; ReVerify ; ApplyHeal` race is reachable and caught, and
   V1/V3 are conformant.
3. At the **enumerated** sites — `db.rs:1354`, `:1863`, `:1911`, `stale_check.rs:353`, and the
   `unwrap_or_else` path substitution two lines above it — no silent default remains: each either
   propagates its error or reports a counted drop. No `unwrap_or(Ordering::Equal)` remains in a
   ranking path. Stated against enumerated sites rather than as a repo-wide sweep, so it is
   verifiable at close; a wider sweep is a follow-up, not a closure condition here.
4. `--limit` is a global limit under federation, and the rank-position character of cross-repo
   ordering is documented and pinned by a test.
5. P1's results are byte-identical to the post-S1 baseline. (Ordering *does* change between HEAD
   and post-S1, by ADR-2's design; the baseline is post-S1, pre-P1.)
6. P2 carries a threshold recorded before its measurement and a written land-or-defer verdict.
7. ADR-5 has explicit user sign-off with A0's audit count on the record, or is deferred with
   recorded rationale.
8. Zero Critical findings at code review; post-impl gates pass; user confirms merge.

## Review history

**Revision 1 → 2.** Critic verdict REJECT; architect pass reached three of the same conclusions
independently. Changes:

| Change | Source |
|---|---|
| ADR-1 rewritten from "implement with `SCHEMA_VERSION` bump" to "measure and decide"; P2 no longer changes the persisted format | Both passes: the bump fires the full re-embed rebuild ADR-1 rejects, and never stamps under `KB_NO_EMBED` |
| P1 redesigned from bounded top-K heap to bounded materialization; byte-identity criterion made achievable | Critic: `db.rs:1964` consumes the full candidate vector before truncation at `:1988` |
| S2's cap-clamping half removed; Principle 3 (shared-boundary invariants) withdrawn | Critic: lens3 #4 is assigned to `bd-21ef.2` by its own description |
| P1 given ownership of `db.rs:1863`/`:1911`; S2 keeps only `:1354` | Architect: P1 rewrites the loops S3 was fixing |
| S2 → S1 edge added | Architect: both rewrite the same two SQL literals |
| ADR-5's internal-consistency argument withdrawn; V4 extended to `safe_join`; V1's exclusion made inode-based | Architect: `safe_join` canonicalizes, so there are three symlink policies and `scan_file`'s check is dead on that path |
| ADR-4's comparability driver withdrawn; local lambdas left unchanged; rank-position character stated | Both passes: RRF is within-corpus rank, not cross-corpus comparable |
| T0 expanded: `path` variable addition, write-path coverage decision, `candidates` unit pinned | Architect: `EvidenceRow` has no path field, so the split is also a variable addition |
| A0 split out as a dependency-free chore | Critic: Revision 1's audit gated the sign-off that gated the task the audit lived in |
| V4 given a test-seam requirement and a no-sign-off fallback | Critic: the oracle is `#[cfg]`-compiled out on Linux |
| V2: contract stated as snapshot consistency; identity check given its own test; `try_from` criterion corrected to 32-bit-only; `verify_evidence` API blast radius named | Critic |
| S3 split into S3a (provenance) and S3b (context budget) | Architect: no shared mechanism, and it removed S3 from the `db.rs` conflict set |
| V1's measurement demoted from closure gate to post-impl reporting obligation | Critic: Revision 1 violated its own Principle 1 |
| ADR-3 extended to `compress.rs:231`; 11 `Embedder` impls and `NoopEmbedder`'s `vec![]` recorded | Architect |
| Finding count corrected 15/5 → 14/5; lens3 #9/#10 added to the shape table | Critic |
| Citations corrected: `db.rs:2293` (not `:2194`), `cite.rs:147` (not `:145`), `db.rs:1926` (not `:1925`) | Both |

**Not adopted.** The architect's synthesis suggested collapsing S1+S2+P1 into one "search_entries
rewrite" task to eliminate the conflict surface. Rejected: a single task that large is
unreviewable and would violate the TDD-per-change guardrail. Dependency edges achieve the same
safety at the cost of one hop on the critical path — and the architect's own recommendation was
edges rather than a merge.

**PM gate: PASS-WITH-CONDITIONS.** Scope fidelity confirmed on both deliberate scope calls — the
lens3 #4 removal was verified from both epic descriptions *and* from C2's own plan, which
independently proposes the identical split; the P2 conversion was judged not-under-delivery given
the epic's "staged" phrasing. All seven blocking conditions are cleared above:

| Condition | Resolution |
|---|---|
| C-1 C1's `SCHEMA_VERSION` bump changes ADR-1's cost basis | Added to ADR-1 and P2; threshold must state its basis |
| C-2 V3's cross-epic edge unwireable (C2 undecomposed) | V3 gains an executor precondition, not just a note |
| C-3 Two facts owed to C2 | Delivered to the lead for routing to `bd-21ef.2` |
| C-4 V4 conflated a gated and an ungated finding | Split into V4a (oracle, ungated) and V4b (parity, gated) |
| C-5 Criterion 3 unverifiable; two uncovered sites | Criterion narrowed to enumerated sites; `stale_check.rs:353` and its `unwrap_or_else` path substitution scoped into V3 |
| C-6 Deferrals lived only as prose | Two follow-up beads created |
| C-7 A stale open question | Struck; C1 answered it (`c1-log-durability.md:238-246`) |

Non-blocking PM recommendations not adopted here: applying C2's spec-waiver precedent to
S3a/S3b/S4 to raise start-time parallelism from 2 to 5 — deferred to the lead, since waiving the
TLA+ gate is a policy call above this plan. The remaining recommendations (ADR-5 remediation path,
A0's corpus scope and sample classification, an explicit security-reviewer pass on V4b at
post-impl since `verification.rs` does not match the auto-trigger pattern) are carried in
open-questions.md.

## Open questions

Tracked in `.state/.omc/plans/open-questions.md`.

## A0 audit result (bd-21ef.3.1)

Throwaway analysis, no production code. Live corpus: `.state/agent-kb/agent-kb.db`
(41 evidence rows, all with a non-null `citation_path`), db mtime
`2026-09-04 13:48:50 +0200`, size 1105920 bytes — captured after the last write to
`.state/agent-kb/agent-kb-events.jsonl` (mtime `2026-09-04 13:48:34 +0200`), so the DB is not
stale relative to the event log. `*.tmp.*` DB files ignored per instructions. `evidence` has no
`status` column — verification status is computed at query time, not persisted — so "would flip
from Verified to rejected" is approximated below by recomputing the current byte hash for every
symlink-traversing row and comparing to the recorded `citation_hash` (i.e. does the row verify
successfully today, under this platform's actual runtime policy).

**Totals (41 evidence rows):**

| Class | Count |
|---|---|
| Traverses an in-repo symlink component | 0 |
| File missing | 2 |
| Clean (no symlink, file present) | 39 |

**Flip count: 0.** No evidence row's `citation_path` traverses a symlink component anywhere in
its resolution (ancestor dirs or final file), so under ADR-5 Option A (`RESOLVE_NO_SYMLINKS` on
Linux) **zero currently-recorded citations would flip from Verified to rejected** in this corpus.
This is a small, single-repo corpus (41 rows) — the audit result does not generalize to a larger
or federated corpus, it only characterizes the present state of this repo's own KB.

The 2 "missing" rows are unrelated to symlinks: both cite `agent-kb/tla/...` paths
(`agent-kb/tla/InnerGap.tla:44-55`, `agent-kb/tla/EvalSplitFreeze.tla:1-20`) that do not exist at
the repo root — the actual directory is `.state/agent-kb/tla/`, so these are stale/mis-recorded
paths, not symlink escapes. Confirmed as a pre-existing, unrelated data quality issue, not scope
for this audit.

Sample table (no symlink-traversing rows exist, so no flip candidates to list; all 41 rows'
outcomes below for completeness — `entry_id`, `citation_path`, class):

| entry_id (truncated) | citation_path | class |
|---|---|---|
| 7dc942f4 | `src/commands/compact.rs:0-38237` | clean |
| 60b83793 | `src/components/db.rs:0-165384` | clean |
| 60b83793 | `src/commands/compact.rs:0-38237` | clean |
| 777b3986 | `.state/agent-kb/agent-kb-events.jsonl:0-21041` | clean |
| bf3e962c | `src/commands/compact.rs:0-38237` | clean |
| 09973904 | `src/components/verification.rs:0-42369` | clean |
| d8b119b3 | `src/config.rs:64-93` | clean |
| d8f627d2 | `src/components/kb_core.rs:219-225` | clean |
| 96245846 | `agent-kb/tla/InnerGap.tla:44-55` | **missing** |
| c713e7d5 | `src/commands/add.rs:115-143` | clean |
| 3feee5a5 | `agent-kb/tla/EvalSplitFreeze.tla:1-20` | **missing** |
| (remaining 30 rows) | various `src/**`, `.github/**`, `.omc/**`, `benches/**`, `scripts/**`, `mcp/**` paths | clean |

Confirmed for reference: `.beads`, `.claude`, and `CLAUDE.md` at the repo root **are** symlinks
(`os.path.islink` true), and `.state` is a plain directory (git worktree, not a symlink) — but no
evidence row's `citation_path` has a top-level component of `.beads`/`.claude`; the distinct
top-level components actually cited are `.github`, `.omc`, `.state`, `agent-kb`, `benches`, `mcp`,
`scripts`, `src`.

**Policy table for `src/components/verification.rs`** (three divergent symlink policies, as
flagged by ADR-5's "Context"):

| Site | Function(s) | Lines | Symlink policy today |
|---|---|---|---|
| Verify (Linux, modern kernel) | `open_citation_file` via `openat2` | `verification.rs:366-390` | **Permits** symlinks, provided the fully resolved path stays contained within `repo_root` — `ResolveFlags::BENEATH` enforces containment across resolution, `NO_MAGICLINKS` blocks magic-link escapes (e.g. `/proc`) but not ordinary symlinks. Atomic kernel-side resolution. |
| Verify (Linux, old kernel / `openat2` returns `NOSYS`; and all non-Linux Unix, always) | `open_citation_file_fallback` | `verification.rs:403-431` | **Rejects** every symlink at every path component (`OFlags::NOFOLLOW` per `openat`), including one that would resolve within the repo root. Strictest of the three where it applies. |
| Verify (non-Unix) | `open_citation_file` (`#[cfg(not(unix))]` arm) | `verification.rs:392-400` | Fails closed unconditionally — never opens the file, regardless of symlinks. |
| Relocation / write-path join | `safe_join` | `verification.rs:182-200` | **Permits** symlinks: canonicalizes both `repo_root` and the candidate path, then checks only that the canonicalized candidate is contained under the canonicalized root. No per-component symlink rejection — agrees with the Linux `openat2` verify policy on containment semantics. |
| Repo-wide excerpt search / relocation candidate walk | directory walk in `search_for_excerpt` + `scan_file` | `verification.rs:703-710`, `760-770` | **Rejects** every symlink unconditionally: `symlink_metadata` + `is_symlink()` check skips symlinked directories during traversal (never descended) and skips symlinked files before opening (never scanned) — regardless of containment. This is the outlier ADR-5's Context section refers to: it disagrees with both the Linux verify policy and `safe_join`, which both permit contained symlinks. |

On this repo's runtime platform (Linux, `uname -r` reports a 6.18 kernel, so `openat2` is
available and the `NOSYS` fallback branch is not exercised), the **effective current behavior**
for verification is: the `openat2` policy (permits contained symlinks) — consistent with the
"currently `Verified`" corpus analysis above, which used this same follow-symlinks-if-contained
behavior to recompute hashes.

**Exact script** (`/tmp/claude-1000/-home-urist-Documents-perso-agentic-kb/d8456c8b-f5ce-4791-886a-f2a45c50193b/scratchpad/a0_audit.py`):

```python
#!/usr/bin/env python3
"""A0 audit (bd-21ef.3.1): ADR-5 corpus symlink audit.

Throwaway analysis script. No production code changes. Reads the live
agent-kb.db, classifies every evidence row's citation_path by whether its
resolution traverses an in-repo symlink component, and (for symlink-
traversing rows) recomputes the current byte hash to determine whether the
row is *currently* verifying successfully (i.e. would flip from Verified to
rejected under ADR-5 Option A, which adds RESOLVE_NO_SYMLINKS on Linux).

Usage: python3 a0_audit.py
"""
import hashlib
import os
import re
import sqlite3
import sys

REPO_ROOT = "/home/urist/Documents/perso/agentic-kb"
DB_PATH = os.path.join(REPO_ROOT, ".state/agent-kb/agent-kb.db")

RANGE_RE = re.compile(r"^(.*):(\d+)-(\d+)$")


def parse_citation_path(raw):
    """Mirror verification.rs::parse_citation_path: split on the LAST ':'
    only if the tail matches \\d+-\\d+. Returns (file_part, (start,end)|None).
    """
    m = RANGE_RE.match(raw)
    if m:
        file_part, start, end = m.group(1), int(m.group(2)), int(m.group(3))
        return file_part, (start, end)
    return raw, None


def classify_path(file_rel):
    """lstat every component of repo_root/file_rel (ancestor dirs + final
    entry). Returns one of: 'symlink', 'missing', 'clean'.
    """
    parts = file_rel.split("/")
    cur = REPO_ROOT
    for i, part in enumerate(parts):
        cur = os.path.join(cur, part)
        try:
            st = os.lstat(cur)
        except OSError:
            return "missing"
        if os.path.islink(cur):
            return "symlink"
    # final component confirmed to exist and be non-symlink at every level
    if not os.path.isfile(cur):
        return "missing"
    return "clean"


def current_hash_matches(file_rel, range_, expected_hash):
    """Recompute sha256 over the resolved file (following symlinks, matching
    this platform's actual runtime behavior: modern Linux openat2 permits
    contained symlinks) and compare to the recorded citation_hash. Mirrors
    verification.rs::hash_citation_bytes without any of the containment /
    cap checks -- this script only asks "does the byte content still match
    today", to approximate whether the row is *currently* verifying.
    """
    abs_path = os.path.join(REPO_ROOT, file_rel)
    expected = expected_hash
    if expected.startswith("sha256:"):
        expected = expected[len("sha256:"):]
    try:
        with open(abs_path, "rb") as f:
            if range_ is not None:
                start, end = range_
                f.seek(start)
                data = f.read(end - start)
            else:
                data = f.read()
    except OSError:
        return False
    got = hashlib.sha256(data).hexdigest()
    return got.lower() == expected.lower()


def main():
    con = sqlite3.connect(DB_PATH)
    cur = con.cursor()
    cur.execute(
        "SELECT id, entry_id, citation_path, citation_hash FROM evidence "
        "WHERE citation_path IS NOT NULL"
    )
    rows = cur.fetchall()

    counts = {"symlink": 0, "missing": 0, "clean": 0}
    symlink_rows = []
    flip_rows = []

    for ev_id, entry_id, citation_path, citation_hash in rows:
        file_rel, range_ = parse_citation_path(citation_path)
        cls = classify_path(file_rel)
        counts[cls] += 1
        if cls == "symlink":
            currently_verified = current_hash_matches(file_rel, range_, citation_hash)
            symlink_rows.append((entry_id, citation_path, currently_verified))
            if currently_verified:
                flip_rows.append((entry_id, citation_path))

    print(f"total evidence rows with citation_path: {len(rows)}")
    print(f"classification counts: {counts}")
    print(f"symlink-traversing rows currently verified (would flip): {len(flip_rows)}")
    print()
    print("symlink-traversing rows (entry_id, citation_path, currently_verified):")
    for r in symlink_rows:
        print("  ", r)
    print()
    print("sample flip candidates (entry_id, citation_path):")
    for r in flip_rows[:15]:
        print("  ", r)


if __name__ == "__main__":
    main()
```

Raw output:

```
total evidence rows with citation_path: 41
classification counts: {'symlink': 0, 'missing': 2, 'clean': 39}
symlink-traversing rows currently verified (would flip): 0

symlink-traversing rows (entry_id, citation_path, currently_verified):

sample flip candidates (entry_id, citation_path):
```
