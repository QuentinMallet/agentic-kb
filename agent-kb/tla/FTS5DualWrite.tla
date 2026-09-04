--------------------------------- MODULE FTS5DualWrite ---------------------------------
(*
  FTS5DualWrite.tla  —  FTS5 dual-write migration safety
  =======================================================

  Models the migration from a contentless FTS5 table (`entries_fts`) to a
  content-mirroring FTS5 table (`entries_fts_v2`, content='entries' with
  triggers).  During migration both tables are dual-written via triggers
  fired by `apply_event`.  A `read_path` flag controls which table search
  queries hit; flipping `read_path` is the cutover (and rollback) step.

  Three safety invariants modelled here:

    1. Idempotency_Invariant
       Applying the same upsert event N times to entries_fts_v2 leaves the
       FTS table in the same state as applying it once.  Required because
       `rebuild` may replay events.  Mechanism in code: INSERT OR REPLACE
       in the trigger.  Modelled here as set-union semantics.

    2. DualWrite_Consistency
       At every apply_event boundary, both FTS tables equal Materialize(jsonl).
       I.e., FTS content mirrors the live entries set which mirrors the
       JSONL log.  Same shape as the InnerGap/CrossBatch boundary invariant.

    3. Rollback_Safety
       When read_path flips from "content_entries" back to "contentless",
       both FTS tables contain the same set of live entry IDs.  Dual-write
       keeps them in sync, so the flip is always safe.

  State variables:
    jsonl       — sequence of [kind, id] events (the durable log)
    entries_v1  — contentless FTS table contents (set of live IDs)
    entries_v2  — content-mirroring FTS table contents (set of live IDs)
    phase       — lifecycle marker: "idle" | "applying" | "rebuilding"
    read_path   — current read source: "contentless" | "content_entries"

  MaxLog bounds the JSONL length so TLC terminates in finite time.
*)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS EntryIds, MaxLog

VARIABLES jsonl, entries_v1, entries_v2, phase, read_path

vars_FTS5 == << jsonl, entries_v1, entries_v2, phase, read_path >>

TypeOK_FTS5 ==
  /\ phase \in {"idle", "applying", "rebuilding"}
  /\ read_path \in {"contentless", "content_entries"}
  /\ jsonl \in Seq([kind : {"expire","upsert"}, id : EntryIds])
  /\ entries_v1 \subseteq EntryIds
  /\ entries_v2 \subseteq EntryIds

Init_FTS5 ==
  /\ phase       = "idle"
  /\ read_path   = "contentless"
  /\ jsonl       = << >>
  /\ entries_v1  = {}
  /\ entries_v2  = {}

\* Order-sensitive materialization: fold over the log left-to-right.
\* Same shape as InnerGap.FoldLog and CrossBatch.FoldLog_CB.
RECURSIVE FoldLog_FTS(_, _)
FoldLog_FTS(log, live) ==
  IF log = << >>
  THEN live
  ELSE LET hd == Head(log)
           tl == Tail(log)
           next == IF hd.kind = "upsert"
                   THEN live \union {hd.id}
                   ELSE live \ {hd.id}
       IN  FoldLog_FTS(tl, next)

Materialize_FTS(log) == FoldLog_FTS(log, {})

\* ApplyUpsert: append an upsert event to jsonl and dual-write both FTS tables.
\* INSERT OR REPLACE semantics in the trigger means re-applying the same id
\* is idempotent (set-union is the perfect abstraction).
ApplyUpsert(id) ==
  /\ phase = "idle"
  /\ Len(jsonl) + 1 <= MaxLog
  /\ jsonl'      = Append(jsonl, [kind |-> "upsert", id |-> id])
  /\ entries_v1' = entries_v1 \union {id}
  /\ entries_v2' = entries_v2 \union {id}
  /\ UNCHANGED << phase, read_path >>

\* ApplyExpire: append an expire event and remove id from both FTS tables.
ApplyExpire(id) ==
  /\ phase = "idle"
  /\ Len(jsonl) + 1 <= MaxLog
  /\ jsonl'      = Append(jsonl, [kind |-> "expire", id |-> id])
  /\ entries_v1' = entries_v1 \ {id}
  /\ entries_v2' = entries_v2 \ {id}
  /\ UNCHANGED << phase, read_path >>

\* ReapplyUpsert: re-fire the upsert trigger for an id that is currently
\* present in entries_v2.  Models INSERT OR REPLACE on a row that already
\* exists — the SQL semantics is "replace with identical content", a no-op.
\* This is the per-event trigger idempotency property: applying an upsert
\* twice in a row leaves the FTS table unchanged.  Does NOT append to jsonl.
\* Note: this is distinct from "replay the full log" (modelled by Rebuild),
\* which is itself idempotent because Materialize is a pure function of jsonl.
ReapplyUpsert(id) ==
  /\ phase = "idle"
  /\ id \in entries_v2
  /\ entries_v1' = entries_v1 \union {id}
  /\ entries_v2' = entries_v2 \union {id}
  /\ UNCHANGED << jsonl, phase, read_path >>

\* Rebuild: truncate both FTS tables, then replay the entire log.  Models the
\* `rebuild` command.  Goes through an intermediate "rebuilding" phase so the
\* DualWrite_Consistency invariant is only checked at idle boundaries.
RebuildStart ==
  /\ phase = "idle"
  /\ phase' = "rebuilding"
  /\ entries_v1' = {}
  /\ entries_v2' = {}
  /\ UNCHANGED << jsonl, read_path >>

RebuildFinish ==
  /\ phase = "rebuilding"
  /\ phase' = "idle"
  /\ entries_v1' = Materialize_FTS(jsonl)
  /\ entries_v2' = Materialize_FTS(jsonl)
  /\ UNCHANGED << jsonl, read_path >>

\* FlipForward: cutover read_path from contentless to content_entries.
FlipForward ==
  /\ phase = "idle"
  /\ read_path = "contentless"
  /\ read_path' = "content_entries"
  /\ UNCHANGED << jsonl, entries_v1, entries_v2, phase >>

\* FlipBack: rollback read_path from content_entries back to contentless.
\* Rollback_Safety requires both tables agree on the live set at this point.
FlipBack ==
  /\ phase = "idle"
  /\ read_path = "content_entries"
  /\ read_path' = "contentless"
  /\ UNCHANGED << jsonl, entries_v1, entries_v2, phase >>

\* Named NEXT relation for TLC cfg.
Next_FTS5 ==
  \/ \E id \in EntryIds : ApplyUpsert(id)
  \/ \E id \in EntryIds : ApplyExpire(id)
  \/ \E id \in EntryIds : ReapplyUpsert(id)
  \/ RebuildStart
  \/ RebuildFinish
  \/ FlipForward
  \/ FlipBack

\* ---------------------------------------------------------------------
\* Invariants
\* ---------------------------------------------------------------------

\* Idempotency_Invariant: while idle, entries_v2 equals Materialize(jsonl).
\* Re-applying an upsert that is already materialized must not change v2.
\* Since ReapplyUpsert is enabled only when the id is already in the log,
\* set-union with that id is a no-op when v2 already mirrors the log.
\* The invariant captures the post-condition: v2 stays consistent with the log.
Idempotency_Invariant ==
  phase = "idle" => entries_v2 = Materialize_FTS(jsonl)

\* DualWrite_Consistency: both FTS tables mirror the JSONL log at every
\* apply_event boundary (i.e. when phase = "idle").
DualWrite_Consistency ==
  phase = "idle" =>
    /\ entries_v1 = Materialize_FTS(jsonl)
    /\ entries_v2 = Materialize_FTS(jsonl)

\* Rollback_Safety: at any point where read_path could be flipped (i.e. at
\* idle), both FTS tables hold the same live set.  Dual-write maintains
\* this, so flipping read_path in either direction is safe.
Rollback_Safety ==
  phase = "idle" => entries_v1 = entries_v2

Spec_FTS5 ==
  /\ Init_FTS5
  /\ [][Next_FTS5]_vars_FTS5

=================================================================================
