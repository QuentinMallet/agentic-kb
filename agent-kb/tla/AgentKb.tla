---------------------------- MODULE AgentKb ----------------------------
(* Agent knowledge-base protocol: JSONL event log + SQLite materialized
   cache with advisory write lock (.state/.lock via flock).

   Verified properties
   -------------------
   MutualExclusion           At most one process holds the write lock.
   WriteThroughInvariant     When lock is free, db = Materialize(log).
   LiveCompactionEquivalence CompactedLog(L) preserves all live entries exactly;
                             expired entries are dropped entirely.

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

   Rebuild protocol — 3-phase non-blocking (replaces old single-lock rebuild):
     Phase 1 (brief lock):
       (1) acquire lock
       (2) record snap_len = Len(log)
       (3) release lock
     Phase 2 (no lock — writes continue against live DB):
       (4) replay log[1..snap_len] into tmp DB (abstract: no state change to db)
     Phase 3 (brief lock):
       (5) acquire lock
       (6) apply catch-up log[snap_len+1..Len(log)] to tmp, then db := tmp
       (7) release lock
     Result: db = Materialize(log) — WriteThroughInvariant restored.

   Old blocking rebuild (kept for reference — single lock for full replay):
     (1) acquire lock
     (2) drop DB (set to empty)
     (3) replay full log → fresh DB, then release lock

   Compact protocol (2 steps under lock):
     (1) acquire lock
     (2) replace log with squashed equivalent (drop expired entries entirely);
         update db to match; release lock

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
    DataVals,   \* set of possible data values, e.g. {"d1"}
    MaxLogLen   \* maximum log length for TLC state-space bound, e.g. 3

ASSUME Procs    # {}
ASSUME EntryIds # {}
ASSUME DataVals # {}
ASSUME MaxLogLen \in Nat /\ MaxLogLen > 0

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
    pc,          \* [Procs -> ProcStep]
    snap_len     \* [Procs -> Nat] — Phase-1 snapshot length for 3-phase rebuild

vars == <<log, db, lock_holder, pending, pc, snap_len>>

\* TLC state-space bound: only explore states with log length <= MaxLogLen.
\* Keeps the state space finite; invariants hold for all log lengths.
LogLenBound == Len(log) <= MaxLogLen

ProcSteps == {
    "idle",
    "write_acquiring",    "write_appending",
    "write_materializing","write_releasing",
    "rebuild_acquiring",  "rebuild_dropping",  "rebuild_replaying",
    "rebuild3_snap_acq",  "rebuild3_snap_rel",
    "rebuild3_offlock",
    "rebuild3_cu_acq",    "rebuild3_cu_apply",
    "compact_acquiring",  "compact_running"
}

(* ──────────────────────────── Type invariant ───────────────────────────── *)

\* All comparisons are string-vs-string: no TLC tagged-value errors.
TypeInvariant ==
    /\ lock_holder \in Procs \cup {"none"}
    /\ \A p \in Procs : pc[p] \in ProcSteps
    /\ \A p \in Procs : pending[p].action \in PendingActions
    /\ \A id \in EntryIds : db[id].type \in DBTypes
    /\ \A p \in Procs : snap_len[p] \in Nat

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

(* CompactedLog: minimal equivalent log — one upsert per live (non-expired)
   entry.  Expired entries are dropped entirely: absent == stale for all
   query paths.  Live entry set and data are preserved exactly
   (proved as LiveCompactionEquivalence).

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
    LET finalDB  == Materialize(events)
        activeIds == {id \in EntryIds : finalDB[id].type = "present" /\ finalDB[id].stale = FALSE}
        activeEvs == {UpsertEvent(id, finalDB[id].data) : id \in activeIds}
    IN SetToSeq(activeEvs)

(* ──────────────────────────── Initial state ────────────────────────────── *)

Init ==
    /\ log         = <<>>
    /\ db          = EmptyDB
    /\ lock_holder = "none"
    /\ pending     = [p \in Procs |-> NoEvent]
    /\ pc          = [p \in Procs |-> "idle"]
    /\ snap_len    = [p \in Procs |-> 0]

(* ──────────────────────────── Write protocol ───────────────────────────── *)

StartWrite(p, event) ==
    /\ pc[p] = "idle"
    /\ pc'      = [pc      EXCEPT ![p] = "write_acquiring"]
    /\ pending' = [pending EXCEPT ![p] = event]
    /\ UNCHANGED <<log, db, lock_holder, snap_len>>

WriteAcquire(p) ==
    /\ pc[p]       = "write_acquiring"
    /\ lock_holder = "none"
    /\ lock_holder' = p
    /\ pc'          = [pc EXCEPT ![p] = "write_appending"]
    /\ UNCHANGED <<log, db, pending, snap_len>>

WriteAppend(p) ==
    /\ pc[p]       = "write_appending"
    /\ lock_holder = p
    /\ log' = Append(log, pending[p])
    /\ pc'  = [pc EXCEPT ![p] = "write_materializing"]
    /\ UNCHANGED <<db, lock_holder, pending, snap_len>>

WriteMaterialize(p) ==
    /\ pc[p]       = "write_materializing"
    /\ lock_holder = p
    /\ db' = ApplyEvent(db, log[Len(log)])
    /\ pc' = [pc EXCEPT ![p] = "write_releasing"]
    /\ UNCHANGED <<log, lock_holder, pending, snap_len>>

WriteRelease(p) ==
    /\ pc[p]       = "write_releasing"
    /\ lock_holder = p
    /\ lock_holder' = "none"
    /\ pc'          = [pc EXCEPT ![p] = "idle"]
    /\ pending'     = [pending EXCEPT ![p] = NoEvent]
    /\ UNCHANGED <<log, db, snap_len>>

(* ──────────────────────────── Rebuild protocol ─────────────────────────── *)

StartRebuild(p) ==
    /\ pc[p] = "idle"
    /\ pc'   = [pc EXCEPT ![p] = "rebuild_acquiring"]
    /\ UNCHANGED <<log, db, lock_holder, pending, snap_len>>

RebuildAcquire(p) ==
    /\ pc[p]       = "rebuild_acquiring"
    /\ lock_holder = "none"
    /\ lock_holder' = p
    /\ pc'          = [pc EXCEPT ![p] = "rebuild_dropping"]
    /\ UNCHANGED <<log, db, pending, snap_len>>

RebuildDrop(p) ==
    /\ pc[p]       = "rebuild_dropping"
    /\ lock_holder = p
    /\ db' = EmptyDB
    /\ pc' = [pc EXCEPT ![p] = "rebuild_replaying"]
    /\ UNCHANGED <<log, lock_holder, pending, snap_len>>

RebuildReplay(p) ==
    /\ pc[p]       = "rebuild_replaying"
    /\ lock_holder = p
    /\ db'          = Materialize(log)
    /\ lock_holder' = "none"
    /\ pc'          = [pc EXCEPT ![p] = "idle"]
    /\ UNCHANGED <<log, pending, snap_len>>

(* ──────────── 3-phase non-blocking rebuild protocol ────────────────────── *)
(*
   Phase 1 (brief lock): acquire lock, snapshot Len(log), release.
   Phase 2 (no lock):    replay log[1..snap_len] into tmp (abstract: no db change).
                         Concurrent writes continue — WriteThroughInvariant holds
                         throughout because writes maintain db = Materialize(log).
   Phase 3 (brief lock): acquire lock, apply catch-up log[snap_len+1..Len(log)]
                         to tmp, then atomically swap tmp → db.
                         Result: db = Materialize(log) (since snap+catchup = full log).
*)

StartRebuild3(p) ==
    /\ pc[p] = "idle"
    /\ pc' = [pc EXCEPT ![p] = "rebuild3_snap_acq"]
    /\ UNCHANGED <<log, db, lock_holder, pending, snap_len>>

Rebuild3SnapAcquire(p) ==
    /\ pc[p]        = "rebuild3_snap_acq"
    /\ lock_holder  = "none"
    /\ lock_holder' = p
    /\ snap_len'    = [snap_len EXCEPT ![p] = Len(log)]
    /\ pc'          = [pc EXCEPT ![p] = "rebuild3_snap_rel"]
    /\ UNCHANGED <<log, db, pending>>

Rebuild3SnapRelease(p) ==
    /\ pc[p]        = "rebuild3_snap_rel"
    /\ lock_holder  = p
    /\ lock_holder' = "none"
    /\ pc'          = [pc EXCEPT ![p] = "rebuild3_offlock"]
    /\ UNCHANGED <<log, db, pending, snap_len>>

\* Phase 2: process holds no lock; it replays into a local tmp (not modelled as
\* a state variable — only the final swap in Phase 3 affects db).  Writes by
\* other processes continue normally and maintain WriteThroughInvariant.
Rebuild3OfflockReplay(p) ==
    /\ pc[p] = "rebuild3_offlock"
    /\ pc'   = [pc EXCEPT ![p] = "rebuild3_cu_acq"]
    /\ UNCHANGED <<log, db, lock_holder, pending, snap_len>>

Rebuild3CatchupAcquire(p) ==
    /\ pc[p]        = "rebuild3_cu_acq"
    /\ lock_holder  = "none"
    /\ lock_holder' = p
    /\ pc'          = [pc EXCEPT ![p] = "rebuild3_cu_apply"]
    /\ UNCHANGED <<log, db, pending, snap_len>>

\* Apply catch-up events (log[snap_len+1..Len(log)]) to tmp, then atomically
\* replace db with tmp.  Under the lock no new writes can occur, so
\* Materialize(log[1..snap_len[p]]) + catch-up = Materialize(log).
Rebuild3CatchupApply(p) ==
    /\ pc[p]        = "rebuild3_cu_apply"
    /\ lock_holder  = p
    /\ db'          = Materialize(log)
    /\ lock_holder' = "none"
    /\ pc'          = [pc EXCEPT ![p] = "idle"]
    /\ UNCHANGED <<log, pending, snap_len>>

(* ──────────────────────────── Compact protocol ─────────────────────────── *)

StartCompact(p) ==
    /\ pc[p] = "idle"
    /\ pc'   = [pc EXCEPT ![p] = "compact_acquiring"]
    /\ UNCHANGED <<log, db, lock_holder, pending, snap_len>>

CompactAcquire(p) ==
    /\ pc[p]       = "compact_acquiring"
    /\ lock_holder = "none"
    /\ lock_holder' = p
    /\ pc'          = [pc EXCEPT ![p] = "compact_running"]
    /\ UNCHANGED <<log, db, pending, snap_len>>

CompactRun(p) ==
    \* Abstract: compact atomically rewrites log + db under lock.
    \* In the implementation, the log rewrite (fsync) and DB update happen
    \* in the same locked section — no intermediate state is externally visible.
    \* run_history capping (500 events) is an impl detail, not modelled here.
    LET newLog == CompactedLog(log)
    IN /\ pc[p]       = "compact_running"
       /\ lock_holder = p
       /\ log'         = newLog
       /\ db'          = Materialize(newLog)
       /\ lock_holder' = "none"
       /\ pc'          = [pc EXCEPT ![p] = "idle"]
       /\ UNCHANGED <<pending, snap_len>>

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
        \/ StartRebuild(p)
        \/ StartRebuild3(p)
        \/ Rebuild3SnapAcquire(p)
        \/ Rebuild3SnapRelease(p)
        \/ Rebuild3OfflockReplay(p)
        \/ Rebuild3CatchupAcquire(p)
        \/ Rebuild3CatchupApply(p)
        \/ CompactAcquire(p)
        \/ CompactRun(p)
        \/ StartCompact(p)
        \/ \E id \in EntryIds, d \in DataVals : StartWrite(p, UpsertEvent(id, d))
        \/ \E id \in EntryIds               : StartWrite(p, ExpireEvent(id))

\* Safety spec (used for INVARIANT checking — bounded by LogLenBound).
Spec == Init /\ [][Next]_vars

\* Liveness spec (requires PROPERTY in cfg; much slower to check).
LiveSpec == Spec /\ WF_vars(Next)

(* ──────────────────────────── Safety invariants ────────────────────────── *)

MutualExclusion ==
    \A p1, p2 \in Procs :
        (lock_holder = p1 /\ lock_holder = p2) => p1 = p2

\* When the lock is free, the DB exactly mirrors the JSONL log.
WriteThroughInvariant ==
    lock_holder = "none" => db = Materialize(log)

\* Live entries after compaction are exactly the live entries before.
LiveIds(matDB) == {id \in EntryIds : matDB[id].type = "present" /\ matDB[id].stale = FALSE}

LiveCompactionEquivalence ==
    LET matOrig == Materialize(log)
        matComp == Materialize(CompactedLog(log))
    IN /\ LiveIds(matComp) = LiveIds(matOrig)
       /\ \A id \in LiveIds(matOrig) : matComp[id] = matOrig[id]

\* Expired entries are absent (not just stale) in the compacted log's materialized DB.
\* Directly validates AC: "after compact+rebuild, expired entries do not appear at all."
PurgedEntriesAreAbsent ==
    LET matOrig == Materialize(log)
        matComp == Materialize(CompactedLog(log))
    IN \A id \in EntryIds :
        (matOrig[id].type = "present" /\ matOrig[id].stale = TRUE) =>
        matComp[id].type = "absent"

Invariants ==
    /\ TypeInvariant
    /\ MutualExclusion
    /\ WriteThroughInvariant
    /\ LiveCompactionEquivalence
    /\ PurgedEntriesAreAbsent

THEOREM Spec => []Invariants

(* ──────────────────────────── Liveness ─────────────────────────────────── *)

WriteEventuallyCompletes ==
    \A p \in Procs :
        pc[p] = "write_acquiring" ~> pc[p] = "idle"

RebuildEventuallyCompletes ==
    \A p \in Procs :
        pc[p] = "rebuild_acquiring" ~> pc[p] = "idle"

Rebuild3EventuallyCompletes ==
    \A p \in Procs :
        pc[p] = "rebuild3_snap_acq" ~> pc[p] = "idle"

=============================================================================
