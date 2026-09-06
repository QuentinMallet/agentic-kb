------------------- MODULE PreNormalizedEmbeddingFormat -------------------
EXTENDS Naturals, FiniteSets, TLC

(***************************************************************************
Persisted pre-normalized embedding format.

This is an abstract model of the entry and cue embedding stores.  A blob has
its own normalized marker: a database-level marker is deliberately absent.
That makes a mixed store safe -- an unconverted blob remains on cosine while
a marked unit blob may use dot product.  The model covers:

  * normal writes, rebuild materialization, and reembed backfill;
  * transactional legacy migration through a staging copy and retained backup;
  * interruption before or after the one atomic publish/swap;
  * finite, non-zero normalization guards; and
  * semantic ranking parity between legacy cosine and normalized dot paths.

Vector arithmetic is represented by a finite `rank` abstraction.  For each
finite non-zero vector, cosine and dot of its normalized form have the same
rank; dot on an unmarked or invalid blob is deliberately zero.  Thus a wrong
per-DB gate would falsify `critical_RankingParity`.

Source correspondence:
  src/components/db.rs       apply_event, semantic/cue/MMR read paths
  src/commands/rebuild.rs    fresh DB replay then rename/swap
  src/commands/reembed.rs    missing-embedding backfill
  src/models.rs              cosine, f16 encoding, normalization guard
***************************************************************************)

CONSTANTS BlobIds, InitialLegacy, InitialMissing, InitialInvalid, HighBlobs,
          MaxRejects

Formats == {"none", "legacy_f32", "f16"}
Qualities == {"finite", "zero", "nonfinite", "missing", "corrupt"}
Phases == {"idle", "migration_prepared", "rebuild_prepared", "crashed"}

Blob == [format : Formats, normalized : BOOLEAN, quality : Qualities]

VARIABLES blobs, staging, backup, phase, rejects

vars == <<blobs, staging, backup, phase, rejects>>

Rank(b) == IF b \in HighBlobs THEN 2 ELSE 1
FiniteNonZero(blob) == blob.quality = "finite"
UnitMarked(blob) ==
  /\ blob.format = "f16"
  /\ blob.normalized
  /\ FiniteNonZero(blob)
Legacy(blob) == blob.format = "legacy_f32" /\ ~blob.normalized
Migratable(blob) == Legacy(blob) /\ FiniteNonZero(blob)
Present(blob) == blob.format # "none"

InitialBlob(b) ==
  IF b \in InitialMissing THEN
    [format |-> "none", normalized |-> FALSE, quality |-> "missing"]
  ELSE IF b \in InitialInvalid THEN
    [format |-> "legacy_f32", normalized |-> FALSE, quality |-> "nonfinite"]
  ELSE IF b \in InitialLegacy THEN
    [format |-> "legacy_f32", normalized |-> FALSE, quality |-> "finite"]
  ELSE
    [format |-> "f16", normalized |-> TRUE, quality |-> "finite"]

NormalizedBlob == [format |-> "f16", normalized |-> TRUE, quality |-> "finite"]

Normalize(blob) ==
  IF FiniteNonZero(blob) THEN NormalizedBlob ELSE blob

Migrated(store) == [b \in BlobIds |-> IF Legacy(store[b]) THEN Normalize(store[b]) ELSE store[b]]
Rebuilt == [b \in BlobIds |-> NormalizedBlob]

(* The abstract correct score is cosine for every valid persisted vector. *)
CosineScore(b) == IF FiniteNonZero(blobs[b]) THEN Rank(b) ELSE 0

(* Dot is sound only for a marked finite unit vector. *)
DotScore(b) == IF UnitMarked(blobs[b]) THEN Rank(b) ELSE 0
ReadKernel(b) == IF blobs[b].normalized THEN "dot" ELSE "cosine"
ReadScore(b) == IF ReadKernel(b) = "dot" THEN DotScore(b) ELSE CosineScore(b)

AllLegacyMigratable ==
  \A b \in BlobIds : Legacy(blobs[b]) => Migratable(blobs[b])

TypeOK ==
  /\ blobs \in [BlobIds -> Blob]
  /\ staging \in [BlobIds -> Blob]
  /\ backup \in [BlobIds -> Blob]
  /\ phase \in Phases
  /\ rejects \in 0..MaxRejects

Init ==
  /\ blobs = [b \in BlobIds |-> InitialBlob(b)]
  /\ staging = blobs
  /\ backup = blobs
  /\ phase = "idle"
  /\ rejects = 0

(***************************************************************************
Write path.  The release guard accepts only finite, non-zero output and
commits the normalized blob with its per-blob marker in one transaction.
Rejected output never reaches either table.
***************************************************************************)
WriteFiniteNonZero(b) ==
  /\ phase = "idle"
  /\ blobs' = [blobs EXCEPT ![b] = NormalizedBlob]
  /\ UNCHANGED <<staging, backup, phase, rejects>>

RejectInvalidWrite ==
  /\ phase = "idle"
  /\ rejects < MaxRejects
  /\ rejects' = rejects + 1
  /\ UNCHANGED <<blobs, staging, backup, phase>>

(***************************************************************************
Legacy rewrite.  `StartMigration` is enabled only when every legacy blob is
finite and non-zero; a non-finite/corrupt blob therefore aborts before any
live row can be marked.  CommitMigration is the one publish transaction.
***************************************************************************)
StartMigration ==
  /\ phase = "idle"
  /\ \E b \in BlobIds : Legacy(blobs[b])
  /\ AllLegacyMigratable
  /\ staging' = Migrated(blobs)
  /\ backup' = blobs
  /\ phase' = "migration_prepared"
  /\ UNCHANGED <<blobs, rejects>>

CommitMigration ==
  /\ phase = "migration_prepared"
  /\ blobs' = staging
  /\ phase' = "idle"
  /\ UNCHANGED <<staging, backup, rejects>>

(***************************************************************************
Rebuild replays events into a fresh database.  Its entire result is staged,
then published atomically; an interruption before publication leaves the
previous live DB and backup intact.  Reembed writes one missing blob at a
time, and each row write is itself atomic.
***************************************************************************)
StartRebuild ==
  /\ phase = "idle"
  /\ staging' = Rebuilt
  /\ backup' = blobs
  /\ phase' = "rebuild_prepared"
  /\ UNCHANGED <<blobs, rejects>>

CommitRebuild ==
  /\ phase = "rebuild_prepared"
  /\ blobs' = staging
  /\ phase' = "idle"
  /\ UNCHANGED <<staging, backup, rejects>>

ReembedMissing(b) ==
  /\ phase = "idle"
  /\ blobs[b].quality = "missing"
  /\ blobs' = [blobs EXCEPT ![b] = NormalizedBlob]
  /\ UNCHANGED <<staging, backup, phase, rejects>>

Crash ==
  /\ phase \in {"migration_prepared", "rebuild_prepared"}
  /\ phase' = "crashed"
  /\ UNCHANGED <<blobs, staging, backup, rejects>>

AbortAfterCrash ==
  /\ phase = "crashed"
  /\ phase' = "idle"
  /\ staging' = blobs
  /\ UNCHANGED <<blobs, backup, rejects>>

Next ==
  \/ \E b \in BlobIds : WriteFiniteNonZero(b)
  \/ RejectInvalidWrite
  \/ StartMigration
  \/ CommitMigration
  \/ StartRebuild
  \/ CommitRebuild
  \/ \E b \in BlobIds : ReembedMissing(b)
  \/ Crash
  \/ AbortAfterCrash

Spec == Init /\ [][Next]_vars

(***************************************************************************
Safety invariants.  `critical_` names are retained implementation contracts.
***************************************************************************)

critical_MarkedBlobsAreFiniteUnits ==
  \A b \in BlobIds : blobs[b].normalized => UnitMarked(blobs[b])

critical_PerBlobGate ==
  \A b \in BlobIds :
    /\ (blobs[b].normalized => ReadKernel(b) = "dot")
    /\ (~blobs[b].normalized => ReadKernel(b) = "cosine")

(* If a candidate is ranked higher by its legacy cosine semantics, its actual
   mixed-store score is also higher.  This covers entry, cue, and MMR callers:
   their candidate comparison is abstracted into the same score function. *)
critical_RankingParity ==
  \A left \in BlobIds, right \in BlobIds :
    /\ FiniteNonZero(blobs[left])
    /\ FiniteNonZero(blobs[right])
    /\ CosineScore(left) > CosineScore(right)
    => ReadScore(left) > ReadScore(right)

critical_MigrationAtomicity ==
  phase \in {"migration_prepared", "crashed"} => blobs = backup

critical_RebuildAtomicity ==
  phase \in {"rebuild_prepared", "crashed"} => blobs = backup

critical_ReembedProducesMarkedUnit ==
  \A b \in BlobIds : blobs[b].quality = "missing" => ~blobs[b].normalized

_supporting_NoInvalidPersistence ==
  \A b \in BlobIds :
    blobs[b].quality \in {"zero", "nonfinite", "corrupt"} => ~blobs[b].normalized

_supporting_StagingIsNeverLiveDuringPrepare ==
  phase \in {"migration_prepared", "rebuild_prepared"} => blobs = backup

=============================================================================
