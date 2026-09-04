------------------------- MODULE CompactMaterialize -------------------------
(*
  CompactMaterialize.tla — C1/T0c (`bd-21ef.1.5`)
  ==============================================

  Scope: the *compaction / materialization* half of C1.  It owns counterexamples
  CE5 (`run_history` positional cap) and CE7 (a compacted log that still
  validates against a `(offset, tail_sha)` cursor).

  Durability framing (per-line append, commit markers, fsync ordering, crash)
  belongs to `DurableBatch.tla` (T0a); the three-phase rebuild belongs to
  `RebuildProtocol.tla` (T0b).  `CrossBatch.tla` is retained unchanged as the
  coarse-grained boundary regression gate and is superseded for durability —
  see `decisions/crossbatch-disposition.md`.

  MODEL
  -----
  log      : the JSONL event log, a sequence of events.
  db       : the materialized SQLite state — entry set + a *count* per run id,
             because the current `run_history` arm is a bare `INSERT`
             (`db.rs:946-957`) and therefore duplicates rows on replay.
  applied  : ghost — how many log events have actually been applied to `db`.
             `applied` is the truth; `cursor.off` is the DB's *claim* about it.
  cursor   : the D3 applied cursor `(generation, offset, tail_sha)` held in
             `kb_meta`.  `tail_sha` is modelled as the event value at `offset`;
             that is the sound abstraction of a collision-free hash of the last
             committed line, and it is deliberately *not* a whole-prefix hash —
             D3's complaint against revision 1 is precisely that the tail hash is
             "strictly weaker than the whole-prefix hash rebuild already uses".
  gen      : the log's generation counter.  Bumped by `Compact` only under the
             fixed design.
  opened   : TRUE exactly in the state produced by `Open` (recovery).

  DESIGN SELECTOR
  ---------------
  `Fixed \in BOOLEAN` selects the design under check.  Three things differ:

    | aspect                  | Fixed = FALSE (current)   | Fixed = TRUE (D5/D3)     |
    |-------------------------|---------------------------|--------------------------|
    | `run_history` apply     | bare INSERT, count + 1    | keyed, count := 1        |
    | compaction retention    | last `Cap` run events     | every run event          |
    | `Compact`               | leaves `gen` alone        | `gen := gen + 1`         |
    | cursor validation       | offset + tail only        | + `cursor.gen = gen`     |

  `Cap = 2` stands in for the real `RUN_HISTORY_CAP = 500` (`compact.rs:16`) so
  the model closes; nothing in the model depends on the cap's magnitude, only on
  its existence.

  MODELLING ASSUMPTIONS (stated, not hidden)
  ------------------------------------------
  A1. Offsets are *event counts*, not byte counts.  Byte offsets are a monotone
      injective function of the applied prefix, so an event-count offset is
      faithful for every property checked here.  A consequence worth naming:
      under this abstraction compaction alone can never move an already-applied
      event *later* in the log, so the double-apply in CE7 requires the DB to be
      ahead of its cursor.  That state is not hypothetical — it is exactly what
      the six non-cursor writers named in D3 (`expire.rs:77`,
      `stale_check.rs:257`, `test_add.rs:68`, `run.rs:60`, `mcp.rs:921`,
      `migrate_citations.rs:302`) produce today, and what D7 version skew
      reproduces even after T4 lands.  `ApplyUntracked` models them, and it stays
      enabled under BOTH designs because an old binary can always do it.
  A2. `Materialize` is a pure function of the log — this is what R1 (D6) buys and
      what every other spec in this directory already assumes.
  A3. No crash action.  Torn tails, commit markers and fsync ordering are
      `DurableBatch.tla`'s obligation; duplicating them here would only enlarge
      the state space without touching CE5 or CE7.
  A4. Entry staleness (`is_stale`) and evidence retention are out of scope; they
      are covered by `AgentKbEvidence.tla`.  The entry arm here is upsert/expire
      only, which is the part compaction's retention rule reasons about.
*)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    EntryIds,   \* entry ids available to the model
    RunIds,     \* run_history run_id values available to the model
    MaxLog,     \* bound: maximum number of events ever appended
    Cap,        \* stand-in for RUN_HISTORY_CAP (compact.rs:16)
    Fixed,      \* FALSE = current design, TRUE = D3/D5 fixed design
    SkewEnabled \* enables the non-cursor writers (see A1); a state-space bound

ASSUME Fixed \in BOOLEAN
ASSUME SkewEnabled \in BOOLEAN
ASSUME MaxLog \in Nat /\ Cap \in Nat
ASSUME EntryIds \cap RunIds = {}

----------------------------------------------------------------------------
(* Events *)

NoEv        == [kind |-> "none", id |-> "none"]
EntryEvents == [kind : {"upsert", "expire"}, id : EntryIds]
RunEvents   == [kind : {"run"},              id : RunIds]
Events      == EntryEvents \cup RunEvents

\* A run row can be inserted at most once per append and once per replay.
MaxCount == 2 * MaxLog

DBStates == [entries : SUBSET EntryIds, runs : [RunIds -> 0..MaxCount]]
EmptyDB  == [entries |-> {}, runs |-> [r \in RunIds |-> 0]]

VARIABLES
    log,        \* Seq(Events)
    nappends,   \* ghost: total events ever appended.  Compaction shortens the
                \* log, so `Len(log) < MaxLog` alone would let a model
                \* append/compact/append forever and leave `gen` unbounded.
                \* MaxLog bounds the number of appends over the WHOLE behaviour.
    db,         \* DBStates
    applied,    \* ghost: number of log events actually applied to db
    cursor,     \* [gen, off, tail]
    gen,        \* log generation
    opened      \* TRUE in the state produced by Open

vars == <<log, nappends, db, applied, cursor, gen, opened>>

----------------------------------------------------------------------------
(* Materialization — a pure function of the log (A2). *)

\* One apply step.  The run arm is the design-selected `db.rs:946-957` behaviour.
ApplyOne(d, e) ==
    CASE e.kind = "upsert" -> [d EXCEPT !.entries = @ \union {e.id}]
      [] e.kind = "expire" -> [d EXCEPT !.entries = @ \ {e.id}]
      [] OTHER             -> [d EXCEPT !.runs[e.id] =
                                  IF Fixed THEN 1 ELSE @ + 1]

RECURSIVE ApplySeq(_, _)
ApplySeq(d, s) ==
    IF s = << >> THEN d ELSE ApplySeq(ApplyOne(d, Head(s)), Tail(s))

Materialize(l) == ApplySeq(EmptyDB, l)

----------------------------------------------------------------------------
(* Compaction — models `compact.rs:186-283`. *)

\* An upsert survives only if it is the last upsert for its id ...
LastUpsert(l, i) ==
    ~\E j \in (i+1)..Len(l) : l[j].kind = "upsert" /\ l[j].id = l[i].id

\* ... and no expire for that id follows it (`expire_last > entry_last`).
NoLaterExpire(l, i) ==
    ~\E j \in (i+1)..Len(l) : l[j].kind = "expire" /\ l[j].id = l[i].id

\* Expire events themselves are never retained: they are not in any of the four
\* `retained_indices` sources in `compact.rs`.
KeptEntries(l) ==
    { i \in 1..Len(l) :
        /\ l[i].kind = "upsert"
        /\ LastUpsert(l, i)
        /\ NoLaterExpire(l, i) }

RunIdx(l) == { i \in 1..Len(l) : l[i].kind = "run" }

\* Current: `run_indices[len - RUN_HISTORY_CAP ..]` — the newest `Cap` only.
\* Fixed (D5.2): the positional cap is removed outright.
KeptRuns(l) ==
    IF Fixed
    THEN RunIdx(l)
    ELSE { i \in RunIdx(l) : Cardinality({ j \in RunIdx(l) : j > i }) < Cap }

Retained(l) == KeptEntries(l) \cup KeptRuns(l)

RECURSIVE FilterIdx(_, _, _)
FilterIdx(l, keep, i) ==
    IF i > Len(l)      THEN << >>
    ELSE IF i \in keep THEN <<l[i]>> \o FilterIdx(l, keep, i+1)
    ELSE                    FilterIdx(l, keep, i+1)

CompactFn(l) == FilterIdx(l, Retained(l), 1)

----------------------------------------------------------------------------
(* Cursor validation — the D3 recovery table, reduced to this module's rows. *)

\* The recorded tail hash must still name the line ending at `offset`.
TailMatches ==
    IF cursor.off = 0            THEN TRUE
    ELSE IF cursor.off > Len(log) THEN FALSE
    ELSE log[cursor.off] = cursor.tail

CursorValid ==
    /\ cursor.off <= Len(log)      \* else: full rebuild (offset > committed_len)
    /\ TailMatches                 \* tail_sha check
    /\ (Fixed => cursor.gen = gen) \* D3 generation check

----------------------------------------------------------------------------
(* Actions *)

\* Append a committed event to the log without applying it.
\* A `run_id` is minted fresh per run (`run.rs:45`, `mcp.rs:915`), so the same
\* run event never appears twice in one log; the guard encodes that and keeps the
\* reachable set focused on distinct runs.  Idempotence under *replay* is what
\* D5.1 buys, and replay is `Open`, not a duplicated log line.
AppendEvent(e) ==
    /\ nappends < MaxLog
    /\ (e.kind = "run" => ~\E i \in 1..Len(log) : log[i] = e)
    /\ log' = log \o <<e>>
    /\ nappends' = nappends + 1
    /\ opened' = FALSE
    /\ UNCHANGED <<db, applied, cursor, gen>>

\* The T4 writer: apply and advance the cursor in one transaction.  Every write
\* path recovers first (D3), so it is guarded on a valid cursor.
ApplyTracked ==
    /\ applied < Len(log)
    /\ CursorValid
    /\ LET e == log[applied + 1] IN
        /\ db'     = ApplyOne(db, e)
        /\ cursor' = [gen |-> gen, off |-> applied + 1, tail |-> e]
    /\ applied' = applied + 1
    /\ opened'  = FALSE
    /\ UNCHANGED <<log, nappends, gen>>

\* The six non-cursor writers (D3) and any D7-skewed old binary: apply without
\* touching the cursor.  Enabled under both designs; see assumption A1.
\* `SkewEnabled` is a state-space bound, never an interleaving constraint: it
\* selects which writers exist in a given model, and TLC still explores every
\* interleaving of the writers that remain.  CE5's configs switch it off so that
\* the only mechanism left that can break materialization is the compaction cap.
ApplyUntracked ==
    /\ SkewEnabled
    /\ applied < Len(log)
    /\ db'      = ApplyOne(db, log[applied + 1])
    /\ applied' = applied + 1
    /\ opened'  = FALSE
    /\ UNCHANGED <<log, nappends, cursor, gen>>

\* `kb compact`: rewrite the log, touch neither the DB nor the cursor.
Compact ==
    /\ CompactFn(log) # log
    /\ log'     = CompactFn(log)
    /\ applied' = Cardinality({ i \in Retained(log) : i <= applied })
    /\ gen'     = IF Fixed THEN gen + 1 ELSE gen
    /\ opened'  = FALSE
    /\ UNCHANGED <<db, nappends, cursor>>

\* `recover_if_needed`: validate the cursor, then either replay the tail or
\* fall back to a full rebuild.
Open ==
    /\ LET rebuild == ~CursorValid
           tailEv  == IF Len(log) = 0 THEN NoEv ELSE log[Len(log)]
       IN /\ db' = IF rebuild
                   THEN Materialize(log)
                   ELSE ApplySeq(db, SubSeq(log, cursor.off + 1, Len(log)))
          /\ cursor' = [gen |-> gen, off |-> Len(log), tail |-> tailEv]
    /\ applied' = Len(log)
    /\ opened'  = TRUE
    /\ UNCHANGED <<log, nappends, gen>>

Init ==
    /\ log      = << >>
    /\ nappends = 0
    /\ db      = EmptyDB
    /\ applied = 0
    /\ cursor  = [gen |-> 0, off |-> 0, tail |-> NoEv]
    /\ gen     = 0
    /\ opened  = FALSE

Next ==
    \/ \E e \in Events : AppendEvent(e)
    \/ ApplyTracked
    \/ ApplyUntracked
    \/ Compact
    \/ Open

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* State constraint, used only by the CE7Strict configs.

   It prunes every state in which the DB has applied something while the cursor
   still reads offset 0.  Such states ARE reachable in production — a rebuild
   over an empty log writes offset 0, and a D7-skewed old binary then appends and
   applies without touching the cursor — but a reviewer is entitled to ask
   whether CE7 depends on that weak starting point.  Pruning states can only hide
   violations, never invent them, so a counterexample found under this constraint
   is a fortiori a counterexample of the unconstrained model. *)

StrictCursor == (applied = 0) \/ (cursor.off > 0)

----------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
    /\ log \in Seq(Events)
    /\ Len(log) <= MaxLog
    /\ nappends \in 0..MaxLog
    /\ db \in DBStates
    /\ applied \in 0..MaxLog
    /\ cursor \in [gen : 0..MaxLog, off : 0..MaxLog, tail : Events \cup {NoEv}]
    /\ gen \in 0..MaxLog
    /\ opened \in BOOLEAN

\* Core invariant.  When the cursor agrees with the log generation and claims the
\* DB is at or past the end of the log, nothing further will ever be replayed —
\* so the DB must already equal what the log materializes to.
MaterializationInvariant ==
    (cursor.gen = gen /\ cursor.off >= Len(log)) => db = Materialize(log)

\* Recovery must converge: the state a process opens is log-current.
RecoveryConverges == opened => db = Materialize(log)

\* Compaction must be a materialization-preserving log rewrite (Principle 1).
CompactionPreservesMaterialization ==
    Materialize(CompactFn(log)) = Materialize(log)

\* CE7, deep form.  BFS returns the shallowest witness for RecoveryConverges,
\* which has `cursor.off = 0`.  This invariant asks the strictly harder question:
\* is there a divergent post-recovery state whose cursor had actually committed a
\* line (`off > 0`) and whose generation still matches?  Violated => yes, the
\* defect is not an artefact of a zero offset.  Held => no such state exists.
NoDivergentCommittedCursor ==
    ~(opened /\ cursor.off > 0 /\ cursor.gen = gen /\ db # Materialize(log))

----------------------------------------------------------------------------
(* Non-vacuity witnesses.  Each is *deliberately false* on the fixed model; TLC
   reporting it as violated proves the corresponding invariant is not vacuous. *)

\* Violated => the guard of MaterializationInvariant is reachable with a
\* non-empty log, so the implication has real work to do.
NV_MaterializationGuard ==
    ~(cursor.gen = gen /\ cursor.off >= Len(log) /\ Len(log) > 0)

\* Violated => Open is reachable, so RecoveryConverges is not vacuous.
NV_Opened == ~opened

\* Violated => compaction genuinely rewrites some reachable log, so
\* CompactionPreservesMaterialization is not an identity on the reachable set.
NV_CompactionNonTrivial == CompactFn(log) = log

\* Violated => reachable DB states are non-empty, so TypeOK's db conjunct
\* constrains something.
NV_DBPopulated == db = EmptyDB

----------------------------------------------------------------------------
(* Deliberate violation, for the TypeOK non-vacuity config.  Mirrors
   DurableBatch.tla's BadTypeInit: an out-of-range `cursor.off` at Init proves
   TypeOK actually excludes states, rather than holding vacuously because
   nothing here can produce one. *)

BadTypeInit ==
    /\ log      = << >>
    /\ nappends = 0
    /\ db       = EmptyDB
    /\ applied  = 0
    /\ cursor   = [gen |-> 0, off |-> MaxLog + 7, tail |-> NoEv]
    /\ gen      = 0
    /\ opened   = FALSE

SpecBadType == BadTypeInit /\ [][Next]_vars

=============================================================================
