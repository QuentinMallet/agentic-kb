# C3 / T0 — `CitationRelocation.tla` amendment: refinement, scope and determinism

Beads task `bd-21ef.3.2`. Plan: `.state/.omc/plans/c3-read-path.md`, section "T0 — spec:
`CitationRelocation.tla` amendment". Companion artefact for part (a):
`../CitationRelocation-planheal-trace.md`. Code references are at HEAD `2e2051d`.

Part (a) — the `path` field, the `PlanHeal` / `ApplyHeal` split and the
`NoStaleHealCommit` invariant — is delivered in the module itself and evidenced by the
trace document. This note records parts (b) and (c).

---

## (b) The write path is out of scope for this module — decision and justification

**Decision.** `CitationRelocation.tla` does **not** gain an `Acquire` / `ResolveHash` /
`Append` split. The write-side TOCTOU in V2 (`bd-21ef.3.4`) and V3 (`bd-21ef.3.5`) is
recorded here as out of this module's scope, with the residual risk stated below.

### The shape of the write-side hazard, stated precisely

Two sites, the same plan-then-commit shape as `heal_relocations`:

- **V2.** `compute_citation_fields` hashes by pathname (`cite.rs:77`), then
  `self_check_citation_fields` re-opens the *same pathname* through `verify_evidence`
  (`cite.rs:87`). Two independent opens of a name, not one descriptor read twice.
- **V3.** `kb_core::add` resolves missing citation hashes at `:154-193`, memoised by path
  **string**, and acquires the flock only at `:238`. A hash resolved before the lock is
  appended as `storedHash` without re-verification.

V3's hazard is the more serious of the two, because `storedHash` is the value
`StoredHashImmutable` protects: nothing in this module may ever rewrite it. A wrong
`storedHash` written at creation is therefore unrecoverable by construction, and every
later verdict about that row is a comparison against a lie.

### Why not model it here

1. **Layered refinement forbids the monolith.** The repo's formal-methods playbook is
   explicit: prefer many small refinement specs over a single large file. Evidence
   *creation* is a different lifecycle from evidence *verification and relocation*.
   `CitationRelocation`'s `Init` materialises rows that already exist and already carry a
   `storedHash`; a write-path model has to make row existence and `storedHash` themselves
   the things that get established, which means a lock variable, per-path content, and a
   pending-resolution set. That is a second module, not a section of this one. This task
   is scoped to `CitationRelocation.tla` and its cfgs, so the correct outcome within that
   scope is to record the decision rather than to fold two lifecycles into one file.
2. **The bound budget is already spent.** Adding `path` cost enough that the main cfg had
   to drop from `MaxRows = 3` to `MaxRows = 1` (see the trace document's run matrix); a
   two-row run at the full candidate bound exhausted memory. A creation phase on top
   would force further bound cuts on the read-path invariants that are this module's
   actual subject.
3. **V3's fix is structural, not a runtime check.** After C2's `L1a` (`bd-21ef.2.3`) the
   logic lives in `kb_core::add_locked(&Lock, &Connection, ..)`, with `add` reduced to a
   thin acquiring wrapper. Resolution moves inside `add_locked`, where the `&Lock` token
   is a compile-time proof that the flock is held. TLA+ earns its keep where an ordering
   can be got wrong at runtime and no type witnesses it; an ordering that cannot be
   expressed without the token is not that case. A model saying "resolve under the same
   lock you append under" would restate what the signature already enforces.
4. **V2's residual window is a decided non-property.** The plan states the contract
   directly: the retained descriptor buys *snapshot consistency* — the emitted hash
   describes bytes the process actually read — and no check-before-emit can make the
   pair `(citation_path, citation_hash)` atomic against a rename after the check. That
   residual window is accepted, not fixed. Modelling V2 would either prove the property
   the single-descriptor construction already gives structurally, or produce a permanent
   counterexample for a property C3 has decided is unattainable. A spec that must carry a
   permanently suppressed violation is worse than no spec.

### Why V2 and V3 still carry the T0 dependency edge

The edge is a **contract** dependency, not a coverage dependency. T0 does not model V2's
or V3's code; it fixes what "recheck the premise" has to mean, and both tasks implement
that contract one level down:

- `ApplyHeal` defines a premise as **every input the plan consumed** — path, liveness and
  content — not one column. V3's current bug is precisely that its under-lock requery
  covers one column (`citation_hash`, `stale_check.rs:239`) and that column is write-once,
  so the check is vacuous. `PremiseHolds` is the shape of the correct requery, and V3's
  acceptance test ("the second heal discarded rather than overwriting the first") is
  `NoStaleHealCommit` restated as a test.
- `ApplyHeal`'s failure arm **discards**; it does not fall back, downgrade, or commit a
  weakened version. That is the behaviour V3 must implement, and it is why the plan's
  third silent-default finding matters here: `stale_check.rs:350` and
  `stale_check.rs:362` both substitute `rel_path.clone()` for an undecodable
  `citation_path` column, which would make `premisePath` a fabricated value and the
  premise check a comparison of one invention against another. Both sites must be fixed
  for the T0 premise contract to hold.
- V2's identity check immediately before emission is the same premise pattern at the
  descriptor level: the premise is "this pathname still names the descriptor I read".

The module deliberately does **not** claim that an unchanged premise implies the same
destination at code level. `PlanHeal` records a nondeterministically chosen destination,
and foreign moves are modelled only through `ReVerify` and `ConcurrentHeal`. A stale
destination with an otherwise unchanged premise is therefore left to the code-side
obligations in V2 (`bd-21ef.3.4`) and V3 (`bd-21ef.3.5`): re-run relocation and emit only
if it still yields the same destination.

### Residual risk, stated

Choosing (b) leaves one gap with no formal cover: nothing in this module's proof chain
establishes that `storedHash` was correct when it was first written. Every invariant here
is conditional on that. If the epic later wants that closed, the right artefact is a
separate minimal module — `EvidenceWritePath.tla`, three actions (`Acquire`,
`ResolveHash`, `Append`), one invariant that an appended `storedHash` corresponds to
content observed under the lock — not an extension of this file.

---

## (c) Refinement mapping, the unit of `candidates`, and the determinism decision

### lens2 #1 and #2 are refinement-mapping failures, not spec failures

`NonUniqueUnverified` holds in the model, and TLC confirms it on every cfg in the run
matrix:

```tla
NonUniqueUnverified ==
  \A row \in RowIds : rows[row].candidates # 1 => rows[row].status # "Relocated"
```

The defect is in the mapping from code state to model state, and there are two of them:

- **#1 — the count is over the wrong domain.** `search_for_excerpt` scans the cited file
  first and returns `ExcerptSearch::Unique` at `verification.rs:662` on a single in-file
  hit, *before* the repo walk at `:686`. Under `FileThenRepo` the repo is never consulted.
  So the code's `Unique` is not the proposition `candidates = 1`; it is `candidates
  restricted to one file = 1`. There is no function from the code's state to the model's
  `candidates` under which the two agree, which is exactly what makes this a mapping
  failure: the model is not describing a state the code computes.
- **#2 — the count uses the wrong arithmetic.** `count_occurrences` advances
  `i += needle.len()` after a hit (`verification.rs:823`), so two overlapping occurrences
  of a periodic excerpt count as one. The code's number is "non-overlapping occurrences";
  until the unit is pinned, "candidates" names two different quantities in the two
  artefacts.

Neither is fixed by changing the spec. Both are fixed by V1 (`bd-21ef.3.3`) changing the
code so the mapping becomes sound.

### The unit of `candidates` — pinned

> `candidates` is the number of **match locations of the excerpt across the whole
> repository walk, including the cited file, counted with overlap**.

Precisely: the number of byte offsets `i` in each visited file such that
`file[i .. i + len(excerpt)] = excerpt`, summed over every file the walk visits, with the
cited file contributing 1 to the count. The model stores the saturating image
`candidates = min(actual, MaxCandidates)`; in the code the distinction that matters is
only 0 / 1 / more-than-1, which is why `MaxCandidates = 2` is a sufficient bound.

Three consequences the implementation must honour for the mapping to hold:

1. The walk must continue past an in-file hit, and must return non-unique if any further
   match exists anywhere. This is V1's first half.
2. `count_occurrences` must advance by one byte after a hit, not by `needle.len()`. This
   is V1's second half. "Counted with overlap" is the unit; the current code computes a
   different one.
3. The cited file must be excluded from the repo walk by `(st_dev, st_ino)` identity, not
   by path string, or a symlinked or hard-linked cited path is counted twice and yields a
   false non-unique. "Exactly once" is part of the unit, not an optimisation.

There is a fourth obligation that is not about counting but destroys the mapping just as
thoroughly: `scan_file` returns `CapExceeded` when a file exceeds the *remaining* budget
(`verification.rs:786-788`) and both callers propagate it as a whole-search abort
(`:657`, `:720`). With the walk made unconditional, a cap hit *after* one candidate has
been found must report cap-exceeded and must never degrade to the in-file candidate.
Degrading would reintroduce exactly the false `Unique` that #1 is about, and the model has
no state for "the search gave up": `CapExceeded` is outside the refinement map, not an
encoding of any `candidates` value.

### Determinism (ADR-2's total order): documented no-change

**Decision.** ADR-2 gets **no TLA+ module**. The obligation is discharged by property
tests plus an extended parity gate, recorded here rather than specified.

**Why.** ADR-2 is one shared comparator, `(score.total_cmp() descending, id ascending)`,
used by every ranking lane. What a module could state is that the comparator is a total
order and that sorting with it is deterministic given unique ids. That is a property of a
pure total function over a finite domain: no interleaving, no shared state, no partial
failure, no lock. TLA+ buys its cost on concurrency and state; a comparator has neither,
and a proptest over generated `(f32, i64)` vectors including `NaN` checks the *actual*
comparator rather than a model of it, which is strictly stronger evidence for this
particular claim. The risk ADR-2 addresses is drift between the six lanes
(`db.rs:1730`, `:1882`, `:1926`, `:1939`, `:2043`, `:2099`), and drift is a static
property — one function, all call sites routed through it — enforced by review and by
deleting the duplicated `unwrap_or(Equal)` sites, not by TLC.

**The one thing a spec would have added, and where it now lives.** The FTS lanes order in
SQL (`ORDER BY rank, e.id`, `db.rs:1415`, `:1455`) while every other lane orders in Rust.
"The same total order" therefore spans two languages, and no TLA+ model can discharge
cross-language agreement on tie ordering — it would restate the obligation without
checking it. That obligation goes to the parity gate at `db.rs:1762`, which S1
(`bd-21ef.3.7`) must move off limit-truncated `BTreeSet` comparison, since a
limit-truncated comparison is blind to precisely the ties at the limit boundary where the
two orders could disagree.

---

## Test obligations handed to V1, V2 and V3

**V1 (`bd-21ef.3.3`) — makes the refinement mapping sound.**

- The same strong excerpt in the cited file and in one other file yields non-unique, not
  a relocation. This is `NonUniqueUnverified`'s mapping, and it is a failing test first.
- A periodic multiline excerpt of at least 64 bytes with two overlapping copies counts 2.
- A property test over a generated tree with *k* planted copies asserts unique iff
  *k* = 1, including a case where the cited path is a hard link to a walked file, so the
  `(st_dev, st_ino)` exclusion is exercised rather than the string one.
- Cap exceeded *after* one candidate is found reports cap-exceeded and never degrades to
  the in-file candidate.

**V2 (`bd-21ef.3.4`) — the premise pattern at the descriptor level.**

- One descriptor, one read: the two-open window between `cite.rs:77` and `:87` is gone by
  construction, asserted structurally rather than by timing.
- A **separate** test for the residual mitigation: replace the pathname binding after the
  self-check and assert the identity check fires. The structural test does not exercise
  this; only this test does.
- `kb cite f.rs:4-4` and the MCP equivalent are rejected at parse, at both front ends.
- No test asserts atomicity of the `(citation_path, citation_hash)` pair. That window is
  accepted per (b) above; a test claiming otherwise would be asserting a property the
  design does not have.

**V3 (`bd-21ef.3.5`) — implements `ApplyHeal`.**

- Two writers contending the flock: the second heal is **discarded**, not applied and not
  merged. This is `NoStaleHealCommit` as a test, and the assertion is on the discard, not
  merely on the absence of corruption.
- The requery under the lock must cover the whole premise — current `citation_path`,
  liveness, and the search inputs — not just `citation_hash`. A fix that re-reads only the
  path leaves `VerdictPremiseHolds` unguarded: the search evidence can go stale without
  the path moving.
- An undecodable `citation_path` column errors rather than substituting `rel_path`
  (`stale_check.rs:350`, `stale_check.rs:362`). Without this the premise path can be a
  fabricated value and the comparison in the previous bullet proves nothing.
- A hard link to the events JSONL is rejected as self-referential, by `(st_dev, st_ino)`
  and not by normalised path string (`migrate_citations.rs:230`).
- A file mutated between hash resolution and append is caught by the pre-append
  re-verify, with the memo keyed on `(st_dev, st_ino)` rather than the path string.
