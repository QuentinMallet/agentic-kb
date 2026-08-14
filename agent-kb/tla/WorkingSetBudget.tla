--------------------------- MODULE WorkingSetBudget ---------------------------
(*
  WorkingSetBudget.tla  —  Budgeted context selection
  ===================================================

  This model captures `kb context --budget` as a pure, deterministic selector:

    * every candidate entry has a bounded relevance score and token cost
    * candidates are ranked by descending relevance
    * ties are broken by a fixed total order on entry id
    * selection is greedy: emit an entry only when its full token count fits
      in the remaining budget; otherwise skip it (never truncate)
    * entries below the relevance floor are never emitted

  The state is deliberately tiny.  Init quantifies over all bounded candidate
  maps, budgets, and floors.  Select is a single atomic action that writes the
  output sequence as the function of the inputs.
*)

EXTENDS Naturals, Sequences

CONSTANTS
  MaxEntries,
  MaxRel,
  MaxTok,
  Budgets

ASSUME MaxEntries \in Nat /\ MaxEntries > 0
ASSUME MaxRel \in Nat
ASSUME MaxTok \in Nat /\ MaxTok > 0
ASSUME Budgets \subseteq Nat /\ Budgets # {}

EntryIds == 1..MaxEntries
Candidate == [relevance : 0..MaxRel, tokens : 1..MaxTok]
Floors == 0..MaxRel

VARIABLES
  candidates,
  B,
  F,
  output,
  phase

vars == <<candidates, B, F, output, phase>>

TypeOK ==
  /\ candidates \in [EntryIds -> Candidate]
  /\ B \in Budgets
  /\ F \in Floors
  /\ output \in Seq(EntryIds)
  /\ phase \in {"idle", "done"}

Init ==
  /\ candidates \in [EntryIds -> Candidate]
  /\ B \in Budgets
  /\ F \in Floors
  /\ output = << >>
  /\ phase = "idle"

Better(i, j, cs) ==
  \/ cs[i].relevance > cs[j].relevance
  \/ /\ cs[i].relevance = cs[j].relevance
     /\ i < j

BestOf(ids, cs) ==
  CHOOSE best \in ids :
    \A other \in ids :
      \/ best = other
      \/ Better(best, other, cs)

RECURSIVE OrderedIds(_, _)
OrderedIds(ids, cs) ==
  IF ids = {}
  THEN << >>
  ELSE LET best == BestOf(ids, cs)
       IN <<best>> \o OrderedIds(ids \ {best}, cs)

RECURSIVE GreedySelect(_, _, _, _, _, _)
GreedySelect(order, idx, spent, cs, budget, floor) ==
  IF idx > Len(order)
  THEN << >>
  ELSE LET eid == order[idx]
           entry == cs[eid]
       IN IF entry.relevance < floor
             THEN GreedySelect(order, idx + 1, spent, cs, budget, floor)
             ELSE IF spent + entry.tokens <= budget
                     THEN <<eid>> \o
                          GreedySelect(order,
                                       idx + 1,
                                       spent + entry.tokens,
                                       cs,
                                       budget,
                                       floor)
                     ELSE GreedySelect(order,
                                       idx + 1,
                                       spent,
                                       cs,
                                       budget,
                                       floor)

SelectFn(cs, budget, floor) ==
  GreedySelect(OrderedIds(EntryIds, cs), 1, 0, cs, budget, floor)

RECURSIVE SumTokens(_, _)
SumTokens(ids, cs) ==
  IF ids = << >>
  THEN 0
  ELSE cs[Head(ids)].tokens + SumTokens(Tail(ids), cs)

AllBelowFloor(cs, floor) ==
  \A eid \in EntryIds : cs[eid].relevance < floor

Select ==
  /\ phase = "idle"
  /\ output' = SelectFn(candidates, B, F)
  /\ phase' = "done"
  /\ UNCHANGED <<candidates, B, F>>

Done ==
  /\ phase = "done"
  /\ UNCHANGED vars

Next ==
  \/ Select
  \/ Done

BudgetNeverExceeded ==
  SumTokens(output, candidates) <= B

FloorSilence ==
  /\ \A i \in 1..Len(output) : candidates[output[i]].relevance >= F
  /\ (AllBelowFloor(candidates, F) => output = << >>)

Deterministic ==
  phase = "idle" \/ output = SelectFn(candidates, B, F)

Spec ==
  /\ Init
  /\ [][Next]_vars

=============================================================================
