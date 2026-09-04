------------------------- MODULE EvalSplitFreeze -------------------------
(* Sealed evaluation split freeze and query-log-driven audit sampling.

   The two concerns share Cases and Runs.  A single Freeze atomically assigns
   every golden case to Dev or Sealed.  Scoring records are append-only, so a
   state invariant also states the corresponding "ever scored" property.

   Audit sampling is per-run and two-armed, with UNIFORM-FIRST ordering: a run
   draws its whole uniform sample before the traffic arm draws anything, and
   the traffic arm then fills from what is left.  That is what keeps the
   uniform arm an unbiased time-series/strata estimator, and it is what the
   UNIQUE(run_id, entry_id) index already enforces in the database.  The
   ordering is modelled as a per-run samplingPhase: Uniform -> Traffic -> Done.
   The traffic arm's distribution is therefore conditioned on the uniform
   draw; that is by design, not a defect.

   Losing or corrupting the separate hit log is abstracted as hitLog="Absent".
   A run that meets an absent log DEGRADES to uniform-only; it never fails.
   "Failed" exists in the runStatus domain with no action that assigns it, so
   DegradeNotFail has something to falsify if a later edit adds one.

   Audit sampling is deliberately NOT guarded by the eval freeze: auditing and
   the sealed eval split are different subsystems that only share this file,
   and an audit run may legitimately precede a freeze.

   Snapshot idiom: prevUniformSet holds the uniform sets immediately before
   the last transition, which turns the "uniform arm is fixed once traffic
   starts" and "traffic never removes a uniform sample" requirements into
   ordinary TLC-checkable state invariants.
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
Phases == {"Uniform", "Traffic", "Done"}
RunStatuses == {"Ok", "Degraded", "Failed"}

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
    trafficSet,      \* [Runs -> SUBSET Cases]
    samplingPhase,   \* [Runs -> Phases], uniform-first ordering per run
    runStatus,       \* [Runs -> RunStatuses]
    prevUniformSet   \* uniformSet immediately before the last transition

evalVars  == <<frozen, split, frozenSplit, devScored, sealedScored>>
auditVars == <<hitLog, trafficMode, uniformCount, trafficCount,
               uniformSet, trafficSet, samplingPhase, runStatus>>
vars == <<evalVars, auditVars, prevUniformSet>>

CountOf(sets) ==
    Cardinality({pair \in Runs \X Cases : pair[2] \in sets[pair[1]]})

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
    /\ trafficSet \in [Runs -> SUBSET Cases]
    /\ samplingPhase \in [Runs -> Phases]
    /\ runStatus \in [Runs -> RunStatuses]
    /\ prevUniformSet \in [Runs -> SUBSET Cases]

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
    /\ trafficSet = [r \in Runs |-> {}]
    /\ samplingPhase = [r \in Runs |-> "Uniform"]
    /\ runStatus = [r \in Runs |-> "Ok"]
    /\ prevUniformSet = uniformSet

Snapshot == prevUniformSet' = uniformSet

Freeze ==
    /\ ~frozen
    /\ \E assignment \in [Cases -> {"Dev", "Sealed"}] :
           /\ split' = assignment
           /\ frozenSplit' = assignment
    /\ frozen' = TRUE
    /\ UNCHANGED <<devScored, sealedScored>>
    /\ UNCHANGED auditVars
    /\ Snapshot

(* Adversarial: a split rewrite that is legal only before the freeze.  It
   exists so that FreezeImmutable has something to catch if the guard is ever
   weakened; the rotation shape keeps its branching factor at one. *)
ReassignSplit ==
    /\ ~frozen
    /\ split' = [c \in Cases |-> IF split[c] = "Dev" THEN "Sealed" ELSE "Dev"]
    /\ UNCHANGED <<frozen, frozenSplit, devScored, sealedScored>>
    /\ UNCHANGED auditVars
    /\ Snapshot

DevRun ==
    /\ frozen
    /\ \E r \in Runs, scored \in SUBSET {c \in Cases : split[c] = "Dev"} :
           devScored' = [devScored EXCEPT ![r] = @ \union scored]
    /\ UNCHANGED <<frozen, split, frozenSplit, sealedScored>>
    /\ UNCHANGED auditVars
    /\ Snapshot

SealedRun ==
    /\ frozen
    /\ \E r \in Runs, scored \in SUBSET {c \in Cases : split[c] = "Sealed"} :
           sealedScored' = [sealedScored EXCEPT ![r] = @ \union scored]
    /\ UNCHANGED <<frozen, split, frozenSplit, devScored>>
    /\ UNCHANGED auditVars
    /\ Snapshot

EnableTraffic ==
    /\ ~trafficMode
    /\ trafficMode' = TRUE
    /\ UNCHANGED <<hitLog, uniformCount, trafficCount, uniformSet,
                    trafficSet, samplingPhase, runStatus>>
    /\ UNCHANGED evalVars
    /\ Snapshot

(* Uniform arm.  Enabled only in a run's Uniform phase, and independent of the
   hit log: a missing hit log must never disable it. *)
UniformSampleRun(r) ==
    /\ samplingPhase[r] = "Uniform"
    /\ \E c \in Cases \ (uniformSet[r] \union trafficSet[r]) :
           uniformSet' = [uniformSet EXCEPT ![r] = @ \union {c}]
    /\ uniformCount' = uniformCount + 1
    /\ UNCHANGED <<hitLog, trafficMode, trafficCount, trafficSet,
                    samplingPhase, runStatus>>
    /\ UNCHANGED evalVars
    /\ Snapshot

(* The run's uniform sample is complete and now frozen; traffic may draw from
   the remainder.  Entering Traffic with no hit log is already a degradation. *)
AdvancePhaseToTraffic(r) ==
    /\ samplingPhase[r] = "Uniform"
    /\ samplingPhase' = [samplingPhase EXCEPT ![r] = "Traffic"]
    /\ runStatus' = IF hitLog = "Absent"
                       THEN [runStatus EXCEPT ![r] = "Degraded"]
                       ELSE runStatus
    /\ UNCHANGED <<hitLog, trafficMode, uniformCount, trafficCount,
                    uniformSet, trafficSet>>
    /\ UNCHANGED evalVars
    /\ Snapshot

AdvancePhaseToDone(r) ==
    /\ samplingPhase[r] = "Traffic"
    /\ samplingPhase' = [samplingPhase EXCEPT ![r] = "Done"]
    /\ UNCHANGED <<hitLog, trafficMode, uniformCount, trafficCount,
                    uniformSet, trafficSet, runStatus>>
    /\ UNCHANGED evalVars
    /\ Snapshot

(* Traffic arm.  Additive, fills from what the uniform arm left, and requires
   the separate hit log.  Without that log the attempt degrades the run and
   samples nothing; it never fails the run and never touches the uniform arm. *)
TrafficSampleRun(r) ==
    /\ samplingPhase[r] = "Traffic"
    /\ trafficMode
    /\ IF hitLog = "Present"
          THEN /\ \E c \in Cases \ (uniformSet[r] \union trafficSet[r]) :
                      trafficSet' = [trafficSet EXCEPT ![r] = @ \union {c}]
               /\ trafficCount' = trafficCount + 1
               /\ UNCHANGED runStatus
          ELSE /\ runStatus' = [runStatus EXCEPT ![r] = "Degraded"]
               /\ UNCHANGED <<trafficSet, trafficCount>>
    /\ UNCHANGED <<hitLog, trafficMode, uniformCount, uniformSet,
                    samplingPhase>>
    /\ UNCHANGED evalVars
    /\ Snapshot

LoseHitLog ==
    /\ hitLog = "Present"
    /\ hitLog' = "Absent"
    /\ runStatus' = [r \in Runs |-> IF samplingPhase[r] = "Traffic"
                                       THEN "Degraded"
                                       ELSE runStatus[r]]
    /\ UNCHANGED <<trafficMode, uniformCount, trafficCount,
                    uniformSet, trafficSet, samplingPhase>>
    /\ UNCHANGED evalVars
    /\ Snapshot

Next ==
    \/ Freeze
    \/ ReassignSplit
    \/ DevRun
    \/ SealedRun
    \/ EnableTraffic
    \/ LoseHitLog
    \/ \E r \in Runs :
           \/ UniformSampleRun(r)
           \/ TrafficSampleRun(r)
           \/ AdvancePhaseToTraffic(r)
           \/ AdvancePhaseToDone(r)

(* (1) No sealed case is present in the append-only DevRun history. *)
SealedNeverInDevRun ==
    \A r \in Runs : \A c \in devScored[r] : split[c] # "Sealed"

(* (2) The captured assignment remains exact after Freeze. *)
FreezeImmutable ==
    frozen => split = frozenSplit

(* (3) Accounting identities: the reported counts are exactly the sizes of the
   append-only per-run sample sets. *)
UniformCountExact == uniformCount = CountOf(uniformSet)
TrafficCountExact == trafficCount = CountOf(trafficSet)

(* (4) The uniform arm only ever grows, in count and in membership. *)
UniformCountMonotone == CountOf(prevUniformSet) <= uniformCount
TrafficNeverRemovesUniform ==
    \A r \in Runs : prevUniformSet[r] \subseteq uniformSet[r]

(* (5) Reports retain arm identity by storing disjoint per-run sets. *)
ArmsSeparate ==
    \A r \in Runs : uniformSet[r] \intersect trafficSet[r] = {}

(* (6) Uniform-first: once a run has left its Uniform phase, its uniform
   sample is fixed.  This is what makes the uniform arm an unbiased estimator
   even though the traffic arm draws from the same run. *)
UniformFixedOnceTraffic ==
    \A r \in Runs :
        samplingPhase[r] # "Uniform" => uniformSet[r] = prevUniformSet[r]

(* (7) A missing hit log degrades a run; no path fails one. *)
DegradeNotFail ==
    \A r \in Runs : runStatus[r] # "Failed"

DegradedWhenLogLost ==
    hitLog = "Absent" =>
        \A r \in Runs : samplingPhase[r] = "Traffic" => runStatus[r] = "Degraded"

(* (8) A missing hit log disables only the traffic arm.  Stated as genuine
   enabledness so that adding a hit-log guard to the uniform arm breaks it. *)
UniformNeverBlockedByHitLog ==
    \A r \in Runs :
        (/\ samplingPhase[r] = "Uniform"
         /\ (Cases \ (uniformSet[r] \union trafficSet[r])) # {})
            => ENABLED UniformSampleRun(r)

Spec == Init /\ [][Next]_vars

=============================================================================
