------------------------- MODULE EvalSplitFreeze -------------------------
(* Sealed evaluation split freeze and query-log-driven audit sampling.

   The two concerns share Cases and Runs.  A single Freeze atomically assigns
   every golden case to Dev or Sealed.  Scoring records are append-only, so a
   state invariant also states the corresponding "ever scored" property.

   Audit samples have separate Uniform and Traffic arms.  Traffic is additive
   and requires both traffic mode and the separate hit log.  Losing or
   corrupting that log is abstracted as hitLog="Absent"; uniform sampling
   remains enabled and the run does not fail.  Re-sampling an existing uniform
   case is an enabled stuttering step, keeping this finite model live after all
   distinct uniform samples have already been selected.
*)

EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    MaxCases,
    MaxRuns

ASSUME MaxCases \in Nat /\ MaxCases > 0
ASSUME MaxRuns  \in Nat /\ MaxRuns  > 0

Cases == 1..MaxCases
Runs  == 1..MaxRuns
Splits == {"Unassigned", "Dev", "Sealed"}

VARIABLES
    frozen,          \* whether the one atomic split assignment has occurred
    split,           \* [Cases -> Splits]
    frozenSplit,     \* immutable snapshot captured by Freeze
    devScored,       \* [Runs -> SUBSET Cases], append-only DevRun results
    sealedScored,    \* [Runs -> SUBSET Cases], append-only SealedRun results
    hitLog,          \* separate-file log availability: Present or Absent
    trafficMode,     \* additive traffic arm has been enabled
    uniformCount,    \* cumulative number of distinct uniform-arm samples
    trafficCount,    \* cumulative number of distinct traffic-arm samples
    uniformSet,      \* [Runs -> SUBSET Cases]
    uniformHistory,  \* ghost: every uniform sample ever reported per run
    trafficSet       \* [Runs -> SUBSET Cases]

vars == <<frozen, split, frozenSplit, devScored, sealedScored,
          hitLog, trafficMode, uniformCount, trafficCount,
          uniformSet, uniformHistory, trafficSet>>

TypeInvariant ==
    /\ frozen \in BOOLEAN
    /\ split \in [Cases -> Splits]
    /\ frozenSplit \in [Cases -> Splits]
    /\ devScored \in [Runs -> SUBSET Cases]
    /\ sealedScored \in [Runs -> SUBSET Cases]
    /\ hitLog \in {"Present", "Absent"}
    /\ trafficMode \in BOOLEAN
    /\ uniformCount \in 0..(MaxCases * MaxRuns)
    /\ trafficCount \in 0..(MaxCases * MaxRuns)
    /\ uniformSet \in [Runs -> SUBSET Cases]
    /\ uniformHistory \in [Runs -> SUBSET Cases]
    /\ trafficSet \in [Runs -> SUBSET Cases]

Init ==
    /\ frozen = FALSE
    /\ split = [c \in Cases |-> "Unassigned"]
    /\ frozenSplit = split
    /\ devScored = [r \in Runs |-> {}]
    /\ sealedScored = [r \in Runs |-> {}]
    /\ hitLog = "Present"
    /\ trafficMode = FALSE
    /\ uniformCount = 0
    /\ trafficCount = 0
    /\ uniformSet = [r \in Runs |-> {}]
    /\ uniformHistory = [r \in Runs |-> {}]
    /\ trafficSet = [r \in Runs |-> {}]

Freeze ==
    /\ ~frozen
    /\ \E assignment \in [Cases -> {"Dev", "Sealed"}] :
           /\ split' = assignment
           /\ frozenSplit' = assignment
    /\ frozen' = TRUE
    /\ UNCHANGED <<devScored, sealedScored, hitLog, trafficMode,
                    uniformCount, trafficCount, uniformSet, uniformHistory,
                    trafficSet>>

DevRun ==
    /\ frozen
    /\ \E r \in Runs, scored \in SUBSET {c \in Cases : split[c] = "Dev"} :
           devScored' = [devScored EXCEPT ![r] = @ \union scored]
    /\ UNCHANGED <<frozen, split, frozenSplit, sealedScored,
                    hitLog, trafficMode, uniformCount, trafficCount,
                    uniformSet, uniformHistory, trafficSet>>

SealedRun ==
    /\ frozen
    /\ \E r \in Runs, scored \in SUBSET {c \in Cases : split[c] = "Sealed"} :
           sealedScored' = [sealedScored EXCEPT ![r] = @ \union scored]
    /\ UNCHANGED <<frozen, split, frozenSplit, devScored,
                    hitLog, trafficMode, uniformCount, trafficCount,
                    uniformSet, uniformHistory, trafficSet>>

EnableTraffic ==
    /\ ~trafficMode
    /\ trafficMode' = TRUE
    /\ UNCHANGED <<frozen, split, frozenSplit, devScored, sealedScored,
                    hitLog, uniformCount, trafficCount, uniformSet,
                    uniformHistory, trafficSet>>

UniformSample ==
    \* Mirror guard: within one run each case is attributed to exactly one arm.
    \E r \in Runs : \E c \in Cases \ trafficSet[r] :
        LET isNew == c \notin uniformSet[r]
        IN /\ uniformSet' = [uniformSet EXCEPT ![r] = @ \union {c}]
           /\ uniformHistory' = [uniformHistory EXCEPT ![r] = @ \union {c}]
           /\ uniformCount' = uniformCount + (IF isNew THEN 1 ELSE 0)
           /\ UNCHANGED <<frozen, split, frozenSplit, devScored, sealedScored,
                           hitLog, trafficMode, trafficCount, trafficSet>>

TrafficSample ==
    /\ trafficMode
    /\ hitLog = "Present"
    /\ \E r \in Runs : \E c \in Cases \ uniformSet[r] :
           LET isNew == c \notin trafficSet[r]
           IN /\ trafficSet' = [trafficSet EXCEPT ![r] = @ \union {c}]
              /\ trafficCount' = trafficCount + (IF isNew THEN 1 ELSE 0)
    /\ UNCHANGED <<frozen, split, frozenSplit, devScored, sealedScored,
                    hitLog, trafficMode, uniformCount, uniformSet,
                    uniformHistory>>

LoseHitLog ==
    /\ hitLog = "Present"
    /\ hitLog' = "Absent"
    /\ UNCHANGED <<frozen, split, frozenSplit, devScored, sealedScored,
                    trafficMode, uniformCount, trafficCount,
                    uniformSet, uniformHistory, trafficSet>>

Next ==
    \/ Freeze
    \/ DevRun
    \/ SealedRun
    \/ EnableTraffic
    \/ UniformSample
    \/ TrafficSample
    \/ LoseHitLog

(* (1) No sealed case is present in the append-only DevRun history. *)
SealedNeverInDevRun ==
    \A r \in Runs : \A c \in devScored[r] : split[c] # "Sealed"

(* (2) The captured assignment remains exact after Freeze. *)
FreezeImmutable ==
    frozen => split = frozenSplit

(* (3) Counts are derived from append-only sample sets and cannot decrease. *)
UniformMonotone ==
    uniformCount =
        Cardinality({pair \in Runs \X Cases : pair[2] \in uniformHistory[pair[1]]})

(* (4) Traffic is additive: its actions never alter the uniform arm. *)
TrafficNeverRemovesUniform ==
    \A r \in Runs : uniformHistory[r] \subseteq uniformSet[r]

(* (5) Reports retain arm identity by storing disjoint per-run sets. *)
ArmsSeparate ==
    \A r \in Runs : uniformSet[r] \intersect trafficSet[r] = {}

(* (6) Missing hit-log data disables only TrafficSample, never UniformSample.
   Modulo saturation: when every case in every run is already attributed to an
   arm, there is nothing left to sample and quiescence is not a failure. *)
DegradeNotFail ==
    hitLog = "Absent" =>
        \/ ENABLED UniformSample
        \/ \A r \in Runs : Cases \subseteq (uniformSet[r] \union trafficSet[r])

Spec == Init /\ [][Next]_vars

=============================================================================
