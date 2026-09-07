//! Transactional persisted-embedding format migration.

use crate::commands::add::acquire_lock;
use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
use anyhow::{bail, Context};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const BACKUP_SUFFIX: &str = "db.pre-normalized-embeddings.bak";
const STAGING_SUFFIX: &str = "db.pre-normalized-embeddings.stage";
const STATE_SUFFIX: &str = "db.pre-normalized-embeddings.state";

#[derive(Debug)]
struct ReadyState {
    source_digest: String,
    migrated: usize,
}

/// Rewrite legacy embedding blobs into normalized f16 blobs.
#[derive(Command, Debug, Parser)]
pub struct MigrateEmbeddings;

impl Runnable for MigrateEmbeddings {
    fn run(&self) {
        self.execute().unwrap_or_else(|error| {
            eprintln!("Error: {error}");
            std::process::exit(1);
        });
    }
}

impl MigrateEmbeddings {
    pub fn execute(&self) -> anyhow::Result<usize> {
        let paths = config::Paths::discover()?;
        self.execute_with(&paths)
    }

    /// Migrate through a durable staged copy, then atomically publish it.
    ///
    /// The state file is written only after the staged copy is normalized,
    /// checked, checkpointed, and bound to a digest of the live source. A
    /// later invocation resumes that exact publish only if the source is
    /// unchanged; otherwise it discards the stale stage and starts again.
    pub fn execute_with(&self, paths: &config::Paths) -> anyhow::Result<usize> {
        let lock = acquire_lock(&paths.lock)?;

        if let Some(migrated) = recover_ready_stage(paths, &lock)? {
            return Ok(migrated);
        }

        // A stage without a ready manifest was interrupted before validation
        // completed. It cannot be published, but the live DB is still the
        // authority and the retained backup remains available for rollback.
        remove_staging_artifacts(paths)?;

        let source_digest = checkpoint_database(&paths.db, "live")?;
        let live = db::open_rw(paths, &lock)?;
        if pending_embeddings(&live)? == 0 {
            return Ok(0);
        }

        let backup = backup_path(paths);
        if !backup.exists() {
            vacuum_into(&live, &backup, "pre-migration backup")?;
        }
        let staged = staging_path(paths);
        vacuum_into(&live, &staged, "migration staging DB")?;
        drop(live);

        let migration = (|| -> anyhow::Result<usize> {
            let staged_conn = db::open_scratch(&staged)?;
            let migrated = db::migrate_embeddings(&staged_conn)?;
            drop(staged_conn);
            verify_staged_database(&staged)?;
            Ok(migrated)
        })();
        let migrated = match migration {
            Ok(migrated) => migrated,
            Err(error) => {
                remove_staging_artifacts(paths)?;
                return Err(error);
            }
        };

        write_ready_state(
            &state_path(paths),
            &ReadyState {
                source_digest,
                migrated,
            },
        )?;
        publish_ready_stage(paths, &read_ready_state(&state_path(paths))?)
    }
}

fn backup_path(paths: &config::Paths) -> PathBuf {
    paths.db.with_extension(BACKUP_SUFFIX)
}

fn staging_path(paths: &config::Paths) -> PathBuf {
    paths.db.with_extension(STAGING_SUFFIX)
}

fn state_path(paths: &config::Paths) -> PathBuf {
    paths.db.with_extension(STATE_SUFFIX)
}

fn pending_embeddings(conn: &rusqlite::Connection) -> anyhow::Result<usize> {
    let entries: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries_emb WHERE normalized=0",
        [],
        |row| row.get(0),
    )?;
    let cues: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cues WHERE embedding IS NOT NULL AND normalized=0",
        [],
        |row| row.get(0),
    )?;
    Ok((entries + cues) as usize)
}

fn vacuum_into(
    conn: &rusqlite::Connection,
    target: &Path,
    description: &str,
) -> anyhow::Result<()> {
    let target_sql = target.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{target_sql}'"))
        .with_context(|| format!("create {description} {}", target.display()))
}

/// Checkpoint the database and prove no live WAL frames remain before any
/// sidecar is removed. The caller holds the application write lock; a busy
/// checkpoint is a hard failure rather than a potentially lossy publish.
fn checkpoint_database(path: &Path, label: &str) -> anyhow::Result<String> {
    let conn = if db::is_live_db_path(path) {
        db::open_live_for_checkpoint(path)?
    } else {
        db::open_scratch(path)?
    };
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 || log_frames != 0 || checkpointed_frames != 0 {
        bail!(
            "cannot safely publish {label} database: WAL checkpoint incomplete \
             (busy={busy}, log_frames={log_frames}, checkpointed_frames={checkpointed_frames})"
        );
    }
    drop(conn);

    remove_database_sidecars(path)?;
    database_digest(path)
}

fn remove_database_sidecars(path: &Path) -> anyhow::Result<()> {
    let raw = path.to_string_lossy();
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{raw}{suffix}"));
        match fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", sidecar.display()))
            }
        }
    }
    Ok(())
}

fn database_digest(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_staged_database(staged: &Path) -> anyhow::Result<()> {
    checkpoint_database(staged, "staged")?;
    let conn = db::open_scratch(staged)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("staged migration DB failed integrity_check: {integrity}");
    }
    if pending_embeddings(&conn)? != 0 {
        bail!("staged migration DB still contains unmarked embeddings");
    }
    drop(conn);
    checkpoint_database(staged, "staged")?;
    Ok(())
}

fn read_ready_state(path: &Path) -> anyhow::Result<ReadyState> {
    let state = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (digest, migrated) = state
        .trim_end()
        .split_once('\t')
        .context("invalid migration recovery state")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid migration recovery source digest");
    }
    Ok(ReadyState {
        source_digest: digest.to_owned(),
        migrated: migrated
            .parse()
            .context("invalid migration recovery migrated count")?,
    })
}

fn write_ready_state(path: &Path, state: &ReadyState) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("state.tmp.{}", std::process::id()));
    {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        writeln!(file, "{}\t{}", state.source_digest, state.migrated)?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
    }
    fs::rename(&temporary, path)
        .with_context(|| format!("publish migration recovery state {}", path.display()))?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path.parent().context("database path has no parent")?;
    fs::File::open(parent)
        .with_context(|| format!("open {} for sync", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync {}", parent.display()))
}

fn recover_ready_stage(
    paths: &config::Paths,
    lock: &crate::commands::add::Lock,
) -> anyhow::Result<Option<usize>> {
    let state = state_path(paths);
    if !state.exists() {
        return Ok(None);
    }
    let staged = staging_path(paths);
    let ready = match read_ready_state(&state) {
        Ok(ready) => ready,
        Err(_) => {
            remove_staging_artifacts(paths)?;
            return Ok(None);
        }
    };

    if !staged.exists() {
        let live = db::open_rw(paths, lock)?;
        let pending = pending_embeddings(&live)?;
        drop(live);
        if pending == 0 {
            fs::remove_file(&state)?;
            return Ok(Some(0));
        }
        fs::remove_file(&state)?;
        return Ok(None);
    }

    if verify_staged_database(&staged).is_err() {
        remove_staging_artifacts(paths)?;
        return Ok(None);
    }
    let live_digest = checkpoint_database(&paths.db, "live")?;
    if live_digest != ready.source_digest {
        // A write arrived after the interrupted staging step. Publishing that
        // snapshot could lose it, so retry from the current live database.
        remove_staging_artifacts(paths)?;
        return Ok(None);
    }
    Ok(Some(publish_ready_stage(paths, &ready)?))
}

fn publish_ready_stage(paths: &config::Paths, ready: &ReadyState) -> anyhow::Result<usize> {
    let live_digest = checkpoint_database(&paths.db, "live")?;
    if live_digest != ready.source_digest {
        bail!("live database changed after migration staging; recovery state retained for retry");
    }
    let staged = staging_path(paths);
    fs::rename(&staged, &paths.db)
        .with_context(|| format!("publish migrated DB {}", paths.db.display()))?;
    sync_parent(&paths.db)?;
    fs::remove_file(state_path(paths))?;
    sync_parent(&paths.db)?;
    println!(
        "migrate-embeddings: migrated {} blob(s); backup retained at {}",
        ready.migrated,
        backup_path(paths).display()
    );
    Ok(ready.migrated)
}

fn remove_staging_artifacts(paths: &config::Paths) -> anyhow::Result<()> {
    let staged = staging_path(paths);
    remove_database_sidecars(&staged)?;
    for path in [staged, state_path(paths)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }
    Ok(())
}
