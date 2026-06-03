---- MODULE AgentKb_TTrace_1780433476 ----
EXTENDS Sequences, TLCExt, Toolbox, AgentKb, Naturals, TLC

_expression ==
    LET AgentKb_TEExpression == INSTANCE AgentKb_TEExpression
    IN AgentKb_TEExpression!expression
----

_trace ==
    LET AgentKb_TETrace == INSTANCE AgentKb_TETrace
    IN AgentKb_TETrace!trace
----

_inv ==
    ~(
        TLCGet("level") = Len(_TETrace)
        /\
        snap_len = ([p1 |-> 0, p2 |-> 0])
        /\
        pc = ([p1 |-> "idle", p2 |-> "idle"])
        /\
        log = (<<>>)
        /\
        pending = ([p1 |-> [action |-> "none"], p2 |-> [action |-> "none"]])
        /\
        db = ([e1 |-> [type |-> "present", data |-> "d1", stale |-> TRUE], e2 |-> [type |-> "absent"]])
        /\
        lock_holder = ("none")
    )
----

_init ==
    /\ log = _TETrace[1].log
    /\ pc = _TETrace[1].pc
    /\ db = _TETrace[1].db
    /\ snap_len = _TETrace[1].snap_len
    /\ pending = _TETrace[1].pending
    /\ lock_holder = _TETrace[1].lock_holder
----

_next ==
    /\ \E i,j \in DOMAIN _TETrace:
        /\ \/ /\ j = i + 1
              /\ i = TLCGet("level")
        /\ log  = _TETrace[i].log
        /\ log' = _TETrace[j].log
        /\ pc  = _TETrace[i].pc
        /\ pc' = _TETrace[j].pc
        /\ db  = _TETrace[i].db
        /\ db' = _TETrace[j].db
        /\ snap_len  = _TETrace[i].snap_len
        /\ snap_len' = _TETrace[j].snap_len
        /\ pending  = _TETrace[i].pending
        /\ pending' = _TETrace[j].pending
        /\ lock_holder  = _TETrace[i].lock_holder
        /\ lock_holder' = _TETrace[j].lock_holder

\* Uncomment the ASSUME below to write the states of the error trace
\* to the given file in Json format. Note that you can pass any tuple
\* to `JsonSerialize`. For example, a sub-sequence of _TETrace.
    \* ASSUME
    \*     LET J == INSTANCE Json
    \*         IN J!JsonSerialize("AgentKb_TTrace_1780433476.json", _TETrace)

=============================================================================

 Note that you can extract this module `AgentKb_TEExpression`
  to a dedicated file to reuse `expression` (the module in the 
  dedicated `AgentKb_TEExpression.tla` file takes precedence 
  over the module `AgentKb_TEExpression` below).

---- MODULE AgentKb_TEExpression ----
EXTENDS Sequences, TLCExt, Toolbox, AgentKb, Naturals, TLC

expression == 
    [
        \* To hide variables of the `AgentKb` spec from the error trace,
        \* remove the variables below.  The trace will be written in the order
        \* of the fields of this record.
        log |-> log
        ,pc |-> pc
        ,db |-> db
        ,snap_len |-> snap_len
        ,pending |-> pending
        ,lock_holder |-> lock_holder
        
        \* Put additional constant-, state-, and action-level expressions here:
        \* ,_stateNumber |-> _TEPosition
        \* ,_logUnchanged |-> log = log'
        
        \* Format the `log` variable as Json value.
        \* ,_logJson |->
        \*     LET J == INSTANCE Json
        \*     IN J!ToJson(log)
        
        \* Lastly, you may build expressions over arbitrary sets of states by
        \* leveraging the _TETrace operator.  For example, this is how to
        \* count the number of times a spec variable changed up to the current
        \* state in the trace.
        \* ,_logModCount |->
        \*     LET F[s \in DOMAIN _TETrace] ==
        \*         IF s = 1 THEN 0
        \*         ELSE IF _TETrace[s].log # _TETrace[s-1].log
        \*             THEN 1 + F[s-1] ELSE F[s-1]
        \*     IN F[_TEPosition - 1]
    ]

=============================================================================



Parsing and semantic processing can take forever if the trace below is long.
 In this case, it is advised to uncomment the module below to deserialize the
 trace from a generated binary file.

\*
\*---- MODULE AgentKb_TETrace ----
\*EXTENDS IOUtils, AgentKb, TLC
\*
\*trace == IODeserialize("AgentKb_TTrace_1780433476.bin", TRUE)
\*
\*=============================================================================
\*

---- MODULE AgentKb_TETrace ----
EXTENDS AgentKb, TLC

trace == 
    <<
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "idle", p2 |-> "idle"],log |-> <<>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "absent"], e2 |-> [type |-> "absent"]],lock_holder |-> "none"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "write_acquiring", p2 |-> "idle"],log |-> <<>>,pending |-> [p1 |-> [data |-> "d1", action |-> "upsert", id |-> "e1"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "absent"], e2 |-> [type |-> "absent"]],lock_holder |-> "none"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "write_appending", p2 |-> "idle"],log |-> <<>>,pending |-> [p1 |-> [data |-> "d1", action |-> "upsert", id |-> "e1"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "absent"], e2 |-> [type |-> "absent"]],lock_holder |-> "p1"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "write_materializing", p2 |-> "idle"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"]>>,pending |-> [p1 |-> [data |-> "d1", action |-> "upsert", id |-> "e1"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "absent"], e2 |-> [type |-> "absent"]],lock_holder |-> "p1"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "write_releasing", p2 |-> "idle"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"]>>,pending |-> [p1 |-> [data |-> "d1", action |-> "upsert", id |-> "e1"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> FALSE], e2 |-> [type |-> "absent"]],lock_holder |-> "p1"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "idle", p2 |-> "idle"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> FALSE], e2 |-> [type |-> "absent"]],lock_holder |-> "none"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "compact_acquiring", p2 |-> "idle"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> FALSE], e2 |-> [type |-> "absent"]],lock_holder |-> "none"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "compact_acquiring", p2 |-> "write_acquiring"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "expire", id |-> "e1"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> FALSE], e2 |-> [type |-> "absent"]],lock_holder |-> "none"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "compact_acquiring", p2 |-> "write_appending"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "expire", id |-> "e1"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> FALSE], e2 |-> [type |-> "absent"]],lock_holder |-> "p2"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "compact_acquiring", p2 |-> "write_materializing"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"], [action |-> "expire", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "expire", id |-> "e1"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> FALSE], e2 |-> [type |-> "absent"]],lock_holder |-> "p2"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "compact_acquiring", p2 |-> "write_releasing"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"], [action |-> "expire", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "expire", id |-> "e1"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> TRUE], e2 |-> [type |-> "absent"]],lock_holder |-> "p2"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "compact_acquiring", p2 |-> "idle"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"], [action |-> "expire", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> TRUE], e2 |-> [type |-> "absent"]],lock_holder |-> "none"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "compact_running", p2 |-> "idle"],log |-> <<[data |-> "d1", action |-> "upsert", id |-> "e1"], [action |-> "expire", id |-> "e1"]>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> TRUE], e2 |-> [type |-> "absent"]],lock_holder |-> "p1"]),
    ([snap_len |-> [p1 |-> 0, p2 |-> 0],pc |-> [p1 |-> "idle", p2 |-> "idle"],log |-> <<>>,pending |-> [p1 |-> [action |-> "none"], p2 |-> [action |-> "none"]],db |-> [e1 |-> [type |-> "present", data |-> "d1", stale |-> TRUE], e2 |-> [type |-> "absent"]],lock_holder |-> "none"])
    >>
----


=============================================================================

---- CONFIG AgentKb_TTrace_1780433476 ----
CONSTANTS
    Procs = { "p1" , "p2" }
    EntryIds = { "e1" , "e2" }
    DataVals = { "d1" }
    MaxLogLen = 3

INVARIANT
    _inv

CHECK_DEADLOCK
    \* CHECK_DEADLOCK off because of PROPERTY or INVARIANT above.
    FALSE

INIT
    _init

NEXT
    _next

CONSTANT
    _TETrace <- _trace

ALIAS
    _expression
=============================================================================
\* Generated on Tue Jun 02 22:51:24 CEST 2026