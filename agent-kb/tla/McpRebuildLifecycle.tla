----------------------- MODULE McpRebuildLifecycle ------------------------
EXTENDS Naturals, TLC

(***************************************************************************
This model describes the lifecycle boundary around `kb rebuild`, not Rust's
replay algorithm. A direct child can exist before it owns the per-store
lifetime lock. Therefore two transient OS PIDs may overlap across OTP-owner
replacement, but only the lock holder may rebuild, emit READY, or receive the
successful `kb_rebuild` acknowledgement. A busy contender emits bounded ERROR
and exits before replay.

The lock belongs to the Rust process and is released at OS child exit, not at
Elixir's later observation of that exit. OwnerDeathKillsChild is an explicit
native-mechanism assumption for the inherited-stdin EOF guard; TLC cannot prove
BEAM or kernel behaviour. Detached and WrongStore configurations must still
violate their named invariants.
***************************************************************************)

CONSTANTS MaxRequests, MaxLogBytes, LaunchResults, McpStore, WrongStore,
          CoalescingSafe, OwnerDeathKillsChild, ExactStoreBinding,
          AcknowledgesFailedLaunch, OutputBounded

VARIABLES phase, owner, child, contender, lockHolder, active, observed,
          pending, terminationRequested, requests, freshRequests, attempt,
          acknowledgedAttempt, workerStore, acceptedAttempt, report, logBytes,
          lastFrame

vars == <<phase, owner, child, contender, lockHolder, active, observed,
          pending, terminationRequested, requests, freshRequests, attempt,
          acknowledgedAttempt, workerStore, acceptedAttempt, report, logBytes,
          lastFrame>>

Phases == {"idle", "launching", "running", "cancelling", "unknown", "orphaned",
           "success", "failed", "stopped"}
Children == {"none", "live", "exited"}
LockHolders == {"none", "incumbent"}
Owners == {"old", "new", "none"}
Reports == {"none", "success", "failure", "cancelled", "stopped"}
Frames == {"none", "ready", "error"}

LogAdd(n) == IF n < MaxLogBytes THEN n + 1 ELSE n

Init ==
  /\ phase = "idle" /\ owner = "old"
  /\ child = "none" /\ contender = "none" /\ lockHolder = "none"
  /\ active = 0 /\ observed = TRUE /\ pending = TRUE
  /\ terminationRequested = FALSE /\ requests = 1 /\ freshRequests = 1
  /\ attempt = 0 /\ acknowledgedAttempt = 0 /\ workerStore = "none"
  /\ acceptedAttempt = 0 /\ report = "none" /\ logBytes = 0 /\ lastFrame = "none"

(******************************************************************************
Only ClientRequest creates a fresh launch ticket. Coalescing is permitted only
after READY: before READY, a synchronous request has no successful ack.
******************************************************************************)
ClientRequest ==
  /\ owner # "none" /\ phase \in {"idle", "success", "failed"} /\ ~pending
  /\ requests < MaxRequests
  /\ pending' = TRUE /\ requests' = requests + 1 /\ freshRequests' = freshRequests + 1
  /\ UNCHANGED <<phase, owner, child, contender, lockHolder, active, observed,
                 terminationRequested, attempt, acknowledgedAttempt, workerStore,
                 acceptedAttempt, report, logBytes, lastFrame>>

CoalesceRequest ==
  /\ owner = "old" /\ phase = "running" /\ child = "live" /\ lockHolder = "incumbent"
  /\ requests < MaxRequests
  /\ requests' = requests + 1
  /\ active' = IF CoalescingSafe THEN active ELSE active + 1
  /\ UNCHANGED <<phase, owner, child, contender, lockHolder, observed, pending,
                 terminationRequested, freshRequests, attempt, acknowledgedAttempt,
                 workerStore, acceptedAttempt, report, logBytes, lastFrame>>

(******************************************************************************
Launch opens a child but deliberately does not acknowledge the MCP request.
AcquireLifetimeLockAndReady is the only action that admits it to rebuild.
******************************************************************************)
Launch ==
  /\ owner # "none" /\ phase \in {"idle", "success", "failed"} /\ pending
  /\ attempt < MaxRequests
  /\ \E result \in LaunchResults:
       IF result = "accepted"
       THEN /\ phase' = "launching" /\ child' = "live" /\ active' = 0
            /\ observed' = FALSE /\ pending' = FALSE /\ attempt' = attempt + 1
            /\ workerStore' = IF ExactStoreBinding THEN McpStore ELSE WrongStore
            /\ report' = "none" /\ lastFrame' = "none" /\ logBytes' = LogAdd(logBytes)
            /\ UNCHANGED <<acknowledgedAttempt, acceptedAttempt>>
       ELSE /\ phase' = "failed" /\ child' = "none" /\ active' = 0
            /\ observed' = TRUE /\ pending' = FALSE /\ attempt' = attempt + 1
            /\ acknowledgedAttempt' = IF AcknowledgesFailedLaunch THEN attempt + 1
                                      ELSE acknowledgedAttempt
            /\ workerStore' = "none" /\ report' = "failure" /\ lastFrame' = "error"
            /\ logBytes' = LogAdd(logBytes) /\ UNCHANGED acceptedAttempt
  /\ lockHolder' = "none" /\ contender' = "none"
  /\ UNCHANGED <<owner, terminationRequested, requests, freshRequests>>

AcquireLifetimeLockAndReady ==
  /\ owner # "none" /\ phase = "launching" /\ child = "live" /\ lockHolder = "none"
  /\ phase' = "running" /\ lockHolder' = "incumbent" /\ active' = 1
  /\ acceptedAttempt' = attempt /\ acknowledgedAttempt' = attempt
  /\ lastFrame' = "ready" /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<owner, child, contender, observed, pending, terminationRequested,
                 requests, freshRequests, attempt, workerStore, report>>

(******************************************************************************
A replacement owner may open another Rust process while the incumbent still
holds the kernel lock. The nonblocking lock attempt is abstracted as atomic:
while this busy contender is live, it must emit ERROR and exit before the old
guard may release the incumbent lock. Thus it cannot be silently discarded or
turn into a later READY/replay attempt.
******************************************************************************)
ReplacementOwner ==
  /\ owner = "none" /\ phase = "orphaned"
  /\ owner' = "new"
  /\ UNCHANGED <<phase, child, contender, lockHolder, active, observed, pending,
                 terminationRequested, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt, report,
                 logBytes, lastFrame>>

SpawnBusyContender ==
  /\ owner = "new" /\ phase = "orphaned" /\ child = "live"
  /\ lockHolder = "incumbent" /\ contender = "none" /\ requests < MaxRequests
  /\ contender' = "live" /\ requests' = requests + 1
  /\ freshRequests' = freshRequests + 1 /\ attempt' = attempt + 1
  /\ UNCHANGED <<phase, owner, child, lockHolder, active, observed, pending,
                 terminationRequested, acknowledgedAttempt, workerStore,
                 acceptedAttempt, report, logBytes, lastFrame>>

RejectBusyContender ==
  /\ contender = "live" /\ lockHolder = "incumbent"
  /\ contender' = "exited" /\ lastFrame' = "error" /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<phase, owner, child, lockHolder, active, observed, pending,
                 terminationRequested, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt, report>>

Finish ==
  /\ owner = "old" /\ phase = "running" /\ child = "live" /\ lockHolder = "incumbent"
  /\ \E outcome \in {"success", "failed"}:
       /\ phase' = outcome /\ child' = "exited" /\ lockHolder' = "none" /\ active' = 0
       /\ observed' = TRUE /\ pending' = FALSE /\ terminationRequested' = FALSE
       /\ report' = IF outcome = "success" THEN "success" ELSE "failure"
       /\ logBytes' = LogAdd(logBytes) /\ lastFrame' = "none"
  /\ UNCHANGED <<owner, contender, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt>>

Cancel ==
  /\ owner = "old" /\ phase = "running" /\ child = "live" /\ lockHolder = "incumbent"
  /\ phase' = "cancelling" /\ terminationRequested' = TRUE
  /\ UNCHANGED <<owner, child, contender, lockHolder, active, observed, pending,
                 requests, freshRequests, attempt, acknowledgedAttempt, workerStore,
                 acceptedAttempt, report, logBytes, lastFrame>>

ObserveTermination ==
  /\ owner = "old" /\ phase = "cancelling" /\ child = "live" /\ lockHolder = "incumbent"
  /\ phase' = "idle" /\ child' = "exited" /\ lockHolder' = "none" /\ active' = 0
  /\ observed' = TRUE /\ terminationRequested' = FALSE /\ pending' = FALSE
  /\ report' = "cancelled" /\ logBytes' = LogAdd(logBytes) /\ lastFrame' = "none"
  /\ UNCHANGED <<owner, contender, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt>>

OwnerDeath ==
  /\ owner = "old" /\ child = "live" /\ lockHolder = "incumbent"
  /\ owner' = "none" /\ phase' = "orphaned" /\ pending' = FALSE
  /\ terminationRequested' = FALSE /\ report' = "stopped" /\ lastFrame' = "none"
  /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<requests, freshRequests, attempt, acknowledgedAttempt,
                 workerStore, acceptedAttempt, child, contender, lockHolder,
                 active, observed>>

(* The guard is enabled by the native mechanism assumption. It releases the
   Rust-held lock only when the OS child exits; replacement observation follows. *)
GuardEOFExit ==
  /\ OwnerDeathKillsChild /\ owner \in {"none", "new"}
  /\ phase = "orphaned" /\ child = "live" /\ lockHolder = "incumbent"
  /\ contender # "live"
  /\ child' = "exited" /\ lockHolder' = "none" /\ active' = 0
  /\ observed' = FALSE /\ lastFrame' = "none" /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<phase, owner, contender, pending, terminationRequested,
                 requests, freshRequests, attempt, acknowledgedAttempt,
                 workerStore, acceptedAttempt, report>>

ObserveGuardExit ==
  /\ owner = "new" /\ phase = "orphaned" /\ child = "exited"
  /\ lockHolder = "none" /\ ~observed
  /\ phase' = "idle" /\ observed' = TRUE /\ report' = "stopped"
  /\ UNCHANGED <<owner, child, contender, lockHolder, active, pending,
                 terminationRequested, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt, logBytes,
                 lastFrame>>

StopAfterGuardExit ==
  /\ owner = "none" /\ phase = "orphaned" /\ child = "exited"
  /\ lockHolder = "none"
  /\ phase' = "stopped" /\ observed' = TRUE
  /\ UNCHANGED <<owner, child, contender, lockHolder, active, pending,
                 terminationRequested, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt, report,
                 logBytes, lastFrame>>

CancellationTimesOut ==
  /\ owner = "old" /\ phase = "cancelling" /\ child = "live"
  /\ lockHolder = "incumbent"
  /\ phase' = "unknown" /\ report' = "failure"
  /\ UNCHANGED <<owner, child, contender, lockHolder, active, observed, pending,
                 terminationRequested, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt, logBytes,
                 lastFrame>>

ObserveUnknownExit ==
  /\ owner = "old" /\ phase = "unknown" /\ child = "live"
  /\ lockHolder = "incumbent"
  /\ phase' = "idle" /\ child' = "exited" /\ lockHolder' = "none" /\ active' = 0
  /\ observed' = TRUE /\ terminationRequested' = FALSE /\ report' = "failure"
  /\ lastFrame' = "none"
  /\ UNCHANGED <<owner, contender, pending, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt, logBytes>>

(* Rust stdout is limited to READY/ERROR control frames and stderr is
   suppressed before work. This counter abstracts Elixir's capped retained
   status/error log; it is not a proof of a concrete rotation implementation. *)
OutputControlFrame ==
  /\ owner # "none" /\ (child = "live" \/ contender = "live")
  /\ logBytes' = IF OutputBounded THEN LogAdd(logBytes) ELSE logBytes + 1
  /\ UNCHANGED <<phase, owner, child, contender, lockHolder, active, observed,
                 pending, terminationRequested, requests, freshRequests, attempt,
                 acknowledgedAttempt, workerStore, acceptedAttempt, report, lastFrame>>

Next == ClientRequest \/ CoalesceRequest \/ Launch \/ AcquireLifetimeLockAndReady \/
        ReplacementOwner \/ SpawnBusyContender \/ RejectBusyContender \/
        Finish \/ Cancel \/ ObserveTermination \/ OwnerDeath \/ GuardEOFExit \/
        ObserveGuardExit \/ StopAfterGuardExit \/ CancellationTimesOut \/
        ObserveUnknownExit \/ OutputControlFrame

Spec == Init /\ [][Next]_vars /\ WF_vars(Launch) /\ WF_vars(AcquireLifetimeLockAndReady) /\
        WF_vars(Finish) /\ WF_vars(ObserveTermination) /\ WF_vars(GuardEOFExit) /\
        WF_vars(ObserveGuardExit) /\ WF_vars(ObserveUnknownExit) /\
        WF_vars(StopAfterGuardExit) /\ WF_vars(RejectBusyContender)

TypeOK ==
  /\ phase \in Phases /\ owner \in Owners /\ child \in Children /\ contender \in Children
  /\ lockHolder \in LockHolders /\ active \in 0..2 /\ observed \in BOOLEAN
  /\ pending \in BOOLEAN /\ terminationRequested \in BOOLEAN
  /\ requests \in 0..MaxRequests /\ freshRequests \in 0..MaxRequests
  /\ attempt \in 0..MaxRequests /\ acknowledgedAttempt \in 0..MaxRequests
  /\ acceptedAttempt \in 0..MaxRequests /\ workerStore \in {"none", McpStore, WrongStore}
  /\ report \in Reports /\ logBytes \in Nat /\ lastFrame \in Frames

(* `active` counts lock-owning replay work, not transient OS PIDs. *)
critical_AtMostOneActiveRebuild == active <= 1
critical_OnlyLockHolderRebuilds == active = 1 <=> /\ child = "live" /\ lockHolder = "incumbent" /\ phase \in {"running", "cancelling", "unknown", "orphaned"}
critical_ReadyRequiresLifetimeLock ==
  /\ acknowledgedAttempt = acceptedAttempt
  /\ (acknowledgedAttempt = attempt /\ phase \in {"launching", "running"})
     => lockHolder = "incumbent"
critical_ReadyFrameRequiresLifetimeLock ==
  lastFrame = "ready" => lockHolder = "incumbent"
critical_CoalescedRequestDoesNotCreateLaunchTicket == attempt <= freshRequests
critical_CancellationObservesExitBeforeFreshSlot ==
  phase = "idle" => /\ active = 0 /\ child # "live" /\ observed /\ ~terminationRequested
critical_NoLiveContenderAtFreshSlot ==
  phase = "idle" => contender # "live"
critical_NoOrphanAfterOwnerDeath ==
  phase = "stopped" => /\ active = 0 /\ child # "live" /\ contender # "live" /\ observed
critical_ExactStoreBinding == active > 0 => workerStore = McpStore
critical_BoundedLogState == logBytes <= MaxLogBytes
critical_AcknowledgementOnlyAfterAcceptedLaunch == acknowledgedAttempt = acceptedAttempt

EventuallyQuiescent == <>(phase \in {"idle", "success", "failed", "stopped"} /\ ~pending)
EventuallyGuardReapsOrphan == [](owner = "none" => <>(child # "live" /\ lockHolder = "none"))

=============================================================================
