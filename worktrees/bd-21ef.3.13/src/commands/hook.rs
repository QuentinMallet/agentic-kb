//! `hook` subcommand — lifecycle hooks for Claude Code integration.

use crate::commands::digest;
use crate::config;
use abscissa_core::{Command, Runnable};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Run a lifecycle hook
#[derive(Command, Debug, Parser)]
pub struct Hook {
    #[command(subcommand)]
    pub cmd: HookCmd,
}

#[derive(Subcommand, Debug)]
pub enum HookCmd {
    /// Run the SessionEnd digest hook
    SessionEnd {
        /// Path to the Claude Code transcript file (falls back to KB_TRANSCRIPT_PATH env var)
        #[arg(long)]
        transcript: Option<PathBuf>,
        /// Session ID (falls back to KB_SESSION_ID env var)
        #[arg(long)]
        session_id: Option<String>,
    },
}

impl Runnable for Hook {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Hook {
    pub fn execute(&self) -> anyhow::Result<()> {
        let paths = config::Paths::discover()?;
        match &self.cmd {
            HookCmd::SessionEnd {
                transcript,
                session_id,
            } => {
                let transcript = transcript
                    .clone()
                    .or_else(|| std::env::var("KB_TRANSCRIPT_PATH").ok().map(PathBuf::from))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "transcript path required: pass --transcript or set KB_TRANSCRIPT_PATH"
                        )
                    })?;
                let session_id = session_id
                    .clone()
                    .or_else(|| {
                        std::env::var("KB_SESSION_ID")
                            .ok()
                            .filter(|v| !v.is_empty())
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "session ID required: pass --session-id or set KB_SESSION_ID"
                        )
                    })?;

                let outcome = digest::digest_session(&session_id, &transcript, &paths)?;
                if outcome.skipped_no_change {
                    println!("digest: no change (session={session_id})");
                } else {
                    println!(
                        "digest: wrote {} turns for session={}",
                        outcome.turns_processed, session_id
                    );
                }
                Ok(())
            }
        }
    }
}
