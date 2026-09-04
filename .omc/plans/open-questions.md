# Open questions across plans

Append-only. One section per plan.

> Note (2026-09-04, C1 planner): a rewrite of this file dropped the pre-existing
> `## c3-read-path — 2026-09-04` section (5 items, incl. the ADR-5 symlink policy decision and the
> C2/V3 lock ordering question). C3's planner should restore it. The C1 section below was also lost
> once and is re-added here.
>
> Follow-up (2026-09-04, C2 planner): the destructive rewrite was mine — I wrote this file believing
> it did not exist (an adversarial review had reported `git log --all -- '*open-questions*'` empty,
> which is true: the file is untracked, so there is no object to recover from). No copy exists on
> disk or in any ref. I have reconstructed the C3 section below **from `c3-read-path.md` itself**,
> not from the lost text, so it is derived and probably incomplete — C3's planner should correct or
> replace it rather than trust it. Apologies for the loss.

## C1 — log durability & crash recovery (`c1-log-durability.md`) - 2026-09-04

Needing user decision:

- [x] **Q1 — RESOLVED 2026-09-04 by the lead.** The empty hand-created `storage-correctness-2`
  branch was deleted and `/meta-epic-init storage-correctness-2 bd-21ef` run: aggregator branch
  recreated from master `2e2051d` and pushed, state file
  `.state/.omc/state/meta-epics/storage-correctness-2.json` (schema v2) written, program worktree at
  `.state/program-worktrees/storage-correctness-2/`, `aggregator-branch` label applied to `bd-21ef`.
  Worktree creation is unblocked and PM-DECOMPOSE's Step-4 precondition is satisfied.

- [x] **Q5 — RULED 2026-09-04: defer.** No `log_format` version line in this epic. It protects
  nothing already deployed (every binary in the field predates it) and is itself a format change, so
  it buys nothing for this transition. D7's posture carries the weight: hard error on a mid-log
  dangling `batch_begin`, the documented `kb compact`-then-downgrade path, and the machines_conf
  notification at post-impl. Recorded as P3 follow-up **`bd-21ef.1.15`** so the next format change
  inherits the question — by then a version-aware reader is deployed and a version line would
  actually gate a skewed binary rather than being decorative.

Needing measurement before they can be answered (do not pre-decide) — both confirmed as
measurement-driven by the lead:

- [ ] **Q2. Does the fsync cost breach the budget, and if so does the durability knob come back?**
  D2 rejects a `durability = sync | relaxed` knob on the grounds it invites an unsafe default — a
  rejection that is only safe if the measured cost is small. `T1b` establishes the first real
  `kb add` write-path baseline; if `fdatasync` proves material against a `KB_NO_EMBED=1` denominator,
  this reopens with data. — *The plan has no fallback if the answer is bad.*

- [ ] **Q3. Does `run_history` need a growth bound at all, and if so what?** D5 removes the positional
  500-cap because it is what breaks materialization-preservation, and proposes an optional
  log-deterministic replacement (drop runs older than N days by the event's own `ts`). No N is chosen
  and none may be needed — the corpus is 116 events. — *Choosing a positional cap again would
  reintroduce Critical 4.*

Deferred within C1, recorded so they are not lost:

- [ ] **Q4. What should a malformed *middle* log line do?** `read_events` hard-errors
  (`events.rs:307-318`), which after `T4` would take down every recovery entry point at once. The
  plan's position is quarantine-the-line-and-continue, but interior corruption (zero-filled blocks,
  `data=writeback`) is explicitly out of the TLA+ model scope — a code-side policy adopted without a
  model behind it.

- [ ] **C1 answers two questions raised elsewhere.** (a) C2-Q1 "reads never recover": C1 yields —
  recovery fires at process entry and write paths only; read paths detect and warn, never taking the
  write lock. (b) C2-Q2 outer-transaction ownership: D3 owns it, C2's `A1` joins via savepoint. (c)
  For C3: `Rebuild`'s swap does **not** preserve arbitrary `kb_meta` keys — the tmp DB receives only
  `schema_version` (`rebuild.rs:148`) and `embed_text_mode` (`db.rs:633`); C1's `T5b` now writes the
  cursor rows explicitly before the swap.

- [ ] **Withdrawn:** a claimed C1/C3 collision on `read_events`/`read_events_up_to`/
  `read_events_from_offset`. C3's decode-error sites are in `db.rs`, not `events.rs`. No conflict.

## C2 — exclusion & boundary discipline (`c2-exclusion-boundary.md`) - 2026-09-04

Lead rulings received 2026-09-04. Q1, Q2 and Q4 are closed; Q3, Q6 and Q7 are converted into task
criteria; Q5 is routed to C3.

- [x] **Q1. ADR-7 trilemma — DECIDED: sacrifice "reads always see a recovered DB".** Reads never
  mutate and never take the write lock; recovery lives in `open_or_init` at process entry and in
  write paths; a behind-the-log reader serves what it has plus the stderr staleness note. Lead's
  rationale: recovery-on-read reintroduces mutation-on-read, which is the disease `L1a` exists to
  cure. Forwarded to C1's planner as a binding input — C1's `T2` must reframe Open-as-recovery-point
  accordingly. Recorded in ADR-7.

- [x] **Q2. Outer SQLite transaction — DECIDED: C1's `D3` owns it, C2's `A1` joins via savepoint.**
  Consistent with the landed savepoint refactor. `A1` is unblocked from `L1a` and runs in parallel.
  Recorded in ADR-5 and in `bd-21ef.2.11`.

- [~] **Q3. Retroactively repair audit rows already split by a past crash — DEFERRED, as an explicit
  checkpoint.** Lead ruling: keep it deferred, but make it a decision checkpoint inside `A1`'s
  acceptance rather than a silent omission. `A1` now counts the `audit_runs` rows whose
  `source_weights` delta never applied and records the deferral *with that count*, so the drift is a
  known quantity surfaced at post-impl.

- [x] **Q4. Lens 3 finding 4 reassignment to C3 — CONFIRMED.** Same hunk as C3's `S2`; C2 carries
  nothing in `search_entries`. Lead confirmed the deviation from the component mandate was correct.

- [→] **Q5. Peer TTL read-filter and federated peer search — ROUTED TO C3.** Lead is passing it to
  C3's federation-contract task. C2's position stands as the input: `L1b` asserts the filter applies
  at `search.rs:127–160`, on the boundary "C2 owns the filter, C3 owns ranking".

- [~] **Q6. Deployed machines_conf pin field enumeration — criterion stands.** Lead confirmed `B1`'s
  blocking pre-landing criterion, and that
  `conventions/cross-repo/evidence-contract-notification` (KB) governs the machines_conf
  notification. `bd-21ef.2.5` cites it. If cross-repo access is unavailable, the fallback remains the
  two-phase accept-and-warn rollout.

- [~] **Q7. ADR-1 Option B fallback — recorded, with a named decision point.** Lead ruling: Option B
  stays a live fallback and the decision point is `L1a`'s post-impl review. If the 107-site blast
  radius produces a red-flag diff, the component drops to Option B *before* `L1c` deletes anything.
  This is why `L1a` deprecates rather than deletes — the fallback must stay reachable. Recorded in
  ADR-1, in `L1a`'s acceptance criteria, and in `bd-21ef.2.3`.

## C3 — read-path integrity & performance (`c3-read-path.md`) - 2026-09-04

Needing user sign-off:

- [ ] **Q1. ADR-5 symlink policy: Option A (`RESOLVE_NO_SYMLINKS` on Linux) or Option B (bounded
  fallback resolver)? — GOING TO THE USER** with the Option A recommendation (lead, 2026-09-04).
  PM's two packet gaps accepted and folded into `A0`'s acceptance: the audit must classify its
  sample **by symlink shape** (legitimate vendored/nix-store links vs. containment-relevant ones)
  and must state **what the user does with the N affected rows** — expected answer: re-cite via
  `kb cite` against the resolved real path, or expire the entry. `A0` also declares its corpus
  scope, since a single-repo count understates a fleet-wide policy. User-visible behaviour change: any existing citation whose path traverses an
  in-repo symlink flips from `Verified` to rejected under Option A. Task `A0` (`bd-21ef.3.1`)
  produces the corpus regression count that sizes the decision. Planner recommends **Option A** —
  failing closed on a containment path is the right default, and Option B puts new hand-rolled
  symlink resolution on the containment path itself. If no sign-off arrives, `V4b` closes as
  deferred and the divergence is carried as known debt; `V4a` (the existence-oracle half) is
  unaffected. — *Two gaps in the sign-off packet, per the PM gate: nobody has said what the user
  DOES with the N affected rows (auto-heal is correctly forbidden on them, so there is no recovery
  path stated), and `A0` should declare its corpus scope and classify its sample by symlink shape
  (vendored/nix-store vs. containment-relevant) — a bare count does not tell the user whether
  rejecting is right.*

- [x] **Q2. C3's scope reduction on staged perf item 2 — ACKNOWLEDGED by the lead** (2026-09-04)
  and surfaced to the user in the lead's summary. Binding condition attached: **P2's threshold
  write-up must state which basis it assumes** — standalone, or riding C1's `SCHEMA_VERSION` 2 → 3
  bump — because that synergy changes the verdict. Already in `bd-21ef.3.13`'s acceptance. ADR-1 revised converts
  pre-normalized embeddings from an implementation into a measure-and-decide task (`P2`,
  `bd-21ef.3.13`), deferring the persisted-format change to `bd-prenorm-embeddings-followup-te13`.
  Evidence: a `SCHEMA_VERSION` bump arms `rebuild_if_schema_obsolete` on six entry points and forces
  the full re-embed rebuild ADR-1 rejects, and under `KB_NO_EMBED` rebuild defers without stamping
  so the migration never completes. PM judged this not-under-delivery. — *Needs explicit
  acknowledgement, not silent acceptance.*

Contested with C2 — **the lead must adjudicate**:

- [x] **Q3. Who owns lens3 finding 4 (caps at the `search_entries` boundary)? — CLOSED by lead
  ruling; C3 owns it, and has acted.** The lead confirmed C2's Q4 ("reassignment to C3 —
  CONFIRMED"). C3 accepts and has restored the work as **`S5` (`bd-21ef.3.17`)**, a task of its
  own rather than folded back into `S2`, with Principle 5 reinstated and the finding count
  corrected back to 15 Importants. `S5 → P1` edge wired (the clamped values feed P1's pool and
  truncation calculations); `S5` is otherwise independent of `S1`/`S2` — different regions of
  `db.rs`. — *One caveat for the record: the ruling was made against C3 **Revision 1**'s plan text,
  whose premise was "C3's `S2` already claims the identical edits". Revision 2 had already removed
  them, so that premise no longer held when the ruling landed. The outcome is still right — without
  the ruling both plans disclaimed the finding, which is the one arrangement guaranteed to drop it
  — so C3 is acting on it rather than re-litigating.*

- [x] **Q4. Two facts about the cap work — CLOSED, absorbed into `S5`'s acceptance criteria** now
  that C3 owns it: clamping `inline_verify_k` inside `search_entries` is a **user-visible CLI
  regression**, because `search.rs:108` deliberately sets `inline_verify_k = limit`, so
  `kb search --limit 50` goes from 50 verified rows to 20 with the rest reported `verified=null` —
  `S5` requires this be an explicit decision (opt-in above the cap, or accepted and documented),
  never a silent side effect. And `verify_pool_size` has a floor (`db.rs:2201-2204`) but **no
  ceiling constant anywhere**, so naming the constant and its value is part of `S5`. Also recorded:
  do not add IN-query batching as dead code — once `limit` is bounded the recency `IN`
  (`db.rs:2062-2066`) cannot exceed 200 parameters (skip-if-dead posture approved by the lead).

  **Lead ruling (2026-09-04): the two decisions defer to implementation time, posture fixed now.**
  Binding before any measurement: no silent user-visible CLI regression, and both ceilings must be
  named constants. The `S5` implementer brings the lead a decision packet — the **measured**
  worst-case fan-out (`inline_verify_k × MAX_EVIDENCE_ROWS_PER_ENTRY`), the proposed
  `verify_pool_size` ceiling, and the **pre-existing CLI/MCP asymmetry** (MCP already clamps
  `inline_verify_k` to 20 at `mcp.rs:284` while the CLI sets it to `limit`, so the front ends
  already disagree and this predates `S5`; the question is which existing behaviour becomes the
  contract, not whether to introduce a regression). The lead rules on the packet; `S5` does not
  land the clamp before that.

- [x] **Q5 (C2's, routed to C3 by the lead) — ANSWERED against C2's actual `L1b` text, and baked
  into `S4`.** *Are expired peers excluded at federation time?* **Yes.** *By which mechanism?*
  C2's `L1b` read-time filter (`AND (expires_at IS NULL OR expires_at >= datetime('now'))` on peer
  reads, plus a locked physical sweep). `L1b`'s acceptance already asserts the filter reaches
  `search.rs:127–160`. Division adopted verbatim — **C2 owns the filter, C3 owns ranking** — and
  `S4` is now explicitly forbidden from adding a second expiry check or `expires_at` predicate. The
  filter is at peer-selection time, so it is upstream of the merge and an expired peer cannot
  consume a truncation slot; `S4`'s only obligation is not to defeat it (no caching peer paths
  across the filter). `S4` also inherits the new observable state C2's design creates: a peer may
  be **logically expired but physically present**.

  **Newly found, and neither plan had it: this is a textual collision, not just a semantic
  boundary.** `L1b` edits `search.rs:127–160` to insert the filter; `S4` rewrites that same loop
  for global limit, dedup and truncation. **Edge wired: `bd-21ef.3.11` → `bd-21ef.2.4`**, so `S4`
  rebases onto the filtered loop rather than merging against it.

Needing cross-component coordination (lead):

- [x] **Q6. Does C2's lock-contract task land before `V3` (`bd-21ef.3.5`)? — CLOSED, edge wired.**
  C2 was not decomposed when C3's PM gate ran, so no edge could be created and `V3` carried the
  ordering as an executor precondition. C2 has since decomposed: the specific task is
  **`bd-21ef.2.3` (`L1a`)**, which lands on the aggregator first and leaves `open_db` as a
  `#[deprecated]` wrapper so `V3` rebases incrementally. `br dep add bd-21ef.3.5 bd-21ef.2.3` is
  done, cycles clean. Per the lead, `V3` now **builds on `kb_core::add_locked`** rather than
  reordering `add`'s own `acquire_lock` — the resolution and pre-append re-verify move inside
  `add_locked`, so the TOCTOU disappears structurally rather than by careful ordering. The reverse
  edge is wired too: **`bd-21ef.2.13` (`L1c`, "delete the deprecated `open_db` after C1 and C3
  rebase") now depends on `bd-21ef.3.5`**, so `open_db` cannot be deleted out from under a
  half-rebased C3. C1's applied-cursor task still needs its own `L1a` edge; that one is open.

- [x] **Q7. Waive the TLA+ gate for `S3a`/`S3b`/`S4`? — GRANTED by the lead, scoped, and applied.**
  Decision file written at `.state/agent-kb/tla/decisions/c3-search-tasks-spec-waiver.md` (lead as
  grantor, same form as the `stale-check-no-spec.md` precedent), and the T0 edges dropped from
  `bd-21ef.3.9`/`.10`/`.11`. Cycles clean. Start-time parallelism rises from 2 to **4**: `S3a` and
  `S3b` join `T0` and `A0` as ready; `S4` does not, because it independently depends on C2's `L1b`
  (`bd-21ef.2.4`) — same hunk. The waiver is explicitly scoped: it does not extend to any other
  task, does not waive post-impl spec compliance, and cannot be claimed by analogy by a task that
  writes an event, mutates an evidence row, or takes the write flock. Two sign-off checkboxes carry
  to post-impl (`bd-21ef.3.14`).

Deferred within C3, recorded so they are not lost:

- [ ] **Q8. FTS parity-gate cost.** Making the gate limit-insensitive means either an unlimited
  debug-build comparison on every search — materially expensive at 100k entries — or extending the
  limit past the tie boundary. `S1` must choose and record; the choice is deliberately not
  pre-made.

- [x] **Q9. Explicit security-reviewer pass on `V4b` at post-impl — AGREED and wired.** Added as a
  named line in `bd-21ef.3.14`'s description so a pattern-miss cannot skip it. ADR-5 changes a
  path-containment control, but `verification.rs` does not match post-impl's security-review trigger
  pattern (`secrets/|auth/|oidc|policies/|pki/|apparmor`), so the gate will not auto-fire. No threat
  model exists in `.omc/state/` for this epic.

- [ ] **Q10. `P1` substitutes a mechanism the epic text names.** The epic specifies "stream+bounded
  top-K heap for semantic+cue scans, metadata for winners only". `P1` delivers streaming and
  winners-only metadata but **drops the heap for the semantic lane**, because `db.rs:1964`
  enumerates the full candidate vector to compute RRF rank contributions before truncation at
  `:1988` — any heap with K < N changes fused scores and order. The findings (lens3 #9/#10) still
  close and the memory win is still real. — *Recorded so a future reader comparing epic text to
  delivery does not see an unexplained gap.*

- [ ] **Q11. Does CI run on any non-Linux target?** Determines whether `V4a`/`V4b` parity coverage
  must be a test seam (the plan assumes yes: seam required, since the oracle is `#[cfg]`-compiled
  out on Linux and the fallback resolver is reached only via `openat2` → `NOSYS`) or can be a
  platform matrix.

- [x] **Answered by C1:** "Does `Rebuild`'s swap preserve arbitrary `kb_meta` keys?" — it does not
  (`c1-log-durability.md:238-246`). This is a precondition of the pre-normalization follow-up: a
  marker-only design is silently dropped by every rebuild unless rebuild writes it.

- [x] **Answered to C1:** the claimed C1/C3 collision on `read_events`* is a false positive. C1's
  functions are in `events.rs:234/242/266`; C3's decode sites are `db.rs:1354/1863/1911`.

## C3 — reconstruction, superseded

The C2 planner's good-faith reconstruction of the lost C3 section stood here. C3's planner has
since restored the authoritative section above (11 items, at "C3 — read-path integrity &
performance"), which supersedes it — no need to hunt for a "fifth unrecoverable item"; the
restored section is written fresh from the plan and the PM gate, not recovered, and is complete.

Two things the reconstruction contributed that the restored section has absorbed:

- **`bd-21ef.2.3` (`L1a`) is the specific C2 task `V3` waits on**, not the whole lock epic, and it
  leaves `open_db` as a `#[deprecated]` wrapper so `V3` can rebase incrementally. C2 had not been
  decomposed when C3's PM gate ran, which is why C3-Q6 asked the lead to wire the edge. It exists
  now, so **the edge is wired**: `br dep add bd-21ef.3.5 bd-21ef.2.3`, cycles clean. C3-Q6 is
  closed.
- ADR-4's contract question is settled in the plan, not open: `--limit` is **global**, with
  `(origin_repo, id)` dedup and a single truncation. What remains open is C2-Q5's TTL-filter
  interaction, answered under C3-Q5 above.

No apology needed for the loss — the reconstruction was accurate on every point it asserted.
