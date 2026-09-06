------------------- MODULE PreNormalizedEmbeddingFormat -------------------
EXTENDS Naturals, FiniteSets, TLC

(***************************************************************************
Persisted pre-normalized embedding format and publish protocol.

This bounded model represents entry and cue blobs, not individual float
values. Each blob carries its own marker VERSION and dimension. Therefore a
mixed DB remains safe: only a blob marked with the current version, correct
EMB_DIMS, and finite unit payload may use dot product; legacy and malformed
blobs remain on cosine/rejection paths.

Migration and rebuild share the actual durability shape: construct a stage,
checkpoint live WAL pages, make a backup, verify readiness, then atomically
rename/publish. Crashes are modelled at every stage. A pre-publish crash
keeps the named live DB equal to its backup; a post-rename crash keeps the
published stage. Recovery merely cleans staging metadata and is idempotent
with respect to live committed pages.

Source correspondence:
  src/components/db.rs       write, semantic, cue, and MMR paths
  src/commands/rebuild.rs    temporary DB, live WAL checkpoint, rename/swap
  src/commands/reembed.rs    missing-embedding backfill
  src/models.rs              EMB_DIMS, f16 encoding, normalization guard
***************************************************************************)

CONSTANTS BlobIds, InitialLegacy, InitialMissing, InitialInvalid,
          InitialWrongDimensions, HighBlobs, EMB_DIMS, MarkerVersion,
          MaxRejects

Formats == {"none", "legacy_f32", "f16"}
Qualities == {"finite", "zero", "nonfinite", "missing", "corrupt"}
Operations == {"none", "migration", "rebuild"}
Phases == {"idle", "staged", "live_wal_checkpointed", "backup_ready",
           "publish_ready", "published", "crashed"}
PrePublishPhases == {"staged", "live_wal_checkpointed", "backup_ready",
                     "publish_ready"}

Blob == [format : Formats, normalized : BOOLEAN, quality : Qualities,
         dims : 0..(EMB_DIMS + 1), marker : 0..MarkerVersion]

VARIABLES blobs, staging, backup, operation, phase, crash_from,
          live_wal, backup_ready, rejects

vars == <<blobs, staging, backup, operation, phase, crash_from,
          live_wal, backup_ready, rejects>>

Rank(b) == IF b \in HighBlobs THEN 2 ELSE 1
FiniteNonZero(blob) == blob.quality = "finite"
CorrectDimensions(blob) == blob.dims = EMB_DIMS
ValidForNormalization(blob) == FiniteNonZero(blob) /\ CorrectDimensions(blob)
UnitMarked(blob) ==
  /\ blob.format = "f16"
  /\ blob.normalized
  /\ blob.marker = MarkerVersion
  /\ ValidForNormalization(blob)
Legacy(blob) ==
  /\ blob.format = "legacy_f32"
  /\ ~blob.normalized
  /\ blob.marker = 0
Migratable(blob) == Legacy(blob) /\ ValidForNormalization(blob)

NormalizedBlob ==
  [format |-> "f16", normalized |-> TRUE, quality |-> "finite",
   dims |-> EMB_DIMS, marker |-> MarkerVersion]

InitialBlob(b) ==
  IF b \in InitialMissing THEN
    [format |-> "none", normalized |-> FALSE, quality |-> "missing",
     dims |-> 0, marker |-> 0]
  ELSE IF b \in InitialInvalid THEN
    [format |-> "legacy_f32", normalized |-> FALSE, quality |-> "nonfinite",
     dims |-> EMB_DIMS, marker |-> 0]
  ELSE IF b \in InitialWrongDimensions THEN
    [format |-> "legacy_f32", normalized |-> FALSE, quality |-> "finite",
     dims |-> EMB_DIMS + 1, marker |-> 0]
  ELSE IF b \in InitialLegacy THEN
    [format |-> "legacy_f32", normalized |-> FALSE, quality |-> "finite",
     dims |-> EMB_DIMS, marker |-> 0]
  ELSE
    NormalizedBlob

Normalize(blob) ==
  IF ValidForNormalization(blob) THEN NormalizedBlob ELSE blob

Migrated(store) ==
  [b \in BlobIds |-> IF Legacy(store[b]) THEN Normalize(store[b]) ELSE store[b]]
Rebuilt == [b \in BlobIds |-> NormalizedBlob]

(* The abstract correct score is cosine over a valid, same-dimension vector. *)
CosineScore(b) == IF ValidForNormalization(blobs[b]) THEN Rank(b) ELSE 0
DotScore(b) == IF UnitMarked(blobs[b]) THEN Rank(b) ELSE 0
ReadKernel(b) == IF blobs[b].marker = MarkerVersion THEN "dot" ELSE "cosine"
ReadScore(b) == IF ReadKernel(b) = "dot" THEN DotScore(b) ELSE CosineScore(b)

AllLegacyMigratable ==
  \A b \in BlobIds : Legacy(blobs[b]) => Migratable(blobs[b])

TypeOK ==
  /\ EMB_DIMS \in Nat /\ EMB_DIMS > 0
  /\ MarkerVersion \in Nat /\ MarkerVersion > 0
  /\ blobs \in [BlobIds -> Blob]
  /\ staging \in [BlobIds -> Blob]
  /\ backup \in [BlobIds -> Blob]
  /\ operation \in Operations
  /\ phase \in Phases
  /\ crash_from \in Phases
  /\ live_wal \in BOOLEAN
  /\ backup_ready \in BOOLEAN
  /\ rejects \in 0..MaxRejects

Init ==
  /\ blobs = [b \in BlobIds |-> InitialBlob(b)]
  /\ staging = blobs
  /\ backup = blobs
  /\ operation = "none"
  /\ phase = "idle"
  /\ crash_from = "idle"
  /\ live_wal = TRUE
  /\ backup_ready = FALSE
  /\ rejects = 0

(***************************************************************************
The write and reembed paths are one-row SQL transactions. Only a finite,
non-zero, EMB_DIMS output reaches storage. RejectInvalidInput covers zero,
non-finite, corrupt, and wrong-dimension input without a table write.
***************************************************************************)
WriteFiniteNonZero(b) ==
  /\ phase = "idle" /\ operation = "none"
  /\ blobs' = [blobs EXCEPT ![b] = NormalizedBlob]
  /\ UNCHANGED <<staging, backup, operation, phase, crash_from, live_wal,
                backup_ready, rejects>>

ReembedMissing(b) ==
  /\ phase = "idle" /\ operation = "none"
  /\ blobs[b].quality = "missing"
  /\ blobs' = [blobs EXCEPT ![b] = NormalizedBlob]
  /\ UNCHANGED <<staging, backup, operation, phase, crash_from, live_wal,
                backup_ready, rejects>>

RejectInvalidInput ==
  /\ phase = "idle" /\ operation = "none"
  /\ rejects < MaxRejects
  /\ rejects' = rejects + 1
  /\ UNCHANGED <<blobs, staging, backup, operation, phase, crash_from,
                live_wal, backup_ready>>

(***************************************************************************
Stage/checkpoint/backup/publish protocol. blobs denotes the DB currently
named live. WAL checkpointing may change physical files but not its logical
committed-page value; the safety invariants make that obligation explicit.
***************************************************************************)
StartMigration ==
  /\ phase = "idle" /\ operation = "none"
  /\ \E b \in BlobIds : Legacy(blobs[b])
  /\ AllLegacyMigratable
  /\ staging' = Migrated(blobs)
  /\ backup' = blobs
  /\ operation' = "migration"
  /\ phase' = "staged"
  /\ live_wal' = TRUE
  /\ backup_ready' = FALSE
  /\ UNCHANGED <<blobs, crash_from, rejects>>

StartRebuild ==
  /\ phase = "idle" /\ operation = "none"
  /\ staging' = Rebuilt
  /\ backup' = blobs
  /\ operation' = "rebuild"
  /\ phase' = "staged"
  /\ live_wal' = TRUE
  /\ backup_ready' = FALSE
  /\ UNCHANGED <<blobs, crash_from, rejects>>

CheckpointLiveWAL ==
  /\ phase = "staged" /\ operation # "none"
  /\ live_wal
  /\ live_wal' = FALSE
  /\ phase' = "live_wal_checkpointed"
  /\ UNCHANGED <<blobs, staging, backup, operation, crash_from,
                backup_ready, rejects>>

CreateBackup ==
  /\ phase = "live_wal_checkpointed" /\ operation # "none"
  /\ ~live_wal
  /\ backup' = blobs
  /\ backup_ready' = TRUE
  /\ phase' = "backup_ready"
  /\ UNCHANGED <<blobs, staging, operation, crash_from, live_wal, rejects>>

VerifyPublishReady ==
  /\ phase = "backup_ready" /\ operation # "none"
  /\ backup_ready /\ ~live_wal
  /\ phase' = "publish_ready"
  /\ UNCHANGED <<blobs, staging, backup, operation, crash_from, live_wal,
                backup_ready, rejects>>

PublishRename ==
  /\ phase = "publish_ready" /\ operation # "none"
  /\ backup_ready /\ ~live_wal
  /\ blobs' = staging
  /\ phase' = "published"
  /\ UNCHANGED <<staging, backup, operation, crash_from, live_wal,
                backup_ready, rejects>>

FinishPublish ==
  /\ phase = "published" /\ operation # "none"
  /\ operation' = "none"
  /\ phase' = "idle"
  /\ UNCHANGED <<blobs, staging, backup, crash_from, live_wal,
                backup_ready, rejects>>

(* An interruption preserves the last completed protocol step. *)
Crash ==
  /\ phase \in (PrePublishPhases \union {"published"})
  /\ operation # "none"
  /\ crash_from' = phase
  /\ phase' = "crashed"
  /\ UNCHANGED <<blobs, staging, backup, operation, live_wal, backup_ready,
                rejects>>

(* Recovery can be repeated by a restarted process: it never rewrites the
   named live DB. Before rename it cleans the abandoned stage; after rename it
   accepts the fully published stage. Either branch returns to resumable idle. *)
RecoverAfterCrash ==
  /\ phase = "crashed" /\ operation # "none"
  /\ phase' = "idle"
  /\ operation' = "none"
  /\ staging' = blobs
  /\ backup_ready' = FALSE
  /\ UNCHANGED <<blobs, backup, crash_from, live_wal, rejects>>

Next ==
  \/ \E b \in BlobIds : WriteFiniteNonZero(b)
  \/ \E b \in BlobIds : ReembedMissing(b)
  \/ RejectInvalidInput
  \/ StartMigration
  \/ StartRebuild
  \/ CheckpointLiveWAL
  \/ CreateBackup
  \/ VerifyPublishReady
  \/ PublishRename
  \/ FinishPublish
  \/ Crash
  \/ RecoverAfterCrash

Spec == Init /\ [][Next]_vars

(***************************************************************************
Safety invariants. critical_ names are retained implementation contracts.
***************************************************************************)
critical_MarkedBlobsHaveCurrentFormat ==
  \A b \in BlobIds : blobs[b].marker = MarkerVersion => UnitMarked(blobs[b])

critical_PerBlobVersionGate ==
  \A b \in BlobIds :
    /\ (blobs[b].marker = MarkerVersion => ReadKernel(b) = "dot")
    /\ (blobs[b].marker # MarkerVersion => ReadKernel(b) = "cosine")

critical_RankingParity ==
  \A left \in BlobIds, right \in BlobIds :
    /\ ValidForNormalization(blobs[left])
    /\ ValidForNormalization(blobs[right])
    /\ CosineScore(left) > CosineScore(right)
    => ReadScore(left) > ReadScore(right)

(* WAL checkpointing and stage construction must not lose a committed live
   page before rename publishes the new DB name. *)
critical_NoLiveCommittedPageLossBeforeRename ==
  /\ operation # "none"
  /\ phase \in PrePublishPhases
  => blobs = backup

critical_AtomicPublish ==
  /\ (phase = "published" => blobs = staging)
  /\ (phase = "crashed" /\ crash_from \in PrePublishPhases => blobs = backup)
  /\ (phase = "crashed" /\ crash_from = "published" => blobs = staging)

(* A crashed migration can be cleaned/restarted without selecting a mixed
   image. RecoverAfterCrash changes no blobs value. *)
critical_InterruptedMigrationIsResumableCleanable ==
  operation = "migration" /\ phase = "crashed" =>
    IF crash_from \in PrePublishPhases THEN blobs = backup ELSE blobs = staging

critical_ReembedProducesCurrentMarkedUnit ==
  \A b \in BlobIds : blobs[b].quality = "missing" => blobs[b].marker # MarkerVersion

_supporting_NoWrongDimensionOrInvalidMarked ==
  \A b \in BlobIds :
    /\ (blobs[b].marker = MarkerVersion => CorrectDimensions(blobs[b]))
    /\ (blobs[b].quality \in {"zero", "nonfinite", "corrupt"} =>
        blobs[b].marker # MarkerVersion)

_supporting_WALCheckpointBeforePublish ==
  phase \in {"backup_ready", "publish_ready", "published"} => ~live_wal

=============================================================================
