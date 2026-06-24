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
    },
}
