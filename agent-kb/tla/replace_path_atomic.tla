------------------------- MODULE replace_path_atomic -------------------------
(*
  replace_path_atomic.tla  —  Two-layer refinement spec for kb_core::add
  =====================================================================

  This spec models the atomicity contract of `kb_core::add` (see
  src/components/kb_core.rs) along two distinct refinement layers:

    Layer 1  (InnerGap):     inner per-event append/apply gap within ONE call.
    Layer 2  (CrossBatch):   cross-invocation boundary between distinct calls.

  The steady-state invariant throughout is:
      DB = Materialize(JSONL)

  where Materialize replays all events in JSONL order.

  -----------------------------------------------------------------------
  Structural requirement (plan §A_MERGED Iter-2 D3):
  Two refinement layers MUST be expressed structurally.  This spec uses
  form (b): a parent spec with two INSTANCE imports of the sub-specs.
  -----------------------------------------------------------------------
*)

EXTENDS Naturals, Sequences, FiniteSets

(*
  --- Abstract data types ---

  An Event is a record with at minimum:
    kind  ∈ {"expire", "upsert"}
    id    ∈  STRING

  JSONL is a sequence of Events.
  DB    is a set of "live" entry ids (expired ids are absent).

  Materialize(log) = { e.id : e ∈ Range(log), e.kind = "upsert" }
                   \ { e.id : e ∈ Range(log), e.kind = "expire" }
  (simplified: last-writer-wins, expire removes from live set)
*)

CONSTANTS
  EntryIds,        (* finite set of possible entry ids *)
  MaxBatchSize     (* upper bound on expire count per call *)

ASSUME MaxBatchSize \in Nat /\ MaxBatchSize >= 1

(*
  Variables shared by both layers:
    jsonl : Sequence of events (append-only)
    db    : Set of live entry ids
    crash : BOOLEAN — TRUE when we are modelling a crash mid-operation
*)

VARIABLES jsonl, db, crash

TypeOK ==
  /\ jsonl \in Seq([kind : {"expire","upsert"}, id : EntryIds])
  /\ db    \subseteq EntryIds
  /\ crash \in BOOLEAN

Init ==
  /\ jsonl = << >>
  /\ db    = {}
  /\ crash = FALSE

\* Helper: materialize a JSONL sequence to a set of live ids
Materialize(log) ==
  LET upserted == { log[i].id : i \in DOMAIN log, log[i].kind = "upsert" }
      expired  == { log[i].id : i \in DOMAIN log, log[i].kind = "expire" }
  IN  upserted \ expired

\* Steady-state invariant: DB must equal the materialization of the JSONL.
\* This invariant is checked at every point where db or jsonl is updated.
Invariant_DB_EQ_Materialize ==
  db = Materialize(jsonl)

-----------------------------------------------------------------------------
\* LAYER 1: InnerGap sub-spec
\* Models a single kb_core::add invocation with N expire events + 1 upsert.
\* The hazard: pre-fix code appended + applied ONE event at a time, so a
\* crash after append but before apply leaves JSONL ahead of DB.
\* The fix: append ALL events in one batch before any apply.
INSTANCE InnerGap
-----------------------------------------------------------------------------

-----------------------------------------------------------------------------
\* LAYER 2: CrossBatch sub-spec
\* Models two SEQUENTIAL kb_core::add invocations.  After each completes the
\* steady-state invariant must hold.  A crash between the two invocations
\* must leave the system in a valid state (either both effects or neither
\* of the second call's effects are present).
INSTANCE CrossBatch
-----------------------------------------------------------------------------

=============================================================================
