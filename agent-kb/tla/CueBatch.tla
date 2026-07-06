--------------------------------- MODULE CueBatch ---------------------------------
(*
  CueBatch.tla  —  Cue-anchor rows ride the upsert event atomically
  ==================================================================

  Memora-pickup task .4 adds cue anchors: per-entry semantic entry points
  stored as extra embedded rows.  Design under verification (locked here,
  implemented in kb_core::add / db::apply_event):

    * Cues are a FIELD of the upsert event — there are NO separate cue
      events in the JSONL.  One entry upsert = one event carrying its
      full cue set (possibly empty).
    * apply_event writes the entry row AND replaces its cue rows in the
      SAME SQLite transaction — a cue row can never be observed without
      its entry nor survive it.
    * expire removes the entry AND its cue rows in the same transaction.

  Hazards modelled:
    H1  crash between JSONL append and DB apply (inner gap, as InnerGap.tla)
    H2  orphan cue rows: cue row exists but its entry is not live
    H3  stale cue rows: entry re-upserted with new cue set, old rows linger

  Safety invariants (checked at idle):
    S1  db_entries = Materialize(jsonl)          — steady-state equality
    S2  DOMAIN of cue rows ⊆ live entries        — no orphans (H2)
    S3  cue rows = cue set of LAST upsert        — no staleness (H3)
*)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
  EntryIds,   (* finite set of entry ids *)
  CueIds,     (* finite set of possible cue anchor labels *)
  MaxLog      (* JSONL length bound for TLC termination *)

ASSUME MaxLog \in Nat /\ MaxLog >= 2

VARIABLES
  jsonl,        (* Seq of events: [kind, id, cues] — cues ignored for expire *)
  db_entries,   (* set of live entry ids *)
  db_cues,      (* function-like set of [entry |-> id, cue |-> label] rows *)
  phase,        (* idle | appended | applying | done | crashed *)
  batch_events,
  apply_idx,
  crash

Event == [kind : {"expire", "upsert"}, id : EntryIds, cues : SUBSET CueIds]

TypeOK_CueBatch ==
  /\ jsonl \in Seq(Event)
  /\ db_entries \subseteq EntryIds
  /\ db_cues \subseteq [entry : EntryIds, cue : CueIds]
  /\ phase \in {"idle", "appended", "applying", "done", "crashed"}
  /\ batch_events \in Seq(Event)
  /\ apply_idx \in Nat
  /\ crash \in BOOLEAN

Init_CueBatch ==
  /\ jsonl        = << >>
  /\ db_entries   = {}
  /\ db_cues      = {}
  /\ phase        = "idle"
  /\ batch_events = << >>
  /\ apply_idx    = 0
  /\ crash        = FALSE

(* --- Order-sensitive materialization over the event log --- *)

(* Apply one event to a [live, cues] state pair. Upsert replaces the entry's
   cue rows wholesale (transactional cue-set replace); expire removes both. *)
ApplyEv(state, ev) ==
  IF ev.kind = "upsert"
  THEN [ live |-> state.live \union {ev.id},
         cues |-> { r \in state.cues : r.entry # ev.id }
                   \union { [entry |-> ev.id, cue |-> c] : c \in ev.cues } ]
  ELSE [ live |-> state.live \ {ev.id},
         cues |-> { r \in state.cues : r.entry # ev.id } ]

RECURSIVE FoldLog(_, _)
FoldLog(log, state) ==
  IF log = << >>
  THEN state
  ELSE FoldLog(Tail(log), ApplyEv(state, Head(log)))

MaterializeState(log) == FoldLog(log, [live |-> {}, cues |-> {}])

(* --- Actions --- *)

\* replace_path add: one expire + one upsert-with-cues, single batch.
StartReplace(expire_id, new_id, cue_set) ==
  /\ phase = "idle"
  /\ Len(jsonl) + 2 <= MaxLog
  /\ expire_id # new_id
  /\ batch_events' = << [kind |-> "expire", id |-> expire_id, cues |-> {}],
                        [kind |-> "upsert", id |-> new_id, cues |-> cue_set] >>
  /\ phase' = "appended"
  /\ apply_idx' = 0
  /\ UNCHANGED << jsonl, db_entries, db_cues, crash >>

\* plain add (possibly re-upsert of an existing id with a NEW cue set — H3).
StartAdd(new_id, cue_set) ==
  /\ phase = "idle"
  /\ Len(jsonl) + 1 <= MaxLog
  /\ batch_events' = << [kind |-> "upsert", id |-> new_id, cues |-> cue_set] >>
  /\ phase' = "appended"
  /\ apply_idx' = 0
  /\ UNCHANGED << jsonl, db_entries, db_cues, crash >>

\* All batch events hit the JSONL in one atomic append (append_events_batch).
AppendBatch ==
  /\ phase = "appended"
  /\ jsonl' = jsonl \o batch_events
  /\ phase' = "applying"
  /\ UNCHANGED << db_entries, db_cues, crash, batch_events, apply_idx >>

\* Apply next event. Entry row and cue rows change in ONE step — this is the
\* transactional guarantee under verification. There is deliberately no state
\* where the entry is written but its cues are not.
ApplyNext ==
  /\ phase = "applying"
  /\ apply_idx < Len(batch_events)
  /\ LET ev == batch_events[apply_idx + 1]
         st == ApplyEv([live |-> db_entries, cues |-> db_cues], ev)
     IN  /\ db_entries' = st.live
         /\ db_cues'    = st.cues
  /\ apply_idx' = apply_idx + 1
  /\ phase' = IF apply_idx + 1 = Len(batch_events) THEN "done" ELSE "applying"
  /\ UNCHANGED << jsonl, crash, batch_events >>

Crash ==
  /\ phase \notin {"done", "idle"}
  /\ crash' = TRUE
  /\ phase' = "crashed"
  /\ UNCHANGED << jsonl, db_entries, db_cues, batch_events, apply_idx >>

\* Recovery: rebuild replays the full JSONL (entries and cue rows together).
Rebuild ==
  /\ phase = "crashed"
  /\ LET st == MaterializeState(jsonl)
     IN  /\ db_entries' = st.live
         /\ db_cues'    = st.cues
  /\ phase' = "idle"
  /\ crash' = FALSE
  /\ apply_idx' = 0
  /\ UNCHANGED << jsonl, batch_events >>

Reset ==
  /\ phase = "done"
  /\ phase' = "idle"
  /\ batch_events' = << >>
  /\ apply_idx' = 0
  /\ UNCHANGED << jsonl, db_entries, db_cues, crash >>

Next_CueBatch ==
  \/ \E eid \in EntryIds, nid \in EntryIds, cs \in SUBSET CueIds :
       StartReplace(eid, nid, cs)
  \/ \E nid \in EntryIds, cs \in SUBSET CueIds : StartAdd(nid, cs)
  \/ AppendBatch
  \/ ApplyNext
  \/ Crash
  \/ Rebuild
  \/ Reset

(* --- Safety invariants (all evaluated at idle) --- *)

\* S1: steady-state equality of live entries.
Safety_Entries_Materialize ==
  phase = "idle" => db_entries = MaterializeState(jsonl).live

\* S2 (H2): no orphan cue rows — every cue row's entry is live.
Safety_No_Orphan_Cues ==
  phase = "idle" => \A r \in db_cues : r.entry \in db_entries

\* S3 (H3): cue rows are exactly the materialized cue rows — re-upsert
\* replaced stale rows, expire removed them.
Safety_Cues_Materialize ==
  phase = "idle" => db_cues = MaterializeState(jsonl).cues

Spec_CueBatch ==
  /\ Init_CueBatch
  /\ [][Next_CueBatch]_<<jsonl, db_entries, db_cues, phase, batch_events, apply_idx, crash>>

=================================================================================
