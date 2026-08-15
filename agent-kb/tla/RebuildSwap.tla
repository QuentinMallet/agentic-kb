------------------------------- MODULE RebuildSwap -------------------------------
(*
  RebuildSwap.tla -- phased rebuild/swap protocol
  =================================================

  This module models the CURRENT protocol.  In Phase 3 the lock is held for an
  O(full log) parse, a fixed tmp-DB open/configure cost, and one unit for every
  backlog event.  LockedWorkBounded makes that cost executable.  The default
  RebuildSwap.cfg uses LockBudget = 7 and passes; LockBudget = 6 fails.  Making
  the current small model pass at 6 is the contention-fix target.

  RebuildSwap.cfg disables compaction and is expected to be green.
  RebuildSwap_compact.cfg enables compaction and is EXPECTED to violate
  SnapshotIndexRemainsValid, reproducing bd-3mr.9 (the snapshot_len.min clamp
  silently treats an index from the old log as an index into the replacement).
  SnapshotIndexRemainsValid is a generation proxy for the dropped tail rather
  than the drop itself; it fires exactly in the bd-3mr.9 hazard window.

  LIMITATION: Spec == Init /\ [][Next]_vars has no WF_vars/SF_vars conjuncts, so
  this module checks SAFETY only.  For the leading fix candidate (iterative
  catch-up outside the lock, then a short final locked hold), this spec can
  validate that lockedWork stays under budget and that the safety invariants
  still hold, but it cannot prove termination/progress or that the iteration
  converges.  If that fix is pursued, fairness plus an explicit liveness
  property is the follow-up.

  Modeling constraint for that future fix: releasing the write lock while
  REMAINING in phase "P3" is still rejected by MutualExclusionOnLock.  A
  lock-free stage must therefore be introduced as a NEW phase name (for
  example a "P2b" catch-up phase), followed by a short final locked hold for
  the residual catch-up and swap.

  This module models the phase structure beneath InnerGap's atomic Rebuild with
  a matching abstraction function: it uses the same order-sensitive FoldLog and
  the same event shape [kind, id], and RebuildRestoresMaterialization mirrors
  InnerGap's Safety_Rebuild_Restores.  This is an asserted invariant
  correspondence, not a machine-checked refinement: there is no INSTANCE
  mapping and no PROPERTY Spec => InnerGap!Spec.  Future work can add a
  machine-checked refinement mapping.

  lockedWork resets at EnterCatchUp and is charged only in P3 stages, so work
  moved outside the lock scores zero.
  FullLogParse charges Len(jsonl) unconditionally, so eliminating only the
  backlog still floors lockedWork at Len(jsonl) + OpenTmpCost; the spec
  therefore shows that fix (a) alone is insufficient.

  InnerGap correspondence detail: events have an id and a kind; upsert adds the
  id, expire removes it, and the last write wins.  Applied log indices expose
  duplicate and out-of-order replay across P2 and P3.

  I3/I4 residuals (deliberately outside this state machine): db = {} at Snapshot
  means creation of a fresh TMP DB; the live DB continues serving readers and no
  reader agent is modeled.  Fresh-DB/swap behavior is covered by the M2
  rebuild-swap regression test.  WAL/SHM and file-identity behavior is covered by
  that regression test and bd-3mr.9; lock-duration realism is covered by the
  rebuild-contention artifact.

  MaxEvents, MaxWriters, and EntryIds keep TLC's state space finite and small.
*)

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS EntryIds, MaxEvents, MaxWriters, OpenTmpCost, LockBudget, EnableCompact

NoLock == "None"
RebuildLock == "Rebuild"
Writers == {"w" \o ToString(i) : i \in 1..MaxWriters}
LockOwners == {NoLock, RebuildLock} \union Writers
Phases == {"Idle", "P1", "P2", "P3", "Done"}
P3Stages == {"None", "FullLogParse", "OpenTmpDb", "CatchUp", "Ready"}
Event == [kind : {"upsert", "expire"}, id : EntryIds]

VARIABLES
  jsonl, snapshotAt, snapshotGeneration, logGeneration,
  db, replayed, appliedIndices,
  lockHolder, phase, p3Stage, wantsWrite, writerAppended, pendingEvent,
  swapAt, p3Initial, p3Applied, lockedWork

vars == << jsonl, snapshotAt, snapshotGeneration, logGeneration,
           db, replayed, appliedIndices,
           lockHolder, phase, p3Stage, wantsWrite, writerAppended, pendingEvent,
           swapAt, p3Initial, p3Applied, lockedWork >>

Prefix(log, n) == SubSeq(log, 1, n)

ApplyEvent(live, ev) ==
  IF ev.kind = "upsert" THEN live \union {ev.id} ELSE live \ {ev.id}

RECURSIVE FoldLog(_, _)
FoldLog(log, live) ==
  IF log = << >>
  THEN live
  ELSE FoldLog(Tail(log), ApplyEvent(live, Head(log)))

Materialize(log) == FoldLog(log, {})

TypeOK ==
  /\ jsonl \in Seq(Event)
  /\ Len(jsonl) <= MaxEvents
  /\ snapshotAt \in 0..MaxEvents
  /\ snapshotGeneration \in Nat
  /\ logGeneration \in Nat
  /\ db \subseteq EntryIds
  /\ replayed \in 0..MaxEvents
  /\ appliedIndices \in Seq(1..MaxEvents)
  /\ Len(appliedIndices) <= MaxEvents
  /\ lockHolder \in LockOwners
  /\ phase \in Phases
  /\ p3Stage \in P3Stages
  /\ wantsWrite \subseteq Writers
  /\ writerAppended \subseteq Writers
  /\ pendingEvent \in [Writers -> Event]
  /\ swapAt \in 0..MaxEvents
  /\ p3Initial \in 0..MaxEvents
  /\ p3Applied \in 0..MaxEvents
  /\ lockedWork \in Nat

Init ==
  /\ jsonl = << >>
  /\ snapshotAt = 0
  /\ snapshotGeneration = 0
  /\ logGeneration = 0
  /\ db = {}
  /\ replayed = 0
  /\ appliedIndices = << >>
  /\ lockHolder = NoLock
  /\ phase = "Idle"
  /\ p3Stage = "None"
  /\ wantsWrite = {}
  /\ writerAppended = {}
  /\ pendingEvent \in [Writers -> Event]
  /\ swapAt = 0
  /\ p3Initial = 0
  /\ p3Applied = 0
  /\ lockedWork = 0

WantWrite(w, ev) ==
  /\ w \in Writers
  /\ w \notin wantsWrite
  /\ Len(jsonl) < MaxEvents
  /\ wantsWrite' = wantsWrite \union {w}
  /\ pendingEvent' = [pendingEvent EXCEPT ![w] = ev]
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, lockHolder, phase, p3Stage, writerAppended,
                  swapAt, p3Initial, p3Applied, lockedWork >>

AcquireWriterLock(w) ==
  /\ w \in wantsWrite
  /\ lockHolder = NoLock
  /\ phase \in {"Idle", "P2"}
  /\ Len(jsonl) < MaxEvents
  /\ w \notin writerAppended
  /\ lockHolder' = w
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, phase, p3Stage, wantsWrite, writerAppended,
                  pendingEvent, swapAt, p3Initial, p3Applied, lockedWork >>

AppendWriter(w) ==
  /\ lockHolder = w
  /\ w \in wantsWrite
  /\ w \notin writerAppended
  /\ Len(jsonl) < MaxEvents
  /\ jsonl' = Append(jsonl, pendingEvent[w])
  /\ db' = IF phase = "Idle" THEN ApplyEvent(db, pendingEvent[w]) ELSE db
  /\ writerAppended' = writerAppended \union {w}
  /\ UNCHANGED << snapshotAt, snapshotGeneration, logGeneration,
                  replayed, appliedIndices, lockHolder, phase, p3Stage,
                  wantsWrite, pendingEvent, swapAt, p3Initial, p3Applied,
                  lockedWork >>

ReleaseWriterLock(w) ==
  /\ lockHolder = w
  /\ w \in wantsWrite
  /\ w \in writerAppended
  /\ lockHolder' = NoLock
  /\ wantsWrite' = wantsWrite \ {w}
  /\ writerAppended' = writerAppended \ {w}
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, phase, p3Stage, pendingEvent,
                  swapAt, p3Initial, p3Applied, lockedWork >>

BeginRebuild ==
  /\ phase = "Idle"
  /\ lockHolder = NoLock
  /\ phase' = "P1"
  /\ lockHolder' = RebuildLock
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, p3Stage, wantsWrite, writerAppended,
                  pendingEvent, swapAt, p3Initial, p3Applied, lockedWork >>

Snapshot ==
  /\ phase = "P1"
  /\ lockHolder = RebuildLock
  /\ snapshotAt' = Len(jsonl)
  /\ snapshotGeneration' = logGeneration
  /\ replayed' = 0
  /\ appliedIndices' = << >>
  /\ db' = {}
  /\ phase' = "P2"
  /\ lockHolder' = NoLock
  /\ p3Stage' = "None"
  /\ swapAt' = 0
  /\ p3Initial' = 0
  /\ p3Applied' = 0
  /\ lockedWork' = 0
  /\ UNCHANGED << jsonl, logGeneration, wantsWrite, writerAppended, pendingEvent >>

ReplayOne ==
  /\ phase = "P2"
  /\ replayed < snapshotAt
  /\ replayed < Len(jsonl)
  /\ replayed' = replayed + 1
  /\ db' = ApplyEvent(db, jsonl[replayed + 1])
  /\ appliedIndices' = Append(appliedIndices, replayed + 1)
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  lockHolder, phase, p3Stage, wantsWrite, writerAppended, pendingEvent,
                  swapAt, p3Initial, p3Applied, lockedWork >>

EnterCatchUp ==
  /\ phase = "P2"
  /\ replayed = snapshotAt
  /\ lockHolder = NoLock
  /\ phase' = "P3"
  /\ p3Stage' = "FullLogParse"
  /\ lockHolder' = RebuildLock
  /\ swapAt' = Len(jsonl)
  /\ p3Initial' = Len(jsonl) - snapshotAt
  /\ p3Applied' = 0
  /\ lockedWork' = 0
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, wantsWrite, writerAppended, pendingEvent >>

FullLogParse ==
  /\ phase = "P3"
  /\ lockHolder = RebuildLock
  /\ p3Stage = "FullLogParse"
  /\ lockedWork' = lockedWork + Len(jsonl)
  /\ p3Stage' = "OpenTmpDb"
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, lockHolder, phase, wantsWrite, writerAppended,
                  pendingEvent, swapAt, p3Initial, p3Applied >>

OpenTmpDb ==
  /\ phase = "P3"
  /\ lockHolder = RebuildLock
  /\ p3Stage = "OpenTmpDb"
  /\ lockedWork' = lockedWork + OpenTmpCost
  /\ p3Stage' = IF p3Initial = 0 THEN "Ready" ELSE "CatchUp"
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, lockHolder, phase, wantsWrite, writerAppended,
                  pendingEvent, swapAt, p3Initial, p3Applied >>

CatchUpOne ==
  /\ phase = "P3"
  /\ lockHolder = RebuildLock
  /\ p3Stage = "CatchUp"
  /\ p3Applied < p3Initial
  /\ LET idx == snapshotAt + p3Applied + 1 IN
       /\ db' = ApplyEvent(db, jsonl[idx])
       /\ appliedIndices' = Append(appliedIndices, idx)
  /\ p3Applied' = p3Applied + 1
  /\ lockedWork' = lockedWork + 1
  /\ p3Stage' = IF p3Applied + 1 = p3Initial THEN "Ready" ELSE "CatchUp"
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  replayed, lockHolder, phase, wantsWrite, writerAppended, pendingEvent,
                  swapAt, p3Initial >>

Swap ==
  /\ phase = "P3"
  /\ lockHolder = RebuildLock
  /\ p3Stage = "Ready"
  /\ phase' = "Done"
  /\ p3Stage' = "None"
  /\ lockHolder' = NoLock
  /\ UNCHANGED << jsonl, snapshotAt, snapshotGeneration, logGeneration,
                  db, replayed, appliedIndices, wantsWrite, writerAppended, pendingEvent,
                  swapAt, p3Initial, p3Applied, lockedWork >>

(* Compact holds the same flock and atomically installs a shorter prefix whose
   fold is equivalent to the old log.  Its replacement has a new generation. *)
Compact(n) ==
  /\ EnableCompact
  /\ phase = "P2"
  /\ lockHolder = NoLock
  /\ Len(jsonl) > 0
  /\ n \in 0..MaxEvents
  /\ n < Len(jsonl)
  /\ Materialize(Prefix(jsonl, n)) = Materialize(jsonl)
  /\ jsonl' = Prefix(jsonl, n)
  /\ logGeneration' = logGeneration + 1
  /\ UNCHANGED << snapshotAt, snapshotGeneration, db, replayed,
                  appliedIndices, lockHolder, phase, p3Stage, wantsWrite, writerAppended,
                  pendingEvent, swapAt, p3Initial, p3Applied, lockedWork >>

Next ==
  \/ \E w \in Writers, ev \in Event : WantWrite(w, ev)
  \/ \E w \in Writers : AcquireWriterLock(w)
  \/ \E w \in Writers : AppendWriter(w)
  \/ \E w \in Writers : ReleaseWriterLock(w)
  \/ BeginRebuild
  \/ Snapshot
  \/ ReplayOne
  \/ EnterCatchUp
  \/ FullLogParse
  \/ OpenTmpDb
  \/ CatchUpOne
  \/ Swap
  \/ \E n \in 0..MaxEvents : Compact(n)

RebuildRestoresMaterialization ==
  phase = "Done" => db = Materialize(Prefix(jsonl, swapAt))

MutualExclusionOnLock ==
  /\ lockHolder \in LockOwners
  /\ (phase \in {"P1", "P3"} => lockHolder = RebuildLock)
  /\ (lockHolder = RebuildLock => phase \in {"P1", "P3"})
  /\ \A w \in Writers : lockHolder = w => w \in wantsWrite

NoEventLostAcrossSwap ==
  phase = "Done" => {appliedIndices[i] : i \in 1..Len(appliedIndices)} = 1..swapAt

WritersProgressOutsideLock ==
  \A w \in Writers :
    (phase = "P2" /\ w \in wantsWrite /\ Len(jsonl) < MaxEvents) =>
      (lockHolder = w
        \/ (lockHolder = NoLock /\ ENABLED AcquireWriterLock(w))
        \/ lockHolder # RebuildLock)

NoDuplicateApply ==
  Cardinality({appliedIndices[i] : i \in 1..Len(appliedIndices)}) = Len(appliedIndices)

NoOutOfOrderApply ==
  \A i \in 1..(Len(appliedIndices) - 1) :
    appliedIndices[i] < appliedIndices[i + 1]

LockedWorkBounded == lockedWork <= LockBudget

SnapshotIndexRemainsValid ==
  phase \in {"P2", "P3", "Done"} => snapshotGeneration = logGeneration

Spec == Init /\ [][Next]_vars

=============================================================================
