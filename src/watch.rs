//! Incremental indexing: watch the repo and patch the graph per changed file.
//!
//! Used by the standalone `watch` command and by `serve --watch` / `ui --watch`,
//! where the *same* shared store is also served. The store is held behind a
//! `tokio::sync::Mutex` so async handlers can `.lock().await` it while this
//! watcher thread `blocking_lock()`s it from a plain OS thread.
//!
//! Per change: re-parse only the changed file, rebuild resolution over the full
//! symbol set in memory, then rewrite just the sub-graph incident to that file
//! (its nodes + every edge touching them, incoming and outgoing).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::Mutex;

use crate::embed::{self, Embedder};
use crate::graph::{Edge, GraphBatch, Node, NodeKind, Store};
use crate::lang::Lang;
use crate::parse::{self, ParseResult};
use crate::store::LadybugStore;
use crate::walk;

pub type Cache = HashMap<String, ParseResult>;
pub type SharedStore = Arc<Mutex<LadybugStore>>;
pub type SharedEmbedder = Arc<Mutex<Option<Embedder>>>;

/// Standalone `watch` command: own the store, index once, then watch forever.
pub fn run(root: &Path, db: &Path, embed_on: bool) -> anyhow::Result<()> {
    let mut store = LadybugStore::open(db)?;
    let mut embedder: Option<Embedder> = None;
    let (root, cache) = index_once_owned(root, &mut store, &mut embedder, embed_on)?;
    let store: SharedStore = Arc::new(Mutex::new(store));
    let embedder: SharedEmbedder = Arc::new(Mutex::new(embedder));
    watch_only(root, cache, store, embedder, embed_on)
}

/// Full index over an owned store (no locking) — used at startup before the
/// store is shared. Returns the canonical root and the per-file parse cache.
pub fn index_once_owned(
    root: &Path,
    store: &mut LadybugStore,
    embedder: &mut Option<Embedder>,
    embed_on: bool,
) -> anyhow::Result<(PathBuf, Cache)> {
    let root = root.canonicalize()?;
    let mut cache: Cache = HashMap::new();
    for sf in walk::collect_files(&root) {
        if let Ok(src) = std::fs::read(&sf.path) {
            if let Ok(pr) = parse::parse_file(&sf.rel, &src, sf.lang) {
                cache.insert(sf.rel.clone(), pr);
            }
        }
    }
    let batch = build_full(&cache);
    store.write_batch(&batch)?;
    if embed_on {
        if embedder.is_none() {
            *embedder = Some(Embedder::new()?);
        }
        let already = store.embedded_ids()?;
        let items = embed::embed_defs(embedder.as_mut().unwrap(), &batch, &already)?;
        store.set_embeddings(&items)?;
    }
    eprintln!("codegraph: indexed {} files", cache.len());
    Ok((root, cache))
}

/// Watch `root` and patch the shared store on each change. Runs on a plain OS
/// thread (uses `blocking_lock`), so it must NOT be called inside the async
/// runtime.
pub fn watch_only(
    root: PathBuf,
    mut cache: Cache,
    store: SharedStore,
    embedder: SharedEmbedder,
    embed_on: bool,
) -> anyhow::Result<()> {
    eprintln!("codegraph: watching {}", root.display());
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
                        remove(&store, &mut cache, &rel);
                        continue;
                    }
                    if let Err(e) =
                        update_file(&store, &embedder, &mut cache, &rel, lang, path, embed_on)
                    {
                        eprintln!("codegraph: update {rel} failed: {e}");
                    }
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
                    remove(&store, &mut cache, &rel);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn remove(store: &SharedStore, cache: &mut Cache, rel: &str) {
    if cache.remove(rel).is_some() {
        match store.blocking_lock().remove_file(rel) {
            Ok(()) => eprintln!("- {rel}"),
            Err(e) => eprintln!("codegraph: remove {rel} failed: {e}"),
        }
    }
}

fn update_file(
    store: &SharedStore,
    embedder: &SharedEmbedder,
    cache: &mut Cache,
    rel: &str,
    lang: Lang,
    path: &Path,
    embed_on: bool,
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

    {
        let mut s = store.blocking_lock();
        s.remove_file(rel)?;
        s.write_batch(&sub)?;
    }
    if embed_on {
        // The changed file's defs were just removed, so they have no cached
        // embedding — embed them all.
        let skip = std::collections::HashSet::new();
        let items = {
            let mut guard = embedder.blocking_lock();
            if guard.is_none() {
                *guard = Some(Embedder::new()?);
            }
            embed::embed_defs(guard.as_mut().unwrap(), &sub, &skip)?
        };
        if !items.is_empty() {
            store.blocking_lock().set_embeddings(&items)?;
        }
    }
    eprintln!("~ {rel} ({n_defs} defs)");
    Ok(())
}

fn build_full(cache: &Cache) -> GraphBatch {
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

/// Nodes/edges incident to `rel`: its File node, its Def nodes, and every edge
/// touching them (incoming + outgoing). Endpoint nodes in other files already
/// exist in the DB, so the MATCH-based edge writes still resolve.
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
