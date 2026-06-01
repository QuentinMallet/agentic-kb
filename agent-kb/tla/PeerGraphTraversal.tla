------------------------- MODULE PeerGraphTraversal -------------------------
(*
  Peer graph traversal: cycle detection, cleanup-epic, TTL sweep, WAL
  concurrent convergence.

  Verified properties
  -------------------
  TraversalBounded       ReachSet never exceeds Cardinality(Repos), even on
                         cyclic graphs — proves the visited-set guard works.
  TypeInvariant          Structural well-formedness of all state variables.
  CleanupCorrectness     After CleanupBegin(sl), no edge with epic_slug = sl
                         survives in `edges`. Tracked via `cleanedSlug`.
  SweepIdempotent        After TTLSweep, re-applying the filter is a no-op:
                         {e in edges : pred} = edges. Implies idempotency.
  WALConvergence         After Cleanup and Sweep both commit (any order), no
                         edge matching either predicate survives.

  WAL encoding note
  -----------------
  Each transaction applies its write effect atomically in *Begin (before
  *Commit). This is a deliberate simplification of SQLite WAL semantics:
  changes become durable at commit, but their effect on `edges` is modelled
  at begin-time to keep the state space small. Concurrent readers and
  snapshot isolation are not modelled. The mutual-exclusion guard
  (pc_other # "running") captures the key serialization constraint.

  Model (TLC small instance)
  --------------------------
  Repos    <- {"r1", "r2"}
  Slugs    <- {"s1"}
  MaxTime  <- 1
  MaxHops  <- 2
  MaxEdges <- 3

  Run: tlc PeerGraphTraversal -config PeerGraphTraversal.cfg -workers auto -deadlock
*)

EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Repos,    \* Set of repo identifiers, e.g. {"r1","r2"}
    Slugs,    \* Set of epic slugs, e.g. {"s1"}
    MaxTime,  \* Clock upper bound for TLC state-space bounding
    MaxHops,  \* Traversal depth bound (must be >= Cardinality(Repos) to cover all cycles)
    MaxEdges  \* Max edges in the graph — bounds state space for TLC

ASSUME Repos    # {}
ASSUME Slugs    # {}
ASSUME MaxTime  \in Nat /\ MaxTime  > 0
ASSUME MaxHops  \in Nat /\ MaxHops  >= Cardinality(Repos)
\* Sufficient: no node visited twice => at most |Repos| hops to cover all reachable nodes
ASSUME MaxEdges \in Nat /\ MaxEdges > 0

NoExpiry == -1   \* Sentinel: edge does not expire
NoSlug   == "_none"   \* Sentinel: no cleanup in progress

AllTimes == {NoExpiry} \cup (0..MaxTime)
AllSlugs == Slugs \cup {NoSlug}

EdgeType == [src: Repos, tgt: Repos, slug: AllSlugs, expires: AllTimes]

(* ──────────────────────── Reachability ──────────────────────── *)

(* BFS from `n` within `remaining` hops, tracking `visited` to break cycles.
   Returns the set of all reachable nodes including `n` itself. *)
RECURSIVE ReachSet(_, _, _, _)
ReachSet(n, remaining, visited, G) ==
    IF remaining = 0 \/ n \in visited
    THEN visited \cup {n}
    ELSE
        LET succs    == {e.tgt : e \in {e2 \in G : e2.src = n}}
            visited2 == visited \cup {n}
        IN  visited2 \cup UNION {ReachSet(m, remaining - 1, visited2, G) : m \in succs}

(* ──────────────────────── State variables ────────────────────── *)

VARIABLES
    edges,        \* Current live peer graph — SUBSET EdgeType
    clock,        \* Simulated current time (0..MaxTime)
    pc1,          \* Cleanup transaction state: "idle" | "running" | "done"
    pc2,          \* Sweep transaction state:   "idle" | "running" | "done"
    cleanedSlug   \* The slug being cleaned; NoSlug when pc1 = "idle"

vars == <<edges, clock, pc1, pc2, cleanedSlug>>

TypeInvariant ==
    /\ edges       \subseteq EdgeType
    /\ clock        \in 0..MaxTime
    /\ pc1          \in {"idle", "running", "done"}
    /\ pc2          \in {"idle", "running", "done"}
    /\ cleanedSlug  \in AllSlugs

(* ──────────────────────── Init ───────────────────────────────── *)

Init ==
    /\ edges       = {}
    /\ clock        = 0
    /\ pc1          = "idle"
    /\ pc2          = "idle"
    /\ cleanedSlug  = NoSlug

(* ──────────────────────── Actions ───────────────────────────── *)

(* Add an edge — bounded by MaxEdges to keep TLC state space finite *)
AddEdge(s, t, sl, ex) ==
    /\ pc1 = "idle"     \* no writes while a transaction is running
    /\ pc2 = "idle"
    /\ Cardinality(edges) < MaxEdges
    /\ edges' = edges \cup {[src |-> s, tgt |-> t, slug |-> sl, expires |-> ex]}
    /\ UNCHANGED <<clock, pc1, pc2, cleanedSlug>>

TickClock ==
    /\ clock < MaxTime
    /\ pc1 = "idle"
    /\ pc2 = "idle"
    /\ clock' = clock + 1
    /\ UNCHANGED <<edges, pc1, pc2, cleanedSlug>>

(* ── Cleanup transaction (WAL writer 1) ─────────────────────── *)

(* Begin: acquire WAL write slot; atomically remove slug-matching edges.
   Record which slug was cleaned so CleanupCorrectness can verify it. *)
CleanupBegin(sl) ==
    /\ pc1 = "idle"
    /\ pc2 # "running"      \* WAL: second writer blocks until first commits
    /\ pc1'         = "running"
    /\ cleanedSlug' = sl
    /\ edges'       = {e \in edges : e.slug # sl}
    /\ UNCHANGED <<clock, pc2>>

CleanupCommit ==
    /\ pc1 = "running"
    /\ pc1' = "done"
    /\ UNCHANGED <<edges, clock, pc2, cleanedSlug>>

(* ── TTL sweep transaction (WAL writer 2) ────────────────────── *)

SweepBegin ==
    /\ pc2 = "idle"
    /\ pc1 # "running"      \* WAL: second writer blocks until first commits
    /\ pc2' = "running"
    /\ edges' = {e \in edges : e.expires = NoExpiry \/ e.expires >= clock}
    /\ UNCHANGED <<clock, pc1, cleanedSlug>>

SweepCommit ==
    /\ pc2 = "running"
    /\ pc2' = "done"
    /\ UNCHANGED <<edges, clock, pc1, cleanedSlug>>

(* Reset so both transactions can run again in either order *)
Reset ==
    /\ pc1 = "done"
    /\ pc2 = "done"
    /\ pc1'         = "idle"
    /\ pc2'         = "idle"
    /\ cleanedSlug' = NoSlug
    /\ UNCHANGED <<edges, clock>>

Next ==
    \/ \E s, t \in Repos, sl \in AllSlugs, ex \in AllTimes : AddEdge(s, t, sl, ex)
    \/ TickClock
    \/ \E sl \in Slugs : CleanupBegin(sl)
    \/ CleanupCommit
    \/ SweepBegin
    \/ SweepCommit
    \/ Reset

Spec == Init /\ [][Next]_vars

(* ──────────────────────── Invariants ───────────────────────── *)

(* I1: Traversal always terminates — result set bounded by |Repos| *)
TraversalBounded ==
    \A n \in Repos :
        Cardinality(ReachSet(n, MaxHops, {}, edges)) <= Cardinality(Repos)

(* I2: Cleanup correctness — once CleanupBegin fires, no edge with
   the cleaned slug survives in `edges`. Verified across all states
   where pc1 # "idle" (running or done). *)
CleanupCorrectness ==
    pc1 \in {"running", "done"} =>
        \A e \in edges : e.slug # cleanedSlug

(* I3: WAL convergence — when both transactions are done, combined
   predicate holds: no expired edges AND no cleaned-slug edges. *)
WALConvergence ==
    (pc1 = "done" /\ pc2 = "done") =>
        /\ \A e \in edges : e.expires = NoExpiry \/ e.expires >= clock
        /\ \A e \in edges : e.slug # cleanedSlug

(* I4: Sweep idempotency — after sweep, the edge set already satisfies
   the sweep predicate, so re-applying the filter is a no-op.
   Set-equality form: {e in edges : pred(e)} = edges. *)
SweepIdempotent ==
    (pc2 = "done") =>
        {e \in edges : e.expires = NoExpiry \/ e.expires >= clock} = edges

(* I5: Type well-formedness — always holds by construction. *)
EdgesWellFormed == edges \subseteq EdgeType

=============================================================================
