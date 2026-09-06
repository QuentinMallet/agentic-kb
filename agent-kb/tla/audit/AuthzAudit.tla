------------------------- MODULE AuthzAudit -------------------------
(*
  Authorization refinement for bd-1orr.

  The existing Audit.tla remains the state-machine proof for the audit data
  model.  This companion model is deliberately boundary-focused: it refines
  the MCP mutation boundary with a transport-bound caller identity, OPA
  availability and scope decisions, run ownership, caller-local quotas, and
  an all-or-nothing audit-record batch.

  `caller_argument` is attacker controlled. `launch_identity` is initialized
  once and intentionally never appears on the left side of an action. Every
  durable mutation records the transport identity, never caller_argument.

  Adversary actions explicitly represented below:
    SpoofCallerArgument, CrossCallerRecord, FloodAudit, PolicyUnavailable,
    FailMidBatch.

  Run with: tlc AuthzAudit -config AuthzAudit.cfg -deadlock
*)

EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Callers, Entries, Runs, MaxQuota, MaxBatch

ASSUME Callers # {}
ASSUME Entries # {}
ASSUME Runs # {}
ASSUME MaxQuota \in Nat /\ MaxQuota > 0
ASSUME MaxBatch \in Nat /\ MaxBatch > 0

NoCaller == "no-caller"
NoRun == "no-run"
NoEntry == "no-entry"
Scopes == {"audit", "traffic", "force_expire"}
Modes == {"uniform", "traffic"}
Decisions == {"none", "allow", "deny", "throttle", "unavailable"}
BatchStates == {"idle", "pending", "committed", "aborted"}

VARIABLES
    \* Transport identity is established at launch, before untrusted input.
    launch_identity, caller_argument,
    granted_scopes, opa_status, policy_decision,
    run_owner, run_mode, run_state,
    quota_used, last_quota_before, last_actor,
    entry_state, ever_stale,
    event_transport, last_mutator, event_run, event_scopes, event_authorized,
    sw_exists, sw_succ, sw_fail, sw_ever_created,
    batch_state, batch_owner, batch_run, batch_entries, committed_entries,
    pre_entry_state, pre_run_owner, pre_sw_fail, pre_batch_state

vars == <<launch_identity, caller_argument, granted_scopes, opa_status,
          policy_decision, run_owner, run_mode, run_state, quota_used,
          last_quota_before, last_actor, entry_state, ever_stale,
          event_transport, last_mutator, event_run, event_scopes,
          event_authorized, sw_exists, sw_succ, sw_fail, sw_ever_created,
          batch_state, batch_owner, batch_run, batch_entries, committed_entries,
          pre_entry_state, pre_run_owner, pre_sw_fail, pre_batch_state>>

Init ==
    /\ launch_identity = [c \in Callers |-> c]
    /\ caller_argument = [c \in Callers |-> c]
    \* Two different policies make both authorized and denied paths reachable.
    /\ granted_scopes = [c \in Callers |->
            IF c = CHOOSE a \in Callers : TRUE
              THEN Scopes
              ELSE {"audit"}]
    /\ opa_status = "up"
    /\ policy_decision = "none"
    /\ run_owner = [r \in Runs |-> NoCaller]
    /\ run_mode = [r \in Runs |-> "uniform"]
    /\ run_state = [r \in Runs |-> "none"]
    /\ quota_used = [c \in Callers |-> 0]
    /\ last_quota_before = [c \in Callers |-> 0]
    /\ last_actor = NoCaller
    /\ entry_state = [e \in Entries |-> "live"]
    /\ ever_stale = [e \in Entries |-> FALSE]
    /\ event_transport = [e \in Entries |-> NoCaller]
    /\ last_mutator = [e \in Entries |-> NoCaller]
    /\ event_run = [e \in Entries |-> NoRun]
    /\ event_scopes = [e \in Entries |-> {}]
    /\ event_authorized = [e \in Entries |-> FALSE]
    /\ sw_exists = [c \in Callers |-> FALSE]
    /\ sw_succ = [c \in Callers |-> 0]
    /\ sw_fail = [c \in Callers |-> 0]
    /\ sw_ever_created = [c \in Callers |-> FALSE]
    /\ batch_state = "idle"
    /\ batch_owner = NoCaller
    /\ batch_run = NoRun
    /\ batch_entries = {}
    /\ committed_entries = {}
    /\ pre_entry_state = entry_state
    /\ pre_run_owner = run_owner
    /\ pre_sw_fail = sw_fail
    /\ pre_batch_state = batch_state

Snapshot ==
    /\ pre_entry_state' = entry_state
    /\ pre_run_owner' = run_owner
    /\ pre_sw_fail' = sw_fail
    /\ pre_batch_state' = batch_state
    /\ last_quota_before' = quota_used

CanAudit(c) ==
    opa_status = "up" /\ "audit" \in granted_scopes[c] /\ quota_used[c] < MaxQuota
CanTraffic(c) == "traffic" \in granted_scopes[c]
CanForceExpire(c) == "force_expire" \in granted_scopes[c]

\* A caller opens a run. Traffic-mode is explicitly scope-gated.
StartRun(c, r, mode) ==
    /\ c \in Callers /\ r \in Runs /\ mode \in Modes
    /\ run_state[r] = "none"
    /\ CanAudit(c)
    /\ (mode = "uniform" \/ CanTraffic(c))
    /\ Snapshot
    /\ run_owner' = [run_owner EXCEPT ![r] = c]
    /\ run_mode' = [run_mode EXCEPT ![r] = mode]
    /\ run_state' = [run_state EXCEPT ![r] = "open"]
    /\ quota_used' = [quota_used EXCEPT ![c] = quota_used[c] + 1]
    /\ policy_decision' = "allow"
    /\ last_actor' = c
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  entry_state, ever_stale, event_transport, last_mutator,
                  event_run, event_scopes, event_authorized, sw_exists, sw_succ,
                  sw_fail, sw_ever_created, batch_state, batch_owner, batch_run,
                  batch_entries, committed_entries>>

\* A valid caller stages a false-verdict batch. No durable entry mutates here.
BeginBatch(c, r, selected) ==
    /\ c \in Callers /\ r \in Runs /\ selected \subseteq Entries
    /\ selected # {} /\ Cardinality(selected) <= MaxBatch
    /\ batch_state \in {"idle", "aborted", "committed"}
    /\ run_state[r] = "open" /\ run_owner[r] = c
    /\ CanAudit(c)
    /\ Snapshot
    /\ batch_state' = "pending"
    /\ batch_owner' = c
    /\ batch_run' = r
    /\ batch_entries' = selected
    /\ committed_entries' = {}
    /\ policy_decision' = "allow"
    /\ last_actor' = c
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  run_owner, run_mode, run_state, quota_used, entry_state,
                  ever_stale, event_transport, last_mutator, event_run,
                  event_scopes, event_authorized, sw_exists, sw_succ, sw_fail,
                  sw_ever_created>>

\* Atomic false-verdict commit: every selected entry is expired together.
CommitBatch(c) ==
    /\ c \in Callers /\ batch_state = "pending" /\ batch_owner = c
    /\ run_state[batch_run] = "open" /\ run_owner[batch_run] = c
    /\ CanAudit(c) /\ CanForceExpire(c) /\ sw_fail[c] < MaxQuota
    /\ Snapshot
    /\ entry_state' = [e \in Entries |->
            IF e \in batch_entries THEN "stale" ELSE entry_state[e]]
    /\ ever_stale' = [e \in Entries |-> ever_stale[e] \/ e \in batch_entries]
    /\ event_transport' = [e \in Entries |->
            IF e \in batch_entries THEN c ELSE event_transport[e]]
    /\ last_mutator' = [e \in Entries |->
            IF e \in batch_entries THEN launch_identity[c] ELSE last_mutator[e]]
    /\ event_run' = [e \in Entries |->
            IF e \in batch_entries THEN batch_run ELSE event_run[e]]
    /\ event_scopes' = [e \in Entries |->
            IF e \in batch_entries THEN granted_scopes[c] ELSE event_scopes[e]]
    /\ event_authorized' = [e \in Entries |->
            IF e \in batch_entries THEN TRUE ELSE event_authorized[e]]
    /\ sw_exists' = [sw_exists EXCEPT ![c] = TRUE]
    /\ sw_fail' = [sw_fail EXCEPT ![c] = sw_fail[c] + 1]
    /\ sw_ever_created' = [sw_ever_created EXCEPT ![c] = TRUE]
    /\ quota_used' = [quota_used EXCEPT ![c] = quota_used[c] + 1]
    /\ batch_state' = "committed"
    /\ committed_entries' = batch_entries
    /\ policy_decision' = "allow"
    /\ last_actor' = c
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  run_owner, run_mode, run_state, sw_succ, batch_owner,
                  batch_run, batch_entries>>

\* Direct kb_expire equivalent; it needs force_expire even outside an audit run.
ForceExpire(c, e) ==
    /\ c \in Callers /\ e \in Entries /\ entry_state[e] = "live"
    /\ opa_status = "up" /\ CanForceExpire(c) /\ quota_used[c] < MaxQuota
    /\ Snapshot
    /\ entry_state' = [entry_state EXCEPT ![e] = "stale"]
    /\ ever_stale' = [ever_stale EXCEPT ![e] = TRUE]
    /\ event_transport' = [event_transport EXCEPT ![e] = c]
    /\ last_mutator' = [last_mutator EXCEPT ![e] = launch_identity[c]]
    /\ event_run' = [event_run EXCEPT ![e] = NoRun]
    /\ event_scopes' = [event_scopes EXCEPT ![e] = granted_scopes[c]]
    /\ event_authorized' = [event_authorized EXCEPT ![e] = TRUE]
    /\ quota_used' = [quota_used EXCEPT ![c] = quota_used[c] + 1]
    /\ policy_decision' = "allow"
    /\ last_actor' = c
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  run_owner, run_mode, run_state, sw_exists, sw_succ, sw_fail,
                  sw_ever_created, batch_state, batch_owner, batch_run,
                  batch_entries, committed_entries>>

\* Untrusted metadata may claim another caller, but cannot alter launch identity.
SpoofCallerArgument(c, claimed) ==
    /\ c \in Callers /\ claimed \in Callers /\ claimed # c
    /\ Snapshot
    /\ caller_argument' = [caller_argument EXCEPT ![c] = claimed]
    /\ policy_decision' = "none"
    /\ last_actor' = c
    /\ UNCHANGED <<launch_identity, granted_scopes, opa_status, run_owner,
                  run_mode, run_state, quota_used, entry_state, ever_stale,
                  event_transport, last_mutator, event_run, event_scopes,
                  event_authorized, sw_exists, sw_succ, sw_fail, sw_ever_created,
                  batch_state, batch_owner, batch_run, batch_entries,
                  committed_entries>>

\* An attacker tries to submit an owner's staged run. It is denied, unchanged.
CrossCallerRecord(attacker) ==
    /\ attacker \in Callers /\ batch_state = "pending" /\ attacker # batch_owner
    /\ Snapshot
    /\ policy_decision' = "deny"
    /\ last_actor' = attacker
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  run_owner, run_mode, run_state, quota_used, entry_state,
                  ever_stale, event_transport, last_mutator, event_run,
                  event_scopes, event_authorized, sw_exists, sw_succ, sw_fail,
                  sw_ever_created, batch_state, batch_owner, batch_run,
                  batch_entries, committed_entries>>

\* A quota-exhausted caller attempts another mutation; no audit state changes.
FloodAudit(c) ==
    /\ c \in Callers /\ quota_used[c] = MaxQuota
    /\ Snapshot
    /\ policy_decision' = "throttle"
    /\ last_actor' = c
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  run_owner, run_mode, run_state, quota_used, entry_state,
                  ever_stale, event_transport, last_mutator, event_run,
                  event_scopes, event_authorized, sw_exists, sw_succ, sw_fail,
                  sw_ever_created, batch_state, batch_owner, batch_run,
                  batch_entries, committed_entries>>

\* OPA fails closed. The attempted operation leaves all durable audit state intact.
PolicyUnavailable(c) ==
    /\ c \in Callers /\ opa_status = "down"
    /\ Snapshot
    /\ policy_decision' = "unavailable"
    /\ last_actor' = c
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  run_owner, run_mode, run_state, quota_used, entry_state,
                  ever_stale, event_transport, last_mutator, event_run,
                  event_scopes, event_authorized, sw_exists, sw_succ, sw_fail,
                  sw_ever_created, batch_state, batch_owner, batch_run,
                  batch_entries, committed_entries>>

SetOpaUnavailable ==
    /\ opa_status = "up"
    /\ Snapshot
    /\ opa_status' = "down"
    /\ policy_decision' = "none"
    /\ last_actor' = NoCaller
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, run_owner,
                  run_mode, run_state, quota_used, entry_state, ever_stale,
                  event_transport, last_mutator, event_run, event_scopes,
                  event_authorized, sw_exists, sw_succ, sw_fail, sw_ever_created,
                  batch_state, batch_owner, batch_run, batch_entries,
                  committed_entries>>

RestoreOpa ==
    /\ opa_status = "down"
    /\ Snapshot
    /\ opa_status' = "up"
    /\ policy_decision' = "none"
    /\ last_actor' = NoCaller
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, run_owner,
                  run_mode, run_state, quota_used, entry_state, ever_stale,
                  event_transport, last_mutator, event_run, event_scopes,
                  event_authorized, sw_exists, sw_succ, sw_fail, sw_ever_created,
                  batch_state, batch_owner, batch_run, batch_entries,
                  committed_entries>>

\* A transactional DB failure abandons the whole staged batch; there is no prefix.
FailMidBatch ==
    /\ batch_state = "pending"
    /\ Snapshot
    /\ batch_state' = "aborted"
    /\ committed_entries' = {}
    /\ policy_decision' = "none"
    /\ last_actor' = batch_owner
    /\ UNCHANGED <<launch_identity, caller_argument, granted_scopes, opa_status,
                  run_owner, run_mode, run_state, quota_used, entry_state,
                  ever_stale, event_transport, last_mutator, event_run,
                  event_scopes, event_authorized, sw_exists, sw_succ, sw_fail,
                  sw_ever_created, batch_owner, batch_run, batch_entries>>

Next ==
    \/ \E c \in Callers, r \in Runs, m \in Modes : StartRun(c, r, m)
    \/ \E c \in Callers, r \in Runs, s \in SUBSET Entries : BeginBatch(c, r, s)
    \/ \E c \in Callers : CommitBatch(c)
    \/ \E c \in Callers, e \in Entries : ForceExpire(c, e)
    \/ \E c \in Callers, a \in Callers : SpoofCallerArgument(c, a)
    \/ \E c \in Callers : CrossCallerRecord(c)
    \/ \E c \in Callers : FloodAudit(c)
    \/ \E c \in Callers : PolicyUnavailable(c)
    \/ SetOpaUnavailable
    \/ RestoreOpa
    \/ FailMidBatch

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ launch_identity \in [Callers -> Callers]
    /\ caller_argument \in [Callers -> Callers]
    /\ granted_scopes \in [Callers -> SUBSET Scopes]
    /\ opa_status \in {"up", "down"}
    /\ policy_decision \in Decisions
    /\ run_owner \in [Runs -> (Callers \cup {NoCaller})]
    /\ run_mode \in [Runs -> Modes]
    /\ run_state \in [Runs -> {"none", "open"}]
    /\ quota_used \in [Callers -> 0..MaxQuota]
    /\ last_quota_before \in [Callers -> 0..MaxQuota]
    /\ last_actor \in Callers \cup {NoCaller}
    /\ entry_state \in [Entries -> {"live", "stale"}]
    /\ ever_stale \in [Entries -> BOOLEAN]
    /\ event_transport \in [Entries -> (Callers \cup {NoCaller})]
    /\ last_mutator \in [Entries -> (Callers \cup {NoCaller})]
    /\ event_run \in [Entries -> (Runs \cup {NoRun})]
    /\ event_scopes \in [Entries -> SUBSET Scopes]
    /\ event_authorized \in [Entries -> BOOLEAN]
    /\ sw_exists \in [Callers -> BOOLEAN]
    /\ sw_succ \in [Callers -> 0..MaxQuota]
    /\ sw_fail \in [Callers -> 0..MaxQuota]
    /\ sw_ever_created \in [Callers -> BOOLEAN]
    /\ batch_state \in BatchStates
    /\ batch_owner \in Callers \cup {NoCaller}
    /\ batch_run \in Runs \cup {NoRun}
    /\ batch_entries \subseteq Entries
    /\ committed_entries \subseteq Entries

\* Existing Audit.tla safety contracts, rechecked over the extended mutation path.
EntryMonotonicity == \A e \in Entries : ever_stale[e] => entry_state[e] = "stale"
ConfidenceInUnitInterval == \A c \in Callers :
    /\ sw_succ[c] + 1 >= 1
    /\ sw_succ[c] + 1 <= sw_succ[c] + sw_fail[c] + 2
SourceWeightsAppendOnly == \A c \in Callers : sw_ever_created[c] => sw_exists[c]
\* This refinement has no provenance mutation; Audit.tla owns the full DAG proof.
ProvenanceAcyclicity == TRUE

AuthorizedMutation == \A e \in Entries :
    last_mutator[e] # NoCaller =>
      /\ event_authorized[e]
      /\ "force_expire" \in event_scopes[e]

AttributionIntegrity == \A e \in Entries :
    last_mutator[e] # NoCaller =>
      /\ event_transport[e] \in Callers
      /\ last_mutator[e] = launch_identity[event_transport[e]]

RunOwnerIsolation == \A e \in Entries :
    event_run[e] # NoRun => last_mutator[e] = run_owner[event_run[e]]

PerCallerQuotaBound == \A c \in Callers : quota_used[c] <= MaxQuota

CallerQuotaIsolation ==
    last_actor \in Callers =>
      \A other \in Callers \ {last_actor} : quota_used[other] = last_quota_before[other]

TrafficRequiresScope == \A r \in Runs :
    (run_state[r] = "open" /\ run_mode[r] = "traffic") =>
      "traffic" \in granted_scopes[run_owner[r]]

ForceExpireRequiresScope == \A e \in Entries :
    entry_state[e] = "stale" => "force_expire" \in event_scopes[e]

NoMutationOnDenyOrThrottle ==
    policy_decision \in {"deny", "throttle", "unavailable"} =>
      /\ entry_state = pre_entry_state
      /\ run_owner = pre_run_owner
      /\ sw_fail = pre_sw_fail
      /\ batch_state = pre_batch_state

NoProperPrefixCommit ==
    /\ batch_state = "committed" => committed_entries = batch_entries
    /\ batch_state = "aborted" => committed_entries = {}

Invariant ==
    /\ TypeOK
    /\ EntryMonotonicity
    /\ ConfidenceInUnitInterval
    /\ SourceWeightsAppendOnly
    /\ ProvenanceAcyclicity
    /\ AuthorizedMutation
    /\ AttributionIntegrity
    /\ RunOwnerIsolation
    /\ PerCallerQuotaBound
    /\ CallerQuotaIsolation
    /\ TrafficRequiresScope
    /\ ForceExpireRequiresScope
    /\ NoMutationOnDenyOrThrottle
    /\ NoProperPrefixCommit

THEOREM Spec => []Invariant
=============================================================================
