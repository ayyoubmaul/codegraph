//! Incremental indexing: watch the repo and patch the graph per changed file.
//!
//! Keeps an in-memory per-file parse cache. On a change it re-parses only that
//! file, rebuilds the (cheap) in-memory resolution over the full symbol set,
//! then writes just the sub-graph *incident to that file* — its nodes plus every
//! edge that touches them (incoming and outgoing) — after a `remove_file`. So
//! only the changed file is re-parsed and re-written, but cross-file call edges
//! stay correct (re-resolved against the whole repo).

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::channel;

use notify::{EventKind, RecursiveMode, Watcher};

use crate::embed::{self, Embedder};
use crate::graph::{Edge, GraphBatch, Node, NodeKind, Store};
use crate::lang::Lang;
use crate::parse::{self, ParseResult};
use crate::store::LadybugStore;
use crate::walk;

pub fn run(root: &Path, db: &Path, embed_on: bool) -> anyhow::Result<()> {
    let root = root.canonicalize()?;
    let mut store = LadybugStore::open(db)?;
    let mut embedder = if embed_on { Some(Embedder::new()?) } else { None };

    // Initial full index into the cache and the database.
    let mut cache: HashMap<String, ParseResult> = HashMap::new();
    for sf in walk::collect_files(&root) {
        if let Ok(src) = std::fs::read(&sf.path) {
            if let Ok(pr) = parse::parse_file(&sf.rel, &src, sf.lang) {
                cache.insert(sf.rel.clone(), pr);
            }
        }
    }
    let batch = build_full(&cache);
    store.write_batch(&batch)?;
    if let Some(emb) = embedder.as_mut() {
        let items = embed::embed_defs(emb, &batch)?;
        store.set_embeddings(&items)?;
    }
    println!(
        "indexed {} files → watching {} (Ctrl-C to stop)",
        cache.len(),
        root.display()
    );

    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    for res in rx {
        let event = match res {
            Ok(e) => e,
            Err(_) => continue,
        };
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                for path in &event.paths {
                    let Some(lang) = Lang::from_path(path) else {
                        continue;
                    };
                    let Ok(stripped) = path.strip_prefix(&root) else {
                        continue;
                    };
                    let rel = stripped.to_string_lossy().replace('\\', "/");
                    if !path.exists() {
                        remove(&mut store, &mut cache, &rel)?;
                        continue;
                    }
                    update_file(&mut store, &mut cache, &rel, lang, path, embedder.as_mut())?;
                }
            }
            EventKind::Remove(_) => {
                for path in &event.paths {
                    if Lang::from_path(path).is_none() {
                        continue;
                    }
                    let Ok(stripped) = path.strip_prefix(&root) else {
                        continue;
                    };
                    let rel = stripped.to_string_lossy().replace('\\', "/");
                    remove(&mut store, &mut cache, &rel)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn remove(
    store: &mut LadybugStore,
    cache: &mut HashMap<String, ParseResult>,
    rel: &str,
) -> anyhow::Result<()> {
    if cache.remove(rel).is_some() {
        store.remove_file(rel)?;
        println!("- {rel}");
    }
    Ok(())
}

fn update_file(
    store: &mut LadybugStore,
    cache: &mut HashMap<String, ParseResult>,
    rel: &str,
    lang: Lang,
    path: &Path,
    embedder: Option<&mut Embedder>,
) -> anyhow::Result<()> {
    let Ok(src) = std::fs::read(path) else {
        return Ok(());
    };
    let Ok(pr) = parse::parse_file(rel, &src, lang) else {
        return Ok(());
    };
    cache.insert(rel.to_string(), pr);

    let batch = build_full(cache);
    let sub = sub_batch_for_file(&batch, rel);
    let n_defs = sub
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Definition)
        .count();

    store.remove_file(rel)?;
    store.write_batch(&sub)?;
    if let Some(emb) = embedder {
        let items = embed::embed_defs(emb, &sub)?;
        store.set_embeddings(&items)?;
    }
    println!("~ {rel} ({n_defs} defs)");
    Ok(())
}

fn build_full(cache: &HashMap<String, ParseResult>) -> GraphBatch {
    let mut files = Vec::new();
    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    let mut imports = Vec::new();
    for (rel, pr) in cache {
        files.push(rel.clone());
        symbols.extend(pr.symbols.iter().cloned());
        calls.extend(pr.calls.iter().cloned());
        imports.extend(pr.imports.iter().cloned());
    }
    GraphBatch::build(&files, &symbols, &calls, &imports)
}

/// The sub-batch of nodes/edges incident to `rel`: its File node, its Def
/// nodes, and every edge touching one of them (incoming + outgoing). Endpoint
/// nodes in other files already exist in the DB, so the MATCH-based edge writes
/// still resolve.
fn sub_batch_for_file(batch: &GraphBatch, rel: &str) -> GraphBatch {
    let def_prefix = format!("{rel}#");
    let touches = |id: &str| id == rel || id.starts_with(&def_prefix);
    let nodes: Vec<Node> = batch
        .nodes
        .iter()
        .filter(|n| n.file == rel)
        .cloned()
        .collect();
    let edges: Vec<Edge> = batch
        .edges
        .iter()
        .filter(|e| touches(&e.from) || touches(&e.to))
        .cloned()
        .collect();
    GraphBatch { nodes, edges }
}
