//! `tests` subcommand

use crate::components::db;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::Parser;
use rusqlite::params;

/// List test cases
#[derive(Command, Debug, Parser)]
pub struct Tests {
    /// Filter by application name
    #[arg(long)]
    pub app: Option<String>,
}

impl Runnable for Tests {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Tests {
    /// Execute the tests command.
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        self.execute_with(&paths)
    }

    /// Execute with explicit paths (exposed for testing).
    pub fn execute_with(&self, paths: &config::Paths) -> anyhow::Result<()> {
        let conn = match db::open_ro(&paths.db) {
            Ok(conn) => conn,
            Err(e) if db::is_db_uninitialized(&e) => {
                db::note_uninitialized(&paths.db);
                println!("(no test cases)");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let mut count = 0;

        if let Some(ref a) = self.app {
            let mut stmt = conn.prepare(
                "SELECT id, app, name, protocol FROM test_cases
                 WHERE app=?1 AND is_stale=0 ORDER BY name",
            )?;
            let rows = stmt.query_map(params![a], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (id, app, name, proto) = row?;
                println!("{app}/{name}  [{proto}]  id={id}");
                count += 1;
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, app, name, protocol FROM test_cases
                 WHERE is_stale=0 ORDER BY app, name",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (id, app, name, proto) = row?;
                println!("{app}/{name}  [{proto}]  id={id}");
                count += 1;
            }
        }

        if count == 0 {
            println!("(no test cases)");
        }
        Ok(())
    }
}
