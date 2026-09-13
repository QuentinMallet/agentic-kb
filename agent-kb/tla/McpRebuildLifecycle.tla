----------------------- MODULE McpRebuildLifecycle ------------------------
EXTENDS Naturals, TLC

(***************************************************************************
The model is the lifecycle boundary around `kb rebuild`, not Rust's rebuild
algorithm.  A client request is either accepted after an OS child exists or
fails before an acknowledgement.  Duplicate requests coalesce.  Cancellation
does not free the slot until an observed child exit.  No transition retries a
failed attempt: a later ClientRequest is required.

OwnerDeathKillsChild is an explicit native-mechanism assumption.  It models a
directly-owned port plus a parent-death guard; TLC cannot prove Unix/BEAM
kernel behaviour.  The Detached configuration turns it off and must violate
critical_NoOrphanAfterOwnerDeath.  ExactStoreBinding similarly models the
chosen path-binding mechanism; WrongStore must violate its invariant.
***************************************************************************)

CONSTANTS MaxRequests, MaxLogBytes, LaunchResults, McpStore, WrongStore,
          CoalescingSafe, OwnerDeathKillsChild, ExactStoreBinding

VARIABLES phase, owner, child, active, observed, pending, terminationRequested,
          requests, freshRequests, attempt, acknowledgedAttempt, workerStore,
          report, logBytes

vars == <<phase, owner, child, active, observed, pending, terminationRequested,
          requests, freshRequests, attempt, acknowledgedAttempt, workerStore,
          report, logBytes>>

Phases == {"idle", "running", "cancelling", "success", "failed", "stopped"}
Children == {"none", "live", "exited"}
Reports == {"none", "success", "failure", "cancelled", "stopped"}
Terminal == {"success", "failed", "stopped"}

LogAdd(n) == IF n < MaxLogBytes THEN n + 1 ELSE n

Init ==
  /\ phase = "idle" /\ owner = TRUE
  /\ child = "none" /\ active = 0 /\ observed = TRUE
  /\ pending = TRUE /\ terminationRequested = FALSE
  /\ requests = 1 /\ freshRequests = 1 /\ attempt = 0
  /\ acknowledgedAttempt = 0 /\ workerStore = "none"
  /\ report = "none" /\ logBytes = 0

(******************************************************************************
Only a client request creates a fresh launch ticket.  Coalesced requests have
the same acknowledgement as the active attempt and cannot create a child.
******************************************************************************)
ClientRequest ==
  /\ owner /\ phase \in {"idle", "success", "failed"} /\ ~pending
  /\ requests < MaxRequests
  /\ pending' = TRUE /\ requests' = requests + 1
  /\ freshRequests' = freshRequests + 1
  /\ UNCHANGED <<phase, owner, child, active, observed, terminationRequested,
                 attempt, acknowledgedAttempt, workerStore, report, logBytes>>

CoalesceRequest ==
  /\ owner /\ phase = "running" /\ child = "live" /\ requests < MaxRequests
  /\ requests' = requests + 1
  /\ active' = IF CoalescingSafe THEN active ELSE active + 1
  /\ UNCHANGED <<phase, owner, child, observed, pending, terminationRequested,
                 freshRequests, attempt, acknowledgedAttempt, workerStore,
                 report, logBytes>>

Launch ==
  /\ owner /\ phase \in {"idle", "success", "failed"} /\ pending
  /\ attempt < MaxRequests
  /\ \E result \in LaunchResults:
       IF result = "accepted"
       THEN /\ phase' = "running" /\ child' = "live" /\ active' = 1
            /\ observed' = FALSE /\ pending' = FALSE
            /\ attempt' = attempt + 1 /\ acknowledgedAttempt' = attempt + 1
            /\ workerStore' = IF ExactStoreBinding THEN McpStore ELSE WrongStore
            /\ report' = "none" /\ logBytes' = LogAdd(logBytes)
       ELSE /\ phase' = "failed" /\ child' = "none" /\ active' = 0
            /\ observed' = TRUE /\ pending' = FALSE
            /\ attempt' = attempt + 1 /\ UNCHANGED acknowledgedAttempt
            /\ workerStore' = "none" /\ report' = "failure"
            /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<owner, terminationRequested, requests, freshRequests>>

Finish ==
  /\ owner /\ phase = "running" /\ child = "live"
  /\ \E outcome \in {"success", "failed"}:
       /\ phase' = outcome /\ child' = "exited" /\ active' = 0
       /\ observed' = TRUE /\ pending' = FALSE /\ terminationRequested' = FALSE
       /\ report' = IF outcome = "success" THEN "success" ELSE "failure"
       /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<owner, requests, freshRequests, attempt, acknowledgedAttempt,
                 workerStore>>

Cancel ==
  /\ owner /\ phase = "running" /\ child = "live"
  /\ phase' = "cancelling" /\ terminationRequested' = TRUE
  /\ UNCHANGED <<owner, child, active, observed, pending, requests,
                 freshRequests, attempt, acknowledgedAttempt, workerStore,
                 report, logBytes>>

ObserveTermination ==
  /\ owner /\ phase = "cancelling" /\ child = "live"
  /\ phase' = "idle" /\ child' = "exited" /\ active' = 0
  /\ observed' = TRUE /\ terminationRequested' = FALSE /\ pending' = FALSE
  /\ report' = "cancelled" /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<owner, requests, freshRequests, attempt, acknowledgedAttempt,
                 workerStore>>

OwnerDeath ==
  /\ owner
  /\ owner' = FALSE /\ phase' = "stopped" /\ pending' = FALSE
  /\ terminationRequested' = FALSE /\ report' = "stopped"
  /\ IF OwnerDeathKillsChild
     THEN /\ child' = "exited" /\ active' = 0 /\ observed' = TRUE
     ELSE UNCHANGED <<child, active, observed>>
  /\ logBytes' = LogAdd(logBytes)
  /\ UNCHANGED <<requests, freshRequests, attempt, acknowledgedAttempt,
                 workerStore>>

Next == ClientRequest \/ CoalesceRequest \/ Launch \/ Finish \/ Cancel \/
        ObserveTermination \/ OwnerDeath

Spec == Init /\ [][Next]_vars /\ WF_vars(Launch) /\ WF_vars(Finish) /\
        WF_vars(ObserveTermination)

TypeOK ==
  /\ phase \in Phases /\ owner \in BOOLEAN /\ child \in Children
  /\ active \in 0..2 /\ observed \in BOOLEAN /\ pending \in BOOLEAN
  /\ terminationRequested \in BOOLEAN
  /\ requests \in 0..MaxRequests /\ freshRequests \in 0..MaxRequests
  /\ attempt \in 0..MaxRequests /\ acknowledgedAttempt \in 0..MaxRequests
  /\ workerStore \in {"none", McpStore, WrongStore} /\ report \in Reports
  /\ logBytes \in 0..MaxLogBytes

critical_AtMostOneActiveChild == active <= 1

critical_CoalescedRequestDoesNotCreateLaunchTicket == attempt <= freshRequests

critical_CancellationObservesExitBeforeFreshSlot ==
  phase = "idle" => /\ active = 0 /\ child # "live" /\ observed /\ ~terminationRequested

critical_NoOrphanAfterOwnerDeath ==
  phase = "stopped" => /\ active = 0 /\ child # "live" /\ observed

(* This is not a type restatement: Launch selects WrongStore whenever the
   binding mechanism is disabled; McpRebuildLifecycle_WrongStore.cfg produces
   a one-launch counterexample. *)
critical_ExactStoreBinding == active > 0 => workerStore = McpStore

critical_BoundedLogState == logBytes <= MaxLogBytes

critical_AcknowledgementOnlyAfterAcceptedLaunch ==
  acknowledgedAttempt <= attempt

EventuallyQuiescent == <>(phase \in {"idle", "success", "failed", "stopped"} /\ ~pending)

=============================================================================
