--------------------------- MODULE PortProtocol ---------------------------
EXTENDS Naturals, Sequences

(***************************************************************************)
(* A bounded model of the BEAM manager / Rust port protocol in ADR-3.       *)
(*                                                                         *)
(* Bounds used by the checked configs: MaxRequests = 2, D = 2,             *)
(* TickBound = 4.  Two ids are the minimum useful correlation model;       *)
(* TickBound > D is the minimum useful deadline-reset witness.             *)
(*                                                                         *)
(* FixedDesign = FALSE models collect_response/3 before P1: final replies  *)
(* are not correlated, every progress line resets the receive window, and  *)
(* OS port death is not observable.  TRUE models ADR-3: ids correlate, an  *)
(* absolute monotonic deadline is consumed by progress and discards, port   *)
(* death is delivered via :exit_status, and timeout does not restart it.    *)
(*                                                                         *)
(* Out of model scope (ADR-3 rules 4-6). Each is a real obligation on      *)
(* P1 (bd-21ef.2.8), covered there by test rather than by this state       *)
(* machine:                                                                *)
(*   Rule 4 (two-timer race, GenServer.call): the interaction between the  *)
(*     outer :infinity call and the inner absolute deadline is a property  *)
(*     of the BEAM's own call/timer semantics, not of the client/port      *)
(*     states modeled here; there is nothing for this spec's actions to    *)
(*     race against.                                                       *)
(*   Rule 5 (queued callers behind a crashed port): this is a              *)
(*     single-client model with no second caller or mailbox-queueing       *)
(*     state, so a caller queued behind `handle_call` on a crashed port    *)
(*     is a BEAM process/exit-propagation concern outside this            *)
(*     abstraction.                                                        *)
(*   Rule 6 (await_ready startup handshake): the model's Init is the       *)
(*     post-handshake steady state; the handshake itself is a distinct,    *)
(*     one-shot action that precedes every run this spec describes and    *)
(*     is not part of the repeatable Send/Reply/Progress/Timeout loop.     *)
(***************************************************************************)

CONSTANTS FixedDesign, MaxRequests, D, TickBound

ASSUME /\ FixedDesign \in BOOLEAN
       /\ MaxRequests \in Nat \ {0}
       /\ D \in Nat \ {0}
       /\ TickBound \in Nat
       /\ TickBound > D

RequestIds == 1..MaxRequests
NoId == 0

VARIABLES clientState, outstanding, received, issued,
          mailbox, portState, elapsed, ticks,
          crashedAt, restarts, deliveredFor

vars == <<clientState, outstanding, received, issued,
          mailbox, portState, elapsed, ticks,
          crashedAt, restarts, deliveredFor>>

TypeOK ==
  /\ clientState \in {"idle", "waiting", "done", "timeout", "crashed"}
  /\ outstanding \in RequestIds \cup {NoId}
  /\ received \in RequestIds \cup {NoId}
  /\ issued \subseteq RequestIds
  /\ mailbox \in Seq(RequestIds)
  /\ portState \in {"up", "down"}
  /\ elapsed \in 0..TickBound
  /\ ticks \in 0..TickBound
  /\ crashedAt \in 0..TickBound
  /\ restarts \in Nat
  /\ deliveredFor \in RequestIds \cup {NoId}

Init ==
  /\ clientState = "idle"
  /\ outstanding = NoId
  /\ received = NoId
  /\ issued = {}
  /\ mailbox = <<>>
  /\ portState = "up"
  /\ elapsed = 0
  /\ ticks = 0
  /\ crashedAt = 0
  /\ restarts = 0
  /\ deliveredFor = NoId

Send(id) ==
  /\ id \in RequestIds \ issued
  /\ clientState \in {"idle", "done", "timeout", "crashed"}
  /\ portState = "up"
  /\ clientState' = "waiting"
  /\ outstanding' = id
  /\ received' = NoId
  /\ issued' = issued \cup {id}
  /\ elapsed' = 0
  /\ deliveredFor' = NoId
  /\ UNCHANGED <<mailbox, portState, ticks, crashedAt, restarts>>

(* The port can finish any issued request after its caller has timed out. *)
EnqueueReply(id) ==
  /\ id \in issued
  /\ Len(mailbox) < MaxRequests
  /\ mailbox' = Append(mailbox, id)
  /\ UNCHANGED <<clientState, outstanding, received, issued, portState,
                  elapsed, ticks, crashedAt, restarts,
                  deliveredFor>>

ConsumeMatching(replyId) ==
  /\ clientState = "waiting"
  /\ replyId = outstanding
  /\ clientState' = "done"
  /\ received' = replyId
  /\ outstanding' = NoId
  /\ deliveredFor' = outstanding
  /\ UNCHANGED <<issued, portState, elapsed, ticks, crashedAt,
                  restarts>>

ConsumeReply ==
  /\ clientState = "waiting"
  /\ Len(mailbox) > 0
  /\ LET replyId == Head(mailbox) IN
       /\ mailbox' = Tail(mailbox)
       /\ IF FixedDesign
             THEN IF replyId = outstanding
                     THEN ConsumeMatching(replyId)
                     ELSE /\ clientState' = clientState
                          /\ outstanding' = outstanding
                          /\ received' = received
                          /\ deliveredFor' = deliveredFor
                          /\ elapsed < D
                          /\ elapsed' = elapsed + 1
                          /\ UNCHANGED <<issued, portState, ticks, crashedAt,
                                          restarts>>
             ELSE /\ clientState' = "done"
                  /\ received' = replyId
                  /\ outstanding' = NoId
                  /\ deliveredFor' = outstanding
                  /\ UNCHANGED <<issued, portState, elapsed, ticks,
                                  crashedAt, restarts>>

Reply == (\E id \in RequestIds : EnqueueReply(id)) \/ ConsumeReply

Progress ==
  /\ clientState = "waiting"
  /\ ticks < TickBound
  /\ elapsed < TickBound
  /\ IF FixedDesign THEN elapsed < D ELSE TRUE
  /\ elapsed' = elapsed + 1
  /\ ticks' = ticks + 1
  /\ UNCHANGED <<clientState, outstanding, received, issued, mailbox,
                  portState, crashedAt, restarts, deliveredFor>>

(* Timeout fires once the absolute deadline is reached, not before: a       *)
(* client cannot time out while its receive window has not yet elapsed.    *)
Timeout ==
  /\ clientState = "waiting"
  /\ elapsed >= D
  /\ clientState' = "timeout"
  /\ outstanding' = NoId
  /\ received' = NoId
  /\ elapsed' = D
  /\ deliveredFor' = NoId
  /\ UNCHANGED <<issued, mailbox, portState, ticks,
                  crashedAt, restarts>>

PortCrash ==
  /\ portState = "up"
  /\ portState' = "down"
  /\ crashedAt' = elapsed
  /\ IF FixedDesign /\ clientState = "waiting"
        THEN /\ clientState' = "crashed"
             /\ outstanding' = NoId
             /\ received' = NoId
             /\ deliveredFor' = NoId
        ELSE UNCHANGED <<clientState, outstanding, received, deliveredFor>>
  /\ UNCHANGED <<issued, mailbox, elapsed, ticks, restarts>>

(* ADR-3 rule 7 ("no restart on timeout") does not need a guard here: no    *)
(* action in Next transitions from a timeout into Restart in the first      *)
(* place — Restart's only precondition is portState = "down", which         *)
(* Timeout never sets. Rule 7 is about *what triggers* a restart (a         *)
(* timeout must not be that trigger, so the warmed embedder survives a      *)
(* slow caller); it does not forbid restarting a port that later crashes    *)
(* for its own, unrelated reason after some earlier caller had timed out.    *)
(* An explicit `~timedOut` conjunct would instead do that: it would leave a *)
(* genuinely crashed port down forever after any prior timeout, which is    *)
(* not what rule 7 says and is not implementable (the supervisor restart    *)
(* has no memory of a past client-side timeout). So there is no `timedOut`  *)
(* variable in this model at all.                                          *)
Restart ==
  /\ portState = "down"
  /\ restarts < MaxRequests
  /\ portState' = "up"
  /\ restarts' = restarts + 1
  /\ UNCHANGED <<clientState, outstanding, received, issued, mailbox,
                  elapsed, ticks, crashedAt, deliveredFor>>

Next ==
  \/ \E id \in RequestIds : Send(id)
  \/ Reply
  \/ Progress
  \/ Timeout
  \/ PortCrash
  \/ Restart

Spec == Init /\ [][Next]_vars

NoCrossTalk == clientState # "done" \/ received = deliveredFor

(* A waiting request cannot survive beyond its absolute D budget.  Because
   Progress is repeatable up to TickBound>D, this is the finite safety form of
   "terminates within D under an unbounded stream". *)
BoundedDeadline == clientState # "waiting" \/ elapsed <= D

(* Environment assumption: FixedDesign makes PortCrash observable.  In the
   implementation this is :exit_status (with trap_exit as complementary
   coverage).  Under that assumption PortCrash replies/stops immediately.
   Written as elapsed <= crashedAt + D rather than elapsed - crashedAt <= D
   because '-' on Naturals is undefined when the minuend is smaller than the
   subtrahend (elapsed can be reset below crashedAt by a later Send), and a
   comparison form has no such domain restriction. *)
CrashIsPrompt ==
  portState # "down" \/ clientState # "waiting"
    \/ elapsed <= crashedAt + D

=============================================================================
