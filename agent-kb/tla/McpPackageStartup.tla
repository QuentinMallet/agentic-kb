-------------------------- MODULE McpPackageStartup --------------------------
EXTENDS TLC

(******************************************************************************)
(* REG-6f32a0b-packaged-mcp-startup                                           *)
(*                                                                            *)
(* The packaged MCP launcher has two eager runtime prerequisites: the Erlang  *)
(* `escript` interpreter that runs the generated escript, and the Rust `kb`   *)
(* binary that backs a discovered database. OPA is request-lazy: it can deny  *)
(* a later mutation, but it does not decide whether initialize/tools/list can *)
(* start.                                                                     *)
(*                                                                            *)
(* The broken wrapper exposes OPA only, so the escript cannot execute (and a *)
(* missing `kb` fails the supervised startup). It terminates Failed; it does  *)
(* not falsely report Ready. The repaired closure reaches Ready precisely when *)
(* both eager prerequisites are present, otherwise it terminates Failed.      *)
(******************************************************************************)

CONSTANTS EscriptInClosure, KbInClosure, OpaInClosure

ASSUME /\ EscriptInClosure \in BOOLEAN
       /\ KbInClosure \in BOOLEAN
       /\ OpaInClosure \in BOOLEAN

VARIABLE phase

vars == <<phase>>

Init ==
  /\ phase = "launching"

Start ==
  /\ phase = "launching"
  /\ phase' = IF EscriptInClosure /\ KbInClosure THEN "ready" ELSE "failed"
  /\ UNCHANGED << >>

Done ==
  /\ phase \in {"ready", "failed"}
  /\ UNCHANGED vars

Next == Start \/ Done

(* A package launch cannot remain indefinitely at the scheduler boundary: once
   it is launched, the one startup transition is weakly fair. *)
Spec == Init /\ [][Next]_vars /\ WF_vars(Start)

TypeOK == phase \in {"launching", "ready", "failed"}

critical_ReadyRequiresEagerRuntime ==
  phase = "ready" => EscriptInClosure /\ KbInClosure

(* OPA is deliberately absent from Start's condition. A fixed launcher may
   fail only for a missing eager prerequisite, never merely because OPA is
   unavailable for a later mutation. *)
critical_FailureRequiresMissingEagerRuntime ==
  phase = "failed" => ~EscriptInClosure \/ ~KbInClosure

EventuallyReady == <>(phase = "ready")

=============================================================================
