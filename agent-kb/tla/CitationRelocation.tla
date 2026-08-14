--------------------------- MODULE CitationRelocation ---------------------------
(*
  CitationRelocation.tla  —  Sound citation verification and relocation
  ======================================================================

  A citation is verified directly when its stored content hash still matches.
  A changed citation may be relocated only from a strong excerpt with exactly
  one candidate.  Zero or multiple candidates, and weak excerpts, are never
  guessed: the citation remains Unverified.

  ReVerify models a content change and starts a new verification pass.  Status
  monotonicity is required only inside one pass; a new pass may reset a row to
  Unverified.  The previous-state snapshot and last action make the temporal
  requirements ordinary TLC-checkable state invariants.
*)

EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
  MaxRows,
  MaxCandidates

ASSUME MaxRows \in Nat /\ MaxRows > 0
ASSUME MaxCandidates \in Nat /\ MaxCandidates > 0

RowIds == 1..MaxRows
Passes == {0, 1}
Statuses == {"Verified", "Relocated", "Unverified"}
EvidenceRow == [status : Statuses,
                hashMatch : BOOLEAN,
                candidates : 0..MaxCandidates,
                excerptStrong : BOOLEAN]

VARIABLES
  rows,          \* evidence rows, indexed by their stable row identifier
  pass,          \* current verification pass
  previousRows,  \* rows immediately before the last transition
  previousPass,  \* pass immediately before the last transition
  lastAction     \* [kind, row, before] witness for action safety

vars == <<rows, pass, previousRows, previousPass, lastAction>>

Rank(status) ==
  CASE status = "Unverified" -> 0
    [] status = "Relocated"  -> 1
    [] status = "Verified"   -> 2

TypeOK ==
  /\ rows \in [RowIds -> EvidenceRow]
  /\ pass \in Passes
  /\ previousRows \in [RowIds -> EvidenceRow]
  /\ previousPass \in Passes
  /\ lastAction \in [kind : {"Init", "Verify", "ReVerify", "Heal"},
                     row : RowIds,
                     before : Statuses]

Init ==
  /\ rows \in [RowIds -> [status : {"Unverified"},
                           hashMatch : BOOLEAN,
                           candidates : 0..MaxCandidates,
                           excerptStrong : BOOLEAN]]
  /\ pass = 0
  /\ previousRows = rows
  /\ previousPass = pass
  /\ lastAction = [kind |-> "Init", row |-> 1, before |-> "Unverified"]

Snapshot(kind, row) ==
  /\ previousRows' = rows
  /\ previousPass' = pass
  /\ lastAction' = [kind |-> kind, row |-> row,
                    before |-> rows[row].status]

(* A matching hash verifies directly; no relocation search is performed. *)
Verify(row) ==
  /\ rows[row].hashMatch
  /\ rows' = [rows EXCEPT ![row].status = "Verified"]
  /\ Snapshot("Verify", row)
  /\ UNCHANGED pass

(* Content change begins a fresh pass and supplies the result of its new
   hash check plus the relocation search evidence, if a search is needed. *)
ReVerify(row, newHash, newCandidates, newExcerptStrong) ==
  /\ newHash \in BOOLEAN
  /\ newCandidates \in 0..MaxCandidates
  /\ newExcerptStrong \in BOOLEAN
  /\ rows' = [rows EXCEPT
       ![row] = [status |-> IF newHash THEN "Verified" ELSE "Unverified",
                 hashMatch |-> newHash,
                 candidates |-> newCandidates,
                 excerptStrong |-> newExcerptStrong]]
  /\ pass' = 1 - pass
  /\ Snapshot("ReVerify", row)

(* Heal attempts relocation only for a non-Verified row.  Its successful
   branch requires a strong, unique match and immediately re-verifies the
   repointed citation.  Every unsafe search outcome remains Unverified. *)
Heal(row) ==
  /\ rows[row].status # "Verified"
  /\ IF rows[row].excerptStrong /\ rows[row].candidates = 1
        THEN rows' = [rows EXCEPT
               ![row].status = "Relocated",
               ![row].hashMatch = TRUE]
        ELSE rows' = [rows EXCEPT ![row].status = "Unverified"]
  /\ Snapshot("Heal", row)
  /\ UNCHANGED pass

Next ==
  \/ \E row \in RowIds : Verify(row)
  \/ \E row \in RowIds,
        newHash \in BOOLEAN,
        newCandidates \in 0..MaxCandidates,
        newExcerptStrong \in BOOLEAN :
       ReVerify(row, newHash, newCandidates, newExcerptStrong)
  \/ \E row \in RowIds : Heal(row)

(* --- Required safety invariants --- *)

NoHealOnVerified ==
  lastAction.kind = "Heal" => lastAction.before # "Verified"

PostHealSoundness ==
  \A row \in RowIds :
    rows[row].status = "Relocated" => rows[row].hashMatch = TRUE

Monotonicity ==
  previousPass = pass =>
    \A row \in RowIds :
      Rank(previousRows[row].status) <= Rank(rows[row].status)

NonUniqueUnverified ==
  \A row \in RowIds :
    rows[row].candidates # 1 => rows[row].status # "Relocated"

Spec == Init /\ [][Next]_vars

=============================================================================
