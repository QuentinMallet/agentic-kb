--------------------------- MODULE DurableBatch ---------------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC

(***************************************************************************
Layer-1 durability model for C1 (bd-21ef.1.3 / T0a).

Two designs share one state machine, selected by the constant `Fixed`:

  Fixed = FALSE   the pre-D1/D2/D3 implementation.  `append_events_batch`
                  writes one unframed line per event (events.rs:117-133),
                  never calls sync_data, applies each event in its own
                  top-level savepoint (kb_core.rs:385-389), and keeps no
                  applied cursor, so recovery fires only on a schema bump.

  Fixed = TRUE    D1 (begin/commit envelope around EVERY append),
                  D2 (sync_data after the commit marker, before any DB
                  write) and D3 ((generation, offset) applied cursor
                  written inside the apply transaction).

Nothing in this module pins a crash point or a step ordering.  The knobs
are `Fixed`, `AllowCrash`, `AllowDeferred`, `AllowLogMissing`, `PoisonBatch`,
`MaxGen`, `MaxBatches`; every config exercises the full `Next` relation under
those bounds.

Reads are intentionally not state transitions in this write/recovery model.
During `deferred` or `LogMissing` they remain available against the current
`db`; only writes, automatic recovery, and compaction are represented and
blocked.  This is the D3 row-9 disposition: serve reads, refuse writes, and
decline the *automatic* rebuild (`recover_if_needed`, modelled by
`RecoverIdle`) when a non-trivial DB has lost its log.  `RebuildAll` models
that same automatic-path decline, not the operator-invoked `kb rebuild`
command -- see "OUT OF MODEL SCOPE -- row 9 boundaries" below.

WRITE SCHEDULE.  Batch contents are fixed so the refinement to InnerGap is
total: InnerGap can only produce a 1-event upsert batch (StartNoReplace) or
a 2-event [expire e, upsert n] batch (Start), so DurableBatch writes only
those shapes.  The plan's n=3 example is the same defect class as n=2 --
"crash after line 1 of N" -- and n=2 is the smallest witness.

  Batch 1 = <<upsert A>>              single-event append (D1 envelopes it too)
  Batch 2 = <<expire A, upsert B>>    the CE1 shape
  Batch 3 = <<expire B, upsert C>>    an append after recovery, exercises the cursor

A batch is never re-attempted: a batch interrupted by a crash is abandoned
(its caller is gone) and the next Start writes the next batch in the
schedule.  That keeps the log monotone under a finite bound.

BOUNDS (all stated up front, all non-binding unless noted):
  EntryIds  = {"A","B","C"}          exactly the ids the schedule touches
  MaxBatches in 2..3                 batches the writer may start
  MaxLogLen = 12                     physical-line cap; the schedule needs
                                     at most 3+4+4 = 11 lines, so this
                                     bound never binds (I1: it is a bound,
                                     not the exact length)
  MaxGen    in 0..1                  compactions allowed (0 disables Compact)
  K         = 2                      apply retries before dead-lettering
  InnerMaxLog = 5                    InnerGap's MaxLog; must be >= the total
                                     event count of the schedule (1+2+2)

OUT OF MODEL SCOPE -- D2 prefix adequacy.  `log_durable` is modelled as a
PREFIX of `log_written`.  That abstraction is adequate only because D2
makes the region whose durability is relied on before the next protocol
step exactly one append: the writer syncs the newline-terminated commit
marker before the first db::apply_event, so no interior region is ever
trusted while unsynced.  Interior zero-fill damage (a data=writeback or
partially-written block producing a garbage MIDDLE line) is therefore not
modelled here; the code-side disposition is the quarantine-the-unparseable
-line policy in the plan (Q4).  Recorded as a named ASSUME below.

OUT OF MODEL SCOPE -- row 9 boundaries.  Three things D3 row 9
(`LogMissing`) does NOT cover, none of them machine-checked here:

  1. Restoration of a vanished log has no transition.  Once `LogVanishes`
     fires, `Fixed`'s only way back to `log_present = TRUE` is the withdrawn
     `UnsafeWriteWhileLogMissing` action, which requires `~Fixed`.  Row 9 is
     therefore an ABSORBING state in the fixed design: `LogMissingFreezesState`
     and `LogMissingBlocksWrites` hold vacuously-forever from that point on,
     not because recovery was attempted and succeeded.  `LogVanishes` is also
     guarded `~deferred`.  This is a MODELLING CHOICE, not a claim that a real
     log file cannot vanish while a deferral is outstanding -- it plainly can.
     The guard mirrors `cursor.rs:inspect`'s dispatch order (a call made after
     both conditions hold reports `LogMissing`, never `Defer`) and keeps
     `DeferredConverges` meaningful by construction: with the guard, the two
     absorbing/outstanding conditions cannot compose in this model, so
     `DeferredConverges` is not exercised by a missing log and is left
     unweakened rather than scoped around an untested composition. Without the
     guard, a `LogVanishes` while `deferred` would need its own semantics (e.g.
     clearing the deferral) and its own re-verified cfgs; that alternative was
     not taken here.
  2. `RebuildAll`'s `~LogMissing` guard models only the AUTOMATIC decline
     (`recover_if_needed`, i.e. the code path also modelled by `RecoverIdle`).
     Operator-invoked `kb rebuild` is a deliberate, ungated override in the
     implementation (`rebuild.rs:393`'s `execute_with` never calls
     `cursor::inspect`) and can destructively empty a row-9 database; that
     override is not represented by any action here.
  3. A cursorless populated database whose log has vanished is NOT row 9 in
     the code: `cursor::inspect` classifies it as `FullRebuild(CursorMissing)`
     before the missing-log check ever runs (`cursor.rs:583`), and that path
     is separately fail-closed (`rebuild.rs`'s `full_rebuild_for` refuses to
     rebuild it). This module always carries a cursor and cannot express a
     cursorless state, so this boundary is outside what it proves.

Recorded as named comments here and in DurableBatch-counterexamples.md rather
than as ASSUMEs: none of the three is a machine-checkable non-fact the way
`OutOfScope_InteriorDamage` is -- they are transitions the model omits, not
premises TLC could contradict.
***************************************************************************)

CONSTANTS EntryIds, MaxLogLen, MaxBatches, MaxGen, K, PoisonBatch,
          AllowCrash, AllowDeferred, AllowLogMissing, InnerMaxLog, Fixed

VARIABLES log_present,    \* distinguishes a fresh empty log from a missing one
          log_written,    \* lines handed to write(2); may not be durable
          log_durable,    \* lines on stable storage; a prefix of log_written
          db,             \* live entry set as the DB currently shows it
          db_committed,   \* live entry set at the last committed SQLite txn
          cursor,         \* [gen |-> Nat, off |-> physical durable line count]
          generation,     \* log generation; bumped by compaction
          phase,
          nstarted,       \* batches started so far
          cur,            \* batch id most recently started (0 = none ever)
          aidx,           \* events of `cur` applied inside the current txn
          attempts,       \* consecutive failed apply attempts on `cur`
          quarantined,    \* batch ids dead-lettered by the poison policy
          deferred,       \* recovery is outstanding after unreadable tail
          damage_repaired, \* the unreadable line has been repaired in place
          log_lost        \* GHOST: monotone, set once LogVanishes has ever fired

vars == <<log_present, log_written, log_durable, db, db_committed, cursor, generation,
          phase, nstarted, cur, aidx, attempts, quarantined, deferred,
          damage_repaired, log_lost>>

ASSUME OutOfScope_InteriorDamage == TRUE
  (* See "OUT OF MODEL SCOPE -- D2 prefix adequacy" above.  This ASSUME is
     the named carrier the plan's section 5 requires; its justification is
     the paragraph above, not a machine-checked fact. *)

ASSUME Bounds ==
  /\ EntryIds = {"A", "B", "C"}
  /\ MaxBatches \in 2..3
  /\ MaxLogLen >= 3 * MaxBatches + 3
  /\ MaxGen \in 0..1
  /\ K >= 1
  /\ PoisonBatch \in 0..MaxBatches
  /\ AllowDeferred \in BOOLEAN
  /\ AllowLogMissing \in BOOLEAN
  /\ InnerMaxLog >= 5

NoId == "-"
EmptyDB == {}
Ln(a, i, b) == [act |-> a, id |-> i, b |-> b]

BatchEvents(b) ==
  CASE b = 1 -> << Ln("upsert", "A", 1) >>
    [] b = 2 -> << Ln("expire", "A", 2), Ln("upsert", "B", 2) >>
    [] OTHER -> << Ln("expire", "B", 3), Ln("upsert", "C", 3) >>

BatchLen(b) == Len(BatchEvents(b))

\* D1: every append -- batch AND single event -- is enveloped.
BatchLines(b) ==
  IF Fixed
  THEN <<Ln("begin", NoId, b)>> \o BatchEvents(b) \o <<Ln("commit", NoId, b)>>
  ELSE BatchEvents(b)

Prefix(s, n) == SubSeq(s, 1, n)
IsPrefix(a, b) == \E n \in 0..Len(b) : a = Prefix(b, n)

IsEvent(l) == l.act \in {"upsert", "expire"}

RECURSIVE FoldEvents(_, _)
FoldEvents(s, live) ==
  IF s = << >> THEN live
  ELSE LET x == Head(s)
       IN FoldEvents(Tail(s),
                     IF x.act = "upsert" THEN live \union {x.id}
                                         ELSE live \ {x.id})

\* Reader rules from D1's table.  A span counts only when its commit marker
\* is present; every legacy (unframed) line is a committed standalone event.
CommitSet(s) == { s[j].b : j \in {i \in 1..Len(s) : s[i].act = "commit"} }

Accepted(s) ==
  IF Fixed
  THEN LET cs == CommitSet(s) IN SelectSeq(s, LAMBDA l : IsEvent(l) /\ l.b \in cs)
  ELSE SelectSeq(s, LAMBDA l : IsEvent(l))

\* Dead-lettered records are deliberately excluded from materialization.
\* Both are pure functions of the log plus an explicit quarantine set `q` --
\* not a hidden read of the state variable `quarantined` -- so every use
\* site's formula states, in its own text, exactly which quarantine set it
\* materializes against.  See CE3 / OpenRestores below.
AcceptedLive(s, q) == SelectSeq(Accepted(s), LAMBDA l : l.b \notin q)

Materialize(s, q) == FoldEvents(AcceptedLive(s, q), {})

\* committed_len(log) as a physical line count.  Spans are contiguous, so the
\* committed prefix ends at the last commit marker; under the current design
\* every complete line is reader-accepted, so it is the whole log.
LastCommitIdx(s) ==
  IF \E i \in 1..Len(s) : s[i].act = "commit"
  THEN CHOOSE i \in 1..Len(s) :
         s[i].act = "commit" /\ \A j \in (i+1)..Len(s) : s[j].act # "commit"
  ELSE 0

CommittedLen(s) == IF Fixed THEN LastCommitIdx(s) ELSE Len(s)

DurCommittedLen == CommittedLen(log_durable)

\* D3 row 9: absence is unsafe only when durable state was previously
\* non-trivial.  A present-but-empty fresh log is deliberately distinct.
LogMissing == ~log_present /\ (cursor.off > 0 \/ db_committed /= EmptyDB)

\* Events of the durable committed log that the cursor has not yet covered.
TailEvents ==
  LET all  == AcceptedLive(log_durable, quarantined)
      done == Len(AcceptedLive(Prefix(log_durable, cursor.off), quarantined))
  IN  SubSeq(all, done + 1, Len(all))

\* D3 recovery table, shared by Open (after a crash) and RecoverIdle
\* (recover_if_needed before every write path).  Three distinct branches:
\* full rebuild, replay-the-tail-from-the-cursor, no-op.
RecTarget ==
  IF cursor.gen # generation \/ cursor.off > DurCommittedLen
  THEN [ d |-> Materialize(log_durable, quarantined),
         c |-> [gen |-> generation, off |-> DurCommittedLen] ]
  ELSE IF cursor.off < DurCommittedLen
  THEN [ d |-> FoldEvents(TailEvents, db_committed),
         c |-> [gen |-> generation, off |-> DurCommittedLen] ]
  ELSE [ d |-> db_committed, c |-> cursor ]

CursorCurrent ==
  \/ ~Fixed
  \/ (cursor.gen = generation /\ cursor.off = DurCommittedLen)

Phases == {"idle", "writing", "ready", "synced", "applying", "applied",
           "crashed", "opened"}

LineOK(l) ==
  /\ l.act \in {"begin", "commit", "upsert", "expire"}
  /\ l.id \in EntryIds \union {NoId}
  /\ l.b \in 1..MaxBatches

TypeOK ==
  /\ log_present \in BOOLEAN
  /\ \A i \in 1..Len(log_written) : LineOK(log_written[i])
  /\ \A i \in 1..Len(log_durable) : LineOK(log_durable[i])
  /\ Len(log_written) <= MaxLogLen
  /\ db \subseteq EntryIds
  /\ db_committed \subseteq EntryIds
  /\ cursor \in [gen : 0..MaxGen, off : 0..MaxLogLen]
  /\ generation \in 0..MaxGen
  /\ phase \in Phases
  /\ nstarted \in 0..MaxBatches
  /\ cur \in 0..MaxBatches
  /\ aidx \in 0..2
  /\ attempts \in 0..K
  /\ quarantined \subseteq (1..MaxBatches)
  /\ deferred \in BOOLEAN
  /\ damage_repaired \in BOOLEAN
  /\ damage_repaired => deferred
  /\ log_lost \in BOOLEAN

Init ==
  /\ log_present = TRUE
  /\ log_lost = FALSE
  /\ log_written = << >>
  /\ log_durable = << >>
  /\ db = {}
  /\ db_committed = {}
  /\ cursor = [gen |-> 0, off |-> 0]
  /\ generation = 0
  /\ phase = "idle"
  /\ nstarted = 0
  /\ cur = 0
  /\ aidx = 0
  /\ attempts = 0
  /\ quarantined = {}
  /\ deferred = FALSE
  /\ damage_repaired = FALSE

LinesOfCur == Len(SelectSeq(log_written, LAMBDA l : l.b = cur))

StartBatch ==
  /\ phase = "idle"
  /\ ~deferred
  /\ ~LogMissing
  /\ nstarted < MaxBatches
  /\ CursorCurrent
  /\ nstarted' = nstarted + 1
  /\ cur' = nstarted + 1
  /\ aidx' = 0
  /\ attempts' = 0
  /\ phase' = "writing"
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, db, db_committed, cursor,
                 generation, quarantined, deferred, damage_repaired>>

AppendLine ==
  /\ phase = "writing"
  /\ LinesOfCur < Len(BatchLines(cur))
  /\ Len(log_written) < MaxLogLen
  /\ log_written' = Append(log_written, BatchLines(cur)[LinesOfCur + 1])
  /\ phase' = IF LinesOfCur + 1 = Len(BatchLines(cur)) THEN "ready" ELSE "writing"
  /\ UNCHANGED <<log_present, log_lost, log_durable, db, db_committed, cursor, generation,
                 nstarted, cur, aidx, attempts, quarantined, deferred,
                 damage_repaired>>

\* Environment writeback: the OS may make an arbitrary prefix durable at any
\* time.  This is what exposes an unframed partial batch to a reader.
PartialFlush ==
  /\ Len(log_durable) < Len(log_written)
  /\ \E n \in (Len(log_durable) + 1)..Len(log_written) :
        log_durable' = Prefix(log_written, n)
  /\ UNCHANGED <<log_present, log_lost, log_written, db, db_committed, cursor, generation, phase,
                 nstarted, cur, aidx, attempts, quarantined, deferred,
                 damage_repaired>>

\* D2: explicit sync_data of the whole written log.
SyncLog ==
  /\ phase = "ready"
  /\ log_durable' = log_written
  /\ phase' = "synced"
  /\ UNCHANGED <<log_present, log_lost, log_written, db, db_committed, cursor, generation,
                 nstarted, cur, aidx, attempts, quarantined, deferred,
                 damage_repaired>>

\* `inspect` finds an unreadable durable line beyond the applied cursor.
\* Any completed append may be damaged, regardless of how many earlier
\* batches were applied.  Damage makes a merely-written batch durable too,
\* so both ready and already-synced-but-unapplied batches are covered.
Damage ==
  /\ AllowDeferred
  /\ ~deferred
  /\ phase \in {"ready", "synced"}
  /\ cursor.gen = generation
  /\ cursor.off < CommittedLen(log_written)
  /\ log_durable' = log_written
  /\ deferred' = TRUE
  /\ damage_repaired' = FALSE
  /\ phase' = "idle"
  /\ cur' = 0
  /\ aidx' = 0
  /\ attempts' = 0
  /\ UNCHANGED <<log_present, log_lost, log_written, db, db_committed, cursor, generation,
                 nstarted, quarantined>>

\* Counterexample for the withdrawn alternative.  The old design proceeds
\* with a write while inspection is deferred, applies it without extending
\* the readable durable prefix, and cannot advance the cursor.  Fixed has no
\* corresponding transition: StartBatch is guarded by ~deferred.
UnsafeWriteWhileDeferred ==
  /\ ~Fixed
  /\ deferred
  /\ ~damage_repaired
  /\ phase = "idle"
  /\ nstarted < MaxBatches
  /\ Len(log_written) + Len(BatchLines(nstarted + 1)) <= MaxLogLen
  /\ LET b == nstarted + 1
         nl == log_written \o BatchLines(b)
         nd == FoldEvents(BatchEvents(b), db_committed)
     IN /\ log_written' = nl
        /\ log_durable' = log_durable
        /\ db' = nd
        /\ db_committed' = nd
        /\ nstarted' = b
  /\ UNCHANGED <<log_present, log_lost, cursor, generation, phase, cur, aidx, attempts,
                 quarantined, deferred, damage_repaired>>

\* External loss of a non-trivial log.  The DB and its applied cursor remain
\* available for reads, while both in-memory views of the vanished file empty.
LogVanishes ==
  /\ AllowLogMissing
  /\ log_present
  /\ phase = "idle"
  /\ ~deferred   \* Modelling choice, not a physical claim: a real log file can
                 \* vanish regardless of `deferred`.  The guard mirrors
                 \* cursor.rs:inspect's dispatch order -- a call made after
                 \* both conditions hold reports LogMissing, never Defer -- and
                 \* keeps `DeferredConverges` meaningful by construction; see
                 \* "OUT OF MODEL SCOPE -- row 9 boundaries" item 1 above.
  /\ (cursor.off > 0 \/ db_committed /= EmptyDB)
  /\ log_present' = FALSE
  /\ log_written' = << >>
  /\ log_durable' = << >>
  /\ log_lost' = TRUE
  /\ UNCHANGED <<db, db_committed, cursor, generation, phase, nstarted,
                 cur, aidx, attempts, quarantined, deferred, damage_repaired>>

\* Withdrawn alternative: treating a missing log as fresh lets a write
\* recreate it from empty.  The old DB state is then neither supported by a
\* durable prefix nor by the cursor-named prefix.
UnsafeWriteWhileLogMissing ==
  /\ ~Fixed
  /\ LogMissing
  /\ phase = "idle"
  /\ nstarted < MaxBatches
  /\ Len(BatchLines(nstarted + 1)) <= MaxLogLen
  /\ LET b == nstarted + 1
         nl == BatchLines(b)
         nd == FoldEvents(BatchEvents(b), db_committed)
     IN /\ log_present' = TRUE
        /\ log_written' = nl
        /\ log_durable' = << >>
        /\ db' = nd
        /\ db_committed' = nd
        /\ nstarted' = b
  /\ UNCHANGED <<log_lost, cursor, generation, phase, cur, aidx, attempts,
                 quarantined, deferred, damage_repaired>>

\* Repair rewrites the malformed line in place.  The deferral remains
\* outstanding until Recovery has replayed from the old cursor to EOF.
Repair ==
  /\ phase = "idle"
  /\ deferred
  /\ ~damage_repaired
  /\ damage_repaired' = TRUE
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, db, db_committed, cursor,
                 generation, phase, nstarted, cur, aidx, attempts,
                 quarantined, deferred>>

Recovery ==
  /\ phase = "idle"
  /\ Fixed
  /\ deferred
  /\ damage_repaired
  /\ ~LogMissing
  /\ db' = RecTarget.d
  /\ db_committed' = RecTarget.d
  /\ cursor' = RecTarget.c
  /\ deferred' = FALSE
  /\ damage_repaired' = FALSE
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, generation, phase, nstarted,
                 cur, aidx, attempts, quarantined>>

\* The fixed design applies only from "synced" and only behind a durable
\* commit marker.  The current design may apply straight from "ready", so
\* both orderings of sync and apply stay enabled and TLC picks (CE2).
ApplyEnabled ==
  IF Fixed
  THEN phase = "synced" /\ log_durable = log_written
       /\ CommittedLen(log_durable) = Len(log_durable)
  ELSE phase \in {"ready", "synced"}

ApplyEvent ==
  /\ cur # 0
  /\ cur # PoisonBatch
  /\ \/ ApplyEnabled /\ aidx = 0
     \/ phase = "applying"
  /\ aidx < BatchLen(cur)
  /\ LET ev == BatchEvents(cur)[aidx + 1]
         nd == IF ev.act = "upsert" THEN db \union {ev.id} ELSE db \ {ev.id}
         last == (aidx + 1 = BatchLen(cur))
     IN /\ db' = nd
        /\ aidx' = aidx + 1
        /\ phase' = IF last THEN "applied" ELSE "applying"
        \* D3: the cursor commits in the same transaction as the last apply.
        /\ cursor' = IF last /\ Fixed
                     THEN [gen |-> generation, off |-> DurCommittedLen]
                     ELSE cursor
        \* the current design has N independent top-level savepoints
        /\ db_committed' = IF Fixed /\ ~last THEN db_committed ELSE nd
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, generation, nstarted, cur,
                 attempts, quarantined, deferred, damage_repaired>>

\* A deterministically failing record (down embedder, malformed event).
ApplyFail ==
  /\ cur # 0
  /\ cur = PoisonBatch
  /\ \/ ApplyEnabled /\ aidx = 0
     \/ phase = "applying"
  /\ ~(Fixed /\ attempts >= K)
  /\ attempts < K
  /\ attempts' = attempts + 1
  /\ phase' = "applying"
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, db, db_committed, cursor,
                 generation, nstarted, cur, aidx, quarantined, deferred,
                 damage_repaired>>

\* D3 poison policy: after K attempts, dead-letter and advance past it.
Quarantine ==
  /\ Fixed
  /\ phase = "applying"
  /\ cur = PoisonBatch
  /\ attempts >= K
  /\ quarantined' = quarantined \union {cur}
  /\ cursor' = [gen |-> generation, off |-> DurCommittedLen]
  /\ aidx' = BatchLen(cur)
  /\ phase' = "applied"
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, db, db_committed, generation,
                 nstarted, cur, attempts, deferred, damage_repaired>>

FinishBatch ==
  /\ phase = "applied"
  /\ phase' = "idle"
  /\ cur' = 0
  /\ aidx' = 0
  /\ attempts' = 0
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, db, db_committed, cursor,
                 generation, nstarted, quarantined, deferred, damage_repaired>>

\* Power loss.  Everything not yet durable is gone; the in-flight SQLite
\* transaction is left dirty and is rolled back at open time, not here.
\* Enabled at every point of a batch from the first line through the last
\* apply; a crash with nothing in flight changes no variable and is elided.
Crash ==
  /\ AllowCrash
  /\ phase \in {"writing", "ready", "synced", "applying"}
  /\ log_written' = log_durable
  /\ phase' = "crashed"
  /\ UNCHANGED <<log_present, log_lost, log_durable, db, db_committed, cursor, generation,
                 nstarted, cur, aidx, attempts, quarantined, deferred,
                 damage_repaired>>

Open ==
  /\ phase = "crashed"
  /\ phase' = "opened"
  /\ aidx' = 0
  /\ IF Fixed
     THEN /\ db' = RecTarget.d
          /\ db_committed' = RecTarget.d
          /\ cursor' = RecTarget.c
     ELSE UNCHANGED <<db, db_committed, cursor>>
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, generation, nstarted, cur,
                 attempts, quarantined, deferred, damage_repaired>>

\* D1 repair: truncate to the committed length.  Under the current design
\* every complete line is reader-accepted, so this removes nothing.
TruncateUncommittedTail ==
  /\ phase = "opened"
  /\ log_written' = Prefix(log_written, CommittedLen(log_written))
  /\ log_durable' = Prefix(log_durable,
                           IF CommittedLen(log_written) < Len(log_durable)
                           THEN CommittedLen(log_written) ELSE Len(log_durable))
  /\ phase' = "idle"
  /\ UNCHANGED <<log_present, log_lost, db, db_committed, cursor, generation, nstarted, cur,
                 aidx, attempts, quarantined, deferred, damage_repaired>>

\* Compaction drops every line of a batch whose events are all dead, and
\* bumps the generation without touching the cursor -- so a generation
\* mismatch is reachable.  Line-granular compaction is T0c's model.
DeadOf(s) ==
  { b \in 1..MaxBatches :
      /\ \E i \in 1..Len(s) : s[i].b = b
      /\ \A j \in 1..BatchLen(b) : BatchEvents(b)[j].id \notin Materialize(s, quarantined) }

Compacted(s) == LET dead == DeadOf(s) IN SelectSeq(s, LAMBDA l : l.b \notin dead)

Compact ==
  /\ phase = "idle"
  /\ ~deferred
  /\ ~LogMissing
  /\ generation < MaxGen
  /\ log_written = log_durable
  /\ Compacted(log_durable) # log_durable
  /\ log_written' = Compacted(log_durable)
  /\ log_durable' = Compacted(log_durable)
  /\ generation' = generation + 1
  /\ UNCHANGED <<log_present, log_lost, db, db_committed, cursor, phase, nstarted, cur, aidx,
                 attempts, quarantined, deferred, damage_repaired>>

\* recover_if_needed, called from open_or_init before every write path.
RecoverIdle ==
  /\ phase = "idle"
  /\ Fixed
  /\ ~deferred
  /\ ~LogMissing
  /\ ~CursorCurrent
  /\ db' = RecTarget.d
  /\ db_committed' = RecTarget.d
  /\ cursor' = RecTarget.c
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, generation, phase, nstarted,
                 cur, aidx, attempts, quarantined, deferred, damage_repaired>>

\* recover_if_needed's full-rebuild branch, unconditional re-materialization
\* from the durable log regardless of whether the cursor names the current
\* prefix.  This is the AUTOMATIC decline path, not the operator-invoked
\* `kb rebuild` command: the code deliberately does NOT gate the operator
\* command on `LogMissing` (rebuild.rs:393's execute_with never calls
\* cursor::inspect), so an operator-invoked rebuild of a row-9 database is a
\* real, destructive, and out-of-model transition -- see "OUT OF MODEL SCOPE
\* -- row 9 boundaries" above the CONSTANTS block.
RebuildAll ==
  /\ phase = "idle"
  /\ Fixed
  /\ ~deferred
  /\ ~LogMissing
  /\ \/ ~CursorCurrent
     \/ db # Materialize(log_durable, quarantined)
  /\ db' = Materialize(log_durable, quarantined)
  /\ db_committed' = Materialize(log_durable, quarantined)
  /\ cursor' = [gen |-> generation, off |-> DurCommittedLen]
  /\ UNCHANGED <<log_present, log_lost, log_written, log_durable, generation, phase, nstarted,
                 cur, aidx, attempts, quarantined, deferred, damage_repaired>>

Next ==
  \/ StartBatch \/ AppendLine \/ PartialFlush \/ SyncLog
  \/ Damage \/ UnsafeWriteWhileDeferred \/ LogVanishes
  \/ UnsafeWriteWhileLogMissing \/ Repair \/ Recovery
  \/ ApplyEvent \/ ApplyFail \/ Quarantine \/ FinishBatch
  \/ Crash \/ Open \/ TruncateUncommittedTail
  \/ Compact \/ RecoverIdle \/ RebuildAll

Spec == Init /\ [][Next]_vars
FairSpec == Init /\ [][Next]_vars /\ WF_vars(Next)
DeferredFairSpec == Spec /\ WF_vars(Recovery)

---------------------------------------------------------------------------
(* Invariants                                                              *)

\* CE1.  Crash atomicity of the reader-accepted log, crash points UNPINNED:
\* the durable committed log never exposes a strict, non-empty part of a
\* batch.  Compaction drops whole batches, so it does not perturb this.
NoHalfBatch ==
  \A b \in 1..MaxBatches :
    Len(SelectSeq(Accepted(log_durable), LAMBDA l : l.b = b)) \in {0, BatchLen(b)}

\* CE2.  Ordering, not an enumeration of values: the committed DB state is
\* always the materialization of SOME prefix of the durable committed log,
\* i.e. the DB is never ahead of the log.
DBNotAheadOfDurable ==
  \/ LogMissing
  \/ \E n \in 0..DurCommittedLen :
       db_committed = Materialize(Prefix(log_durable, n), quarantined)

\* CE3.  Recovery restores the invariant at open time without a schema bump.
OpenRestores == phase = "opened" => db = Materialize(log_durable, quarantined)

\* D3 invariant: the cursor and the DB prefix it names always agree.
CursorAgreesWithDB ==
  \/ LogMissing
  \/ (cursor.gen = generation =>
       db_committed = Materialize(Prefix(log_durable, cursor.off), quarantined))

\* When generation/offset are comparable, replaying everything after the
\* cursor over the current DB converges to the durable log's materialization.
\* This is the operational meaning of the cursor never being ahead of DB.
CursorNeverAheadOfDB ==
  \/ LogMissing
  \/ cursor.gen # generation
  \/ cursor.off > DurCommittedLen
  \/ FoldEvents(TailEvents, db_committed) =
       Materialize(log_durable, quarantined)

CursorCaughtUp ==
  /\ cursor.gen = generation
  /\ cursor.off = DurCommittedLen
  /\ db_committed = Materialize(log_durable, quarantined)

\* Row 9 is a stable read-only state in the fixed design.  The state
\* invariant rules out any in-flight write; the action property separately
\* proves that no transition can start another scheduled batch.
LogMissingBlocksWrites == LogMissing => phase = "idle"
LogMissingDoesNotStart == [][LogMissing => nstarted' = nstarted]_vars

\* Tighter than the bare state invariants above: while the log is missing,
\* the fixed design's committed state does not merely avoid a *reported*
\* violation, it does not move at all.  This is the actual content of "reads
\* served, writes refused", and it catches a guard omitted from any future
\* DB-mutating action even where the three-invariant disjunct would not (that
\* disjunct is only non-vacuous today because LogMissing happens to be
\* absorbing under Fixed; this property does not rely on that fact).
LogMissingFreezesState ==
  [][LogMissing => UNCHANGED <<db, db_committed, cursor, generation>>]_vars

\* Repair leaves `deferred` set until fair Recovery replays from cursor.off.
DeferredConverges == damage_repaired ~> CursorCaughtUp

\* Counterexample gates: unlike the unconditional fixed-design invariants,
\* these become obligations only after Damage.  This makes Deferred_Current
\* report the withdrawn write-while-damaged defect instead of the older
\* current-design fact that no cursor exists at all.
DeferredCursorAgreesWithDB == ~deferred \/ CursorAgreesWithDB
DeferredDBNotAheadOfDurable == ~deferred \/ DBNotAheadOfDurable
DeferredCursorNeverAheadOfDB == ~deferred \/ CursorNeverAheadOfDB

\* Counterexample gates for the withdrawn LogMissing alternative.  `log_lost`
\* is a monotone ghost (set once by LogVanishes, never cleared), so unlike
\* `LogMissing` itself it stays true across a resurrection -- exactly what is
\* needed to isolate `UnsafeWriteWhileLogMissing`'s defect from the shallow,
\* LogMissing-unrelated CE2 violation that otherwise pre-empts it in
\* `DurableBatch_LogMissing_Current.cfg` (see DurableBatch-counterexamples.md,
\* "LogMissing -- row 9 refuses resurrection").
LostDBNotAheadOfDurable == ~log_lost \/ DBNotAheadOfDurable
LostCursorAgreesWithDB == ~log_lost \/ CursorAgreesWithDB
LostCursorNeverAheadOfDB == ~log_lost \/ CursorNeverAheadOfDB

DurableIsPrefix == IsPrefix(log_durable, log_written)

\* D1 repair safety, as its own property rather than a TypeOK conjunct.
TruncationPreservesAccepted ==
  [][ (phase = "opened" /\ phase' = "idle")
        => Accepted(log_written') = Accepted(log_written) ]_vars

\* CE8.  Bounded recovery (Principle 4).  Fairness is on Next; the fixed
\* design must leave "applying" via the dead-letter path after K attempts.
RetryTerminates == (phase = "applying") ~> (phase = "idle")


---------------------------------------------------------------------------
(* Branch-reachability witnesses for the D3 recovery table.               *)
(* Each is deliberately FALSE somewhere: TLC violating it is the proof     *)
(* that the corresponding branch of RecTarget is reachable, so `Open` and  *)
(* `RecoverIdle` are genuinely three-way and not a disguised unconditional *)
(* re-materialization.                                                     *)
W_Rec_FullRebuild ==
  ~(phase \in {"crashed", "idle"} /\ cursor.gen # generation)
W_Rec_TailReplay ==
  ~(phase = "crashed" /\ cursor.gen = generation /\ cursor.off < DurCommittedLen)
W_Rec_NoOp ==
  ~(phase = "crashed" /\ cursor.gen = generation /\ cursor.off = DurCommittedLen)

\* Depth witness: the whole schedule really runs to completion under the
\* bounds, so a green Fixed config is not green because it stopped early.
W_ScheduleCompletes ==
  ~(nstarted = MaxBatches /\ "C" \in db /\ phase = "idle" /\ cur = 0)

\* Row 9 reachability: `LogMissing` is not vacuously false in the LogMissing
\* configs -- `LogMissingBlocksWrites` and `LogMissingDoesNotStart` would
\* otherwise hold by never firing `LogVanishes` at all.
W_NotLogMissing == ~LogMissing

---------------------------------------------------------------------------
(* Deliberate violations, for the non-vacuity configs                      *)

BadTruncate ==
  /\ phase = "opened"
  /\ Accepted(log_written) # << >>
  /\ log_written' = << >>
  /\ log_durable' = << >>
  /\ phase' = "idle"
  /\ UNCHANGED <<log_present, log_lost, db, db_committed, cursor, generation, nstarted, cur,
                 aidx, attempts, quarantined, deferred, damage_repaired>>

SpecBadTruncate == Init /\ [][Next \/ BadTruncate]_vars

BadTypeInit ==
  /\ log_present = TRUE
  /\ log_written = << >> /\ log_durable = << >>
  /\ db = {} /\ db_committed = {}
  /\ cursor = [gen |-> 0, off |-> MaxLogLen + 7]
  /\ generation = 0 /\ phase = "idle" /\ nstarted = 0 /\ cur = 0
  /\ aidx = 0 /\ attempts = 0 /\ quarantined = {}
  /\ deferred = FALSE /\ damage_repaired = FALSE
  /\ log_lost = FALSE

SpecBadType == BadTypeInit /\ [][Next]_vars

---------------------------------------------------------------------------
(* Refinement of InnerGap.tla                                              *)
(*                                                                         *)
(* Data-carrying, not a constant projection:                               *)
(*   jsonl        <- the record-valued projection of the ACCEPTED durable  *)
(*                   log, so it grows exactly when a commit marker becomes *)
(*                   durable.  That step IS InnerGap's atomic AppendBatch. *)
(*   db           <- db                                                    *)
(*   crash        <- phase = "crashed"                                     *)
(*   apply_idx    <- aidx, the events of the batch the cursor's transaction *)
(*                   has applied so far                                    *)
(*   batch_events <- the current batch's events                            *)
(*                                                                         *)
(* Under the current design a PartialFlush makes ONE event of a two-event  *)
(* batch durable, so jsonl grows by a strict part of batch_events, which   *)
(* no InnerGap step permits -- and the pre-sync apply moves db while the   *)
(* abstract phase is still "appended", which ApplyNext forbids.  Both are  *)
(* genuine violations, checked by DurableBatch_Refinement_Current.cfg.     *)
(*                                                                         *)
(* Compaction is excluded from the refinement configs (MaxGen = 0):        *)
(* InnerGap's jsonl only ever grows, so a log rewrite is outside the       *)
(* abstraction by construction.  Compaction correctness is T0c's model.    *)
(*                                                                         *)
(* LogVanishes is excluded from the refinement configs for the identical   *)
(* reason (AllowLogMissing = FALSE): it empties log_durable, so the mapped *)
(* jsonl would shrink, which is equally outside InnerGap's grows-only      *)
(* abstraction.  Confirmed: flipping AllowLogMissing to TRUE on            *)
(* DurableBatch_Refinement_Fixed.cfg alone violates InnerSafety.           *)
(***************************************************************************)
ToInner(l) == [kind |-> l.act, id |-> l.id]
ToInnerSeq(s) == [i \in 1..Len(s) |-> ToInner(s[i])]

EventsDurable(b) ==
  Len(SelectSeq(Accepted(log_durable), LAMBDA l : l.b = b)) = BatchLen(b)

InnerPhase ==
  IF phase = "crashed" THEN "crashed"
  ELSE IF phase \in {"idle", "opened"} \/ cur = 0 THEN "idle"
  ELSE IF aidx = BatchLen(cur) THEN "done"
  ELSE IF EventsDurable(cur) THEN "applying"
  ELSE "appended"

Inner == INSTANCE InnerGap WITH
  EntryIds     <- EntryIds,
  MaxBatchSize <- 2,
  MaxLog       <- InnerMaxLog,
  jsonl        <- ToInnerSeq(AcceptedLive(log_durable, quarantined)),
  db           <- db,
  crash        <- (phase = "crashed"),
  phase        <- InnerPhase,
  batch_events <- IF cur = 0 THEN << >> ELSE ToInnerSeq(BatchEvents(cur)),
  apply_idx    <- aidx

Spec_InnerGap == Inner!Spec_InnerGap
InnerTypeOK == Inner!TypeOK_InnerGap
InnerSafety == Inner!Safety_DB_NotAhead

(* Non-degeneracy witnesses for the mapping: each is deliberately FALSE    *)
(* somewhere, so a TLC violation proves the abstract variables really move. *)
W_Inner_JsonlGrows == ToInnerSeq(AcceptedLive(log_durable, quarantined)) = << >>
W_Inner_Applying   == InnerPhase # "applying"
W_Inner_Done       == InnerPhase # "done"
W_Inner_Crashed    == InnerPhase # "crashed"
=============================================================================
