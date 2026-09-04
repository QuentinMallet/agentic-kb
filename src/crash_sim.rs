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
    SwapAfterRename,
    SwapAfterUnlink,
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
            Self::SwapAfterRename => "swap-after-rename",
            Self::SwapAfterUnlink => "swap-after-unlink",
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
            "swap-after-rename" => Ok(Self::SwapAfterRename),
            "swap-after-unlink" => Ok(Self::SwapAfterUnlink),
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
}
