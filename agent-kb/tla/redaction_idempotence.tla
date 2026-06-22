----------------------- MODULE redaction_idempotence -----------------------
(* kb_core redactor: token-sequence model for idempotent secret elimination.

   Verified properties
   -------------------
   SecretElimination   No secret pattern survives in Redact(Input).
   IdempotenceInvariant Redact(Redact(Input)) = Redact(Input) for all
                        reachable Input configurations.
   RedactedTokenSafe   REDACTED_TOKEN is never a member of Secrets, so a
                       second pass of Redact leaves it untouched — key
                       premise for idempotence.

   Design notes
   ------------
   A "string" is modelled as a sequence of tokens (Nat-indexed symbols).
   Secrets is a finite set of tokens drawn from TokenVals.
   REDACTED_TOKEN is a distinguished symbol disjoint from all TokenVals.

   Redact scans the sequence and replaces every token that is a member of
   Secrets with REDACTED_TOKEN.  Because REDACTED_TOKEN ∉ Secrets, a
   second application of Redact leaves every REDACTED_TOKEN in place and
   replaces nothing new — this is the structural reason idempotence holds.

   The "system" here is purely functional (no mutable state protocol), so
   the model is a single initial state plus a stuttering-only Next action.
   TLC exhausts the state space by enumerating all possible Input sequences
   over the bounded alphabet.

   Bounding for TLC
   ----------------
   MaxLen bounds the length of Input (keeps the state space finite).
   TokenVals and Secrets are concrete small sets in the .cfg file.

   Run: tlc redaction_idempotence -config redaction_idempotence.cfg -workers auto -deadlock
*)

EXTENDS Sequences, FiniteSets, Naturals, TLC

CONSTANTS
    TokenVals,       \* finite set of token symbols, e.g. {"a","b","c","secret"}
    Secrets,         \* subset of TokenVals that are secret patterns
    MaxLen           \* maximum Input length for TLC state-space bound

ASSUME Secrets \subseteq TokenVals
ASSUME TokenVals # {}
ASSUME MaxLen \in Nat /\ MaxLen > 0

\* Distinguished replacement token — must be outside TokenVals so it
\* is never itself a member of Secrets.
REDACTED_TOKEN == "REDACTED"

\* Structural guarantee that the replacement token is safe.
RedactedTokenDisjoint == REDACTED_TOKEN \notin TokenVals

(* ──────────────────────────── Core operator ─────────────────────────── *)

\* Replace every token that belongs to Secrets with REDACTED_TOKEN.
\* Applied position-by-position; order and non-secret tokens are preserved.
Redact(seq) ==
    [i \in DOMAIN seq |->
        IF seq[i] \in Secrets
        THEN REDACTED_TOKEN
        ELSE seq[i]]

(* ──────────────────────────── State variables ────────────────────────── *)

VARIABLES
    Input   \* Seq — the current input being examined (varies across states)

vars == <<Input>>

\* All sequences of length 0..MaxLen over TokenVals.
AllInputs ==
    UNION {[1..n -> TokenVals] : n \in 0..MaxLen}

(* ──────────────────────────── Initial state ─────────────────────────── *)

\* Start from any bounded input — TLC enumerates all of them.
Init ==
    Input \in AllInputs

(* ──────────────────────────── Actions ───────────────────────────────── *)

\* The redactor is a pure function; no state transitions are needed.
\* We model "the system stays in place" as a stuttering step so TLC can
\* enumerate all Init states without a non-trivial Next relation.
Next ==
    UNCHANGED vars

Spec == Init /\ [][Next]_vars

(* ──────────────────────────── Invariants ─────────────────────────────── *)

\* I1: No secret token survives in the output.
SecretElimination ==
    \A i \in DOMAIN Redact(Input) :
        Redact(Input)[i] \notin Secrets

\* I2: A second redaction is identical to the first.
\*     Holds because REDACTED_TOKEN ∉ Secrets (RedactedTokenDisjoint),
\*     so every position already replaced stays replaced.
IdempotenceInvariant ==
    Redact(Redact(Input)) = Redact(Input)

\* I3: REDACTED_TOKEN is always outside the secret set.
RedactedTokenSafe ==
    REDACTED_TOKEN \notin Secrets

\* I4: Non-secret tokens are preserved unchanged.
NonSecretPreservation ==
    \A i \in DOMAIN Input :
        Input[i] \notin Secrets => Redact(Input)[i] = Input[i]

\* I5: Output length equals input length (no tokens are dropped, only replaced).
LengthPreservation ==
    Len(Redact(Input)) = Len(Input)

Invariants ==
    /\ RedactedTokenSafe
    /\ RedactedTokenDisjoint
    /\ SecretElimination
    /\ IdempotenceInvariant
    /\ NonSecretPreservation
    /\ LengthPreservation

THEOREM Spec => []Invariants

=============================================================================
