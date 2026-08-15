//! kb Subcommands

pub mod add;
pub mod add_validation;
pub mod cited_by;
pub mod compact;
pub mod compress;
pub mod context;
pub mod digest;
pub mod eval;
pub mod expire;
pub mod hook;
pub mod import_cmd;
pub mod ingest;
pub mod mcp;
pub mod older_than;
pub mod peers;
pub mod rebuild;
pub mod reembed;
pub mod run;
pub mod search;
pub mod stale_check;
pub mod test_add;
pub mod tests;

use crate::config::KbConfig;
use abscissa_core::{Command, Configurable, FrameworkError, Runnable};
use std::path::PathBuf;

/// kb Subcommands
#[derive(clap::Parser, Command, Debug, Runnable)]
pub enum KbCmd {
    /// Add or update a knowledge entry
    Add(add::Add),
    /// Search knowledge entries
    Search(search::Search),
    /// Run MCP port protocol server (line-delimited JSON over stdio)
    Mcp(mcp::Mcp),
    /// Check if kb entries for given files are stale
    StaleCheck(stale_check::StaleCheck),
    /// Replay all events and rebuild agent-kb.db from scratch
    Rebuild(rebuild::Rebuild),
    /// Compact the event log (squash superseded events)
    Compact(compact::Compact),
    /// Compress a KB entry via semantic paragraph deduplication
    Compress(compress::Compress),
    /// List live KB entries that cite a file
    CitedBy(cited_by::CitedBy),
    /// Select relevant KB context within an approximate token budget
    Context(context::Context),
    /// Evaluate retrieval quality against a golden set (recall@k, MRR)
    Eval(eval::Eval),
    /// Mark an entry stale
    Expire(expire::Expire),
    /// Re-embed entries that are missing embeddings
    Reembed(reembed::Reembed),
    /// List test cases
    Tests(tests::Tests),
    /// Add or update a test case
    TestAdd(test_add::TestAdd),
    /// Record a test run result
    Run(run::Run),
    /// Chunk a long document into KB entries
    Ingest(ingest::Ingest),
    /// Bulk-import KB entries from a JSON seed file (with stamp-gating)
    Import(import_cmd::Import),
    /// List KB paths whose latest upsert is older than N days (TSV output)
    OlderThan(older_than::OlderThan),
    /// Manage peer repo graph edges
    #[command(subcommand)]
    Peers(peers::Peers),
    /// Run a lifecycle hook (e.g. SessionEnd digest)
    Hook(hook::Hook),
}

/// Entry point for the application.
#[derive(clap::Parser, Command, Debug)]
#[command(name = "kb", about = "Agent knowledge base CLI", version)]
pub struct EntryPoint {
    #[command(subcommand)]
    cmd: KbCmd,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Use the specified config file
    #[arg(short, long)]
    pub config: Option<String>,
}

impl Runnable for EntryPoint {
    fn run(&self) {
        self.cmd.run()
    }
}

impl Configurable<KbConfig> for EntryPoint {
    fn config_path(&self) -> Option<PathBuf> {
        let filename = self
            .config
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| "kb.toml".into());

        if filename.exists() {
            Some(filename)
        } else {
            None
        }
    }

    fn process_config(&self, config: KbConfig) -> Result<KbConfig, FrameworkError> {
        Ok(config)
    }
}
