//! Local, offline text embeddings via fastembed (ONNX Runtime, CPU).
//!
//! The default model (BGE-small-en-v1.5, 384-dim) downloads once on first use
//! (~130 MB), is cached under the fastembed cache dir, and then runs fully
//! offline — no API keys, no per-query network.

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

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
