---------------------------- MODULE AgentKbEvidence ----------------------------
(* Refinement of AgentKb covering Phase 0 + Phase 1 (code-kind only) of the
   defensibility plan.

   Adds
   ----
   * entries.kind enum                 — per-entry kind in {observation, belief,
                                         procedure, convention, memory}
   * evidence table                    — set of evidence rows per entry id
   * entries.evidence_status enum      — {missing, present, n_a}
   * EvidenceAdd / EvidenceExpire events — new event variants in the JSONL log
   * Soft-mandate rule                 — empty evidence on belief/procedure/
                                         observation entries forces
                                         evidence_status = "missing"
   * LegacyAdd event variant + is_legacy state — models the AC1/AC2 migration
                                         backfill (legacy entries get
                                         kind="belief" and evidence_status="n_a"
                                         explicitly, bypassing the StatusOf rule)
   * Compact action with evidence-preserving CompactedLogE — models AC13/T-S6a's
                                         proptest action set

   Does NOT model
   ---------------
   * audit_runs table          — DB-only cache (not event-logged; L4)
   * HEAD-byte-hash verification — read-time computed, never persisted; covered
                                   by Verified flag at retrieval, modelled as a
                                   pure function of the cited bytes (no state)
   * Confidence formula        — Phase 2+, out of scope

   Verified properties
   -------------------
   TypeInvariantE          All state variables are well-typed.
   EvidenceMaterialization When the lock is free, materialized state matches
                           the event log replayed through ApplyEventE.
   OrphanTolerated         An EvidenceAdd for a non-existent entry id leaves
                           the DB well-typed and is filtered at apply time
                           (does not create a phantom entry).
   StatusConsistent        Every present entry: if is_legacy[id], status = "n_a";
                           else status = StatusOf(kind, evidence).
   OrphanAddIsSoftMandate  Codifies the ADR-B contract: an Add with no matching
                           EvidenceAdd surfaces as evidence_status="missing"
                           (not as a defect) — the soft-mandate state IS the
                           failure-tolerant semantic for partial batch writes.
   AbsentEntriesClean      Expired entries have empty evidence and "n_a" status.
   EvidenceKindRestricted  Phase 1 = code-only at the materialized state level.
   PartitionEquivalent     The 3-phase rebuild property: snapshot + catchup
                           materializes identically to single replay.
   CompactionEquivalenceE  Materialize(CompactedLogE(log)) = Materialize(log)
                           — compact preserves entries + evidence + status.

   Atomic write-through abstraction
   ---------------------------------
   The base AgentKb spec models a 4-step locked write protocol (acquire /
   append / materialize / release).  AgentKbEvidence abstracts this as a
   single atomic WriteThrough action.  Safety properties of the locked
   protocol are inherited *by assumption*, not by refinement mapping: base
   AgentKb's MutualExclusion serializes WriteAppend+WriteMaterialize, so any
   interleaved schedule of the base spec is observationally equivalent to a
   sequential schedule under WriteThrough.

   Run
   ---
   tlc AgentKbEvidence -config AgentKbEvidence.cfg -workers 4 -deadlock
*)

EXTENDS Sequences, FiniteSets, Naturals, TLC

CONSTANTS
    EntryIds,    \* set of possible entry IDs, e.g. {"e1","e2"}
    EvidenceIds, \* set of possible evidence IDs, e.g. {"v1","v2"}
    MaxLogLen    \* state-space bound for TLC

ASSUME EntryIds    # {}
ASSUME EvidenceIds # {}
ASSUME MaxLogLen \in Nat /\ MaxLogLen > 0

(* ──────────────────────────── Domain enums ─────────────────────────────── *)

\* Entry kinds — per L6, Phase 1 ships all 5 in the schema but only `code`
\* evidence is accepted at write time.  The kinds shape the soft-mandate rule.
EntryKinds == {"observation", "belief", "procedure", "convention", "memory"}

\* Soft-mandate triggers — these kinds require evidence; missing-evidence
\* forces evidence_status = "missing".  Other kinds get "n_a".
EvidenceRequiredKinds == {"observation", "belief", "procedure"}

\* Evidence kinds — Phase 1 = code only; the schema CHECK allows the full set
\* but kb_add rejects all but "code" at write time (out of model — modelled
\* purely at the events layer here).
EvidenceKinds == {"code"}  \* Phase 1 scope (L6)

EvidenceStatuses == {"missing", "present", "n_a"}

EventActions == {"add", "legacy_add", "evidence_add", "evidence_expire", "expire"}

(* ──────────────────────────── Tagged-union values ──────────────────────── *)

\* An entry is either absent or present.  Present carries its kind.
AbsentEntry == [type |-> "absent"]
PresentEntry(k) == [type |-> "present", kind |-> k]

\* An evidence row is a (eid, kind) pair attached to an entry.  Concrete
\* citation_path / citation_sha / citation_hash are abstracted away — TLC
\* only needs identity + kind to verify materialization correctness.
Evidence == [eid : EvidenceIds, kind : EvidenceKinds]

\* Event constructors (match the JSONL event schema introduced in Phase 1).
AddEvent(id, k) ==
    [action |-> "add", id |-> id, kind |-> k]

\* Legacy add — models pre-Phase-0 entries replayed by kb_rebuild.  Carries
\* no kind field; AC1 backfills kind="belief", AC2 sets evidence_status="n_a"
\* explicitly (bypassing StatusOf).  This is the migration semantic.
LegacyAddEvent(id) ==
    [action |-> "legacy_add", id |-> id]

EvidenceAddEvent(id, ev) ==
    [action |-> "evidence_add", id |-> id, evidence |-> ev]

EvidenceExpireEvent(id, eid) ==
    [action |-> "evidence_expire", id |-> id, eid |-> eid]

ExpireEvent(id) ==
    [action |-> "expire", id |-> id]

(* ──────────────────────────── State variables ──────────────────────────── *)

VARIABLES
    log,        \* Seq(Event)
    entries,    \* [EntryIds -> AbsentEntry | PresentEntry(k)]
    evidence,   \* [EntryIds -> SUBSET Evidence]
    estatus,    \* [EntryIds -> EvidenceStatuses]
    is_legacy   \* [EntryIds -> BOOLEAN] — true iff last-add for id was legacy_add

vars == <<log, entries, evidence, estatus, is_legacy>>

LogLenBound == Len(log) <= MaxLogLen

(* ──────────────────────────── Type invariant ───────────────────────────── *)

TypeInvariantE ==
    /\ \A id \in EntryIds : entries[id].type \in {"absent", "present"}
    /\ \A id \in EntryIds :
            entries[id].type = "present" => entries[id].kind \in EntryKinds
    /\ \A id \in EntryIds : evidence[id] \subseteq Evidence
    /\ \A id \in EntryIds : estatus[id] \in EvidenceStatuses
    /\ \A id \in EntryIds : is_legacy[id] \in BOOLEAN

(* ──────────────────────────── Soft-mandate function ────────────────────── *)

\* Given a present entry's kind and its evidence set, what evidence_status
\* must the materialized state carry?  Models L2 of the defensibility spec.
\* NOT applied to legacy entries (is_legacy=true): those get "n_a" per AC2.
StatusOf(k, evs) ==
    IF k \notin EvidenceRequiredKinds
        THEN "n_a"
        ELSE IF evs = {} THEN "missing" ELSE "present"

(* ──────────────────────────── Materialization ──────────────────────────── *)

EmptyEntries  == [id \in EntryIds |-> AbsentEntry]
EmptyEvidence == [id \in EntryIds |-> {}]
EmptyStatus   == [id \in EntryIds |-> "n_a"]
EmptyLegacy   == [id \in EntryIds |-> FALSE]

\* ApplyEventE: refinement of AgentKb.ApplyEvent that also threads the
\* evidence + estatus + is_legacy state.  Returns a 4-tuple.
\*
\* Key semantic rules (locked decisions L2 + L4 + L6 + AC1/AC2 migration):
\*
\*   "add"             — install/overwrite the entry with kind k.  Reset
\*                       evidence to empty, recompute status via StatusOf.
\*                       Clears is_legacy (this is a fresh write-time claim).
\*
\*   "legacy_add"      — install entry with kind="belief" (AC1 backfill default)
\*                       and evidence_status="n_a" (AC2 explicit grandfather).
\*                       Sets is_legacy=true.  Re-add via "add" later overrides.
\*
\*   "evidence_add"    — add an evidence row to evidence[id] IFF the entry is
\*                       present.  Orphan EvidenceAdd (id absent) is FILTERED:
\*                       no state change.  This is the OrphanTolerated property
\*                       that lets the batch-append protocol survive partial
\*                       writes (ADR-B).  After adding, recompute status —
\*                       UNLESS the entry is legacy (keep "n_a").
\*
\*   "evidence_expire" — remove the named evidence id from evidence[id] IFF
\*                       present.  Recompute status (or keep "n_a" if legacy).
\*                       Orphan (id absent) is filtered.
\*
\*   "expire"          — mark entry absent.  Clear evidence + reset status to
\*                       "n_a" + clear is_legacy.
\*
ApplyEventE(state, ev) ==
    LET ents == state[1]
        evs  == state[2]
        sts  == state[3]
        lgy  == state[4]
    IN CASE ev.action = "add" ->
            LET ents2 == [ents EXCEPT ![ev.id] = PresentEntry(ev.kind)]
                evs2  == [evs  EXCEPT ![ev.id] = {}]
                sts2  == [sts  EXCEPT ![ev.id] = StatusOf(ev.kind, {})]
                lgy2  == [lgy  EXCEPT ![ev.id] = FALSE]
            IN <<ents2, evs2, sts2, lgy2>>
      [] ev.action = "legacy_add" ->
            LET ents2 == [ents EXCEPT ![ev.id] = PresentEntry("belief")]
                evs2  == [evs  EXCEPT ![ev.id] = {}]
                sts2  == [sts  EXCEPT ![ev.id] = "n_a"]
                lgy2  == [lgy  EXCEPT ![ev.id] = TRUE]
            IN <<ents2, evs2, sts2, lgy2>>
      [] ev.action = "evidence_add" ->
            IF ents[ev.id].type = "absent"
                THEN state  \* orphan tolerated (L4 + OrphanTolerated)
                ELSE LET newSet == evs[ev.id] \cup {ev.evidence}
                         evs2   == [evs EXCEPT ![ev.id] = newSet]
                         newSts == IF lgy[ev.id]
                                       THEN "n_a"
                                       ELSE StatusOf(ents[ev.id].kind, newSet)
                         sts2   == [sts EXCEPT ![ev.id] = newSts]
                     IN <<ents, evs2, sts2, lgy>>
      [] ev.action = "evidence_expire" ->
            IF ents[ev.id].type = "absent"
                THEN state
                ELSE LET filtered == { e \in evs[ev.id] : e.eid # ev.eid }
                         evs2 == [evs EXCEPT ![ev.id] = filtered]
                         newSts == IF lgy[ev.id]
                                       THEN "n_a"
                                       ELSE StatusOf(ents[ev.id].kind, filtered)
                         sts2 == [sts EXCEPT ![ev.id] = newSts]
                     IN <<ents, evs2, sts2, lgy>>
      [] ev.action = "expire" ->
            LET ents2 == [ents EXCEPT ![ev.id] = AbsentEntry]
                evs2  == [evs  EXCEPT ![ev.id] = {}]
                sts2  == [sts  EXCEPT ![ev.id] = "n_a"]
                lgy2  == [lgy  EXCEPT ![ev.id] = FALSE]
            IN <<ents2, evs2, sts2, lgy2>>
      [] OTHER -> state

RECURSIVE MatHelperE(_, _)
MatHelperE(events, i) ==
    IF i = 0
        THEN <<EmptyEntries, EmptyEvidence, EmptyStatus, EmptyLegacy>>
        ELSE ApplyEventE(MatHelperE(events, i - 1), events[i])

MaterializeE(events) == MatHelperE(events, Len(events))

(* ──────────────────────────── CompactedLogE ────────────────────────────── *)
(*
   Compact preserves the materialized (entries, evidence, status, is_legacy)
   tuple.  For Phase 1 the squashed log emits, for each present entry:
     1. An Add event (or LegacyAdd if is_legacy) carrying the entry's kind.
     2. One EvidenceAdd event per evidence row.
   Absent entries are dropped entirely (compaction's whole point).

   The result Materializes to the same state as the original log
   (CompactionEquivalenceE invariant).  This is the property AC13 / T-S6a's
   proptest exercises.
*)
RECURSIVE SetToSeqAux(_, _)
SetToSeqAux(S, acc) ==
    IF S = {}
        THEN acc
        ELSE LET x == CHOOSE e \in S : TRUE
             IN SetToSeqAux(S \ {x}, Append(acc, x))

SetToSeq(S) == SetToSeqAux(S, <<>>)

\* For a single present id, emit one Add (or LegacyAdd) + N EvidenceAdds.
EventsForEntry(id, ents, evs, lgy) ==
    LET addEv == IF lgy[id]
                     THEN LegacyAddEvent(id)
                     ELSE AddEvent(id, ents[id].kind)
        evidenceEvs == { EvidenceAddEvent(id, e) : e \in evs[id] }
    IN <<addEv>> \o SetToSeq(evidenceEvs)

\* Concatenate per-entry event sequences for all present ids.
RECURSIVE ConcatEntries(_, _, _, _, _)
ConcatEntries(idSeq, i, ents, evs, lgy) ==
    IF i > Len(idSeq)
        THEN <<>>
        ELSE EventsForEntry(idSeq[i], ents, evs, lgy)
             \o ConcatEntries(idSeq, i + 1, ents, evs, lgy)

CompactedLogE(events) ==
    LET state    == MaterializeE(events)
        ents     == state[1]
        evs      == state[2]
        lgy      == state[4]
        presentIds == { id \in EntryIds : ents[id].type = "present" }
        idSeq    == SetToSeq(presentIds)
    IN ConcatEntries(idSeq, 1, ents, evs, lgy)

(* ──────────────────────────── Initial state ────────────────────────────── *)

Init ==
    /\ log       = <<>>
    /\ entries   = EmptyEntries
    /\ evidence  = EmptyEvidence
    /\ estatus   = EmptyStatus
    /\ is_legacy = EmptyLegacy

(* ──────────────────────────── Actions ──────────────────────────────────── *)

\* Atomic write-through: append event to log + apply to materialized state.
WriteThrough(ev) ==
    LET nextState == ApplyEventE(<<entries, evidence, estatus, is_legacy>>, ev)
    IN /\ log'       = Append(log, ev)
       /\ entries'   = nextState[1]
       /\ evidence'  = nextState[2]
       /\ estatus'   = nextState[3]
       /\ is_legacy' = nextState[4]

DoAdd ==
    \E id \in EntryIds, k \in EntryKinds : WriteThrough(AddEvent(id, k))

DoLegacyAdd ==
    \E id \in EntryIds : WriteThrough(LegacyAddEvent(id))

DoEvidenceAdd ==
    \E id \in EntryIds, eid \in EvidenceIds, ek \in EvidenceKinds :
        WriteThrough(EvidenceAddEvent(id, [eid |-> eid, kind |-> ek]))

DoEvidenceExpire ==
    \E id \in EntryIds, eid \in EvidenceIds :
        WriteThrough(EvidenceExpireEvent(id, eid))

DoExpire ==
    \E id \in EntryIds : WriteThrough(ExpireEvent(id))

\* Compact: atomically replace the log with its squashed equivalent.  The
\* materialized state is unchanged (CompactionEquivalenceE).
DoCompact ==
    /\ log'       = CompactedLogE(log)
    /\ UNCHANGED <<entries, evidence, estatus, is_legacy>>

Next ==
    \/ DoAdd
    \/ DoLegacyAdd
    \/ DoEvidenceAdd
    \/ DoEvidenceExpire
    \/ DoExpire
    \/ DoCompact

Spec == Init /\ [][Next]_vars

(* ──────────────────────────── Safety invariants ────────────────────────── *)

\* The materialized state matches MaterializeE(log) at every step.
EvidenceMaterialization ==
    LET mat == MaterializeE(log)
    IN /\ entries   = mat[1]
       /\ evidence  = mat[2]
       /\ estatus   = mat[3]
       /\ is_legacy = mat[4]

\* OrphanTolerated: any present entry was created by some prior "add" or
\* "legacy_add" event in the log.
OrphanTolerated ==
    \A id \in EntryIds :
        entries[id].type = "present" =>
            \E i \in 1..Len(log) :
                /\ log[i].action \in {"add", "legacy_add"}
                /\ log[i].id = id

\* StatusConsistent: derived state matches the soft-mandate function (or
\* the grandfather "n_a" for legacy entries).
StatusConsistent ==
    \A id \in EntryIds :
        entries[id].type = "present" =>
            IF is_legacy[id]
                THEN estatus[id] = "n_a"
                ELSE estatus[id] = StatusOf(entries[id].kind, evidence[id])

\* OrphanAddIsSoftMandate: codifies the ADR-B contract — an orphan Add
\* (present entry, required-kind, empty evidence, non-legacy) surfaces as
\* evidence_status="missing", NOT as a defect.  Implied by StatusConsistent
\* but named here to lock the contract for implementers.
OrphanAddIsSoftMandate ==
    \A id \in EntryIds :
        (   entries[id].type = "present"
         /\ entries[id].kind \in EvidenceRequiredKinds
         /\ evidence[id] = {}
         /\ ~is_legacy[id])
        => estatus[id] = "missing"

\* Absent entries always have empty evidence, "n_a" status, and is_legacy=FALSE.
AbsentEntriesClean ==
    \A id \in EntryIds :
        entries[id].type = "absent" =>
            /\ evidence[id]  = {}
            /\ estatus[id]   = "n_a"
            /\ is_legacy[id] = FALSE

\* Evidence kinds restricted to Phase 1 scope (L6).
EvidenceKindRestricted ==
    \A id \in EntryIds :
        \A e \in evidence[id] : e.kind \in EvidenceKinds

Invariants ==
    /\ TypeInvariantE
    /\ EvidenceMaterialization
    /\ OrphanTolerated
    /\ StatusConsistent
    /\ OrphanAddIsSoftMandate
    /\ AbsentEntriesClean
    /\ EvidenceKindRestricted

THEOREM Spec => []Invariants

(* ──────────────────────────── Rebuild equivalence (3-phase) ────────────── *)

RECURSIVE ReplayFrom(_, _, _)
ReplayFrom(events, i, state) ==
    IF i > Len(events)
        THEN state
        ELSE ReplayFrom(events, i + 1, ApplyEventE(state, events[i]))

\* PartitionEquivalent: for every split point k, replaying log[1..k] then
\* log[k+1..Len(log)] yields the same state as replaying the whole log.
PartitionEquivalent ==
    \A k \in 0..Len(log) :
        LET snapState == MatHelperE(log, k)
            catchup   == ReplayFrom(log, k + 1, snapState)
            full      == MaterializeE(log)
        IN catchup = full

\* CompactionEquivalenceE: the squashed log materializes to the same state.
\* Codifies AC13 / T-S6a's proptest claim (Add/EvidenceAdd/EvidenceExpire/
\* Compact arbitrary sequences → DB state equals direct reduction).
CompactionEquivalenceE ==
    MaterializeE(CompactedLogE(log)) = MaterializeE(log)

=============================================================================
