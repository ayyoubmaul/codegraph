//! Local, offline text embeddings via fastembed (ONNX Runtime, CPU).
//!
//! The default model (BGE-small-en-v1.5, 384-dim) downloads once on first use
//! (~130 MB), is cached under the fastembed cache dir, and then runs fully
//! offline — no API keys, no per-query network.

use std::collections::HashSet;
use std::sync::Arc;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use tokio::sync::Mutex;

use crate::graph::{GraphBatch, NodeKind};

/// Embedding dimensionality of the default model. Must match the `FLOAT[384]`
/// column in the store schema (`store::LadybugStore::init_schema`).
#[allow(dead_code)]
pub const DIM: usize = 384;

pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Load the embedding model (downloads on first use, then cached/offline).
    pub fn new() -> anyhow::Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(true),
        )
        .map_err(|e| anyhow::anyhow!("load embedding model: {e}"))?;
        Ok(Self { model })
    }

    /// Embed a batch of documents, preserving order.
    pub fn embed_batch(&mut self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        self.model
            .embed(texts, None)
            .map_err(|e| anyhow::anyhow!("embed batch: {e}"))
    }

    /// Embed a single query string.
    pub fn embed_one(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut out = self
            .model
            .embed(vec![text.to_string()], None)
            .map_err(|e| anyhow::anyhow!("embed query: {e}"))?;
        out.pop()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))
    }
}

/// Build the `(def_id, embedding)` pairs for every definition node in `batch`.
/// The embedded text is `"{Kind} {name}"`, which the model maps to meaning.
pub fn embed_defs(
    embedder: &mut Embedder,
    batch: &GraphBatch,
    skip: &HashSet<String>,
) -> anyhow::Result<Vec<(String, Vec<f32>)>> {
    let defs: Vec<(String, String)> = batch
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Definition && !skip.contains(&n.id))
        .map(|n| {
            let text = match n.symbol_kind {
                Some(k) => format!("{k:?} {}", n.name),
                None => n.name.clone(),
            };
            (n.id.clone(), text)
        })
        .collect();
    if defs.is_empty() {
        return Ok(Vec::new());
    }
    let vectors = embedder.embed_batch(defs.iter().map(|(_, t)| t.clone()).collect())?;
    Ok(defs
        .into_iter()
        .zip(vectors)
        .map(|((id, _), v)| (id, v))
        .collect())
}

/// Eagerly load the embedding model on a background thread so the first `search`
/// is fast — it pays the ~3 s ONNX init (and one-time model download) at startup
/// instead of on the first query. No-op if already loaded.
pub fn warm(embedder: Arc<Mutex<Option<Embedder>>>) {
    std::thread::spawn(move || {
        let mut guard = embedder.blocking_lock();
        if guard.is_none() {
            match Embedder::new() {
                Ok(e) => {
                    *guard = Some(e);
                    eprintln!("codegraph: embedding model ready");
                }
                Err(e) => eprintln!("codegraph: embedding model load failed: {e}"),
            }
        }
    });
}
