--------------------------------- MODULE InnerGap ---------------------------------
(*
  InnerGap.tla  —  Layer 1 refinement: per-event append/apply gap
  ================================================================

  Models a single kb_core::add call that:
    1. Appends a batch of [expire, upsert] events to JSONL in ONE
       atomic write (append_events_batch).
    2. Applies each event to DB in order.

  Safety property:
    At "idle" (before a call starts or after it fully completes):
      db = Materialize(jsonl)

  The key correctness property:
    After a crash during applying, Rebuild restores db = Materialize(jsonl).
    The JSONL is never partial — AppendBatch is all-or-nothing.

  MaxLog bounds the JSONL length so TLC terminates in finite time.
*)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS EntryIds, MaxBatchSize, MaxLog

VARIABLES jsonl, db, crash, phase, batch_events, apply_idx

TypeOK_InnerGap ==
  /\ phase \in {"idle", "appended", "applying", "done", "crashed"}
  /\ batch_events \in Seq([kind : {"expire","upsert"}, id : EntryIds])
  /\ apply_idx \in Nat
  /\ jsonl \in Seq([kind : {"expire","upsert"}, id : EntryIds])
  /\ db \subseteq EntryIds
  /\ crash \in BOOLEAN

Init_InnerGap ==
  /\ phase        = "idle"
  /\ batch_events = << >>
  /\ apply_idx    = 0
  /\ jsonl        = << >>
  /\ db           = {}
  /\ crash        = FALSE

\* Order-sensitive materialization: fold over the log left-to-right.
\* Each event either adds or removes an id from the live set.
RECURSIVE FoldLog(_, _)
FoldLog(log, live) ==
  IF log = << >>
  THEN live
  ELSE LET hd == Head(log)
           tl == Tail(log)
           next == IF hd.kind = "upsert"
                   THEN live \union {hd.id}
                   ELSE live \ {hd.id}
       IN  FoldLog(tl, next)

Materialize_Inner(log) == FoldLog(log, {})

\* Start a new add call: expire one entry, upsert a new one.
\* Guarded by MaxLog so JSONL stays bounded (TLC termination).
Start(expire_id, new_id) ==
  /\ phase = "idle"
  /\ Len(jsonl) + 2 <= MaxLog
  /\ expire_id # new_id
  /\ batch_events' = << [kind |-> "expire", id |-> expire_id],
                        [kind |-> "upsert", id |-> new_id] >>
  /\ phase'     = "appended"
  /\ apply_idx' = 0
  /\ UNCHANGED << jsonl, db, crash >>

\* Start a new add call with NO expire (replace_path=false).
StartNoReplace(new_id) ==
  /\ phase = "idle"
  /\ Len(jsonl) + 1 <= MaxLog
  /\ batch_events' = << [kind |-> "upsert", id |-> new_id] >>
  /\ phase'     = "appended"
  /\ apply_idx' = 0
  /\ UNCHANGED << jsonl, db, crash >>

\* Atomically append ALL batch events to JSONL (append_events_batch).
AppendBatch ==
  /\ phase = "appended"
  /\ jsonl' = jsonl \o batch_events
  /\ phase' = "applying"
  /\ UNCHANGED << db, crash, batch_events, apply_idx >>

\* Apply the next event from the batch to DB (apply_event).
ApplyNext ==
  /\ phase = "applying"
  /\ apply_idx < Len(batch_events)
  /\ LET ev == batch_events[apply_idx + 1]
     IN  db' = IF ev.kind = "upsert"
               THEN db \union {ev.id}
               ELSE db \ {ev.id}
  /\ apply_idx' = apply_idx + 1
  /\ phase' = IF apply_idx + 1 = Len(batch_events) THEN "done" ELSE "applying"
  /\ UNCHANGED << jsonl, crash, batch_events >>

\* Crash at any point mid-operation.
Crash ==
  /\ phase \notin {"done", "idle"}
  /\ crash' = TRUE
  /\ phase' = "crashed"
  /\ UNCHANGED << jsonl, db, batch_events, apply_idx >>

\* Recovery: rebuild replays JSONL into DB.
Rebuild ==
  /\ phase  = "crashed"
  /\ db'    = Materialize_Inner(jsonl)
  /\ phase' = "idle"
  /\ crash' = FALSE
  /\ apply_idx' = 0
  /\ UNCHANGED << jsonl, batch_events >>

\* Reset to idle after done.
Reset ==
  /\ phase = "done"
  /\ phase' = "idle"
  /\ batch_events' = << >>
  /\ apply_idx'    = 0
  /\ UNCHANGED << jsonl, db, crash >>

\* Named NEXT relation for TLC cfg.
Next_InnerGap ==
  \/ \E eid \in EntryIds, nid \in EntryIds : Start(eid, nid)
  \/ \E nid \in EntryIds : StartNoReplace(nid)
  \/ AppendBatch
  \/ ApplyNext
  \/ Crash
  \/ Rebuild
  \/ Reset

\* Safety: at idle (between calls), DB == Materialize(JSONL).
Safety_DB_NotAhead ==
  phase = "idle" => db = Materialize_Inner(jsonl)

\* After crash+rebuild, invariant is restored (same as Safety_DB_NotAhead).
Safety_Rebuild_Restores ==
  (phase = "idle" /\ ~crash) => db = Materialize_Inner(jsonl)

Spec_InnerGap ==
  /\ Init_InnerGap
  /\ [][Next_InnerGap]_<<phase, batch_events, apply_idx, jsonl, db, crash>>

=================================================================================
