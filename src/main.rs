//! codegraph — structural + semantic codebase memory for AI coding agents.
//!
//! v1 slice: walk a repo, parse supported languages with tree-sitter, and
//! extract definitions. Graph store (Kùzu), semantic embeddings (fastembed),
//! incremental watch (notify), and the MCP server (rmcp) land in later slices.

mod analyze;
mod cli;
mod embed;
mod graph;
mod lang;
mod mcp;
mod parse;
mod store;
mod symbol;
mod ui;
mod vector;
mod walk;
mod watch;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Index {
            paths,
            json,
            db,
            embed,
        } => index(&paths, json, db.as_deref(), embed),
        Command::Search { query, db, k, repo } => search(&query, &db, k, repo.as_deref()),
        Command::WhoCalls { name, db, repo } => who_calls(&name, &db, repo.as_deref()),
        Command::CallChain {
            name,
            db,
            depth,
            repo,
        } => call_chain(&name, &db, depth, repo.as_deref()),
        Command::Analyze { db, iters } => analyze_cmd(&db, iters),
        Command::Important { db, k, repo } => important(&db, k, repo.as_deref()),
        Command::Outline { db, repo, limit } => outline(&db, repo.as_deref(), limit),
        Command::Communities { db, k } => communities(&db, k),
        Command::Watch { paths, db, embed } => watch::run(&paths, &db, embed),
        Command::Serve {
            db,
            watch,
            embed,
            reanalyze,
        } => serve_cmd(&db, &watch, embed, reanalyze),
        Command::Ui {
            db,
            port,
            watch,
            embed,
            reanalyze,
        } => ui_cmd(&db, port, &watch, embed, reanalyze),
    }
}

fn serve_cmd(db: &Path, watch: &[PathBuf], embed: bool, reanalyze: Option<u64>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    if watch.is_empty() {
        rt.block_on(mcp::serve(db))
    } else {
        rt.block_on(mcp::serve_watch(db, watch, embed, reanalyze))
    }
}

fn ui_cmd(
    db: &Path,
    port: u16,
    watch: &[PathBuf],
    embed: bool,
    reanalyze: Option<u64>,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(ui::serve(db, port, watch, embed, reanalyze))
}

fn index(paths: &[PathBuf], json: bool, db: Option<&Path>, embed: bool) -> anyhow::Result<()> {
    let start = Instant::now();

    let mut files = Vec::new();
    for path in paths {
        let repo = walk::repo_name(path);
        files.extend(walk::collect_files(path, &repo));
    }
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
        let store = store::LadybugStore::open(db_path)?;
        // Fresh DB → fast bulk CSV load; existing DB → incremental MERGE.
        if store.def_count()? == 0 {
            let tmp = std::env::temp_dir().join(format!("codegraph-bulk-{}", std::process::id()));
            store.bulk_load(&batch, &tmp)?;
            let _ = std::fs::remove_dir_all(&tmp);
        } else {
            store.write_batch(&batch)?;
        }

        if embed {
            let embed_start = Instant::now();
            let already = store.embedded_ids()?;
            let pending = batch
                .nodes
                .iter()
                .filter(|n| n.kind == graph::NodeKind::Definition && !already.contains(&n.id))
                .count();
            if pending == 0 {
                println!("embeddings up to date ({} cached)", already.len());
            } else {
                let mut embedder = embed::Embedder::new()?;
                let items = embed::embed_defs(&mut embedder, &batch, &already)?;
                let n = items.len();
                store.set_embeddings(&items)?;
                println!(
                    "embedded {n} new definitions ({} cached) in {:.2?}",
                    already.len(),
                    embed_start.elapsed()
                );
            }
        }

        let (files, defs_c, defines, calls, imports) = store.summary()?;
        println!(
            "persisted to {} → {files} files, {defs_c} defs, {defines} defines, {calls} calls, {imports} imports in {:.2?}",
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

fn who_calls(name: &str, db: &Path, repo: Option<&str>) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let hits = store.who_calls(name, repo)?;
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

fn call_chain(name: &str, db: &Path, depth: u8, repo: Option<&str>) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let hits = store.call_chain(name, depth, repo)?;
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

fn search(query: &str, db: &Path, k: usize, repo: Option<&str>) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let mut embedder = embed::Embedder::new()?;
    let q = embedder.embed_one(query)?;
    let hits = store.semantic_search(&q, k, repo)?;
    if hits.is_empty() {
        println!("no results for `{query}` (did you index with --embed?)");
    } else {
        println!("top {} for `{query}`:", hits.len());
        for (h, sim) in hits {
            println!("  {sim:.3}  {:<28} {}:{}", h.name, h.file, h.start_line);
        }
    }
    Ok(())
}

fn analyze_cmd(db: &Path, iters: usize) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let start = Instant::now();
    let (defs, communities) = analyze::run(&store, iters)?;
    println!(
        "analyzed {defs} defs → {communities} communities + PageRank in {:.2?}",
        start.elapsed()
    );
    Ok(())
}

fn important(db: &Path, k: usize, repo: Option<&str>) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let hits = store.top_important(k, repo)?;
    if hits.is_empty() {
        println!("no analysis yet — run `analyze` first");
    } else {
        println!("top {} most-depended-on definitions:", hits.len());
        for (h, pr) in hits {
            println!("  {pr:.4}  {:<28} {}:{}", h.name, h.file, h.start_line);
        }
    }
    Ok(())
}

fn outline(db: &Path, repo: Option<&str>, limit: usize) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let rows = store.outline(repo, limit)?;
    if rows.is_empty() {
        println!("no definitions in scope (did you `index` first?)");
        return Ok(());
    }
    // Rows arrive ordered by (file, start_line); group consecutively by file.
    let mut i = 0;
    let mut files = 0;
    while i < rows.len() {
        let file = &rows[i].file;
        let start = i;
        while i < rows.len() && &rows[i].file == file {
            i += 1;
        }
        let group = &rows[start..i];
        println!("\n{file}  ({} defs)", group.len());
        for r in group {
            println!("  {:<9} {:<28} :{}", r.kind.to_lowercase(), r.name, r.start_line);
        }
        files += 1;
    }
    println!("\n{} definitions across {files} files", rows.len());
    if rows.len() >= limit {
        println!("(truncated at {limit} — narrow with --repo or raise --limit)");
    }
    Ok(())
}

fn communities(db: &Path, k: usize) -> anyhow::Result<()> {
    let store = store::LadybugStore::open(db)?;
    let members = store.community_members()?;
    if members.is_empty() {
        println!("no analysis yet — run `analyze` first");
        return Ok(());
    }

    // Members arrive ordered by (community, pagerank desc); group consecutively.
    let mut groups: Vec<(i64, Vec<String>)> = Vec::new();
    for (c, hit, _pr) in members {
        match groups.last_mut() {
            Some(last) if last.0 == c => last.1.push(hit.name),
            _ => groups.push((c, vec![hit.name])),
        }
    }
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let show = k.min(groups.len());
    println!("{} communities (top {show} by size):", groups.len());
    for (c, names) in groups.into_iter().take(show) {
        let sample: Vec<&str> = names.iter().take(6).map(String::as_str).collect();
        println!("  community {c}: {} defs — {}", names.len(), sample.join(", "));
    }
    Ok(())
}
