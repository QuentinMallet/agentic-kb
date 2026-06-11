-------------------------------- MODULE CrossBatch --------------------------------
(*
  CrossBatch.tla  —  Layer 2 refinement: cross-invocation boundary
  ================================================================

  Models two SEQUENTIAL kb_core::add calls (Call A, then Call B).
  Each call is atomic at this layer (inner-gap is modelled by InnerGap.tla).

  Boundary invariant: at every boundary state DB = Materialize(JSONL).

  Materialize is order-sensitive (last-writer-wins): a later upsert for an id
  that was previously expired will re-add it.  We use the same FoldLog
  approach as InnerGap to get this right.
*)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS EntryIds, MaxBatchSize

VARIABLES jsonl, db, phase_cb

TypeOK_CrossBatch ==
  /\ phase_cb \in {"before_a", "after_a", "after_b"}
  /\ jsonl \in Seq([kind : {"expire","upsert"}, id : EntryIds])
  /\ db \subseteq EntryIds

Init_CrossBatch ==
  /\ phase_cb = "before_a"
  /\ jsonl    = << >>
  /\ db       = {}

\* Order-sensitive materialization — same semantics as InnerGap.FoldLog.
RECURSIVE FoldLog_CB(_, _)
FoldLog_CB(log, live) ==
  IF log = << >>
  THEN live
  ELSE LET hd == Head(log)
           tl == Tail(log)
           next == IF hd.kind = "upsert"
                   THEN live \union {hd.id}
                   ELSE live \ {hd.id}
       IN  FoldLog_CB(tl, next)

Materialize_CB(log) == FoldLog_CB(log, {})

\* Commit Call A: expire one entry, upsert a new one.
CommitCallA(expire_id, new_id_a) ==
  /\ phase_cb   = "before_a"
  /\ expire_id  # new_id_a
  /\ LET batch_a == << [kind |-> "expire", id |-> expire_id],
                        [kind |-> "upsert", id |-> new_id_a] >>
     IN
     /\ jsonl' = jsonl \o batch_a
     /\ db'    = (db \ {expire_id}) \union {new_id_a}
  /\ phase_cb' = "after_a"

\* Commit Call A with no replace.
CommitCallA_NoReplace(new_id_a) ==
  /\ phase_cb  = "before_a"
  /\ LET batch_a == << [kind |-> "upsert", id |-> new_id_a] >>
     IN
     /\ jsonl' = jsonl \o batch_a
     /\ db'    = db \union {new_id_a}
  /\ phase_cb' = "after_a"

\* Commit Call B: expire one entry, upsert a new one.
CommitCallB(expire_id, new_id_b) ==
  /\ phase_cb  = "after_a"
  /\ expire_id # new_id_b
  /\ LET batch_b == << [kind |-> "expire", id |-> expire_id],
                        [kind |-> "upsert", id |-> new_id_b] >>
     IN
     /\ jsonl' = jsonl \o batch_b
     /\ db'    = (db \ {expire_id}) \union {new_id_b}
  /\ phase_cb' = "after_b"

\* Commit Call B with no replace.
CommitCallB_NoReplace(new_id_b) ==
  /\ phase_cb  = "after_a"
  /\ LET batch_b == << [kind |-> "upsert", id |-> new_id_b] >>
     IN
     /\ jsonl' = jsonl \o batch_b
     /\ db'    = db \union {new_id_b}
  /\ phase_cb' = "after_b"

\* Named NEXT for TLC cfg.
Next_CrossBatch ==
  \/ \E eid \in EntryIds, nid \in EntryIds : CommitCallA(eid, nid)
  \/ \E nid \in EntryIds : CommitCallA_NoReplace(nid)
  \/ \E eid \in EntryIds, nid \in EntryIds : CommitCallB(eid, nid)
  \/ \E nid \in EntryIds : CommitCallB_NoReplace(nid)

\* Boundary invariant: at each boundary state DB = Materialize(JSONL).
Boundary_Invariant ==
  phase_cb \in {"before_a", "after_a", "after_b"} =>
    db = Materialize_CB(jsonl)

Spec_CrossBatch ==
  /\ Init_CrossBatch
  /\ [][Next_CrossBatch]_<<phase_cb, jsonl, db>>

=================================================================================
