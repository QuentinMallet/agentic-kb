--------------------------- MODULE McpBoundary ---------------------------
EXTENDS Naturals, Sequences

CONSTANTS MaxFrame, FixedDesign
ASSUME /\ MaxFrame = 2 /\ FixedDesign \in BOOLEAN

Data == 0
NL == 1
EmptyEOF == <<>>
AtLimitNewline == <<Data, Data, NL>>
AtLimitEOF == <<Data, Data>>
MultiFrame == <<Data, NL, Data, Data, NL>>
OversizeRecovery == <<Data, Data, Data, NL, Data, NL>>
OversizeEOF == <<Data, Data, Data>>
Scenarios == {EmptyEOF, AtLimitNewline, AtLimitEOF,
              MultiFrame, OversizeRecovery, OversizeEOF}

VARIABLES stream, pos, bufferLen, discarding, oversizeRejected,
          dispatches, rejectedDispatches, recoveryHandled,
          partialEOFHandled, stopped, maxBuffered
vars == <<stream, pos, bufferLen, discarding, oversizeRejected,
          dispatches, rejectedDispatches, recoveryHandled,
          partialEOFHandled, stopped, maxBuffered>>

TypeOK ==
  /\ stream \in Scenarios /\ pos \in 1..(Len(stream) + 1)
  /\ bufferLen \in 0..(MaxFrame + 1) /\ discarding \in BOOLEAN
  /\ oversizeRejected \in BOOLEAN /\ dispatches \in 0..2
  /\ rejectedDispatches \in 0..1 /\ recoveryHandled \in BOOLEAN
  /\ partialEOFHandled \in BOOLEAN /\ stopped \in BOOLEAN
  /\ maxBuffered \in 0..(MaxFrame + 1)

Init ==
  /\ stream \in Scenarios /\ pos = 1 /\ bufferLen = 0
  /\ discarding = FALSE /\ oversizeRejected = FALSE
  /\ dispatches = 0 /\ rejectedDispatches = 0
  /\ recoveryHandled = FALSE /\ partialEOFHandled = FALSE
  /\ stopped = FALSE /\ maxBuffered = 0

ReadData ==
  /\ ~stopped /\ pos <= Len(stream) /\ stream[pos] = Data /\ pos' = pos + 1
  /\ IF discarding THEN
       UNCHANGED <<bufferLen, discarding, oversizeRejected, dispatches,
                   rejectedDispatches, recoveryHandled, partialEOFHandled, maxBuffered>>
     ELSE IF bufferLen < MaxFrame THEN
       /\ bufferLen' = bufferLen + 1
       /\ maxBuffered' = IF maxBuffered >= bufferLen + 1 THEN maxBuffered ELSE bufferLen + 1
       /\ UNCHANGED <<discarding, oversizeRejected, dispatches,
                       rejectedDispatches, recoveryHandled, partialEOFHandled>>
     ELSE IF FixedDesign THEN
       /\ bufferLen' = 0 /\ discarding' = TRUE /\ oversizeRejected' = TRUE
       /\ UNCHANGED <<dispatches, rejectedDispatches, recoveryHandled,
                       partialEOFHandled, maxBuffered>>
     ELSE
       /\ bufferLen' = MaxFrame + 1 /\ maxBuffered' = MaxFrame + 1
       /\ UNCHANGED <<discarding, oversizeRejected, dispatches,
                       rejectedDispatches, recoveryHandled, partialEOFHandled>>
  /\ UNCHANGED <<stream, stopped>>

ReadNewline ==
  /\ ~stopped /\ pos <= Len(stream) /\ stream[pos] = NL /\ pos' = pos + 1
  /\ IF discarding THEN
       /\ bufferLen' = 0 /\ discarding' = FALSE
       /\ UNCHANGED <<oversizeRejected, dispatches, rejectedDispatches,
                       recoveryHandled, partialEOFHandled, maxBuffered>>
     ELSE
       /\ dispatches' = dispatches + 1
       /\ rejectedDispatches' = rejectedDispatches + IF bufferLen > MaxFrame THEN 1 ELSE 0
       /\ recoveryHandled' = (recoveryHandled \/ oversizeRejected)
       /\ bufferLen' = 0
       /\ UNCHANGED <<discarding, oversizeRejected, partialEOFHandled, maxBuffered>>
  /\ UNCHANGED <<stream, stopped>>

EndOfInput ==
  /\ ~stopped /\ pos = Len(stream) + 1 /\ stopped' = TRUE
  /\ partialEOFHandled' = (bufferLen > 0 /\ ~discarding)
  /\ dispatches' = dispatches + IF bufferLen > 0 /\ ~discarding THEN 1 ELSE 0
  /\ rejectedDispatches' = rejectedDispatches + IF bufferLen > MaxFrame /\ ~discarding THEN 1 ELSE 0
  /\ UNCHANGED <<stream, pos, bufferLen, discarding, oversizeRejected,
                  recoveryHandled, maxBuffered>>

Next == ReadData \/ ReadNewline \/ EndOfInput
Spec == Init /\ [][Next]_vars /\ WF_vars(ReadData) /\ WF_vars(ReadNewline) /\ WF_vars(EndOfInput)

critical_BufferBounded == maxBuffered <= MaxFrame
critical_RejectedFrameNeverDispatched == rejectedDispatches = 0
critical_OversizeNeverBecomesPartialEOF == stream = OversizeEOF /\ stopped => ~partialEOFHandled
critical_ExactLimitEOFHandled == stream = AtLimitEOF /\ stopped => partialEOFHandled
critical_EmptyEOFDoesNotDispatch == stream = EmptyEOF /\ stopped => dispatches = 0
eventually_AllScenariosStop == <> stopped
eventually_RecoveryRealigns == stream = OversizeRecovery => <> recoveryHandled

=============================================================================
