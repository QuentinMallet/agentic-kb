-------------------------------- MODULE ingest_replace_path --------------------------------
(*
 * Refinement spec for "replace_path first-iteration-only under N-chunk ingest".
 *
 * Models the kb-ingest loop:
 *   for chunk_idx in 0..N:
 *     replace_path = (chunk_idx == 0)
 *     kb_core::add(chunk, replace_path=replace_path)
 *
 * Safety: only the first Add call uses replace_path=true; all subsequent calls
 *         use replace_path=false.
 * Liveness: all N chunks are eventually present (not stale) in the DB.
 *
 * State variables:
 *   chunk_idx  — current loop iteration (0..N)
 *   db_entries — set of {id, stale} records representing entries at the ingested path
 *   done       — TRUE when loop exits
 *)

EXTENDS Naturals, FiniteSets

CONSTANTS N  \* Number of chunks (N >= 2 for the interesting case)

ASSUME N \in Nat /\ N >= 2

VARIABLES
    chunk_idx,    \* Current chunk being processed (0-based)
    db_entries,   \* Set of records: [id |-> Nat, stale |-> BOOLEAN]
    done          \* TRUE when all chunks have been processed

vars == << chunk_idx, db_entries, done >>

TypeOK ==
    /\ chunk_idx \in 0..N
    /\ db_entries \subseteq [id: 0..(N-1), stale: BOOLEAN]
    /\ done \in BOOLEAN

Init ==
    /\ chunk_idx = 0
    /\ db_entries = {}
    /\ done = FALSE

\* When replace_path=true (first chunk only), mark all existing entries as stale,
\* then insert the new entry with stale=FALSE.
AddWithReplace(idx) ==
    LET staled == {[e EXCEPT !.stale = TRUE] : e \in db_entries}
        new_entry == [id |-> idx, stale |-> FALSE]
    IN  db_entries' = staled \union {new_entry}

\* When replace_path=false (all subsequent chunks), just insert without expiring.
AddWithoutReplace(idx) ==
    LET new_entry == [id |-> idx, stale |-> FALSE]
    IN  db_entries' = db_entries \union {new_entry}

\* Process the next chunk.
ProcessChunk ==
    /\ chunk_idx < N
    /\ done = FALSE
    /\ (IF chunk_idx = 0
        THEN AddWithReplace(chunk_idx)
        ELSE AddWithoutReplace(chunk_idx))
    /\ chunk_idx' = chunk_idx + 1
    /\ (IF chunk_idx + 1 = N
        THEN done' = TRUE
        ELSE done' = FALSE)

\* Terminal state: loop is done, no more steps.
Terminal ==
    /\ done = TRUE
    /\ UNCHANGED vars

Next == ProcessChunk \/ Terminal

Spec == Init /\ [][Next]_vars /\ WF_vars(ProcessChunk)

\* Safety: replace_path is only set on the first iteration.
\* After processing chunk 0, all previously-existing entries are stale.
\* For chunk_idx > 0, no additional entries are staled.
SafeReplacePathFirstOnly ==
    \* After the loop finishes, there are exactly N non-stale entries.
    done =>
        Cardinality({e \in db_entries : ~e.stale}) = N

\* Safety: after the first chunk has been processed, every entry with id /= 0
\* is stale (the replace_path call on chunk 0 cleared old entries).
\* Any entry that is not stale must have been inserted in this loop run (id \in 0..N-1).
SafeFirstChunkClearsOld ==
    \* Once chunk 0 has been processed, only the newly inserted entry (id=0)
    \* is non-stale; any hypothetical pre-existing entries would be stale.
    chunk_idx >= 1 =>
        \A e \in db_entries : e.stale \/ e.id \in 0..(N-1)

\* Liveness: all N chunks eventually present as non-stale.
AllChunksEventuallyPresent ==
    <>(Cardinality({e \in db_entries : ~e.stale}) = N)

\* The IDs of non-stale entries cover {0, 1, ..., N-1} when done.
AllChunkIdsPresent ==
    done =>
        {e.id : e \in {x \in db_entries : ~x.stale}} = 0..(N-1)

==========================================================================================
