---------------------------- MODULE AgentKbEvidence ----------------------------
(* Refinement of AgentKb covering Phase 0 + Phase 1 (code-kind only) of the
   defensibility plan, revised at epic `evidence-storage-integrity` T0 to
   encode ADR-1 and ADR-2 and to model compaction as the implementation
   actually performs it.

   Adds
   ----
   * entries.kind enum                 — per-entry kind in {observation, belief,
                                         procedure, convention, memory}
   * evidence table                    — set of evidence rows per entry id
   * entries.evidence_status enum      — {missing, present, n_a}
   * EvidenceAdd / EvidenceExpire /
     CitationHealed events             — evidence-table event variants in the
                                         JSONL log
   * Soft-mandate rule                 — empty evidence on belief/procedure/
                                         observation entries forces
                                         evidence_status = "missing"
   * LegacyAdd event variant + is_legacy state — models the AC1/AC2 migration
                                         backfill (legacy entries get
                                         kind="belief" and evidence_status="n_a"
                                         explicitly, bypassing the StatusOf rule)
   * Compact action with a filter-and-retain CompactedLogE — models the real
                                         `compact` command, not an idealized one

   Does NOT model
   ---------------
   * audit_runs table          — DB-only cache (not event-logged; L4)
   * HEAD-byte-hash verification — read-time computed, never persisted; covered
                                   by Verified flag at retrieval, modelled as a
                                   pure function of the cited bytes (no state)
   * Confidence formula        — Phase 2+, out of scope
   * test_cases / run_history retention — compaction handles them, but they do
                                   not interact with the evidence lifecycle

   ─────────────────────────── ADR-1 (ratified) ───────────────────────────
   Upsert PRESERVES evidence, and `evidence_status` is ALWAYS derived from the
   evidence row set — never taken from the event payload.

   Revision 1 of this module had the opposite contract: its `add` arm cleared
   `evidence[id]` and derived the status from `{}`.  That is what made this
   module blind to the compaction evidence-loss defect: the failing interleaving
   materialized to `{}` on BOTH sides of CompactionEquivalenceE, so the
   invariant passed vacuously.  See gotchas/tla/compact-spec-fidelity-gap.

   `AddEvent` therefore carries a `claimed_status` field — the payload written
   by `add_validation.rs` — and `ApplyEventE` deliberately IGNORES it.  The
   invariant `AddIgnoresClaimedStatus` states that ignoring exactly.

   ─────────────────────────── ADR-2 (ratified) ───────────────────────────
   Expiring an entry GCs its evidence rows, and `CompactionEquivalenceE` is
   restated as LIVE-STATE equivalence.  Full-state equivalence is not the
   contract: `compact.rs:139-141` deliberately drops entries whose last expire
   follows their last upsert, so demanding full-state equality would demand
   compaction retain expired entries — reversing an intentional design
   decision (br-joj).

   Modelling note.  Because ADR-2 makes an expired entry's materialized state
   (absent, {}, "n_a", FALSE) identical to a never-created entry's, live-state
   and full-state equivalence happen to COINCIDE inside this abstraction —
   `AbsentEntriesClean` is what collapses them.  The distinction is real only at
   the implementation level, where expire leaves an `is_stale=1` row behind that
   a compacted log cannot reproduce.  The live-state form is stated here because
   it is the contract T2 must implement; the coincidence is a property of the
   abstraction, not a licence to strengthen the contract.

   Verified properties
   -------------------
   TypeInvariantE          All state variables are well-typed and every logged
                           event carries a known action.
   OrphanTolerated         An EvidenceAdd for a non-existent entry id leaves
                           the DB well-typed and is filtered at apply time
                           (does not create a phantom entry).
   StatusConsistent        Every present entry: if is_legacy[id], status = "n_a";
                           else status = StatusOf(kind, evidence).
   AddIgnoresClaimedStatus ADR-1: the upsert payload's evidence_status field is
                           not authoritative — replay ignores it.
   OrphanAddIsSoftMandate  Codifies the ADR-B contract: an Add with no matching
                           EvidenceAdd surfaces as evidence_status="missing"
                           (not as a defect) — the soft-mandate state IS the
                           failure-tolerant semantic for partial batch writes.
   AbsentEntriesClean      ADR-2: expired entries have empty evidence, "n_a"
                           status, and no legacy grandfathering.
   EvidenceKindRestricted  Phase 1 = code-only at the materialized state level.
   PartitionEquivalent     The 3-phase rebuild property: snapshot + catchup
                           materializes identically to single replay.
   CompactionEquivalenceE  ADR-2 live-state equivalence: replaying the log —
                           including after `compact` has rewritten it — still
                           yields the DB's live state.  This FAILED against the
                           pre-T1/T2 CompactedLogE; it holds against the
                           implemented one modelled below.

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
   Four configurations, all from this directory.  Since T1/T2 landed and the
   legacy_add arm was amended (CE3), ALL FOUR MUST PASS; the CE harnesses are
   kept as named regression gates for the two shapes they were built to expose.

     1. Regression — every invariant, compaction disabled.  MUST PASS.
        tlc AgentKbEvidence -config AgentKbEvidence_NoCompact.cfg -workers 4 -deadlock

     2. Primary counterexample harness — the upsert-reordering evidence loss.
        tlc AgentKbEvidence -config AgentKbEvidence_CE1.cfg -workers 4 -deadlock

     3. Secondary counterexample harness — the dropped evidence_expire
        resurrection.  MaxLogLen raised to 5 for headroom.
        tlc AgentKbEvidence -config AgentKbEvidence_CE2.cfg -workers 4 -deadlock

     4. Full model — every action, every invariant.
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

\* Symmetry reduction for the action generators.  `StatusOf` reads `kind` only
\* through membership in EvidenceRequiredKinds, and no other arm of ApplyEventE
\* reads it at all, so two representatives — one required, one not — cover every
\* distinct behaviour.  TypeInvariantE still ranges over the full EntryKinds set,
\* and CompactedLogE never inspects kind.
ModelKinds == {"belief", "convention"}

\* Evidence kinds — Phase 1 = code only; the schema CHECK allows the full set
\* but kb_add rejects all but "code" at write time (out of model — modelled
\* purely at the events layer here).
EvidenceKinds == {"code"}  \* Phase 1 scope (L6)

EvidenceStatuses == {"missing", "present", "n_a"}

\* Citation paths.  Two values suffice: the path recorded at evidence_add, and
\* the path a citation_healed event rewrites it to.  `citation_healed`
\* (db.rs:932-951) writes citation_path and nothing else.
InitialPath == "p0"
HealedPath  == "p1"
Paths       == {InitialPath, HealedPath}

EventActions ==
    {"add", "legacy_add", "evidence_add", "evidence_expire",
     "citation_healed", "expire"}

(* ──────────────────────────── Tagged-union values ──────────────────────── *)

\* An entry is either absent or present.  Present carries its kind.
AbsentEntry == [type |-> "absent"]
PresentEntry(k) == [type |-> "present", kind |-> k]

\* An evidence row is a (eid, kind, path) triple attached to an entry.
\* citation_sha / citation_hash / excerpt are abstracted away — TLC only needs
\* identity, kind, and the one field `citation_healed` mutates.
Evidence == [eid : EvidenceIds, kind : EvidenceKinds, path : Paths]

\* Event constructors (match the JSONL event schema).
\*
\* ADR-1: `claimed_status` is the evidence_status the writer put in the payload
\* (db.rs:726 reads it today; add_validation.rs:170-180 writes it).  It is
\* recorded here precisely so the model can state that replay must NOT read it.
AddEvent(id, k, cs) ==
    [action |-> "add", id |-> id, kind |-> k, claimed_status |-> cs]

\* Legacy add — models pre-Phase-0 entries replayed by kb_rebuild.  Carries
\* no kind field; AC1 backfills kind="belief", AC2 sets evidence_status="n_a"
\* explicitly (bypassing StatusOf).  This is the migration semantic.
LegacyAddEvent(id) ==
    [action |-> "legacy_add", id |-> id]

EvidenceAddEvent(id, ev) ==
    [action |-> "evidence_add", id |-> id, evidence |-> ev]

EvidenceExpireEvent(id, eid) ==
    [action |-> "evidence_expire", id |-> id, eid |-> eid]

CitationHealedEvent(id, eid, np) ==
    [action |-> "citation_healed", id |-> id, eid |-> eid, new_path |-> np]

ExpireEvent(id) ==
    [action |-> "expire", id |-> id]

(* ──────────────────────────── State variables ──────────────────────────── *)

VARIABLES
    log,        \* Seq(Event)
    entries,    \* [EntryIds -> AbsentEntry | PresentEntry(k)]
    evidence,   \* [EntryIds -> SUBSET Evidence]
    estatus,    \* [EntryIds -> EvidenceStatuses]
    is_legacy   \* [EntryIds -> BOOLEAN] — true iff the entry is still a
                \* grandfathered no-evidence legacy entry (AC2)

vars == <<log, entries, evidence, estatus, is_legacy>>

\* The materialized database, as the 4-tuple ApplyEventE threads.
DbState == <<entries, evidence, estatus, is_legacy>>

LogLenBound == Len(log) <= MaxLogLen

(* ──────────────────────────── Type invariant ───────────────────────────── *)

TypeInvariantE ==
    /\ \A id \in EntryIds : entries[id].type \in {"absent", "present"}
    /\ \A id \in EntryIds :
            entries[id].type = "present" => entries[id].kind \in EntryKinds
    /\ \A id \in EntryIds : evidence[id] \subseteq Evidence
    /\ \A id \in EntryIds : estatus[id] \in EvidenceStatuses
    /\ \A id \in EntryIds : is_legacy[id] \in BOOLEAN
    /\ \A i \in 1..Len(log) : log[i].action \in EventActions

(* ──────────────────────────── Soft-mandate function ────────────────────── *)

\* Given a present entry's kind and its evidence set, what evidence_status
\* must the materialized state carry?  Models L2 of the defensibility spec and
\* mirrors compute_evidence_status (db.rs:86-112) exactly.
\* NOT applied to entries still carrying the legacy grandfather (is_legacy=TRUE).
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
\* Key semantic rules (ADR-1, ADR-2, L2/L4/L6, AC1/AC2 migration):
\*
\*   "add"             — install/overwrite the entry with kind k.  ADR-1:
\*                       evidence is PRESERVED (db.rs:686-800 has no cascade;
\*                       db.rs:964 is the only DELETE FROM evidence in the tree)
\*                       and evidence_status is DERIVED from the surviving
\*                       evidence set, never from ev.claimed_status.  Clears the
\*                       legacy grandfather: this is a fresh write-time claim.
\*
\*   "legacy_add"      — install entry with kind="belief" (AC1 backfill default).
\*                       ADR-1 applies here too — a legacy add is still an upsert,
\*                       so evidence survives.  The AC2 grandfather
\*                       (evidence_status="n_a") is an INITIALIZATION only: it
\*                       lands on an absent entry, and an existing entry keeps
\*                       the status the evidence lifecycle last derived.  A
\*                       legacy upsert that RE-grandfathered an existing entry
\*                       would be exactly the payload-style authority ADR-1
\*                       abolishes, and it is order-sensitive under compaction —
\*                       see CE3 in T0-counterexample.md.
\*
\*   "evidence_add"    — add an evidence row to evidence[id] IFF the entry is
\*                       present.  Orphan EvidenceAdd (id absent) is FILTERED:
\*                       no state change (db.rs:889-899).  This is the
\*                       OrphanTolerated property that lets the batch-append
\*                       protocol survive partial writes (ADR-B) — and it is the
\*                       exact mechanism by which compaction loses evidence, once
\*                       an evidence event is replayed ahead of its parent.
\*                       Insertion is INSERT OR IGNORE on the evidence id
\*                       (db.rs:911, PRIMARY KEY(id)), so re-adding a known eid
\*                       is a no-op.  Status is recomputed UNCONDITIONALLY
\*                       (db.rs:920-931, br-f7y): the legacy grandfather is
\*                       dropped the moment the entry acquires evidence, which is
\*                       modelled by clearing is_legacy.
\*
\*   "evidence_expire" — remove the named evidence id from evidence[id].  Status
\*                       is recomputed unconditionally when the parent exists
\*                       (db.rs:968-984), same grandfather-drop as evidence_add.
\*                       Orphan (id absent) is filtered.
\*
\*   "citation_healed" — rewrite the citation_path of the row with the named eid
\*                       (db.rs:932-951: a bare UPDATE, writing citation_path and
\*                       nothing else).  No orphan guard exists in the code and
\*                       none is needed: the UPDATE silently matches no rows.
\*                       That silence is why the T2 emission rule must keep a
\*                       citation_healed event ordered after its own evidence_add.
\*                       Row count is unchanged, so status is unchanged.
\*
\*   "expire"          — ADR-2: mark entry absent AND GC its evidence, reset
\*                       status to "n_a", clear the legacy grandfather.
\*
ApplyEventE(state, ev) ==
    LET ents == state[1]
        evs  == state[2]
        sts  == state[3]
        lgy  == state[4]
    IN CASE ev.action = "add" ->
            \* ADR-1: evidence UNCHANGED; status derived; ev.claimed_status unread.
            LET ents2 == [ents EXCEPT ![ev.id] = PresentEntry(ev.kind)]
                sts2  == [sts  EXCEPT ![ev.id] = StatusOf(ev.kind, evs[ev.id])]
                lgy2  == [lgy  EXCEPT ![ev.id] = FALSE]
            IN <<ents2, evs, sts2, lgy2>>
      [] ev.action = "legacy_add" ->
            \* ADR-1 corollary (CE3): a legacy upsert may INITIALIZE a fresh
            \* entry's grandfather but may never RE-GRANDFATHER an existing one.
            \* `sts` is therefore left UNCHANGED: an absent entry already carries
            \* "n_a" (AbsentEntriesClean), so the fresh-insert case initializes to
            \* "n_a" for free, while an existing entry keeps whatever status the
            \* evidence lifecycle last derived for it.
            \*
            \* is_legacy becomes a DERIVED predicate — "this entry's status is a
            \* grandfather rather than a derivation" — rather than independent
            \* state.  That is forced, not chosen: the arm overwrites kind with
            \* the AC1 backfill default "belief", so an entry that carried "n_a"
            \* legitimately under kind="convention" acquires a status the belief
            \* rule would not derive, and only re-raising the grandfather keeps
            \* StatusConsistent true.  On every entry whose kind was already
            \* belief-like the predicate reproduces the preceding flag exactly,
            \* which is the "preserves the current is_legacy flag" clause.
            LET ents2 == [ents EXCEPT ![ev.id] = PresentEntry("belief")]
                lgy2  == [lgy EXCEPT ![ev.id] =
                             sts[ev.id] # StatusOf("belief", evs[ev.id])]
            IN <<ents2, evs, sts, lgy2>>
      [] ev.action = "evidence_add" ->
            IF ents[ev.id].type = "absent"
                THEN state  \* orphan tolerated (L4 + OrphanTolerated)
                ELSE LET known  == \E e \in evs[ev.id] : e.eid = ev.evidence.eid
                         newSet == IF known
                                       THEN evs[ev.id]            \* INSERT OR IGNORE
                                       ELSE evs[ev.id] \cup {ev.evidence}
                         evs2   == [evs EXCEPT ![ev.id] = newSet]
                         sts2   == [sts EXCEPT ![ev.id] =
                                       StatusOf(ents[ev.id].kind, newSet)]
                         lgy2   == [lgy EXCEPT ![ev.id] = FALSE]
                     IN <<ents, evs2, sts2, lgy2>>
      [] ev.action = "evidence_expire" ->
            IF ents[ev.id].type = "absent"
                THEN state
                ELSE LET filtered == { e \in evs[ev.id] : e.eid # ev.eid }
                         evs2 == [evs EXCEPT ![ev.id] = filtered]
                         sts2 == [sts EXCEPT ![ev.id] =
                                     StatusOf(ents[ev.id].kind, filtered)]
                         lgy2 == [lgy EXCEPT ![ev.id] = FALSE]
                     IN <<ents, evs2, sts2, lgy2>>
      [] ev.action = "citation_healed" ->
            LET rewritten == { (IF e.eid = ev.eid
                                    THEN [e EXCEPT !.path = ev.new_path]
                                    ELSE e) : e \in evs[ev.id] }
                evs2 == [evs EXCEPT ![ev.id] = rewritten]
            IN <<ents, evs2, sts, lgy>>
      [] ev.action = "expire" ->
            \* ADR-2: evidence is GC'd with the entry.
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
   Faithful model of `compact` (compact.rs:88-178).  Revision 1 of this module
   REGENERATED a canonical log from the materialized state, which made
   compaction correct by construction and unable to express any retention
   defect.  The real command does not synthesize events: it selects a subset of
   the ORIGINAL events by index and re-sorts that subset by index.

   Revision 2 (this one) tracks the POST-T1/T2 implementation, plus the one
   TARGET rule that has not landed yet (the live-at-index retention bound, CE5).
   The pre-fix rules that CE1 and CE2 exploit are recorded in
   T0-counterexample.md, not here.

   The algorithm, arm by arm:

     * entry_last[id]   — index of the last ("upsert","entries") event for id
                          (compact.rs:124).  Both `add` and `legacy_add` model
                          that event.
     * expire_last[id]  — index of the last ("expire","entries") event
                          (compact.rs:127).
     * an entry is LIVE iff expire_last[id] < entry_last[id]; live entries
                          contribute their entry_last index, dead ones are
                          dropped entirely (compact.rs:157-161).  Orphan expire
                          events are never emitted.
     * evidence_indices — every ("evidence_add"|"citation_healed"|
                          "evidence_expire","evidence") event
                          (compact.rs:135-137).  T1 added the third arm; before
                          it, dropping evidence_expire resurrected deleted rows
                          (CE2).
     * an evidence event is retained iff its parent is live, its index is
                          strictly greater than expire_last[parent], and its
                          parent was live AT ITS OWN INDEX.  The expire bound
                          keeps pre-expire rows from reattaching to a later
                          revive; the live-at-index bound (CE5) drops events
                          that were orphan no-ops on the original replay.  The
                          latter is the target rule — the code currently uses a
                          weaker first-upsert bound (compact.rs:186-196), which
                          is self-consistent only while expire leaves an
                          is_stale=1 row behind.  See RetainedEvidenceIdxs.
     * emission          (compact.rs:199-214) — retained upserts are emitted in
                          original-index order, and each live entry's retained
                          evidence events are spliced in immediately AFTER that
                          entry's retained upsert, in their original relative
                          order.  This is what closes CE1: an evidence event can
                          no longer replay before the upsert that parents it.
*)

EntryUpsertActions == {"add", "legacy_add"}

\* T1 added ("evidence_expire","evidence") to the match arm, so all three
\* evidence-table variants are now retention candidates (compact.rs:135-137).
EvidenceEventActions == {"evidence_add", "citation_healed", "evidence_expire"}

MaxOfSet(S) == CHOOSE i \in S : \A j \in S : j <= i

\* Index of the last event in `events` with an action in `acts` naming `id`.
\* 0 when there is none — the "absent from the HashMap" case.
LastIdxOf(events, id, acts) ==
    LET S == { i \in 1..Len(events) :
                 /\ events[i].action \in acts
                 /\ events[i].id = id }
    IN IF S = {} THEN 0 ELSE MaxOfSet(S)

\* Index of the last event with an action in `acts` naming `id` STRICTLY BEFORE
\* position i.  0 when there is none.
LastIdxBefore(events, id, acts, i) ==
    LET S == { j \in 1..(i - 1) :
                 /\ events[j].action \in acts
                 /\ events[j].id = id }
    IN IF S = {} THEN 0 ELSE MaxOfSet(S)

\* Was `id` a present entry at the moment event i applied?  Exactly the
\* condition ApplyEventE's orphan guard tests at that point in the replay.
LiveAtIdx(events, id, i) ==
      LastIdxBefore(events, id, EntryUpsertActions, i)
    > LastIdxBefore(events, id, {"expire"}, i)

LiveIdsIn(events) ==
    { id \in EntryIds :
        LET u == LastIdxOf(events, id, EntryUpsertActions)
        IN /\ u > 0
           /\ LastIdxOf(events, id, {"expire"}) < u }

\* Retention.  An evidence event survives iff
\*
\*   (1) its parent entry is live at the end of the log,
\*   (2) it sits strictly after that entry's LAST expire — an entry expire is an
\*       evidence-GC boundary (ADR-2), so pre-expire rows must not reattach to a
\*       later revive, and
\*   (3) its parent was LIVE AT ITS OWN INDEX.
\*
\* Bound (3) is the CE5 fix.  Without it, an evidence event landing in the gap
\* between an expire and a later revive upsert is retained even though it was an
\* orphan no-op during the original replay — and the emission rule then places it
\* AFTER the revive, where it becomes effective and resurrects evidence that
\* never applied.  It subsumes the implementation's current `entry_first` bound
\* (live-at-i implies some upsert precedes i), which is therefore not restated.
\*
\* This models the TARGET algorithm, not the code as it stands.  Today the
\* implementation is self-consistent without bound (3) because `expire` leaves an
\* `is_stale=1` row behind and `evidence_add`'s orphan guard counts stale rows,
\* so the gap event really does apply.  Bound (3) and the ADR-2 evidence GC must
\* land together — see T0-counterexample.md CE5 and beads task
\* bd-evidence-storage-integrity-w3xo.7.
RetainedEvidenceIdxs(events, id) ==
    LET lastX == LastIdxOf(events, id, {"expire"})
    IN { i \in 1..Len(events) :
           /\ events[i].action \in EvidenceEventActions
           /\ events[i].id = id
           /\ i > lastX
           /\ LiveAtIdx(events, id, i) }

\* Emit the named indices in ascending original order.
RECURSIVE FilterByIdx(_, _, _)
FilterByIdx(events, idxs, i) ==
    IF i > Len(events)
        THEN <<>>
        ELSE IF i \in idxs
                THEN <<events[i]>> \o FilterByIdx(events, idxs, i + 1)
                ELSE FilterByIdx(events, idxs, i + 1)

\* Emission (compact.rs:199-214).  Retained upserts are walked in original-index
\* order, and each live entry's retained evidence events are spliced in
\* IMMEDIATELY AFTER that entry's retained (last) upsert, keeping their relative
\* order.  This is the T2 rule: an evidence event can no longer replay ahead of
\* the upsert that gives it a parent, which is what CE1 exploited.
RECURSIVE EmitFrom(_, _, _)
EmitFrom(events, live, i) ==
    IF i > Len(events)
        THEN <<>>
        ELSE IF /\ events[i].action \in EntryUpsertActions
                /\ events[i].id \in live
                /\ LastIdxOf(events, events[i].id, EntryUpsertActions) = i
                THEN   <<events[i]>>
                     \o FilterByIdx(events, RetainedEvidenceIdxs(events,
                                                                 events[i].id), 1)
                     \o EmitFrom(events, live, i + 1)
                ELSE EmitFrom(events, live, i + 1)

CompactedLogE(events) == EmitFrom(events, LiveIdsIn(events), 1)

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
    LET nextState == ApplyEventE(DbState, ev)
    IN /\ log'       = Append(log, ev)
       /\ entries'   = nextState[1]
       /\ evidence'  = nextState[2]
       /\ estatus'   = nextState[3]
       /\ is_legacy' = nextState[4]

\* The generated upsert always claims "present" — the adversarial payload, and
\* the one add_validation.rs writes for an entry it believes has evidence.
\* Generating the other payload values would only multiply the state space:
\* AddIgnoresClaimedStatus proves at every reachable state that no arm reads
\* the field, which is the whole content of ADR-1's payload clause.
DoAdd ==
    \E id \in EntryIds, k \in ModelKinds :
        WriteThrough(AddEvent(id, k, "present"))

\* Writer-producible alphabet (CE4).  A `legacy_add` models an upsert event
\* written by pre-Phase-0 code, and the only production writer of entry upserts
\* — `kb_core::add` (src/components/kb_core.rs:244-258) — has emitted `kind`
\* unconditionally since Phase 0.  A kindless upsert can therefore only come
\* from a log segment that predates every explicit-kind upsert, so no real
\* events.jsonl can contain a `legacy_add` for an id that already carries an
\* `add`.  Compaction cannot manufacture the shape either: it only ever selects
\* original events, and a kindless LAST upsert implies every upsert for that
\* entry is kindless.
\*
\* The guard restricts the event alphabet to logs a writer can actually produce.
\* It does NOT weaken CompactionEquivalenceE, and CE3 — whose trace is
\* legacy-only — remains reachable, so the regression value is retained.
DoLegacyAdd ==
    \E id \in EntryIds :
        /\ ~\E i \in 1..Len(log) : log[i].action = "add" /\ log[i].id = id
        /\ WriteThrough(LegacyAddEvent(id))

DoEvidenceAdd ==
    \E id \in EntryIds, eid \in EvidenceIds, ek \in EvidenceKinds :
        WriteThrough(EvidenceAddEvent(
            id, [eid |-> eid, kind |-> ek, path |-> InitialPath]))

DoEvidenceExpire ==
    \E id \in EntryIds, eid \in EvidenceIds :
        WriteThrough(EvidenceExpireEvent(id, eid))

DoCitationHealed ==
    \E id \in EntryIds, eid \in EvidenceIds :
        WriteThrough(CitationHealedEvent(id, eid, HealedPath))

DoExpire ==
    \E id \in EntryIds : WriteThrough(ExpireEvent(id))

\* Compact: atomically replace the log with its compacted form.  The
\* materialized state is NOT recomputed — compaction rewrites events.jsonl and
\* leaves the live database alone (compact.rs:180-190).  Whether the rewritten
\* log still replays to that database is exactly CompactionEquivalenceE.
DoCompact ==
    /\ log' = CompactedLogE(log)
    /\ UNCHANGED <<entries, evidence, estatus, is_legacy>>

Next ==
    \/ DoAdd
    \/ DoLegacyAdd
    \/ DoEvidenceAdd
    \/ DoEvidenceExpire
    \/ DoCitationHealed
    \/ DoExpire
    \/ DoCompact

Spec == Init /\ [][Next]_vars

(* ──────────────────────────── Counterexample harnesses ─────────────────── *)
(*
   Two restricted action sets, each isolating one compaction defect so the
   counterexample TLC reports is shape-specific rather than "some trace".  Run
   them with EntryIds = {"e1"} and EvidenceIds = {"v1"} (see the CE cfgs): the
   restriction plus the singleton constants makes the shortest violating trace
   unique, so the reported trace is reproducible rather than search-order
   dependent.
*)

\* CE1 — the upsert-reordering defect.  Only add / evidence_add / compact, and
\* only the evidence-requiring kind.  Shortest violation is necessarily
\* add(e1) · evidence_add(e1,v1) · add(e1) · compact.
DoAddBelief ==
    \E id \in EntryIds : WriteThrough(AddEvent(id, "belief", "present"))

NextCE1 == DoAddBelief \/ DoEvidenceAdd \/ DoCompact
SpecCE1 == Init /\ [][NextCE1]_vars

\* CE2 — the dropped evidence_expire defect.  Re-upsert is forbidden (add only
\* fires on an absent entry), which removes CE1's shape from the search space,
\* leaving the resurrection of an expired evidence row as the only violation.
DoAddFreshBelief ==
    \E id \in EntryIds :
        /\ entries[id].type = "absent"
        /\ WriteThrough(AddEvent(id, "belief", "present"))

NextCE2 == DoAddFreshBelief \/ DoEvidenceAdd \/ DoEvidenceExpire \/ DoCompact
SpecCE2 == Init /\ [][NextCE2]_vars

\* Regression harness — every action except compaction.  Used to prove the
\* ADR-1/ADR-2 rewrite is internally consistent independently of the known
\* compaction defect.
NextNoCompact ==
    \/ DoAdd
    \/ DoLegacyAdd
    \/ DoEvidenceAdd
    \/ DoEvidenceExpire
    \/ DoCitationHealed
    \/ DoExpire

SpecNoCompact == Init /\ [][NextNoCompact]_vars

(* ──────────────────────────── Safety invariants ────────────────────────── *)

\* OrphanTolerated: any present entry was created by some prior "add" or
\* "legacy_add" event in the log.
OrphanTolerated ==
    \A id \in EntryIds :
        entries[id].type = "present" =>
            \E i \in 1..Len(log) :
                /\ log[i].action \in EntryUpsertActions
                /\ log[i].id = id

\* StatusConsistent (ADR-1): evidence_status is a function of (kind, evidence),
\* never of an event payload.  The single exception is the AC2 grandfather, and
\* is_legacy is cleared by the first evidence op precisely so that the exception
\* cannot outlive the condition that justified it.
StatusConsistent ==
    \A id \in EntryIds :
        entries[id].type = "present" =>
            IF is_legacy[id]
                THEN estatus[id] = "n_a"
                ELSE estatus[id] = StatusOf(entries[id].kind, evidence[id])

\* AddIgnoresClaimedStatus (ADR-1): the upsert payload's evidence_status field
\* is not authoritative.  Applying an add to the current state yields the same
\* state whatever the payload claims.
AddIgnoresClaimedStatus ==
    \A id \in EntryIds, k \in ModelKinds, s \in EvidenceStatuses :
        ApplyEventE(DbState, AddEvent(id, k, s))
            = ApplyEventE(DbState, AddEvent(id, k, "present"))

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

\* AbsentEntriesClean (ADR-2): expire GCs the entry's evidence, so an absent
\* entry is indistinguishable from one that never existed.  This is what makes
\* compaction's deliberate drop of dead entries sound.
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
    /\ OrphanTolerated
    /\ StatusConsistent
    /\ AddIgnoresClaimedStatus
    /\ OrphanAddIsSoftMandate
    /\ AbsentEntriesClean
    /\ EvidenceKindRestricted

THEOREM SpecNoCompact => []Invariants

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

(* ──────────────────────── Live-state equivalence (ADR-2) ───────────────── *)

\* Two materialized states agree on every entry that is live in either of them.
\* Entries absent from both are unconstrained — that is the ADR-2 weakening
\* which makes compaction's deliberate drop of dead entries admissible.
LiveStateEq(s, t) ==
    \A id \in EntryIds :
        (s[1][id].type = "present" \/ t[1][id].type = "present") =>
            /\ s[1][id] = t[1][id]
            /\ s[2][id] = t[2][id]
            /\ s[3][id] = t[3][id]
            /\ s[4][id] = t[4][id]

\* CompactionEquivalenceE (ADR-2 live-state form): replaying the event log
\* still yields the live database.
\*
\* Stated against the DB rather than as MaterializeE(CompactedLogE(log)) =
\* MaterializeE(log) on purpose.  WriteThrough keeps the two in lockstep, so
\* this predicate can only break at a state where `compact` has rewritten the
\* log — which makes any counterexample TLC reports contain the compact step
\* that caused it.  The log-to-log form is violated one state EARLIER, before
\* compaction has run, and its trace therefore says nothing about compaction.
\*
\* This subsumes the former EvidenceMaterialization invariant: AbsentEntriesClean
\* makes absent entries indistinguishable from never-created ones inside this
\* abstraction, so live-state and full-state equality coincide here (see the
\* ADR-2 modelling note in the header).
CompactionEquivalenceE == LiveStateEq(MaterializeE(log), DbState)

=============================================================================
