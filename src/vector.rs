//! Approximate nearest-neighbour vector index (HNSW, pure Rust via `hnsw_rs`).
//!
//! For semantic search at scale, the long-running servers build this in memory
//! from the embeddings already stored in the graph DB at startup, then answer
//! queries in ~O(log n) instead of brute-force O(n) `array_cosine_similarity`.
//! Def ids map to integer keys; results are joined back to metadata in the DB.

use std::collections::HashMap;
use std::sync::Arc;

use hnsw_rs::prelude::*;
use tokio::sync::Mutex;

use crate::store::{DefHit, LadybugStore};

pub type SharedVector = Arc<Mutex<Option<VectorIndex>>>;

pub struct VectorIndex {
    hnsw: Hnsw<'static, f32, DistCosine>,
    ids: Vec<String>,
    lookup: HashMap<String, usize>,
}

impl VectorIndex {
    /// Build an index from `(def_id, embedding)` pairs.
    pub fn build(entries: Vec<(String, Vec<f32>)>) -> Self {
        let n = entries.len();
        let nb_layer = 16.min(((n.max(2) as f32).ln() as usize).max(1));
        let hnsw = Hnsw::<f32, DistCosine>::new(16, n.max(1024), nb_layer, 200, DistCosine {});
        let mut index = VectorIndex {
            hnsw,
            ids: Vec::with_capacity(n),
            lookup: HashMap::with_capacity(n),
        };
        for (id, vec) in &entries {
            index.insert(id, vec);
        }
        index
    }

    /// Add a single vector. A def's embedding is fixed by its id, so an existing
    /// id is left as-is.
    pub fn add(&mut self, id: &str, vec: &[f32]) {
        self.insert(id, vec);
    }

    fn insert(&mut self, id: &str, vec: &[f32]) {
        if self.lookup.contains_key(id) {
            return;
        }
        let data_id = self.ids.len();
        self.hnsw.insert((vec, data_id));
        self.ids.push(id.to_string());
        self.lookup.insert(id.to_string(), data_id);
    }

    /// The `k` nearest def ids to `query`, each with cosine similarity.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        if self.ids.is_empty() {
            return Vec::new();
        }
        let ef = (k * 4).max(64);
        self.hnsw
            .search(query, k, ef)
            .into_iter()
            .filter_map(|n| self.ids.get(n.d_id).map(|id| (id.clone(), 1.0 - n.distance)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Build a vector index from the embeddings stored in `store`, or `None` if no
/// embeddings have been computed yet.
pub fn build_from_store(store: &LadybugStore) -> anyhow::Result<Option<VectorIndex>> {
    let entries = store.all_embeddings()?;
    if entries.is_empty() {
        Ok(None)
    } else {
        Ok(Some(VectorIndex::build(entries)))
    }
}

/// Semantic search: use the HNSW index if present (joining metadata from the
/// graph DB), else fall back to the DB's brute-force cosine search.
pub fn hybrid_search(
    store: &LadybugStore,
    vindex: Option<&VectorIndex>,
    query: &[f32],
    k: usize,
) -> anyhow::Result<Vec<(DefHit, f32)>> {
    match vindex {
        Some(vi) if vi.len() > 0 => {
            // Over-fetch so orphaned ids (deleted defs) can be filtered out.
            let hits = vi.search(query, k * 2);
            let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
            let meta = store.def_hits_by_ids(&ids)?;
            let mut out = Vec::with_capacity(k);
            for (id, sim) in hits {
                if let Some(hit) = meta.get(&id) {
                    out.push((hit.clone(), sim));
                    if out.len() >= k {
                        break;
                    }
                }
            }
            Ok(out)
        }
        _ => store.semantic_search(query, k),
    }
}
