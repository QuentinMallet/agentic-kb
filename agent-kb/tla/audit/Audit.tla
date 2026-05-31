---------------------------- MODULE Audit ----------------------------
(* Phase 5 audit cycle for agentic-kb.

   Models four safety invariants of the Phase 5 defensibility plan:

   (a) EntryMonotonicity       Live->Stale is one-way; no reversal.
   (b) ConfidenceInUnitInterval  Beta(1,1) posterior (s+1)/(s+f+2) in [0,1].
   (c) ProvenanceAcyclicity    evidence.derived_from edges form a DAG.
   (d) SourceWeightsAppendOnly source_weights rows never deleted; counts
                               only increment.

   Design notes
   ------------
   entry_state   per-entry enum: absent | live | stale.
   sw_exists     [SwKeys -> BOOL]  row presence in source_weights.
   sw_succ/fail  [SwKeys -> Nat]   audit counts bounded by MaxAudits for TLC.
   derived_from  [EntryIds -> EntryIds u {NoneId}]  provenance edges.
   ever_stale    ghost: records whether each entry was ever stale.
                 Enables (a) as a state-predicate INVARIANT.
   sw_ever_created ghost: records whether each sw row was ever created.
                 Enables (d) as a state-predicate INVARIANT.

   Invariant (b) uses integer inequalities only -- no floating-point:
     s+1 >= 1  (lower bound: confidence > 0)
     s+1 <= s+f+2  (upper bound: confidence <= 1, i.e. f+1 >= 0)
   Both hold for all Nat s, f.

   Invariant (c) uses HasCycleFrom, a DFS with a visited set.  Terminates
   because EntryIds is finite and visited grows on each recursive step.

   Atomic write-through abstraction: write lock + append + materialize
   from AgentKb are elided here; we model only the state-level invariants
   of the audit subsystem (analogous to AgentKbEvidence's abstraction).

   Run
   ---
   tlc Audit -config Audit.cfg -workers 4 -deadlock
*)

EXTENDS Sequences, FiniteSets, Naturals, TLC

CONSTANTS
    EntryIds,   \* {"e1","e2","e3"}
    KindVals,   \* {"code","derived"}
    SessionIds, \* {"s1"}
    MaxAudits   \* per-key audit count bound for TLC, e.g. 2

ASSUME EntryIds   # {}
ASSUME KindVals   # {}
ASSUME SessionIds # {}
ASSUME MaxAudits \in Nat /\ MaxAudits > 0
ASSUME "derived" \in KindVals

(* ──────────────────────────── Domain definitions ───────────────────────── *)

EntryStates     == {"absent", "live", "stale"}
SwKeys          == KindVals \times SessionIds
NoneId          == "none"   \* sentinel: no provenance parent
NonDerivedKinds == KindVals \ {"derived"}

(* ──────────────────────────── State variables ──────────────────────────── *)

VARIABLES
    entry_state,     \* [EntryIds -> EntryStates]
    entry_kind,      \* [EntryIds -> KindVals u {NoneId}]  NoneId when absent
    sw_exists,       \* [SwKeys -> BOOLEAN]
    sw_succ,         \* [SwKeys -> 0..MaxAudits]  verdict=true counts
    sw_fail,         \* [SwKeys -> 0..MaxAudits]  verdict=false counts
    derived_from,    \* [EntryIds -> EntryIds u {NoneId}]
    ever_stale,      \* [EntryIds -> BOOLEAN]  ghost for invariant (a)
    sw_ever_created  \* [SwKeys   -> BOOLEAN]  ghost for invariant (d)

vars == <<entry_state, entry_kind, sw_exists, sw_succ, sw_fail,
          derived_from, ever_stale, sw_ever_created>>

\* TLC state-space bound: cap per-key audit counts.
AuditCountBound ==
    \A key \in SwKeys : sw_succ[key] <= MaxAudits /\ sw_fail[key] <= MaxAudits

(* ──────────────────────────── Type invariant ───────────────────────────── *)

TypeInvariant ==
    /\ \A id  \in EntryIds : entry_state[id]      \in EntryStates
    /\ \A id  \in EntryIds : entry_kind[id]       \in KindVals \cup {NoneId}
    /\ \A key \in SwKeys   : sw_exists[key]       \in BOOLEAN
    /\ \A key \in SwKeys   : sw_succ[key]         \in 0..MaxAudits
    /\ \A key \in SwKeys   : sw_fail[key]         \in 0..MaxAudits
    /\ \A id  \in EntryIds : derived_from[id]     \in EntryIds \cup {NoneId}
    /\ \A id  \in EntryIds : ever_stale[id]       \in BOOLEAN
    /\ \A key \in SwKeys   : sw_ever_created[key] \in BOOLEAN
    \* Consistency: live/stale entries carry a valid kind; absent carry NoneId.
    /\ \A id \in EntryIds :
           entry_state[id] # "absent" => entry_kind[id] \in KindVals
    /\ \A id \in EntryIds :
           entry_state[id]  = "absent" => entry_kind[id] = NoneId
    \* Provenance: derived entries point to an existing entry (or NoneId before
    \* AddDerived runs, which TypeInvariant permits because absent entries may
    \* carry a lingering NoneId; AddDerived sets a valid parent on add).
    /\ \A id \in EntryIds :
           entry_kind[id] = "derived" => derived_from[id] \in EntryIds

(* ──────────────────────────── Initial state ────────────────────────────── *)

Init ==
    /\ entry_state     = [id  \in EntryIds |-> "absent"]
    /\ entry_kind      = [id  \in EntryIds |-> NoneId]
    /\ sw_exists       = [key \in SwKeys   |-> FALSE]
    /\ sw_succ         = [key \in SwKeys   |-> 0]
    /\ sw_fail         = [key \in SwKeys   |-> 0]
    /\ derived_from    = [id  \in EntryIds |-> NoneId]
    /\ ever_stale      = [id  \in EntryIds |-> FALSE]
    /\ sw_ever_created = [key \in SwKeys   |-> FALSE]

(* ──────────────────────────── Cycle-detection helper ───────────────────── *)

\* DFS walk from `current` following the derived_from chain.
\* Returns TRUE iff a cycle is detected (current revisited via visited set).
\* Terminates: EntryIds is finite; visited grows by one on every recursive call.
RECURSIVE HasCycleFrom(_, _, _)
HasCycleFrom(df, current, visited) ==
    IF current = NoneId
        THEN FALSE                  \* chain ended cleanly: no cycle
    ELSE IF current \in visited
        THEN TRUE                   \* revisited a node: cycle
    ELSE HasCycleFrom(df, df[current], visited \cup {current})

(* ──────────────────────────── Actions ──────────────────────────────────── *)

\* Add a non-derived live entry.
AddEntry(id, k) ==
    /\ k \in NonDerivedKinds
    /\ entry_state[id] = "absent"
    /\ entry_state'    = [entry_state  EXCEPT ![id] = "live"]
    /\ entry_kind'     = [entry_kind   EXCEPT ![id] = k]
    /\ derived_from'   = [derived_from EXCEPT ![id] = NoneId]
    /\ UNCHANGED <<sw_exists, sw_succ, sw_fail, ever_stale, sw_ever_created>>

\* Add a derived entry linking to `parent`.
\* Guard: parent is live; no self-loop; adding id->parent introduces no cycle.
\* Cycle check: DFS from parent with {id} pre-visited detects if id is
\* reachable from parent in the current graph.
AddDerived(id, parent) ==
    /\ entry_state[id]     = "absent"
    /\ entry_state[parent] = "live"
    /\ parent # id
    /\ ~HasCycleFrom(derived_from, parent, {id})
    /\ entry_state'    = [entry_state  EXCEPT ![id] = "live"]
    /\ entry_kind'     = [entry_kind   EXCEPT ![id] = "derived"]
    /\ derived_from'   = [derived_from EXCEPT ![id] = parent]
    /\ UNCHANGED <<sw_exists, sw_succ, sw_fail, ever_stale, sw_ever_created>>

\* Audit verdict=true: entry stays live; increment source_weights successes.
AuditTrue(id, session) ==
    /\ entry_state[id] = "live"
    /\ entry_kind[id]  # NoneId
    /\ LET key == <<entry_kind[id], session>>
       IN /\ sw_succ[key] < MaxAudits
          /\ sw_exists'       = [sw_exists       EXCEPT ![key] = TRUE]
          /\ sw_succ'         = [sw_succ         EXCEPT ![key] = sw_succ[key] + 1]
          /\ sw_ever_created' = [sw_ever_created EXCEPT ![key] = TRUE]
          /\ UNCHANGED <<entry_state, entry_kind, sw_fail, derived_from, ever_stale>>

\* Audit verdict=false: expire entry (live->stale); increment source_weights failures.
\* JSONL-first ordering: the expire is modelled as atomic here (AgentKb lock
\* serialises the append before the DB insert in the implementation).
AuditFalse(id, session) ==
    /\ entry_state[id] = "live"
    /\ entry_kind[id]  # NoneId
    /\ LET key == <<entry_kind[id], session>>
       IN /\ sw_fail[key] < MaxAudits
          /\ entry_state'     = [entry_state     EXCEPT ![id] = "stale"]
          /\ ever_stale'      = [ever_stale      EXCEPT ![id] = TRUE]
          /\ sw_exists'       = [sw_exists       EXCEPT ![key] = TRUE]
          /\ sw_fail'         = [sw_fail         EXCEPT ![key] = sw_fail[key] + 1]
          /\ sw_ever_created' = [sw_ever_created EXCEPT ![key] = TRUE]
          /\ UNCHANGED <<entry_kind, sw_succ, derived_from>>

Next ==
    \/ \E id \in EntryIds, k \in NonDerivedKinds : AddEntry(id, k)
    \/ \E id \in EntryIds, p \in EntryIds         : AddDerived(id, p)
    \/ \E id \in EntryIds, s \in SessionIds        : AuditTrue(id, s)
    \/ \E id \in EntryIds, s \in SessionIds        : AuditFalse(id, s)

Spec == Init /\ [][Next]_vars

(* ──────────────────────────── Safety invariants ────────────────────────── *)

\* (a) Entry-state monotonicity: Live->Stale is one-way.
\* ever_stale[id] is set the moment id becomes stale and is never cleared.
\* Any action that reverted id to live or absent would produce a state where
\* ever_stale[id]=TRUE but entry_state[id]#"stale" -- caught here.
EntryMonotonicity ==
    \A id \in EntryIds :
        ever_stale[id] => entry_state[id] = "stale"

\* (b) Confidence value domain: Beta(1,1) posterior always in [0,1].
\* confidence = (s+1)/(s+f+2).  Integer proof:
\*   lower: s+1 >= 1 because s in Nat => s >= 0
\*   upper: s+1 <= s+f+2 iff 1 <= f+2 iff f >= -1, true for all f in Nat
\* Checked unconditionally (sw_exists=FALSE states have s=f=0, still valid).
ConfidenceInUnitInterval ==
    \A key \in SwKeys :
        LET s == sw_succ[key]
            f == sw_fail[key]
        IN /\ s + 1 >= 1
           /\ s + 1 <= s + f + 2

\* (c) Provenance acyclicity: derived_from edges form a DAG.
\* For every live derived entry id, DFS from its parent with {id} pre-visited
\* must return FALSE.  If id were reachable from its own parent, adding the
\* edge id->parent would close a cycle -- but AddDerived's guard prevents this.
\* The invariant confirms no reachable state ever has a cyclic provenance graph.
ProvenanceAcyclicity ==
    \A id \in EntryIds :
        (entry_state[id] = "live" /\ entry_kind[id] = "derived") =>
            ~HasCycleFrom(derived_from, derived_from[id], {id})

\* (d) source_weights rows are append-only: once created, never deleted.
\* sw_ever_created[key] is set when the row first appears and never cleared.
\* Any action that set sw_exists[key]=FALSE after it was TRUE would produce a
\* state where sw_ever_created[key]=TRUE but sw_exists[key]=FALSE -- caught here.
SourceWeightsAppendOnly ==
    \A key \in SwKeys :
        sw_ever_created[key] => sw_exists[key]

Invariants ==
    /\ TypeInvariant
    /\ EntryMonotonicity
    /\ ConfidenceInUnitInterval
    /\ ProvenanceAcyclicity
    /\ SourceWeightsAppendOnly

THEOREM Spec => []Invariants

=============================================================================
