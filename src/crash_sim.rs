//! Crash-simulation kill points for subprocess durability tests.
//!
//! Add a new kill point by extending [`KillPoint`] with a stable label, then
//! call [`kill_point`] at the exact crash window you want to exercise. Crash
//! tests should spawn a subprocess, set `KB_CRASH_AFTER=<label>`, trigger the
//! write path, and inspect on-disk state from the parent after the child exits.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum KillPoint {
    AfterLogBatch,
    AfterLogLine,
    AfterCommitMarker,
    AfterSync,
    BeforeApply,
    AfterApply,
    CompactAfterRewrite,
    /// Between compaction's generation bump and its rename of the rewritten
    /// log into place.
    CompactAfterGenerationBump,
    /// D4 step 1 gate, `KP_PRE_CHECKPOINT`: before the live-DB checkpoint.
    SwapPreCheckpoint,
    /// D4 step 1, `KP_POST_CHECKPOINT`: after the checkpoint, before the
    /// zero-length `-wal` verification.
    SwapPostCheckpoint,
    /// D4 steps 2-3, `KP_POST_TMP_SYNC`: after verify + close + tmp `sync_all`.
    SwapPostTmpSync,
    /// D4 step 4, `KP_POST_RENAME`.
    SwapAfterRename,
    /// D4 step 5, `KP_POST_UNLINK`.
    SwapAfterUnlink,
    /// D4 step 6, `KP_POST_DIR_SYNC`.
    SwapPostDirSync,
    AuditAfterRunInsert,
}

impl KillPoint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AfterLogBatch => "after-log-batch",
            Self::AfterLogLine => "after-log-line",
            Self::AfterCommitMarker => "after-commit-marker",
            Self::AfterSync => "after-sync",
            Self::BeforeApply => "before-apply",
            Self::AfterApply => "after-apply",
            Self::CompactAfterRewrite => "compact-after-rewrite",
            Self::CompactAfterGenerationBump => "compact-after-generation-bump",
            Self::SwapPreCheckpoint => "swap-pre-checkpoint",
            Self::SwapPostCheckpoint => "swap-post-checkpoint",
            Self::SwapPostTmpSync => "swap-post-tmp-sync",
            Self::SwapAfterRename => "swap-after-rename",
            Self::SwapAfterUnlink => "swap-after-unlink",
            Self::SwapPostDirSync => "swap-post-dir-sync",
            Self::AuditAfterRunInsert => "audit-after-run-insert",
        }
    }
}

impl fmt::Display for KillPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ParseKillPointError {
    input: String,
}

impl fmt::Display for ParseKillPointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown kill point {:?}", self.input)
    }
}

impl std::error::Error for ParseKillPointError {}

impl FromStr for KillPoint {
    type Err = ParseKillPointError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "after-log-batch" => Ok(Self::AfterLogBatch),
            "after-log-line" => Ok(Self::AfterLogLine),
            "after-commit-marker" => Ok(Self::AfterCommitMarker),
            "after-sync" => Ok(Self::AfterSync),
            "before-apply" => Ok(Self::BeforeApply),
            "after-apply" => Ok(Self::AfterApply),
            "compact-after-rewrite" => Ok(Self::CompactAfterRewrite),
            "compact-after-generation-bump" => Ok(Self::CompactAfterGenerationBump),
            "swap-pre-checkpoint" => Ok(Self::SwapPreCheckpoint),
            "swap-post-checkpoint" => Ok(Self::SwapPostCheckpoint),
            "swap-post-tmp-sync" => Ok(Self::SwapPostTmpSync),
            "swap-after-rename" => Ok(Self::SwapAfterRename),
            "swap-after-unlink" => Ok(Self::SwapAfterUnlink),
            "swap-post-dir-sync" => Ok(Self::SwapPostDirSync),
            "audit-after-run-insert" => Ok(Self::AuditAfterRunInsert),
            _ => Err(ParseKillPointError {
                input: value.to_string(),
            }),
        }
    }
}

#[cfg(any(test, feature = "crash-sim"))]
#[inline]
pub fn kill_point(label: KillPoint) {
    let armed = std::env::var("KB_CRASH_AFTER")
        .ok()
        .is_some_and(|value| value == label.as_str());
    if armed {
        std::process::exit(137);
    }
}

#[cfg(not(any(test, feature = "crash-sim")))]
#[inline(always)]
pub fn kill_point(_label: KillPoint) {}

#[cfg(test)]
mod tests {
    use super::KillPoint;
    use std::str::FromStr;

    #[test]
    fn test_kill_point_round_trips_stable_label() {
        let label = KillPoint::AfterLogBatch.to_string();
        assert_eq!(
            KillPoint::from_str(&label).unwrap(),
            KillPoint::AfterLogBatch
        );
    }

    #[test]
    fn test_every_d4_swap_kill_point_round_trips() {
        // Enumerated, not sampled: D4's six named swap kill points must each
        // have a stable label the crash tests can pass through KB_CRASH_AFTER.
        for point in [
            KillPoint::SwapPreCheckpoint,
            KillPoint::SwapPostCheckpoint,
            KillPoint::SwapPostTmpSync,
            KillPoint::SwapAfterRename,
            KillPoint::SwapAfterUnlink,
            KillPoint::SwapPostDirSync,
        ] {
            assert_eq!(KillPoint::from_str(&point.to_string()).unwrap(), point);
        }
    }
}
