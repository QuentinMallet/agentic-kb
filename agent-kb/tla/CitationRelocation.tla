--------------------------- MODULE CitationRelocation ---------------------------
(*
  CitationRelocation.tla  —  Sound citation verification and relocation
  ======================================================================

  A citation is verified directly when its stored content hash still matches
  the content at its recorded path.  A changed citation may be relocated only
  from a strong excerpt with exactly one candidate.  Zero or multiple
  candidates, and weak excerpts, are never guessed: the citation remains
  Unverified.

  Heal rewrites the citation PATH only.  It never writes the stored hash and
  never asserts a hash match: an excerpt match over a fragment does not imply
  that the full cited byte range still hashes to the recorded value.  The
  model therefore carries the recorded hash and the hash of the content at the
  current path as separate values, so that "the hash matches" is derived and
  cannot be assigned by fiat.  After a successful Heal the content at the new
  path may or may not match; that outcome is nondeterministic.  A relocated
  row reaches Verified only in a LATER pass, by re-hashing (ReVerify) against
  the unchanged stored hash.

  ReVerify models a content change and starts a new verification pass.  Status
  monotonicity is required only inside one pass; a new pass may reset a row to
  Unverified.  The previous-state snapshot and last action make the temporal
  requirements ordinary TLC-checkable state invariants.

  C3/T0 amendment — the citation PATH is now a modelled value
  ------------------------------------------------------------
  Before this amendment the row carried no path at all and relocation was
  modelled by changing `contentHash`.  A citation's identity is the PAIR
  (path, hash), so the path had to become a field before the plan-then-commit
  race could be expressed.  `path` is that field.

  `Heal` is split into `PlanHeal` and `ApplyHeal` because that is the shape of
  the code.  `stale_check::heal_relocations` (stale_check.rs:220) computes the
  whole relocation plan in `run_stale_check` (:194) BEFORE taking the flock at
  :230, and under the lock it requeries only `citation_hash` (:239) — a
  write-once column that by construction cannot have changed.  It never
  requeries `citation_path`.  So a plan "row r moves A -> B", recorded while r
  pointed at A, is committed unconditionally even though another writer has
  since moved r to C.  The commit silently overwrites C with B.

  `PlanHeal` records a plan against a row snapshot: the path it searched from,
  the row's liveness, and the content it hashed.  `ApplyHeal` commits only if
  that premise still holds, and otherwise discards the plan.  The premise
  check on the PATH is the one the code omits, so it is the one the constant
  `UnsafeApply` removes: with `UnsafeApply = TRUE` the model reproduces the
  code's behaviour and `NoStaleHealCommit` fails, and only that invariant
  fails.  See CitationRelocation-planheal-trace.md for the trace and the run
  matrix, and decisions/c3-citation-relocation-t0.md for the refinement
  mapping, the write-path scope decision, and the determinism decision.

  Relocation search is treated as a deterministic function of the row's search
  inputs (path, contentHash, excerptStrong, candidates).  `ApplyHeal` therefore
  re-checks those inputs rather than re-deriving a destination: an unchanged
  premise yields the same destination by construction, which is exactly what
  V3's "re-run relocation and emit only if it still yields the same
  destination" acceptance means.
*)

EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
  MaxRows,
  MaxCandidates,
  MaxPaths,
  UnsafeApply    \* TRUE drops ONLY the path premise check, modelling the code

ASSUME MaxRows \in Nat /\ MaxRows > 0
ASSUME MaxCandidates \in Nat /\ MaxCandidates > 0
(* Two distinct paths are the minimum that lets a citation move. *)
ASSUME MaxPaths \in Nat /\ MaxPaths > 1
ASSUME UnsafeApply \in BOOLEAN

RowIds == 1..MaxRows
PathIds == 1..MaxPaths
Passes == {0, 1}
Statuses == {"Verified", "Relocated", "Unverified"}

(* Abstract hash values.  Two distinct values are enough to distinguish
   "matches the recorded hash" from "does not". *)
Hashes == {0, 1}

EvidenceRow == [status : Statuses,
                storedHash : Hashes,     \* recorded at kb_add; write-once
                contentHash : Hashes,    \* content at the citation's current path
                candidates : 0..MaxCandidates,
                excerptStrong : BOOLEAN,
                path : PathIds]          \* the citation's current path

(* A relocation plan, recorded by PlanHeal and consumed by ApplyHeal.  The
   premise fields are the row snapshot the plan was computed against. *)
NoPlan == [kind |-> "none"]

HealPlans ==
  {NoPlan} \cup
  [kind : {"plan"},
   newPath : PathIds,
   premisePath : PathIds,        \* the path the search ran from
   premiseLive : BOOLEAN,        \* the row still needed healing
   premiseContent : Hashes]      \* the content the search hashed

ActionKinds == {"Init", "Verify", "ReVerify", "PlanHeal", "ApplyHeal"}

VARIABLES
  rows,          \* evidence rows, indexed by their stable row identifier
  pass,          \* current verification pass
  plans,         \* outstanding relocation plan per row
  previousRows,  \* rows immediately before the last transition
  previousPass,  \* pass immediately before the last transition
  lastAction     \* [kind, row, before, pathStale, liveStale, committed] witness

vars == <<rows, pass, plans, previousRows, previousPass, lastAction>>

Rank(status) ==
  CASE status = "Unverified" -> 0
    [] status = "Relocated"  -> 1
    [] status = "Verified"   -> 2

(* Derived, never assigned: the recorded hash still describes the content at
   the path the citation currently points to. *)
HashMatch(row) == rows[row].storedHash = rows[row].contentHash

(* The row still needs healing: a Verified row is never a heal target. *)
Live(row) == rows[row].status # "Verified"

(* The relocation search verdict: a strong excerpt with exactly one candidate
   is the only outcome that may repoint a citation. *)
SafeSearch(row) == rows[row].excerptStrong /\ rows[row].candidates = 1

TypeOK ==
  /\ rows \in [RowIds -> EvidenceRow]
  /\ pass \in Passes
  /\ plans \in [RowIds -> HealPlans]
  /\ previousRows \in [RowIds -> EvidenceRow]
  /\ previousPass \in Passes
  /\ lastAction \in [kind : ActionKinds,
                     row : RowIds,
                     before : Statuses,
                     pathStale : BOOLEAN,
                     liveStale : BOOLEAN,
                     committed : BOOLEAN]

Init ==
  /\ rows \in [RowIds -> [status : {"Unverified"},
                           storedHash : Hashes,
                           contentHash : Hashes,
                           candidates : 0..MaxCandidates,
                           excerptStrong : BOOLEAN,
                           path : PathIds]]
  /\ pass = 0
  /\ plans = [row \in RowIds |-> NoPlan]
  /\ previousRows = rows
  /\ previousPass = pass
  /\ lastAction = [kind |-> "Init", row |-> 1, before |-> "Unverified",
                   pathStale |-> FALSE, liveStale |-> FALSE, committed |-> FALSE]

Mark(kind, row, isPathStale, isLiveStale, didCommit) ==
  /\ previousRows' = rows
  /\ previousPass' = pass
  /\ lastAction' = [kind |-> kind, row |-> row,
                    before |-> rows[row].status,
                    pathStale |-> isPathStale,
                    liveStale |-> isLiveStale,
                    committed |-> didCommit]

(* Only ApplyHeal consumes a plan, so only ApplyHeal can be stale or can
   discard.  For every other action the effect is applied unconditionally and
   the staleness flags do not apply. *)
Snapshot(kind, row) == Mark(kind, row, FALSE, FALSE, TRUE)

(* A matching hash verifies directly; no relocation search is performed.
   Verify only resolves a row still unresolved in the current pass, so it can
   never promote a row this pass has just relocated: that promotion requires a
   fresh re-hash, which is a new pass (ReVerify). *)
Verify(row) ==
  /\ rows[row].status = "Unverified"
  /\ HashMatch(row)
  /\ rows' = [rows EXCEPT ![row].status = "Verified"]
  /\ Snapshot("Verify", row)
  /\ UNCHANGED <<pass, plans>>

(* Content change begins a fresh pass: the content at the citation's path is
   re-hashed and compared against the UNCHANGED stored hash.  The relocation
   search evidence for the new pass is supplied at the same time.

   ReVerify also re-reads the citation's PATH.  That is how a move made by
   another writer — a concurrent heal that committed A -> C — becomes visible
   to this verifier.  It is the <path change> step of the
   PlanHeal ; <path change> ; ApplyHeal race. *)
ReVerify(row, newContent, newCandidates, newExcerptStrong, newPath) ==
  /\ newContent \in Hashes
  /\ newCandidates \in 0..MaxCandidates
  /\ newExcerptStrong \in BOOLEAN
  /\ newPath \in PathIds
  /\ rows' = [rows EXCEPT
       ![row] = [status |-> IF newContent = rows[row].storedHash
                              THEN "Verified"
                              ELSE "Unverified",
                 storedHash |-> rows[row].storedHash,
                 contentHash |-> newContent,
                 candidates |-> newCandidates,
                 excerptStrong |-> newExcerptStrong,
                 path |-> newPath]]
  /\ pass' = 1 - pass
  /\ Snapshot("ReVerify", row)
  /\ UNCHANGED plans

(* PlanHeal runs the relocation search for a non-Verified row.  This is
   `run_stale_check` building the report, outside the lock.

   Only a SAFE search produces a plan, because only a row the report marks
   Relocated with a new path produces a `citation_healed` event
   (stale_check.rs:233-236); every other outcome writes nothing durable.  The
   unsafe branch therefore keeps the original `Heal` ELSE arm verbatim — and
   note it is a no-op on `rows`, since a Relocated row always has a strong
   excerpt and exactly one candidate (WeakExcerptUnverified, NonUniqueUnverified),
   so a row failing SafeSearch and not Verified is already Unverified. *)
PlanHeal(row) ==
  /\ rows[row].status # "Verified"
  /\ plans[row] = NoPlan
  /\ IF SafeSearch(row)
        THEN /\ \E dest \in PathIds :
                  plans' = [plans EXCEPT ![row] =
                    [kind |-> "plan",
                     newPath |-> dest,
                     premisePath |-> rows[row].path,
                     premiseLive |-> Live(row),
                     premiseContent |-> rows[row].contentHash]]
             /\ UNCHANGED rows
        ELSE /\ rows' = [rows EXCEPT ![row].status = "Unverified"]
             /\ UNCHANGED plans
  /\ Snapshot("PlanHeal", row)
  /\ UNCHANGED pass

(* The four halves of the premise, evaluated against the CURRENT row. *)
PathPremiseHolds(row)    == rows[row].path = plans[row].premisePath
LivePremiseHolds(row)    == Live(row) = plans[row].premiseLive
ContentPremiseHolds(row) == rows[row].contentHash = plans[row].premiseContent
VerdictPremiseHolds(row) == SafeSearch(row)

(* UnsafeApply drops ONLY the path check, because that is precisely the check
   `heal_relocations` omits: it requeries the hash under the lock and never the
   path.  Isolating that one omission is what makes the counterexample name the
   real defect instead of a strawman with no premise checking at all. *)
PremiseHolds(row) ==
  /\ (UnsafeApply \/ PathPremiseHolds(row))
  /\ LivePremiseHolds(row)
  /\ ContentPremiseHolds(row)
  /\ VerdictPremiseHolds(row)

(* ApplyHeal is the commit under the lock.  A held premise commits the plan:
   the status becomes Relocated, the citation is repointed to the planned
   path, and the content now under the citation is the content at the NEW path
   — which may or may not hash to the stored value.  The stored hash is not
   touched.  A broken premise discards the plan and writes nothing.  Every
   unsafe search outcome remains Unverified. *)
ApplyHeal(row) ==
  /\ plans[row].kind = "plan"
  /\ IF PremiseHolds(row)
        THEN /\ \E relocatedContent \in Hashes :
                  rows' = [rows EXCEPT
                    ![row].status = "Relocated",
                    ![row].path = plans[row].newPath,
                    ![row].contentHash = relocatedContent]
             /\ Mark("ApplyHeal", row,
                     ~PathPremiseHolds(row), ~LivePremiseHolds(row), TRUE)
        ELSE /\ UNCHANGED rows
             /\ Mark("ApplyHeal", row,
                     ~PathPremiseHolds(row), ~LivePremiseHolds(row), FALSE)
  /\ plans' = [plans EXCEPT ![row] = NoPlan]
  /\ UNCHANGED pass

Next ==
  \/ \E row \in RowIds : Verify(row)
  \/ \E row \in RowIds,
        newHash \in Hashes,
        newCandidates \in 0..MaxCandidates,
        newExcerptStrong \in BOOLEAN,
        newPath \in PathIds :
       ReVerify(row, newHash, newCandidates, newExcerptStrong, newPath)
  \/ \E row \in RowIds : PlanHeal(row)
  \/ \E row \in RowIds : ApplyHeal(row)

(* --- Required safety invariants --- *)

(* Neither half of a heal may be taken against a Verified row: PlanHeal is
   guarded, and ApplyHeal may only COMMIT when the liveness premise still
   holds.  A discarding ApplyHeal over a row that has since become Verified is
   exactly the behaviour wanted, so the commit flag is part of the statement. *)
NoHealOnVerified ==
  /\ (lastAction.kind = "PlanHeal") => lastAction.before # "Verified"
  /\ (lastAction.kind = "ApplyHeal" /\ lastAction.committed)
       => lastAction.before # "Verified"

(* The C3/T0 invariant.  No heal commits over a row whose path has changed
   since its plan was recorded.  This is what forbids the A -> B / A -> C
   overwrite in `heal_relocations`. *)
NoStaleHealCommit ==
  (lastAction.kind = "ApplyHeal" /\ lastAction.committed)
    => ~lastAction.pathStale

(* Non-vacuity probe.  Checked ONLY by CitationRelocation_NV_Discard.cfg, and
   expected to be VIOLATED there.  If a stale plan never actually reached
   ApplyHeal in the safe model, NoStaleHealCommit would hold vacuously and the
   premise check would be untested.  A violation of this predicate is a trace
   ending in a discarding ApplyHeal over a row whose path moved — the behaviour
   V3 must implement.  It is deliberately NOT listed in any other cfg.

   The `~liveStale` conjunct is what makes the probe sharp: it demands a
   discard driven by the PATH having moved while the row was still live, which
   is exactly the case `heal_relocations` cannot currently detect.  Without it
   the probe would be satisfied by a discard the liveness check alone would
   have caught, and the path check would still be untested. *)
NoStalePlanEverDiscarded ==
  ~(lastAction.kind = "ApplyHeal"
    /\ ~lastAction.committed
    /\ lastAction.pathStale
    /\ ~lastAction.liveStale)

(* Replaces the earlier "relocated implies hash matches", which was true only
   because Heal asserted it.  Now that Heal cannot assert it, this states the
   property that actually matters: a Verified row is one whose recorded hash
   matches the bytes currently under its citation. *)
VerifiedImpliesHashMatch ==
  \A row \in RowIds :
    rows[row].status = "Verified" => HashMatch(row)

(* No action rewrites recorded evidence.  This is what forbids "healing" a
   citation by moving the hash to whatever the new path contains.  The witness
   is previousRows, whose type — now carrying `path` — is pinned by TypeOK;
   the row's path is free to change, its stored hash never is. *)
StoredHashImmutable ==
  \A row \in RowIds :
    rows[row].storedHash = previousRows[row].storedHash

(* Relocation is not a promotion.  A relocated row may only become Verified
   after a new pass has re-hashed it. *)
NoSilentPromotion ==
  previousPass = pass =>
    \A row \in RowIds :
      ~(previousRows[row].status = "Relocated"
        /\ rows[row].status = "Verified")

Monotonicity ==
  previousPass = pass =>
    \A row \in RowIds :
      Rank(previousRows[row].status) <= Rank(rows[row].status)

NonUniqueUnverified ==
  \A row \in RowIds :
    rows[row].candidates # 1 => rows[row].status # "Relocated"

(* The excerpt floor is a precondition of relocation, not a heuristic. *)
WeakExcerptUnverified ==
  \A row \in RowIds :
    ~rows[row].excerptStrong => rows[row].status # "Relocated"

Spec == Init /\ [][Next]_vars

=============================================================================
