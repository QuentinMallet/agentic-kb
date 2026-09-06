//! Transactional persisted-embedding format migration.

use crate::commands::add::acquire_lock;
use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
use anyhow::{bail, Context};
use clap::Parser;
use std::fs;

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

    /// Stage and validate a complete database copy before one atomic publish.
    /// The retained backup is intentionally never overwritten: an operator can
    /// restore it if migration is aborted after publication for any reason.
    pub fn execute_with(&self, paths: &config::Paths) -> anyhow::Result<usize> {
        let _lock = acquire_lock(&paths.lock)?;
        let backup = paths.db.with_extension("db.pre-normalized-embeddings.bak");
        if backup.exists() {
            bail!(
                "refusing to overwrite retained pre-migration backup {}",
                backup.display()
            );
        }
        let staged = paths
            .db
            .with_extension(format!("db.normalized.tmp.{}", std::process::id()));
        if staged.exists() {
            bail!("staging database already exists at {}", staged.display());
        }

        let backup_sql = backup.to_string_lossy().replace('\'', "''");
        let staged_sql = staged.to_string_lossy().replace('\'', "''");
        {
            let conn = db::open_db(&paths.db)?;
            conn.execute_batch(&format!("VACUUM INTO '{backup_sql}'"))
                .with_context(|| format!("create pre-migration backup {}", backup.display()))?;
            conn.execute_batch(&format!("VACUUM INTO '{staged_sql}'"))
                .with_context(|| format!("create migration staging DB {}", staged.display()))?;
        }

        let migration = (|| -> anyhow::Result<usize> {
            let staged_conn = db::open_db(&staged)?;
            let migrated = db::migrate_embeddings(&staged_conn)?;
            staged_conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(migrated)
        })();
        let migrated = match migration {
            Ok(migrated) => migrated,
            Err(error) => {
                let staged_raw = staged.to_string_lossy();
                let _ = fs::remove_file(&staged);
                let _ = fs::remove_file(format!("{staged_raw}-wal"));
                let _ = fs::remove_file(format!("{staged_raw}-shm"));
                return Err(error);
            }
        };

        let db_raw = paths.db.to_string_lossy();
        let _ = fs::remove_file(format!("{db_raw}-wal"));
        let _ = fs::remove_file(format!("{db_raw}-shm"));
        fs::rename(&staged, &paths.db)
            .with_context(|| format!("publish migrated DB {}", paths.db.display()))?;
        println!(
            "migrate-embeddings: migrated {migrated} blob(s); backup retained at {}",
            backup.display()
        );
        Ok(migrated)
    }
}
