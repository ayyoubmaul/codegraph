//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "codegraph",
    version,
    about = "Structural + semantic codebase memory for AI agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Index a codebase: walk, parse, and extract symbols.
    Index {
        /// Path to the repository root.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Emit the graph batch as JSON instead of a summary.
        #[arg(long)]
        json: bool,

        /// Persist the graph to a LadybugDB database at this path.
        #[arg(long)]
        db: Option<PathBuf>,

        /// Also compute + store local embeddings (requires `--db`; downloads the
        /// model once, then runs offline).
        #[arg(long)]
        embed: bool,
    },

    /// Semantic search: find definitions by meaning (needs a `--db` indexed
    /// with `--embed`).
    Search {
        /// Natural-language query.
        query: String,
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
        /// Number of results.
        #[arg(long, default_value_t = 8)]
        k: usize,
    },

    /// Show the direct callers of a symbol (uses a `--db` built by `index`).
    WhoCalls {
        /// Symbol name to look up.
        name: String,
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
    },

    /// Show the definitions transitively reachable from a symbol via calls.
    CallChain {
        /// Symbol name to start from.
        name: String,
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
        /// Max hops to traverse (1..=10).
        #[arg(long, default_value_t = 3)]
        depth: u8,
    },

    /// Compute graph intelligence (PageRank importance + Louvain communities)
    /// and store it on the graph.
    Analyze {
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
        /// PageRank iterations.
        #[arg(long, default_value_t = 30)]
        iters: usize,
    },

    /// Show the most important (most-depended-on) definitions, by PageRank.
    Important {
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
        /// Number of results.
        #[arg(long, default_value_t = 10)]
        k: usize,
    },

    /// Show the largest code communities (modules) found by Louvain.
    Communities {
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
        /// How many communities to show.
        #[arg(long, default_value_t = 6)]
        k: usize,
    },

    /// Watch a repository and incrementally patch the graph as files change.
    Watch {
        /// Path to the repository root.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
        /// Also keep embeddings updated (loads the model).
        #[arg(long)]
        embed: bool,
    },

    /// Run the MCP server over stdio so AI agents can query the graph.
    Serve {
        /// LadybugDB database path.
        #[arg(long)]
        db: PathBuf,
    },
}
