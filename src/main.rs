//! codegraph — structural + semantic codebase memory for AI coding agents.
//!
//! v1 slice: walk a repo, parse supported languages with tree-sitter, and
//! extract definitions. Graph store (Kùzu), semantic embeddings (fastembed),
//! incremental watch (notify), and the MCP server (rmcp) land in later slices.

mod cli;
mod lang;
mod parse;
mod symbol;
mod walk;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use clap::Parser;
use rayon::prelude::*;

use cli::{Cli, Command};
use symbol::Symbol;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Index { path, json } => index(&path, json),
    }
}

fn index(root: &Path, json: bool) -> anyhow::Result<()> {
    let start = Instant::now();

    let files = walk::collect_files(root);
    let file_count = files.len();

    // Parse files in parallel; failures on a single file are skipped, not fatal.
    let symbols: Vec<Symbol> = files
        .par_iter()
        .flat_map_iter(|f| match std::fs::read(&f.path) {
            Ok(src) => parse::parse_file(&f.rel, &src, f.lang).unwrap_or_default(),
            Err(_) => Vec::new(),
        })
        .collect();

    let elapsed = start.elapsed();

    if json {
        println!("{}", serde_json::to_string_pretty(&symbols)?);
        return Ok(());
    }

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for s in &symbols {
        *by_kind.entry(format!("{:?}", s.kind)).or_default() += 1;
    }

    println!(
        "indexed {file_count} files, {} symbols in {elapsed:.2?}",
        symbols.len()
    );
    for (kind, count) in by_kind {
        println!("  {kind:<10} {count}");
    }

    Ok(())
}
