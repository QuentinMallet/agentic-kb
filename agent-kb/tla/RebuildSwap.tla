------------------------------- MODULE RebuildSwap -------------------------------
(*
  RebuildSwap.tla -- phased rebuild/swap protocol
  =================================================

  Empirical motivation (2026-08-15 rebuild-contention benchmark): the Phase-3
  write-lock hold was 2432 ms.  Catch-up replay consumed 2431.99 ms, while the
  rename plus WAL/SHM unlink operations consumed about 0.15 ms combined.

  This module models the CURRENT protocol.  A future fix must preserve
  invariants 1-4 below while improving invariant 5: Phase-3 work must remain a
  function only of the events accumulated in the lock-free Phase 2 backlog.

  This refines InnerGap's atomic Safety_Rebuild_Restores by exposing Snapshot
  (P1, locked), Replay (P2, lock-free), and CatchUp+Swap (P3, locked).
  Events are unique natural-number tokens, so materialization is simply the set
  of tokens in a log prefix.  MaxEvents and MaxWriters keep TLC's model tiny.
*)

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS MaxEvents, MaxWriters

NoLock == "None"
RebuildLock == "Rebuild"
Writers == {"w" \o ToString(i) : i \in 1..MaxWriters}
LockOwners == {NoLock, RebuildLock} \union Writers
Phases == {"Idle", "P1", "P2", "P3", "Done"}

VARIABLES
  jsonl,       \* durable sequence of appended event tokens
  db,          \* live DB in Idle; rebuild candidate after Snapshot
  snapshotAt,  \* length of JSONL captured under the P1 lock
  replayed,    \* number of snapshot events replayed into the candidate
  lockHolder,  \* NoLock, RebuildLock, or one writer id
  phase,
  backlog,     \* events appended during P2 and awaiting P3 catch-up
  wantsWrite,  \* writers that currently want to append
  swapAt,      \* JSONL length frozen on entry to P3
  p3Initial,   \* backlog size on entry to P3
  p3Applied    \* number of backlog events applied under the P3 lock

vars == << jsonl, db, snapshotAt, replayed, lockHolder, phase,
           backlog, wantsWrite, swapAt, p3Initial, p3Applied >>

Prefix(log, n) == SubSeq(log, 1, n)
Materialize(log) == {log[i] : i \in 1..Len(log)}

TypeOK ==
  /\ jsonl \in Seq(1..MaxEvents)
  /\ Len(jsonl) <= MaxEvents
  /\ db \subseteq 1..MaxEvents
  /\ snapshotAt \in 0..MaxEvents
  /\ replayed \in 0..MaxEvents
  /\ lockHolder \in LockOwners
  /\ phase \in Phases
  /\ backlog \in Seq(1..MaxEvents)
  /\ Len(backlog) <= MaxEvents
  /\ wantsWrite \subseteq Writers
  /\ swapAt \in 0..MaxEvents
  /\ p3Initial \in 0..MaxEvents
  /\ p3Applied \in 0..MaxEvents

Init ==
  /\ jsonl = << >>
  /\ db = {}
  /\ snapshotAt = 0
  /\ replayed = 0
  /\ lockHolder = NoLock
  /\ phase = "Idle"
  /\ backlog = << >>
  /\ wantsWrite = {}
  /\ swapAt = 0
  /\ p3Initial = 0
  /\ p3Applied = 0

WantWrite(w) ==
  /\ w \in Writers
  /\ w \notin wantsWrite
  /\ Len(jsonl) < MaxEvents
  /\ wantsWrite' = wantsWrite \union {w}
  /\ UNCHANGED << jsonl, db, snapshotAt, replayed, lockHolder, phase,
                  backlog, swapAt, p3Initial, p3Applied >>

\* Lock acquisition and the append are one atomic writer-path step. Thus every
\* requesting writer is enabled in P2 whenever capacity remains; lock owners
\* remain type-uniform strings to keep TLC comparisons fingerprintable.
AppendWriter(w) ==
  /\ w \in wantsWrite
  /\ lockHolder = NoLock
  /\ phase \in {"Idle", "P2"}
  /\ Len(jsonl) < MaxEvents
  /\ LET ev == Len(jsonl) + 1 IN
       /\ jsonl' = Append(jsonl, ev)
       /\ db' = IF phase = "Idle" THEN db \union {ev} ELSE db
       /\ backlog' = IF phase = "P2" THEN Append(backlog, ev) ELSE backlog
  /\ lockHolder' = NoLock
  /\ wantsWrite' = wantsWrite \ {w}
  /\ UNCHANGED << snapshotAt, replayed, phase, swapAt,
                  p3Initial, p3Applied >>

BeginRebuild ==
  /\ phase = "Idle"
  /\ lockHolder = NoLock
  /\ phase' = "P1"
  /\ lockHolder' = RebuildLock
  /\ UNCHANGED << jsonl, db, snapshotAt, replayed, backlog, wantsWrite,
                  swapAt, p3Initial, p3Applied >>

Snapshot ==
  /\ phase = "P1"
  /\ lockHolder = RebuildLock
  /\ snapshotAt' = Len(jsonl)
  /\ replayed' = 0
  /\ db' = {}
  /\ backlog' = << >>
  /\ phase' = "P2"
  /\ lockHolder' = NoLock
  /\ p3Initial' = 0
  /\ p3Applied' = 0
  /\ UNCHANGED << jsonl, wantsWrite, swapAt >>

ReplayOne ==
  /\ phase = "P2"
  /\ replayed < snapshotAt
  /\ replayed' = replayed + 1
  /\ db' = db \union {jsonl[replayed + 1]}
  /\ UNCHANGED << jsonl, snapshotAt, lockHolder, phase, backlog,
                  wantsWrite, swapAt, p3Initial, p3Applied >>

EnterCatchUp ==
  /\ phase = "P2"
  /\ replayed = snapshotAt
  /\ lockHolder = NoLock
  /\ phase' = "P3"
  /\ lockHolder' = RebuildLock
  /\ swapAt' = Len(jsonl)
  /\ p3Initial' = Len(backlog)
  /\ p3Applied' = 0
  /\ UNCHANGED << jsonl, db, snapshotAt, replayed, backlog, wantsWrite >>

CatchUpOne ==
  /\ phase = "P3"
  /\ lockHolder = RebuildLock
  /\ p3Applied < p3Initial
  /\ p3Applied' = p3Applied + 1
  /\ db' = db \union {backlog[p3Applied + 1]}
  /\ UNCHANGED << jsonl, snapshotAt, replayed, lockHolder, phase,
                  backlog, wantsWrite, swapAt, p3Initial >>

Swap ==
  /\ phase = "P3"
  /\ lockHolder = RebuildLock
  /\ p3Applied = p3Initial
  /\ phase' = "Done"
  /\ lockHolder' = NoLock
  /\ UNCHANGED << jsonl, db, snapshotAt, replayed, backlog, wantsWrite,
                  swapAt, p3Initial, p3Applied >>

Next ==
  \/ \E w \in Writers : WantWrite(w)
  \/ \E w \in Writers : AppendWriter(w)
  \/ BeginRebuild
  \/ Snapshot
  \/ ReplayOne
  \/ EnterCatchUp
  \/ CatchUpOne
  \/ Swap

(* 1. Refinement of InnerGap.Safety_Rebuild_Restores at the swap boundary. *)
RebuildRestoresMaterialization ==
  phase = "Done" => db = Materialize(Prefix(jsonl, swapAt))

(* 2. A scalar owner gives uniqueness; phase clauses tie ownership to protocol. *)
MutualExclusionOnLock ==
  /\ lockHolder \in LockOwners
  /\ (phase \in {"P1", "P3"} => lockHolder = RebuildLock)
  /\ (lockHolder = RebuildLock => phase \in {"P1", "P3"})

(* 3. No token durable before the swap boundary disappears from the candidate. *)
NoEventLostAcrossSwap ==
  phase = "Done" => Materialize(Prefix(jsonl, swapAt)) \subseteq db

(* 4. In P2, a requesting writer is enabled now or can acquire the free lock. *)
WritersProgressOutsideLock ==
  \A w \in Writers :
    (phase = "P2" /\ w \in wantsWrite /\ Len(jsonl) < MaxEvents) =>
      (lockHolder # RebuildLock /\ ENABLED AppendWriter(w))

(* 5. Exactly one P3 catch-up unit exists per event accumulated in P2. *)
CatchUpBoundedByBacklog ==
  (phase \in {"P3", "Done"}) =>
    /\ p3Initial = Len(backlog)
    /\ p3Applied <= p3Initial
    /\ swapAt = snapshotAt + p3Initial

Spec == Init /\ [][Next]_vars

=============================================================================
