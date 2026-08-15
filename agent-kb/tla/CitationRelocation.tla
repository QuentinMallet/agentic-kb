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

(* Abstract hash values.  Two distinct values are enough to distinguish
   "matches the recorded hash" from "does not". *)
Hashes == {0, 1}

EvidenceRow == [status : Statuses,
                storedHash : Hashes,     \* recorded at kb_add; write-once
                contentHash : Hashes,    \* content at the citation's current path
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

(* Derived, never assigned: the recorded hash still describes the content at
   the path the citation currently points to. *)
HashMatch(row) == rows[row].storedHash = rows[row].contentHash

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
                           storedHash : Hashes,
                           contentHash : Hashes,
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

(* A matching hash verifies directly; no relocation search is performed.
   Verify only resolves a row still unresolved in the current pass, so it can
   never promote a row this pass has just relocated: that promotion requires a
   fresh re-hash, which is a new pass (ReVerify). *)
Verify(row) ==
  /\ rows[row].status = "Unverified"
  /\ HashMatch(row)
  /\ rows' = [rows EXCEPT ![row].status = "Verified"]
  /\ Snapshot("Verify", row)
  /\ UNCHANGED pass

(* Content change begins a fresh pass: the content at the citation's path is
   re-hashed and compared against the UNCHANGED stored hash.  The relocation
   search evidence for the new pass is supplied at the same time. *)
ReVerify(row, newContent, newCandidates, newExcerptStrong) ==
  /\ newContent \in Hashes
  /\ newCandidates \in 0..MaxCandidates
  /\ newExcerptStrong \in BOOLEAN
  /\ rows' = [rows EXCEPT
       ![row] = [status |-> IF newContent = rows[row].storedHash
                              THEN "Verified"
                              ELSE "Unverified",
                 storedHash |-> rows[row].storedHash,
                 contentHash |-> newContent,
                 candidates |-> newCandidates,
                 excerptStrong |-> newExcerptStrong]]
  /\ pass' = 1 - pass
  /\ Snapshot("ReVerify", row)

(* Heal attempts relocation only for a non-Verified row.  Its successful
   branch requires a strong, unique match and repoints the citation: the
   status becomes Relocated and the content now under the citation is the
   content at the NEW path — which may or may not hash to the stored value.
   The stored hash is not touched.  Every unsafe search outcome remains
   Unverified. *)
Heal(row) ==
  /\ rows[row].status # "Verified"
  /\ IF rows[row].excerptStrong /\ rows[row].candidates = 1
        THEN \E relocatedContent \in Hashes :
               rows' = [rows EXCEPT
                 ![row].status = "Relocated",
                 ![row].contentHash = relocatedContent]
        ELSE rows' = [rows EXCEPT ![row].status = "Unverified"]
  /\ Snapshot("Heal", row)
  /\ UNCHANGED pass

Next ==
  \/ \E row \in RowIds : Verify(row)
  \/ \E row \in RowIds,
        newHash \in Hashes,
        newCandidates \in 0..MaxCandidates,
        newExcerptStrong \in BOOLEAN :
       ReVerify(row, newHash, newCandidates, newExcerptStrong)
  \/ \E row \in RowIds : Heal(row)

(* --- Required safety invariants --- *)

NoHealOnVerified ==
  lastAction.kind = "Heal" => lastAction.before # "Verified"

(* Replaces the earlier "relocated implies hash matches", which was true only
   because Heal asserted it.  Now that Heal cannot assert it, this states the
   property that actually matters: a Verified row is one whose recorded hash
   matches the bytes currently under its citation. *)
VerifiedImpliesHashMatch ==
  \A row \in RowIds :
    rows[row].status = "Verified" => HashMatch(row)

(* No action rewrites recorded evidence.  This is what forbids "healing" a
   citation by moving the hash to whatever the new path contains. *)
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
