-------------------------- MODULE RebuildProtocol --------------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC

(***************************************************************************
Full three-phase rebuild model.

Bounds: MaxLogLen = 4 physical lines and MaxConcurrentAppends = 2.  The CE6
span is <<begin, A, B, commit>>; two of its lines may be appended while Phase
2 is unlocked.  These remain the minimal complete CE6 envelope after the T0b
fixup: removing the CE6 replay-order pin (finding 3, see Phase2Replay) only
adds interleavings of the same four lines, it never introduces a new line or
a new append, so no bound changed.

Fixed=FALSE models today's raw-length Phase-1 boundary, offset-only Phase-3
reader, a tmp forced into DELETE mode with no action ever transitioning it,
and unlink-before-rename swap.  Fixed=TRUE models a committed_len boundary,
span-aware catch-up, an explicit SetTmpWalMode transition before the tmp is
ever named live, checkpoint/verify, rename-then-unlink, and directory fsync.

Storage assumption: a crash preserves the last completed protocol step.  The
model deliberately covers process kill points, not torn sectors/interior
zero-fill.  Directory fsync is therefore represented as the final durability
step.  NameResolvesCommitted must hold at every named kill point and again
after a modelled restart (Reopen, findings 4 and 6).

Delta (bd-21ef.2.22): reembed's per-batch writer, which suppresses SQLite's
close-time checkpoint.  BatchOpen/BatchCommitAndClose let wal_frames rise
during Phase 1/2 (previously wal_frames was monotonically non-increasing, so
a dirty WAL could only ever be an Init-time condition).  The flock and the
connection are two separate variables: batch_lock (a batch holds the
universal flock -- this is what blocks rebuild's Phase2Replay, the action
that first takes rebuild into its locked phases) and writer_open (a
connection is open on the live inode -- this is what NoWriterConnAtSwap
checks, and it is deliberately NOT what gates Phase2Replay, since a released
lock with a still-open connection is exactly the retained-connection bug the
invariant exists to catch).  Batch frames use the "B" symbol (already in the
module's frame alphabet) rather than "W", so the bound "B" \notin wal_frames
is not vacuously true under the CE4 scenario the way it would be with "W"
(CE4's Init already sets wal_frames = {"W"}).  RetainedConn is a single
non-vacuity toggle, mirroring how Fixed already gates this module's
correct-vs-buggy branches: RetainedConn = TRUE selects the rejected
alternative (a connection held open past the lock release) so
NoWriterConnAtSwap can be shown to actually distinguish the two.
***************************************************************************)
CONSTANTS Fixed, Scenario, MaxLogLen, MaxConcurrentAppends, RetainedConn

VARIABLES log, committed_len, tmp_db, live_db, files, wal_frames, phase,
          cursor, snapshot_boundary, concurrent_appends, tmp_mode, killed,
          replayed, batch_lock, writer_open

vars == <<log, committed_len, tmp_db, live_db, files, wal_frames, phase,
          cursor, snapshot_boundary, concurrent_appends, tmp_mode, killed,
          replayed, batch_lock, writer_open>>

Lines == {"begin", "A", "B", "commit", "W"}
FileIds == {"old", "tmp"}
Modes == {"WAL", "DELETE"}
TailShas == {"none", "span", "w"}
KillPoints == {"KP_PRE_CHECKPOINT", "KP_POST_CHECKPOINT",
  "KP_POST_TMP_SYNC", "KP_POST_RENAME", "KP_POST_UNLINK",
  "KP_POST_DIR_SYNC"}

Prefix(s, n) == SubSeq(s, 1, n)
IsSpan(s) == Len(s) = 4 /\ s = <<"begin", "A", "B", "commit">>
CommittedEvents(s) ==
  IF IsSpan(s) THEN {"A", "B"}
  ELSE IF Len(s) = 1 /\ s[1] = "W" THEN {"W"}
  ELSE {}
CommittedLength(s) == IF IsSpan(s) THEN 4 ELSE IF s = <<"W">> THEN 1 ELSE 0

(* The current offset reader cannot see a begin before its offset. *)
StandaloneTail(s, off) ==
  LET tail == SubSeq(s, off + 1, Len(s))
      elems == {tail[i] : i \in 1..Len(tail)}
  IN {x \in {"A", "B", "W"} : x \in elems}

TypeOK ==
  /\ log \in Seq(Lines) /\ Len(log) <= MaxLogLen
  /\ committed_len \in 0..MaxLogLen
  /\ tmp_db \subseteq {"A", "B", "W"}
  /\ live_db \in [db: FileIds]
  /\ files \in [FileIds -> SUBSET {"A", "B", "W"}]
  /\ wal_frames \subseteq {"A", "B", "W"}
  /\ phase \in {"p1", "p2", "p3", "KP_PRE_CHECKPOINT",
       "KP_POST_CHECKPOINT", "KP_POST_TMP_SYNC", "KP_POST_RENAME",
       "KP_POST_UNLINK", "KP_POST_DIR_SYNC", "done", "reopened"}
  /\ cursor \in [generation: {0}, offset: 0..MaxLogLen, tail_sha: TailShas]
  /\ snapshot_boundary \in 0..MaxLogLen
  /\ concurrent_appends \in 0..MaxConcurrentAppends
  /\ tmp_mode \in Modes /\ killed \in BOOLEAN /\ replayed \in BOOLEAN
  /\ batch_lock \in BOOLEAN /\ writer_open \in BOOLEAN

Init ==
  /\ IF Scenario = "CE4"
     THEN /\ log = <<"W">> /\ committed_len = 1
          /\ files = [old |-> {}, tmp |-> {}] /\ wal_frames = {"W"}
     ELSE /\ log = <<"begin", "A">> /\ committed_len = 0
          /\ files = [old |-> {}, tmp |-> {}] /\ wal_frames = {}
  /\ tmp_db = {} /\ live_db = [db |-> "old"] /\ phase = "p1"
  /\ cursor = [generation |-> 0, offset |-> 0, tail_sha |-> "none"]
  /\ snapshot_boundary = 0 /\ concurrent_appends = 0
  /\ tmp_mode = "DELETE" /\ killed = FALSE /\ replayed = FALSE
  /\ batch_lock = FALSE /\ writer_open = FALSE

Phase1Snapshot ==
  /\ phase = "p1" /\ ~killed
  /\ snapshot_boundary' = (IF Fixed THEN committed_len ELSE Len(log))
  /\ phase' = "p2"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  cursor, concurrent_appends, tmp_mode, killed, replayed,
                  batch_lock, writer_open>>

(* Phase 2 is unlocked: the writer finishes the span one physical line at a time. *)
WriterAppend ==
  /\ Scenario = "CE6" /\ phase = "p2" /\ ~killed
  /\ concurrent_appends < MaxConcurrentAppends /\ Len(log) < MaxLogLen
  /\ IF Len(log) = 2
     THEN /\ log' = Append(log, "B") /\ committed_len' = committed_len
     ELSE /\ Len(log) = 3 /\ log' = Append(log, "commit")
          /\ committed_len' = 4
  /\ concurrent_appends' = concurrent_appends + 1
  /\ UNCHANGED <<tmp_db, live_db, files, wal_frames, phase, cursor,
                  snapshot_boundary, tmp_mode, killed, replayed,
                  batch_lock, writer_open>>

(* Finding 3: the CE6 replay-order pin (Len(log) = MaxLogLen) is removed so
   the Fixed model explores every interleaving of replay against the
   concurrent writer, including a rebuild completing its snapshot replay
   while the span is still open (committed_len < 4).  Both Current and Fixed
   share this action, so Current keeps the same reachable violating trace it
   had under the pin -- removing a restriction only adds behaviour, it never
   removes the counterexample -- while Fixed now also explores the
   replay-before-append schedules the pin had excluded.  No separate pinned
   locator config was needed: TLC still finds the Current violation (see the
   counterexamples doc), so none is shipped. *)
(* bd-21ef.2.22: gated on ~batch_lock, not ~writer_open.  This is rebuild's
   flock acquisition -- it must wait for a concurrent batch to release the
   lock, but a lingering connection on the live inode does not block a lock
   acquisition, and must not appear to here.  Guarding on ~writer_open
   instead would make NoWriterConnAtSwap true by construction (Phase2Replay
   could never fire with a connection open, correct or not), which is
   exactly the vacuity this guard placement avoids. *)
Phase2Replay ==
  /\ phase = "p2" /\ ~killed /\ ~batch_lock
  /\ tmp_db' = CommittedEvents(Prefix(log, snapshot_boundary))
  /\ phase' = "p3"
  /\ UNCHANGED <<log, committed_len, live_db, files, wal_frames, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed, batch_lock, writer_open>>

Phase3CatchUp ==
  /\ phase = "p3" /\ ~killed
  /\ tmp_db' = (IF Fixed
                THEN tmp_db \union CommittedEvents(SubSeq(log, snapshot_boundary + 1, Len(log)))
                ELSE tmp_db \union StandaloneTail(log, snapshot_boundary))
  /\ cursor' = [generation |-> 0, offset |-> committed_len,
                 tail_sha |-> (IF Scenario = "CE4" THEN "w" ELSE "span")]
  /\ phase' = "KP_PRE_CHECKPOINT"
  /\ UNCHANGED <<log, committed_len, live_db, files, wal_frames,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed, batch_lock, writer_open>>

Checkpoint ==
  /\ phase = "KP_PRE_CHECKPOINT" /\ ~killed
  /\ IF Fixed
     THEN /\ files' = [files EXCEPT !["old"] = @ \union wal_frames]
          /\ wal_frames' = {}
     ELSE /\ UNCHANGED <<files, wal_frames>>
  /\ phase' = "KP_POST_CHECKPOINT"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed, batch_lock, writer_open>>

VerifyAndClose ==
  /\ phase = "KP_POST_CHECKPOINT" /\ ~killed
  /\ (~Fixed \/ wal_frames = {})
  /\ files' = [files EXCEPT !["tmp"] = tmp_db]
  /\ phase' = "KP_POST_TMP_SYNC"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, wal_frames, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed, batch_lock, writer_open>>

(* Finding 5: the fixed design must explicitly transition the tmp DB into WAL
   mode before it is ever named live.  DELETE mode -- today's forced pragma,
   now also Init's unconditional starting mode -- is never healed by any
   later action, so SwappedInWalMode checks a real transition instead of
   restating an Init-fixed constant.  FirstNameOperation's Fixed branch is
   gated on the transition having already happened. *)
SetTmpWalMode ==
  /\ Fixed /\ phase = "KP_POST_TMP_SYNC" /\ ~killed /\ tmp_mode = "DELETE"
  /\ tmp_mode' = "WAL"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  phase, cursor, snapshot_boundary, concurrent_appends,
                  killed, replayed, batch_lock, writer_open>>

FirstNameOperation ==
  /\ phase = "KP_POST_TMP_SYNC" /\ ~killed
  /\ IF Fixed
     THEN /\ tmp_mode = "WAL"
          /\ live_db' = [db |-> "tmp"] /\ phase' = "KP_POST_RENAME"
          /\ wal_frames' = wal_frames
     ELSE /\ live_db' = live_db /\ wal_frames' = {}
          /\ phase' = "KP_POST_UNLINK"
  /\ UNCHANGED <<log, committed_len, tmp_db, files, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed, batch_lock, writer_open>>

SecondNameOperation ==
  /\ ~killed
  /\ IF Fixed
     THEN /\ phase = "KP_POST_RENAME" /\ wal_frames' = {}
          /\ live_db' = live_db /\ phase' = "KP_POST_UNLINK"
     ELSE /\ phase = "KP_POST_UNLINK"
          /\ live_db' = [db |-> "tmp"] /\ wal_frames' = wal_frames
          /\ phase' = "KP_POST_RENAME"
  /\ UNCHANGED <<log, committed_len, tmp_db, files, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed, batch_lock, writer_open>>

DirSync ==
  /\ phase = (IF Fixed THEN "KP_POST_UNLINK" ELSE "KP_POST_RENAME")
  /\ ~killed /\ phase' = "KP_POST_DIR_SYNC"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  cursor, snapshot_boundary, concurrent_appends, tmp_mode,
                  killed, replayed, batch_lock, writer_open>>

Finish ==
  /\ phase = "KP_POST_DIR_SYNC" /\ ~killed /\ phase' = "done"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  cursor, snapshot_boundary, concurrent_appends, tmp_mode,
                  killed, replayed, batch_lock, writer_open>>

Kill ==
  /\ phase \in KillPoints \union {"current_pre_rename"} /\ ~killed
  /\ killed' = TRUE
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  phase, cursor, snapshot_boundary, concurrent_appends,
                  tmp_mode, replayed, batch_lock, writer_open>>

(* Findings 4 and 6: the Phase-3 cursor write (cursor', in Phase3CatchUp) was
   never read by any action.  Reopen models the next process start -- either
   after a crash (killed) or after a clean Finish ("done") -- and is the only
   action that reads cursor.  It must be a no-op (no replay) whenever the
   cursor already matches the log's committed_len, which is T5b's acceptance
   criterion (NoReplayOnMatchedCursor below).  Because Kill can only fire
   from a KillPoint, and every KillPoint is reachable only after
   Phase3CatchUp has already set the cursor, this holds on every reachable
   restart, not only the clean-finish one.  Reopen after a killed restart is
   also the second point at which NameResolvesCommitted is checked -- Kill
   alone only re-checks the instant of the kill, never a restart. *)
Reopen ==
  /\ phase # "reopened"
  /\ (killed \/ phase = "done")
  /\ phase' = "reopened"
  /\ killed' = FALSE
  /\ replayed' = ~(cursor.offset = committed_len /\ cursor.generation = 0)
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  cursor, snapshot_boundary, concurrent_appends, tmp_mode,
                  batch_lock, writer_open>>

(* Delta (bd-21ef.2.22): reembed's per-batch writer, which suppresses
   SQLite's close-time checkpoint.  Two obligations the module could not
   previously express: a batch RAISES wal_frames in the window where rebuild
   holds no lock (previously wal_frames was monotonically non-increasing, so
   a dirty WAL could only ever be an Init-time condition), and it closes
   every connection on the live inode before releasing the flock -- modelled
   as two separate variables, not one (see the module header): batch_lock is
   the flock rebuild's Phase2Replay must wait for; writer_open is the
   connection NoWriterConnAtSwap checks.

   BatchOpen fires only in the phases where rebuild holds no lock
   ("p1"/"p2"), and is bounded by "B" \notin wal_frames so the state space
   stays finite without a new CONSTANT -- wal_frames is a set, so a second
   batch's frame is indistinguishable from the first once "B" is already
   present.  Unlike "W", "B" is not already in CE4's Init, so this bound is
   not vacuous there. *)
BatchOpen ==
  /\ phase \in {"p1", "p2"} /\ ~killed /\ ~batch_lock /\ ~writer_open
  /\ "B" \notin wal_frames
  /\ batch_lock' = TRUE
  /\ writer_open' = TRUE
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  phase, cursor, snapshot_boundary, concurrent_appends,
                  tmp_mode, killed, replayed>>

(* Commit and close.  The frames STAY in the WAL -- this is the change: no
   close-time checkpoint.  batch_lock always drops (the flock is released);
   RetainedConn decides whether writer_open drops with it.  RetainedConn is a
   non-vacuity toggle mirroring how Fixed already gates this module's
   correct-vs-buggy branches: RetainedConn = FALSE is the shipped behaviour
   (both connections drop before the flock is released, so writer_open goes
   back to FALSE in the same step as batch_lock); RetainedConn = TRUE selects
   the rejected alternative from the commit message ("retained connection
   across batches | opens the old inode to a rebuild swap"), releasing the
   lock while writer_open' stays TRUE -- exactly the state Phase2Replay's
   ~batch_lock guard (and not a ~writer_open guard) lets through. *)
BatchCommitAndClose ==
  /\ batch_lock /\ writer_open /\ ~killed
  /\ wal_frames' = wal_frames \union {"B"}
  /\ batch_lock' = FALSE
  /\ writer_open' = RetainedConn
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, phase, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed>>

Next == Phase1Snapshot \/ WriterAppend \/ Phase2Replay \/ Phase3CatchUp \/
        Checkpoint \/ VerifyAndClose \/ SetTmpWalMode \/ FirstNameOperation \/
        SecondNameOperation \/ DirSync \/ Finish \/ Kill \/ Reopen \/
        BatchOpen \/ BatchCommitAndClose
Spec == Init /\ [][Next]_vars

NamedDbContents == files[live_db.db] \union
  (IF live_db.db = "old" THEN wal_frames ELSE {})

NameResolvesCommitted ==
  (Scenario = "CE4" /\ ((killed /\ phase \in KillPoints) \/ phase = "reopened"))
    => "W" \in NamedDbContents

(* The load-bearing new obligation, quoting the commit: "no connection is
   open on the live inode when the lock is released".  Phase2Replay's guard
   is ~batch_lock, not ~writer_open (see BatchCommitAndClose/Phase2Replay
   comments), so this invariant is the only thing that catches a batch
   releasing the flock while its connection is still open. *)
NoWriterConnAtSwap == phase \notin {"p1", "p2"} => ~writer_open

(* Finding 1: BatchAtomic previously required `killed`, but Kill is never
   enabled at "done" (it is not a KillPoint), so the `\union {"done"}` arm
   was unreachable and the half-applied-batch check never ran on the clean,
   no-crash completion path.  Dropping the killed conjunct checks every
   phase from KP_PRE_CHECKPOINT through done, crash or not. *)
BatchAtomic ==
  (Scenario = "CE6" /\ (phase \in KillPoints \/ phase = "done"))
    => (("A" \in tmp_db) = ("B" \in tmp_db))

(***************************************************************************
WAL self-heal hazard / named invariant: rebuild currently creates the tmp in
journal_mode=DELETE and never transitions it.  After ADR-1 makes open_ro omit
the WAL pragma, no later read repairs that persistent mode.  The fixed
protocol must rename a DB whose header already records WAL mode -- modelled
by SetTmpWalMode, an explicit action gated on Fixed that fires before the tmp
is ever named live (see finding 5).
***************************************************************************)
SwappedInWalMode == live_db.db = "tmp" => tmp_mode = "WAL"

(* Finding 4: the Phase-3 cursor write must actually describe the completed
   state, and reading it back (Reopen) must never trigger a replay when it
   already matches.  There is no separate compaction/generation-bump action
   in this module -- that lives in DurableBatch.tla -- so `generation` is
   instantiated as the constant 0 that cursor.generation and Init already
   use throughout this spec. *)
CursorMatchesAtDone ==
  phase = "done" => cursor.offset = committed_len /\ cursor.generation = 0

NoReplayOnMatchedCursor == phase = "reopened" => ~replayed
=============================================================================
