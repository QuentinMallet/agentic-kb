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
***************************************************************************)
CONSTANTS Fixed, Scenario, MaxLogLen, MaxConcurrentAppends

VARIABLES log, committed_len, tmp_db, live_db, files, wal_frames, phase,
          cursor, snapshot_boundary, concurrent_appends, tmp_mode, killed,
          replayed

vars == <<log, committed_len, tmp_db, live_db, files, wal_frames, phase,
          cursor, snapshot_boundary, concurrent_appends, tmp_mode, killed,
          replayed>>

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

Phase1Snapshot ==
  /\ phase = "p1" /\ ~killed
  /\ snapshot_boundary' = (IF Fixed THEN committed_len ELSE Len(log))
  /\ phase' = "p2"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  cursor, concurrent_appends, tmp_mode, killed, replayed>>

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
                  snapshot_boundary, tmp_mode, killed, replayed>>

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
Phase2Replay ==
  /\ phase = "p2" /\ ~killed
  /\ tmp_db' = CommittedEvents(Prefix(log, snapshot_boundary))
  /\ phase' = "p3"
  /\ UNCHANGED <<log, committed_len, live_db, files, wal_frames, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed>>

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
                  replayed>>

Checkpoint ==
  /\ phase = "KP_PRE_CHECKPOINT" /\ ~killed
  /\ IF Fixed
     THEN /\ files' = [files EXCEPT !["old"] = @ \union wal_frames]
          /\ wal_frames' = {}
     ELSE /\ UNCHANGED <<files, wal_frames>>
  /\ phase' = "KP_POST_CHECKPOINT"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed>>

VerifyAndClose ==
  /\ phase = "KP_POST_CHECKPOINT" /\ ~killed
  /\ (~Fixed \/ wal_frames = {})
  /\ files' = [files EXCEPT !["tmp"] = tmp_db]
  /\ phase' = "KP_POST_TMP_SYNC"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, wal_frames, cursor,
                  snapshot_boundary, concurrent_appends, tmp_mode, killed,
                  replayed>>

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
                  killed, replayed>>

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
                  replayed>>

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
                  replayed>>

DirSync ==
  /\ phase = (IF Fixed THEN "KP_POST_UNLINK" ELSE "KP_POST_RENAME")
  /\ ~killed /\ phase' = "KP_POST_DIR_SYNC"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  cursor, snapshot_boundary, concurrent_appends, tmp_mode,
                  killed, replayed>>

Finish ==
  /\ phase = "KP_POST_DIR_SYNC" /\ ~killed /\ phase' = "done"
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  cursor, snapshot_boundary, concurrent_appends, tmp_mode,
                  killed, replayed>>

Kill ==
  /\ phase \in KillPoints \union {"current_pre_rename"} /\ ~killed
  /\ killed' = TRUE
  /\ UNCHANGED <<log, committed_len, tmp_db, live_db, files, wal_frames,
                  phase, cursor, snapshot_boundary, concurrent_appends,
                  tmp_mode, replayed>>

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
                  cursor, snapshot_boundary, concurrent_appends, tmp_mode>>

Next == Phase1Snapshot \/ WriterAppend \/ Phase2Replay \/ Phase3CatchUp \/
        Checkpoint \/ VerifyAndClose \/ SetTmpWalMode \/ FirstNameOperation \/
        SecondNameOperation \/ DirSync \/ Finish \/ Kill \/ Reopen
Spec == Init /\ [][Next]_vars

NamedDbContents == files[live_db.db] \union
  (IF live_db.db = "old" THEN wal_frames ELSE {})

NameResolvesCommitted ==
  (Scenario = "CE4" /\ ((killed /\ phase \in KillPoints) \/ phase = "reopened"))
    => "W" \in NamedDbContents

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
