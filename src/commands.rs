//! kb Subcommands

pub mod add;
pub mod add_validation;
pub mod cite;
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
pub mod migrate_citations;
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
    /// Heal legacy whole-file workaround citations (`path:0-N`) to bare paths
    MigrateCitations(migrate_citations::MigrateCitations),
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
    /// Emit citation fields for a file or byte range
    Cite(cite::Cite),
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

impl KbCmd {
    /// Whether this subcommand may mutate the knowledge base.
    ///
    /// Drives the C1/D3 recovery point at CLI dispatch. `rebuild` is excluded
    /// because it *is* the repair, and `mcp` because it derives its paths from
    /// `--db` and initializes itself at startup.
    fn mutates(&self) -> bool {
        !matches!(
            self,
            KbCmd::Search(_)
                | KbCmd::CitedBy(_)
                | KbCmd::Cite(_)
                | KbCmd::Context(_)
                | KbCmd::Eval(_)
                | KbCmd::Tests(_)
                | KbCmd::OlderThan(_)
                | KbCmd::Rebuild(_)
                | KbCmd::Mcp(_)
        )
    }
}

impl Runnable for EntryPoint {
    fn run(&self) {
        // C1/D3 + C2/ADR-7: recovery fires at CLI dispatch for mutating
        // subcommands only. `open_or_init` takes the write lock, which a read
        // must never do; reads detect the same condition on their own
        // read-only connection and warn. Best-effort — outside a repository
        // `discover` fails and the subcommand reports that itself.
        if self.cmd.mutates() {
            if let Ok(paths) = crate::config::Paths::discover() {
                if let Err(e) = crate::components::db::open_or_init(&paths) {
                    eprintln!("kb: WARNING event-log recovery failed at startup: {e}");
                }
            }
        }
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
