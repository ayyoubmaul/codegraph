//! codegraph — structural + semantic codebase memory for AI coding agents.
//!
//! v1 slice: walk a repo, parse supported languages with tree-sitter, and
//! extract definitions. Graph store (Kùzu), semantic embeddings (fastembed),
//! incremental watch (notify), and the MCP server (rmcp) land in later slices.

mod cli;
mod graph;
mod lang;
mod parse;
mod store;
mod symbol;
mod walk;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use clap::Parser;
use rayon::prelude::*;

use cli::{Cli, Command};
use graph::Store;
use symbol::Symbol;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Index { path, json, db } => index(&path, json, db.as_deref()),
        Command::WhoCalls { name, db } => who_calls(&name, &db),
        Command::CallChain { name, db, depth } => call_chain(&name, &db, depth),
    }
}

fn index(root: &Path, json: bool, db: Option<&Path>) -> anyhow::Result<()> {
    let start = Instant::now();

    let files = walk::collect_files(root);
    let file_count = files.len();
    let rel_paths: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();

    // Parse files in parallel; failures on a single file are skipped, not fatal.
    let parsed: Vec<parse::ParseResult> = files
        .par_iter()
        .map(|f| match std::fs::read(&f.path) {
            Ok(src) => parse::parse_file(&f.rel, &src, f.lang).unwrap_or_default(),
            Err(_) => parse::ParseResult::default(),
        })
        .collect();

    let mut symbols: Vec<Symbol> = Vec::new();
    let mut calls: Vec<graph::CallRef> = Vec::new();
    let mut imports: Vec<graph::ImportRef> = Vec::new();
    for p in parsed {
        symbols.extend(p.symbols);
        calls.extend(p.calls);
        imports.extend(p.imports);
    }

    // Assemble the graph batch and (optionally) persist it to LadybugDB.
    let batch = graph::GraphBatch::build(&rel_paths, &symbols, &calls, &imports);
    let elapsed = start.elapsed();

    if let Some(db_path) = db {
        let persist_start = Instant::now();
        let mut store = store::LadybugStore::open(db_path)?;
        store.write_batch(&batch)?;
        let (files, defs, defines, calls, imports) = store.summary()?;
        println!(
            "persisted to {} → {files} files, {defs} defs, {defines} defines, {calls} calls, {imports} imports in {:.2?}",
            db_path.display(),
            persist_start.elapsed()
        );
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&batch)?);
        return Ok(());
    }

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for s in &symbols {
        *by_kind.entry(format!("{:?}", s.kind)).or_default() += 1;
    }

    println!(
        "indexed {file_count} files → {} nodes, {} edges ({} symbols) in {elapsed:.2?}",
        batch.nodes.len(),
        batch.edges.len(),
        symbols.len()
    );
    for (kind, count) in by_kind {
        println!("  {kind:<10} {count}");
    }

    Ok(())
}

fn who_calls(name: &str, db: &Path) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let hits = store.who_calls(name)?;
    if hits.is_empty() {
        println!("no callers of `{name}` found");
    } else {
        println!("{} caller(s) of `{name}`:", hits.len());
        for h in hits {
            println!("  {:<28} {}:{}", h.name, h.file, h.start_line);
        }
    }
    Ok(())
}

fn call_chain(name: &str, db: &Path, depth: u8) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let hits = store.call_chain(name, depth)?;
    if hits.is_empty() {
        println!("`{name}` reaches nothing within depth {depth}");
    } else {
        println!(
            "`{name}` reaches {} definition(s) within depth {depth}:",
            hits.len()
        );
        for h in hits {
            println!("  {:<28} {}:{}", h.name, h.file, h.start_line);
        }
    }
    Ok(())
}
