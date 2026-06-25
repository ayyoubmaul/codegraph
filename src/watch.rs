//! Incremental indexing: watch repos and patch the graph per changed file.
//!
//! Supports a multi-repo workspace — each watched root is qualified by its repo
//! name, and filesystem events are mapped back to the right repo prefix. On
//! create/modify, only the changed file is re-parsed and its incident sub-graph
//! rewritten (re-resolved against the full in-memory cache); on delete, the
//! file's nodes/edges are removed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::Mutex;

use crate::embed::{self, Embedder};
use crate::graph::{Edge, GraphBatch, Node, NodeKind, Store};
use crate::lang::Lang;
use crate::parse::{self, ParseResult};
use crate::store::LadybugStore;
use crate::vector::SharedVector;
use crate::walk;

pub type Cache = HashMap<String, ParseResult>;
/// The store is shared *without* a Rust lock: `LadybugStore` is `Sync` and
/// serializes writers internally, so readers (the MCP/UI query handlers) run
/// concurrently with the watcher's writes instead of queueing behind them.
pub type SharedStore = Arc<LadybugStore>;
pub type SharedEmbedder = Arc<Mutex<Option<Embedder>>>;
/// `(canonical root, repo prefix)` for each watched repo.
type Roots = Vec<(PathBuf, String)>;

/// Standalone `watch` command: own the store, index all roots, watch forever.
pub fn run(paths: &[PathBuf], db: &Path, embed_on: bool) -> anyhow::Result<()> {
    let (roots, cache) = prepare(paths)?;
    let store = LadybugStore::open(db)?;
    let mut embedder: Option<Embedder> = None;
    let batch = build_full(&cache);
    if store.def_count()? == 0 {
        bulk_or_tmp(&store, &batch)?;
    } else {
        store.write_batch(&batch)?;
    }
    if embed_on {
        if embedder.is_none() {
            embedder = Some(Embedder::new()?);
        }
        let already = store.embedded_ids()?;
        let items = embed::embed_defs(embedder.as_mut().unwrap(), &batch, &already)?;
        store.set_embeddings(&items)?;
    }
    let store: SharedStore = Arc::new(store);
    let embedder: SharedEmbedder = Arc::new(Mutex::new(embedder));
    watch_only(roots, cache, store, embedder, embed_on, None)
}

/// Index all roots into an *already-shared* store, then watch — on a background
/// thread so a server can serve immediately.
pub fn index_and_watch(
    paths: &[PathBuf],
    store: SharedStore,
    embedder: SharedEmbedder,
    vindex: SharedVector,
    embed_on: bool,
    reanalyze: Option<u64>,
) -> anyhow::Result<()> {
    let (roots, cache) = prepare(paths)?;
    let batch = build_full(&cache);
    if store.def_count()? == 0 {
        bulk_or_tmp(&store, &batch)?;
    } else {
        store.write_batch(&batch)?;
    }
    if embed_on {
        let already = store.embedded_ids()?;
        let items = {
            let mut guard = embedder.blocking_lock();
            if guard.is_none() {
                *guard = Some(Embedder::new()?);
            }
            embed::embed_defs(guard.as_mut().unwrap(), &batch, &already)?
        };
        if !items.is_empty() {
            store.set_embeddings(&items)?;
        }
    }
    {
        let built = crate::vector::build_from_store(&store)?;
        *vindex.blocking_write() = built;
    }
    eprintln!(
        "codegraph: indexed {} files across {} repo(s)",
        cache.len(),
        roots.len()
    );

    // Periodically re-run analyze (PageRank/communities are batch, not
    // incremental) on a background thread sharing the store. `analyze::run`
    // takes `&LadybugStore` and computes lock-free — only its final write
    // briefly serializes against other writers — so this never blocks queries.
    if let Some(secs) = reanalyze {
        let s = store.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(secs.max(1)));
                match crate::analyze::run(&s, 30) {
                    Ok((n, c)) => eprintln!("codegraph: re-analyzed {n} defs → {c} communities"),
                    Err(e) => eprintln!("codegraph: re-analyze failed: {e}"),
                }
            }
        });
    }

    watch_only(roots, cache, store, embedder, embed_on, Some(vindex))
}

/// Index a single repo/directory into an already-open shared store **on demand**
/// (e.g. an MCP `index_repo` call), without starting a watch loop. This is the
/// only way to add a repo while a server holds the database — a separate
/// `codegraph index` process can't, because LadybugDB is single-writer.
///
/// Resolves calls within the repo, writes its sub-graph, and (when `embed_on`)
/// embeds its defs and adds them to the live vector index. Returns
/// `(files, defs)`. Cross-repo edges to already-indexed repos aren't formed
/// here (a one-shot index only sees this repo's symbols); the repo is still
/// fully queryable, and `important`/PageRank picks it up on the next analyze.
pub fn index_path(
    path: &Path,
    store: &LadybugStore,
    embed_on: bool,
    vindex: Option<&SharedVector>,
) -> anyhow::Result<(usize, usize)> {
    let (_roots, cache) = prepare(&[path.to_path_buf()])?;
    let batch = build_full(&cache);
    let n_files = cache.len();
    let n_defs = batch
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Definition)
        .count();
    store.write_batch(&batch)?;
    if embed_on && n_defs > 0 {
        let already = store.embedded_ids()?;
        let mut embedder = Embedder::new()?;
        let items = embed::embed_defs(&mut embedder, &batch, &already)?;
        if !items.is_empty() {
            store.set_embeddings(&items)?;
            if let Some(vi) = vindex {
                let mut guard = vi.blocking_write();
                match guard.as_mut() {
                    Some(idx) => {
                        for (id, vec) in &items {
                            idx.add(id, vec);
                        }
                    }
                    None => *guard = crate::vector::build_from_store(store)?,
                }
            }
        }
    }
    Ok((n_files, n_defs))
}

/// Watch every root and patch the shared store on each change. Runs on a plain
/// OS thread (uses `blocking_lock`), not inside the async runtime.
pub fn watch_only(
    roots: Roots,
    mut cache: Cache,
    store: SharedStore,
    embedder: SharedEmbedder,
    embed_on: bool,
    vindex: Option<SharedVector>,
) -> anyhow::Result<()> {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    for (root, _) in &roots {
        watcher.watch(root, RecursiveMode::Recursive)?;
    }
    eprintln!("codegraph: watching {} repo(s)", roots.len());

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
                    let Some(rel) = rel_for(&roots, path) else {
                        continue;
                    };
                    if !path.exists() {
                        remove(&store, &mut cache, &rel);
                        continue;
                    }
                    if let Err(e) = update_file(
                        &store,
                        &embedder,
                        &mut cache,
                        &rel,
                        lang,
                        path,
                        embed_on,
                        vindex.as_ref(),
                    ) {
                        eprintln!("codegraph: update {rel} failed: {e}");
                    }
                }
            }
            EventKind::Remove(_) => {
                for path in &event.paths {
                    if Lang::from_path(path).is_none() {
                        continue;
                    }
                    let Some(rel) = rel_for(&roots, path) else {
                        continue;
                    };
                    remove(&store, &mut cache, &rel);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Parse every root into a cache; return the canonical roots + their repo names.
fn prepare(paths: &[PathBuf]) -> anyhow::Result<(Roots, Cache)> {
    let mut roots = Roots::new();
    let mut cache = Cache::new();
    for path in paths {
        let repo = walk::repo_name(path);
        let root = path.canonicalize()?;
        for sf in walk::collect_files(&root, &repo) {
            if let Ok(src) = std::fs::read(&sf.path) {
                if let Ok(pr) = parse::parse_file(&sf.rel, &src, sf.lang) {
                    cache.insert(sf.rel.clone(), pr);
                }
            }
        }
        roots.push((root, repo));
    }
    Ok((roots, cache))
}

/// Map an absolute event path to its qualified `repo/rel`, or `None` if it isn't
/// under any watched root.
fn rel_for(roots: &Roots, path: &Path) -> Option<String> {
    for (root, repo) in roots {
        if let Ok(stripped) = path.strip_prefix(root) {
            let s = stripped.to_string_lossy().replace('\\', "/");
            return Some(format!("{repo}/{s}"));
        }
    }
    None
}

fn bulk_or_tmp(store: &LadybugStore, batch: &GraphBatch) -> anyhow::Result<()> {
    let tmp = std::env::temp_dir().join(format!("codegraph-bulk-{}", std::process::id()));
    store.bulk_load(batch, &tmp)?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

fn remove(store: &SharedStore, cache: &mut Cache, rel: &str) {
    if cache.remove(rel).is_some() {
        match store.remove_file(rel) {
            Ok(()) => eprintln!("- {rel}"),
            Err(e) => eprintln!("codegraph: remove {rel} failed: {e}"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_file(
    store: &SharedStore,
    embedder: &SharedEmbedder,
    cache: &mut Cache,
    rel: &str,
    lang: Lang,
    path: &Path,
    embed_on: bool,
    vindex: Option<&SharedVector>,
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

    // Re-point the file's sub-graph: one writer-serialized transaction would be
    // ideal, but remove+write are two; readers still only ever see committed
    // state, and the window is tiny (one file's nodes/edges).
    store.remove_file(rel)?;
    store.write_batch(&sub)?;
    if embed_on {
        let skip = std::collections::HashSet::new();
        let items = {
            let mut guard = embedder.blocking_lock();
            if guard.is_none() {
                *guard = Some(Embedder::new()?);
            }
            embed::embed_defs(guard.as_mut().unwrap(), &sub, &skip)?
        };
        if !items.is_empty() {
            store.set_embeddings(&items)?;
            if let Some(vi) = vindex {
                let mut guard = vi.blocking_write();
                if let Some(idx) = guard.as_mut() {
                    for (id, vec) in &items {
                        idx.add(id, vec);
                    }
                }
            }
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
/// touching them (incoming + outgoing).
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
