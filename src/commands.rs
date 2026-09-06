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
    /// `--db` and initializes itself at startup. Five subcommands classify
    /// per invocation rather than per subcommand -- this is the complete set;
    /// every other variant classifies statically regardless of its flags:
    /// `stale-check` only escalates when `--relocate` requests it (erring
    /// toward `true` when the flag is set, even though `heal_relocations`
    /// itself may still find nothing to heal); `compress`, `reembed`,
    /// `ingest`, and `import` all invert on their own `--dry-run` flag, since
    /// each of those bodies returns before any write once `dry_run` is set.
    fn mutates(&self) -> bool {
        match self {
            KbCmd::Add(_) => true,
            KbCmd::Search(_) => false,
            KbCmd::Mcp(_) => false,
            KbCmd::MigrateCitations(_) => true,
            KbCmd::StaleCheck(c) => c.relocate != stale_check::RelocateArg::Never,
            KbCmd::Rebuild(_) => false,
            KbCmd::Compact(_) => true,
            KbCmd::Compress(c) => !c.dry_run,
            KbCmd::CitedBy(_) => false,
            KbCmd::Cite(_) => false,
            KbCmd::Context(_) => false,
            KbCmd::Eval(_) => false,
            KbCmd::Expire(_) => true,
            KbCmd::Reembed(c) => !c.dry_run,
            KbCmd::Tests(_) => false,
            KbCmd::TestAdd(_) => true,
            KbCmd::Run(_) => true,
            KbCmd::Ingest(c) => !c.dry_run,
            KbCmd::Import(c) => !c.dry_run,
            KbCmd::OlderThan(_) => false,
            KbCmd::Peers(cmd) => cmd.mutates(),
            KbCmd::Hook(_) => true,
        }
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

#[cfg(test)]
mod mutation_classification_tests {
    use super::*;
    use clap::Parser;

    /// Independent, wildcard-free duplicate of `KbCmd::mutates()`'s
    /// classification, including the nested `Peers`/`PeersEdge` leaves.
    /// Adding a `KbCmd` variant -- or a `Peers`/`PeersEdge` leaf -- breaks
    /// this match's exhaustiveness -- a compile error in this test file, not
    /// only in production code -- so a new variant cannot silently stay
    /// untested. The compiler owns exhaustiveness; this duplicate plus the
    /// argv table below together own agreement on each variant's specific
    /// verdict.
    fn expected_mutates(cmd: &KbCmd) -> bool {
        match cmd {
            KbCmd::Add(_) => true,
            KbCmd::Search(_) => false,
            KbCmd::Mcp(_) => false,
            KbCmd::MigrateCitations(_) => true,
            KbCmd::StaleCheck(c) => c.relocate != stale_check::RelocateArg::Never,
            KbCmd::Rebuild(_) => false,
            KbCmd::Compact(_) => true,
            KbCmd::Compress(c) => !c.dry_run,
            KbCmd::CitedBy(_) => false,
            KbCmd::Cite(_) => false,
            KbCmd::Context(_) => false,
            KbCmd::Eval(_) => false,
            KbCmd::Expire(_) => true,
            KbCmd::Reembed(c) => !c.dry_run,
            KbCmd::Tests(_) => false,
            KbCmd::TestAdd(_) => true,
            KbCmd::Run(_) => true,
            KbCmd::Ingest(c) => !c.dry_run,
            KbCmd::Import(c) => !c.dry_run,
            KbCmd::OlderThan(_) => false,
            KbCmd::Peers(p) => match p {
                peers::Peers::Add(_) => true,
                peers::Peers::List(_) => false,
                peers::Peers::Remove(_) => true,
                peers::Peers::Show(_) => false,
                peers::Peers::Import(_) => true,
                peers::Peers::Edge(e) => match e {
                    peers::PeersEdge::Add(_) => true,
                    peers::PeersEdge::List(_) => false,
                    peers::PeersEdge::Remove(_) => true,
                    peers::PeersEdge::CleanupEpic(_) => true,
                },
            },
            KbCmd::Hook(_) => true,
        }
    }

    fn assert_classification(args: &[&str], expected: bool) {
        let entry = EntryPoint::try_parse_from(args)
            .unwrap_or_else(|e| panic!("invalid classification-test argv {args:?}: {e}"));
        assert_eq!(entry.cmd.mutates(), expected, "argv: {args:?}");
        assert_eq!(
            expected_mutates(&entry.cmd),
            expected,
            "argv: {args:?} -- table's expected value disagrees with the \
             independent classification match; a new or changed KbCmd \
             variant needs a case here too"
        );
    }

    #[test]
    fn every_cli_and_peers_variant_has_an_explicit_mutation_classification() {
        let cases: &[(&[&str], bool)] = &[
            (
                &[
                    "kb",
                    "add",
                    "--path",
                    "p",
                    "--summary",
                    "s",
                    "--content",
                    "c",
                    "--tags",
                    "t",
                ],
                true,
            ),
            (&["kb", "search", "q"], false),
            (&["kb", "mcp", "--db", "agent-kb.db"], false),
            (&["kb", "migrate-citations"], true),
            (&["kb", "stale-check", "p"], false),
            (&["kb", "stale-check", "p", "--relocate", "file"], true),
            (&["kb", "rebuild"], false),
            (&["kb", "compact"], true),
            (&["kb", "compress", "p"], true),
            (&["kb", "compress", "p", "--dry-run"], false),
            (&["kb", "cited-by", "p"], false),
            (&["kb", "cite", "p"], false),
            (&["kb", "context", "--budget", "100"], false),
            (&["kb", "eval", "golden.jsonl"], false),
            (&["kb", "expire", "id"], true),
            (&["kb", "reembed"], true),
            (&["kb", "reembed", "--dry-run"], false),
            (&["kb", "tests"], false),
            (
                &[
                    "kb",
                    "test-add",
                    "--app",
                    "a",
                    "--name",
                    "n",
                    "--protocol",
                    "rust_tool",
                    "--config",
                    "{}",
                ],
                true,
            ),
            (&["kb", "run", "id", "--result", "pass"], true),
            (
                &[
                    "kb",
                    "ingest",
                    "--path",
                    "p",
                    "--summary",
                    "s",
                    "--tags",
                    "t",
                ],
                true,
            ),
            (
                &[
                    "kb",
                    "ingest",
                    "--path",
                    "p",
                    "--summary",
                    "s",
                    "--tags",
                    "t",
                    "--dry-run",
                ],
                false,
            ),
            (&["kb", "import", "p"], true),
            (&["kb", "import", "p", "--dry-run"], false),
            (&["kb", "older-than", "1"], false),
            (
                &[
                    "kb",
                    "hook",
                    "session-end",
                    "--transcript",
                    "p",
                    "--session-id",
                    "s",
                ],
                true,
            ),
        ];
        for (args, expected) in cases {
            assert_classification(args, *expected);
        }

        let peer_cases: &[(&[&str], bool)] = &[
            (&["kb", "peers", "add", "target", "--type", "dep"], true),
            (&["kb", "peers", "list"], false),
            (&["kb", "peers", "remove", "id"], true),
            (&["kb", "peers", "show", "repo"], false),
            (&["kb", "peers", "import", "seeds.json"], true),
            (
                &[
                    "kb", "peers", "edge", "add", "source", "target", "--type", "dep",
                ],
                true,
            ),
            (&["kb", "peers", "edge", "list"], false),
            (&["kb", "peers", "edge", "remove", "id"], true),
            (&["kb", "peers", "edge", "cleanup-epic", "slug"], true),
        ];
        for (args, expected) in peer_cases {
            assert_classification(args, *expected);
        }
    }
}
