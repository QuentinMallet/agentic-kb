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
are `Fixed`, `AllowCrash`, `PoisonBatch`, `MaxGen`, `MaxBatches`; every
config exercises the full `Next` relation under those bounds.

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
***************************************************************************)

CONSTANTS EntryIds, MaxLogLen, MaxBatches, MaxGen, K, PoisonBatch,
          AllowCrash, InnerMaxLog, Fixed

VARIABLES log_written,    \* lines handed to write(2); may not be durable
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
          quarantined     \* batch ids dead-lettered by the poison policy

vars == <<log_written, log_durable, db, db_committed, cursor, generation,
          phase, nstarted, cur, aidx, attempts, quarantined>>

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
  /\ InnerMaxLog >= 5

NoId == "-"
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

Init ==
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

LinesOfCur == Len(SelectSeq(log_written, LAMBDA l : l.b = cur))

StartBatch ==
  /\ phase = "idle"
  /\ nstarted < MaxBatches
  /\ CursorCurrent
  /\ nstarted' = nstarted + 1
  /\ cur' = nstarted + 1
  /\ aidx' = 0
  /\ attempts' = 0
  /\ phase' = "writing"
  /\ UNCHANGED <<log_written, log_durable, db, db_committed, cursor,
                 generation, quarantined>>

AppendLine ==
  /\ phase = "writing"
  /\ LinesOfCur < Len(BatchLines(cur))
  /\ Len(log_written) < MaxLogLen
  /\ log_written' = Append(log_written, BatchLines(cur)[LinesOfCur + 1])
  /\ phase' = IF LinesOfCur + 1 = Len(BatchLines(cur)) THEN "ready" ELSE "writing"
  /\ UNCHANGED <<log_durable, db, db_committed, cursor, generation,
                 nstarted, cur, aidx, attempts, quarantined>>

\* Environment writeback: the OS may make an arbitrary prefix durable at any
\* time.  This is what exposes an unframed partial batch to a reader.
PartialFlush ==
  /\ Len(log_durable) < Len(log_written)
  /\ \E n \in (Len(log_durable) + 1)..Len(log_written) :
        log_durable' = Prefix(log_written, n)
  /\ UNCHANGED <<log_written, db, db_committed, cursor, generation, phase,
                 nstarted, cur, aidx, attempts, quarantined>>

\* D2: explicit sync_data of the whole written log.
SyncLog ==
  /\ phase = "ready"
  /\ log_durable' = log_written
  /\ phase' = "synced"
  /\ UNCHANGED <<log_written, db, db_committed, cursor, generation,
                 nstarted, cur, aidx, attempts, quarantined>>

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
  /\ UNCHANGED <<log_written, log_durable, generation, nstarted, cur,
                 attempts, quarantined>>

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
  /\ UNCHANGED <<log_written, log_durable, db, db_committed, cursor,
                 generation, nstarted, cur, aidx, quarantined>>

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
  /\ UNCHANGED <<log_written, log_durable, db, db_committed, generation,
                 nstarted, cur, attempts>>

FinishBatch ==
  /\ phase = "applied"
  /\ phase' = "idle"
  /\ cur' = 0
  /\ aidx' = 0
  /\ attempts' = 0
  /\ UNCHANGED <<log_written, log_durable, db, db_committed, cursor,
                 generation, nstarted, quarantined>>

\* Power loss.  Everything not yet durable is gone; the in-flight SQLite
\* transaction is left dirty and is rolled back at open time, not here.
\* Enabled at every point of a batch from the first line through the last
\* apply; a crash with nothing in flight changes no variable and is elided.
Crash ==
  /\ AllowCrash
  /\ phase \in {"writing", "ready", "synced", "applying"}
  /\ log_written' = log_durable
  /\ phase' = "crashed"
  /\ UNCHANGED <<log_durable, db, db_committed, cursor, generation,
                 nstarted, cur, aidx, attempts, quarantined>>

Open ==
  /\ phase = "crashed"
  /\ phase' = "opened"
  /\ aidx' = 0
  /\ IF Fixed
     THEN /\ db' = RecTarget.d
          /\ db_committed' = RecTarget.d
          /\ cursor' = RecTarget.c
     ELSE UNCHANGED <<db, db_committed, cursor>>
  /\ UNCHANGED <<log_written, log_durable, generation, nstarted, cur,
                 attempts, quarantined>>

\* D1 repair: truncate to the committed length.  Under the current design
\* every complete line is reader-accepted, so this removes nothing.
TruncateUncommittedTail ==
  /\ phase = "opened"
  /\ log_written' = Prefix(log_written, CommittedLen(log_written))
  /\ log_durable' = Prefix(log_durable,
                           IF CommittedLen(log_written) < Len(log_durable)
                           THEN CommittedLen(log_written) ELSE Len(log_durable))
  /\ phase' = "idle"
  /\ UNCHANGED <<db, db_committed, cursor, generation, nstarted, cur,
                 aidx, attempts, quarantined>>

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
  /\ generation < MaxGen
  /\ log_written = log_durable
  /\ Compacted(log_durable) # log_durable
  /\ log_written' = Compacted(log_durable)
  /\ log_durable' = Compacted(log_durable)
  /\ generation' = generation + 1
  /\ UNCHANGED <<db, db_committed, cursor, phase, nstarted, cur, aidx,
                 attempts, quarantined>>

\* recover_if_needed, called from open_or_init before every write path.
RecoverIdle ==
  /\ phase = "idle"
  /\ Fixed
  /\ ~CursorCurrent
  /\ db' = RecTarget.d
  /\ db_committed' = RecTarget.d
  /\ cursor' = RecTarget.c
  /\ UNCHANGED <<log_written, log_durable, generation, phase, nstarted,
                 cur, aidx, attempts, quarantined>>

\* Operator-invoked `kb rebuild`.
RebuildAll ==
  /\ phase = "idle"
  /\ Fixed
  /\ \/ ~CursorCurrent
     \/ db # Materialize(log_durable, quarantined)
  /\ db' = Materialize(log_durable, quarantined)
  /\ db_committed' = Materialize(log_durable, quarantined)
  /\ cursor' = [gen |-> generation, off |-> DurCommittedLen]
  /\ UNCHANGED <<log_written, log_durable, generation, phase, nstarted,
                 cur, aidx, attempts, quarantined>>

Next ==
  \/ StartBatch \/ AppendLine \/ PartialFlush \/ SyncLog
  \/ ApplyEvent \/ ApplyFail \/ Quarantine \/ FinishBatch
  \/ Crash \/ Open \/ TruncateUncommittedTail
  \/ Compact \/ RecoverIdle \/ RebuildAll

Spec == Init /\ [][Next]_vars
FairSpec == Init /\ [][Next]_vars /\ WF_vars(Next)

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
  \E n \in 0..DurCommittedLen : db_committed = Materialize(Prefix(log_durable, n), quarantined)

\* CE3.  Recovery restores the invariant at open time without a schema bump.
OpenRestores == phase = "opened" => db = Materialize(log_durable, quarantined)

\* D3.  A valid cursor describes exactly the committed DB state.
CursorAgreesWithDB ==
  cursor.gen = generation =>
    db_committed = Materialize(Prefix(log_durable, cursor.off), quarantined)

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

---------------------------------------------------------------------------
(* Deliberate violations, for the non-vacuity configs                      *)

BadTruncate ==
  /\ phase = "opened"
  /\ Accepted(log_written) # << >>
  /\ log_written' = << >>
  /\ log_durable' = << >>
  /\ phase' = "idle"
  /\ UNCHANGED <<db, db_committed, cursor, generation, nstarted, cur,
                 aidx, attempts, quarantined>>

SpecBadTruncate == Init /\ [][Next \/ BadTruncate]_vars

BadTypeInit ==
  /\ log_written = << >> /\ log_durable = << >>
  /\ db = {} /\ db_committed = {}
  /\ cursor = [gen |-> 0, off |-> MaxLogLen + 7]
  /\ generation = 0 /\ phase = "idle" /\ nstarted = 0 /\ cur = 0
  /\ aidx = 0 /\ attempts = 0 /\ quarantined = {}

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
