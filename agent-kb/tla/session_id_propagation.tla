-------------------------- MODULE session_id_propagation --------------------------
(*
  session_id_propagation.tla  —  A2 refinement: CLI command ⇒ session_id in event
  =================================================================================

  Models the refinement property for Lane A, task A2:

    "When OMC_SESSION_ID is set and a CLI command is invoked,
     the emitted upsert event MUST carry session_id = $OMC_SESSION_ID,
     and entries.session_id MUST equal that value after apply_event."

  Scope: `add`, `expire`, `run`, `test_add` CLI commands.
  The model abstracts away JSONL/DB mechanics (proven in InnerGap / CrossBatch)
  and focuses solely on the session_id field propagation contract.

  State machine:
    - OMC_SESSION_ID env var: set to a concrete value or absent.
    - CLI command: invoked with optional OMC_SESSION_ID in environment.
    - Emitted event: JSON object that may or may not carry "session_id".
    - DB row: session_id column value after apply_event.

  Safety property (SessionIdPropagation):
    IF env_session_id /= "UNSET"
    THEN event_session_id = env_session_id
      /\ db_session_id    = env_session_id

  Liveness property (EventAlwaysEmitted):
    Every CLI invocation eventually produces an emitted event.
*)

EXTENDS Naturals, FiniteSets

CONSTANTS
    SessionIds,   \* finite set of possible session ID strings, e.g. {"test123", "abc"}
    Unset         \* sentinel representing "OMC_SESSION_ID not set"

ASSUME Unset \notin SessionIds

VARIABLES
    env_session_id,   \* OMC_SESSION_ID value: Unset | element of SessionIds
    cmd_phase,        \* command lifecycle: "idle" | "invoked" | "emitted" | "applied"
    event_session_id, \* "session_id" field value in the emitted event (Unset = absent)
    db_session_id     \* entries.session_id after apply_event (Unset = NULL)

TypeOK ==
    /\ env_session_id   \in (SessionIds \union {Unset})
    /\ cmd_phase        \in {"idle", "invoked", "emitted", "applied"}
    /\ event_session_id \in (SessionIds \union {Unset})
    /\ db_session_id    \in (SessionIds \union {Unset})

Init ==
    /\ env_session_id   = Unset
    /\ cmd_phase        = "idle"
    /\ event_session_id = Unset
    /\ db_session_id    = Unset

\* Environment changes: OMC_SESSION_ID is set to some value.
SetEnvSessionId(sid) ==
    /\ sid \in SessionIds
    /\ cmd_phase       = "idle"
    /\ env_session_id' = sid
    /\ UNCHANGED << cmd_phase, event_session_id, db_session_id >>

\* Environment changes: OMC_SESSION_ID is cleared.
ClearEnvSessionId ==
    /\ cmd_phase       = "idle"
    /\ env_session_id' = Unset
    /\ UNCHANGED << cmd_phase, event_session_id, db_session_id >>

\* CLI command is invoked (add / expire / run / test_add).
InvokeCLI ==
    /\ cmd_phase  = "idle"
    /\ cmd_phase' = "invoked"
    /\ UNCHANGED << env_session_id, event_session_id, db_session_id >>

\* Command emits the event payload.
\* REFINEMENT: the event carries session_id = env_session_id when env is set.
EmitEvent ==
    /\ cmd_phase  = "invoked"
    /\ cmd_phase' = "emitted"
    \* When OMC_SESSION_ID is set, it MUST be carried in the event.
    /\ event_session_id' = env_session_id
    /\ UNCHANGED << env_session_id, db_session_id >>

\* apply_event writes session_id to the DB row (entries table).
\* For expire/run/test_add the field is in the event payload;
\* for add it is written to entries.session_id via apply_event.
ApplyEvent ==
    /\ cmd_phase  = "emitted"
    /\ cmd_phase' = "applied"
    /\ db_session_id' = event_session_id
    /\ UNCHANGED << env_session_id, event_session_id >>

\* Reset to idle after the command completes.
Reset ==
    /\ cmd_phase  = "applied"
    /\ cmd_phase' = "idle"
    /\ UNCHANGED << env_session_id, event_session_id, db_session_id >>

Next ==
    \/ \E sid \in SessionIds : SetEnvSessionId(sid)
    \/ ClearEnvSessionId
    \/ InvokeCLI
    \/ EmitEvent
    \/ ApplyEvent
    \/ Reset

\* Safety: when OMC_SESSION_ID is set, the emitted event and DB row carry it.
SessionIdPropagation ==
    (cmd_phase = "emitted" /\ env_session_id /= Unset)
        => event_session_id = env_session_id

SessionIdInDB ==
    (cmd_phase = "applied" /\ env_session_id /= Unset)
        => db_session_id = env_session_id

\* When OMC_SESSION_ID is absent, session_id is Unset (NULL) — no phantom values.
NoPhantomSessionId ==
    (cmd_phase \in {"emitted", "applied"} /\ env_session_id = Unset)
        => event_session_id = Unset

Spec ==
    /\ Init
    /\ [][Next]_<<env_session_id, cmd_phase, event_session_id, db_session_id>>
    /\ WF_<<env_session_id, cmd_phase, event_session_id, db_session_id>>(InvokeCLI)
    /\ WF_<<env_session_id, cmd_phase, event_session_id, db_session_id>>(EmitEvent)
    /\ WF_<<env_session_id, cmd_phase, event_session_id, db_session_id>>(ApplyEvent)
    /\ WF_<<env_session_id, cmd_phase, event_session_id, db_session_id>>(Reset)

\* Liveness: every CLI invocation eventually produces an applied DB row.
EventAlwaysApplied ==
    [](cmd_phase = "invoked" => <>(cmd_phase = "applied"))

===================================================================================
