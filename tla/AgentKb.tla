---------------------------- MODULE AgentKb ----------------------------
(* Agent knowledge-base protocol: JSONL event log + SQLite materialized
   cache with advisory write lock (.state/.lock via flock).

   Verified properties
   -------------------
   MutualExclusion         At most one process holds the write lock.
   WriteThroughInvariant   When lock is free, db = Materialize(log).
   CompactionEquivalence   Materialize(CompactedLog(L)) = Materialize(L).

   Design notes
   ------------
   All values use tagged unions (records with a "type" or "action" field)
   to avoid TLC errors when comparing records with strings.

     DB values:  [type |-> "absent"]
               | [type |-> "present", data : DataVals, stale : BOOLEAN]

     Pending:   [action |-> "none"]
               | [action |-> "upsert", id : EntryIds, data : DataVals]
               | [action |-> "expire", id : EntryIds]

   Process model
   -------------
   N concurrent processes (agents / CLI invocations).

   Write protocol (4 steps):
     (1) acquire exclusive flock on .state/.lock
     (2) append event to JSONL log
     (3) materialize event into SQLite DB
     (4) release lock

   Rebuild protocol (3 steps):
     (1) acquire lock
     (2) drop DB (set to empty)
     (3) replay full log → fresh DB, then release lock

   Compact protocol (2 steps under lock):
     (1) acquire lock
     (2) replace log with squashed equivalent, then release lock

   TLC model (small instance):
     Procs    <- {"p1", "p2"}
     EntryIds <- {"e1", "e2"}
     DataVals <- {"d1"}

   Run: tlc AgentKb -config AgentKb.cfg -workers 4 -deadlock
*)

EXTENDS Sequences, FiniteSets, Naturals, TLC

CONSTANTS
    Procs,      \* set of process IDs, e.g. {"p1","p2"}
    EntryIds,   \* set of possible entry IDs, e.g. {"e1","e2"}
    DataVals    \* set of possible data values, e.g. {"d1"}

ASSUME Procs    # {}
ASSUME EntryIds # {}
ASSUME DataVals # {}

(* ──────────────────────────── Tagged-union values ──────────────────────── *)

\* DB sentinel: entry is absent.
AbsentEntry == [type |-> "absent"]

\* DB present entry.
PresentEntry(d, s) == [type |-> "present", data |-> d, stale |-> s]

\* Pending-event sentinel: no event staged.
NoEvent == [action |-> "none"]

\* Event constructors (match the JSONL event schema).
UpsertEvent(id, d) == [action |-> "upsert", id |-> id, data |-> d]
ExpireEvent(id)    == [action |-> "expire", id |-> id]

\* Domains (for type invariant and TLC constraint).
EventActions  == {"upsert", "expire"}
PendingActions == {"none", "upsert", "expire"}
DBTypes       == {"absent", "present"}

(* ──────────────────────────── State variables ──────────────────────────── *)

VARIABLES
    log,         \* Seq(Event)  — the append-only JSONL event log
    db,          \* [EntryIds -> AbsentEntry | PresentEntry(...)]
    lock_holder, \* Procs \cup {"none"}
    pending,     \* [Procs -> NoEvent | UpsertEvent | ExpireEvent]
    pc           \* [Procs -> ProcStep]

vars == <<log, db, lock_holder, pending, pc>>

ProcSteps == {
    "idle",
    "write_acquiring",    "write_appending",
    "write_materializing","write_releasing",
    "rebuild_acquiring",  "rebuild_dropping",  "rebuild_replaying",
    "compact_acquiring",  "compact_running"
}

(* ──────────────────────────── Type invariant ───────────────────────────── *)

\* All comparisons are string-vs-string: no TLC tagged-value errors.
TypeInvariant ==
    /\ lock_holder \in Procs \cup {"none"}
    /\ \A p \in Procs : pc[p] \in ProcSteps
    /\ \A p \in Procs : pending[p].action \in PendingActions
    /\ \A id \in EntryIds : db[id].type \in DBTypes

(* ──────────────────────────── Materialization ──────────────────────────── *)

EmptyDB == [id \in EntryIds |-> AbsentEntry]

ApplyEvent(state, event) ==
    CASE event.action = "upsert" ->
            [state EXCEPT ![event.id] = PresentEntry(event.data, FALSE)]
      [] event.action = "expire" ->
            IF state[event.id].type = "absent"
            THEN state
            ELSE [state EXCEPT ![event.id].stale = TRUE]
      [] OTHER -> state

RECURSIVE MatHelper(_, _)
MatHelper(events, i) ==
    IF i = 0
    THEN EmptyDB
    ELSE ApplyEvent(MatHelper(events, i - 1), events[i])

Materialize(events) == MatHelper(events, Len(events))

(* CompactedLog: minimal equivalent log — one upsert per active entry,
   one upsert+expire pair per stale entry.  Materialize of the result
   equals Materialize of the original (proved as CompactionEquivalence).

   All db lookups use .type to distinguish absent from present, avoiding
   any record/string comparison errors in TLC.
*)
RECURSIVE SetToSeqAux(_, _)
SetToSeqAux(S, acc) ==
    IF S = {}
    THEN acc
    ELSE LET x == CHOOSE e \in S : TRUE
         IN SetToSeqAux(S \ {x}, Append(acc, x))

SetToSeq(S) == SetToSeqAux(S, <<>>)

CompactedLog(events) ==
    LET finalDB   == Materialize(events)
        presentIds == {id \in EntryIds : finalDB[id].type = "present"}
        staleIds   == {id \in presentIds : finalDB[id].stale = TRUE}
        activeIds  == presentIds \ staleIds
        activeEvs  == {UpsertEvent(id, finalDB[id].data) : id \in activeIds}
        staleUps   == {UpsertEvent(id, finalDB[id].data) : id \in staleIds}
        staleExps  == {ExpireEvent(id)                   : id \in staleIds}
    IN SetToSeq(activeEvs) \o SetToSeq(staleUps) \o SetToSeq(staleExps)

(* ──────────────────────────── Initial state ────────────────────────────── *)

Init ==
    /\ log         = <<>>
    /\ db          = EmptyDB
    /\ lock_holder = "none"
    /\ pending     = [p \in Procs |-> NoEvent]
    /\ pc          = [p \in Procs |-> "idle"]

(* ──────────────────────────── Write protocol ───────────────────────────── *)

StartWrite(p, event) ==
    /\ pc[p] = "idle"
    /\ pc'      = [pc      EXCEPT ![p] = "write_acquiring"]
    /\ pending' = [pending EXCEPT ![p] = event]
    /\ UNCHANGED <<log, db, lock_holder>>

WriteAcquire(p) ==
    /\ pc[p]       = "write_acquiring"
    /\ lock_holder = "none"
    /\ lock_holder' = p
    /\ pc'          = [pc EXCEPT ![p] = "write_appending"]
    /\ UNCHANGED <<log, db, pending>>

WriteAppend(p) ==
    /\ pc[p]       = "write_appending"
    /\ lock_holder = p
    /\ log' = Append(log, pending[p])
    /\ pc'  = [pc EXCEPT ![p] = "write_materializing"]
    /\ UNCHANGED <<db, lock_holder, pending>>

WriteMaterialize(p) ==
    /\ pc[p]       = "write_materializing"
    /\ lock_holder = p
    /\ db' = ApplyEvent(db, log[Len(log)])
    /\ pc' = [pc EXCEPT ![p] = "write_releasing"]
    /\ UNCHANGED <<log, lock_holder, pending>>

WriteRelease(p) ==
    /\ pc[p]       = "write_releasing"
    /\ lock_holder = p
    /\ lock_holder' = "none"
    /\ pc'          = [pc EXCEPT ![p] = "idle"]
    /\ pending'     = [pending EXCEPT ![p] = NoEvent]
    /\ UNCHANGED <<log, db>>

(* ──────────────────────────── Rebuild protocol ─────────────────────────── *)

StartRebuild(p) ==
    /\ pc[p] = "idle"
    /\ pc'   = [pc EXCEPT ![p] = "rebuild_acquiring"]
    /\ UNCHANGED <<log, db, lock_holder, pending>>

RebuildAcquire(p) ==
    /\ pc[p]       = "rebuild_acquiring"
    /\ lock_holder = "none"
    /\ lock_holder' = p
    /\ pc'          = [pc EXCEPT ![p] = "rebuild_dropping"]
    /\ UNCHANGED <<log, db, pending>>

RebuildDrop(p) ==
    /\ pc[p]       = "rebuild_dropping"
    /\ lock_holder = p
    /\ db' = EmptyDB
    /\ pc' = [pc EXCEPT ![p] = "rebuild_replaying"]
    /\ UNCHANGED <<log, lock_holder, pending>>

RebuildReplay(p) ==
    /\ pc[p]       = "rebuild_replaying"
    /\ lock_holder = p
    /\ db'          = Materialize(log)
    /\ lock_holder' = "none"
    /\ pc'          = [pc EXCEPT ![p] = "idle"]
    /\ UNCHANGED <<log, pending>>

(* ──────────────────────────── Compact protocol ─────────────────────────── *)

StartCompact(p) ==
    /\ pc[p] = "idle"
    /\ pc'   = [pc EXCEPT ![p] = "compact_acquiring"]
    /\ UNCHANGED <<log, db, lock_holder, pending>>

CompactAcquire(p) ==
    /\ pc[p]       = "compact_acquiring"
    /\ lock_holder = "none"
    /\ lock_holder' = p
    /\ pc'          = [pc EXCEPT ![p] = "compact_running"]
    /\ UNCHANGED <<log, db, pending>>

CompactRun(p) ==
    /\ pc[p]       = "compact_running"
    /\ lock_holder = p
    /\ log'         = CompactedLog(log)
    /\ lock_holder' = "none"
    /\ pc'          = [pc EXCEPT ![p] = "idle"]
    /\ UNCHANGED <<db, pending>>

(* ──────────────────────────── Next / Spec ──────────────────────────────── *)

Next ==
    \E p \in Procs :
        \/ WriteAcquire(p)
        \/ WriteAppend(p)
        \/ WriteMaterialize(p)
        \/ WriteRelease(p)
        \/ RebuildAcquire(p)
        \/ RebuildDrop(p)
        \/ RebuildReplay(p)
        \/ CompactAcquire(p)
        \/ CompactRun(p)
        \/ StartRebuild(p)
        \/ StartCompact(p)
        \/ \E id \in EntryIds, d \in DataVals : StartWrite(p, UpsertEvent(id, d))
        \/ \E id \in EntryIds               : StartWrite(p, ExpireEvent(id))

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

(* ──────────────────────────── Safety invariants ────────────────────────── *)

MutualExclusion ==
    \A p1, p2 \in Procs :
        (lock_holder = p1 /\ lock_holder = p2) => p1 = p2

\* When the lock is free, the DB exactly mirrors the JSONL log.
WriteThroughInvariant ==
    lock_holder = "none" => db = Materialize(log)

\* Compaction produces a log with the same materialized state.
CompactionEquivalence ==
    Materialize(CompactedLog(log)) = Materialize(log)

Invariants ==
    /\ TypeInvariant
    /\ MutualExclusion
    /\ WriteThroughInvariant
    /\ CompactionEquivalence

THEOREM Spec => []Invariants

(* ──────────────────────────── Liveness ─────────────────────────────────── *)

WriteEventuallyCompletes ==
    \A p \in Procs :
        pc[p] = "write_acquiring" ~> pc[p] = "idle"

RebuildEventuallyCompletes ==
    \A p \in Procs :
        pc[p] = "rebuild_acquiring" ~> pc[p] = "idle"

=============================================================================
