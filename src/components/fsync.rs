//! Filesystem durability helpers shared by log compaction and DB rebuild.

use anyhow::Context;
use std::fs;
use std::path::Path;

/// Fsync the directory containing `path`, making a create or rename durable.
pub(crate) fn sync_parent_dir(path: &Path) -> anyhow::Result<()> {
    let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    sync_dir(dir)
}

/// Fsync a directory entry itself.
pub(crate) fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .with_context(|| format!("fsync directory {}", dir.display()))?;
    Ok(())
}
