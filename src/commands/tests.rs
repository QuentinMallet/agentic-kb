//! `tests` subcommand

#![allow(deprecated)] // db::open_db (ADR-1) — remaining call sites migrate in C2/L1b, L2, L3, L1c
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
        let conn = db::open_db(&paths.db)?;
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
