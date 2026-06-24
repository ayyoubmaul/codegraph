//! The code-graph data model and the storage seam (`Store`).
//!
//! Parsing produces a [`GraphBatch`] of nodes and edges; a [`Store`] persists
//! batches and answers queries. The concrete store (LadybugDB via the `lbug`
//! crate) lives behind the trait — isolating the FFI/cmake boundary and keeping
//! the backend swappable (fallback: SQLite + sqlite-vec).

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::symbol::{Symbol, SymbolKind};

/// What a node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Definition,
}

/// A graph node: a source file, or a definition within one.
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    /// Stable identity, e.g. `src/main.rs` or `src/main.rs#index@34`.
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub file: String,
    /// Definition kind; `None` for file nodes.
    pub symbol_kind: Option<SymbolKind>,
    pub start_line: usize,
    pub end_line: usize,
}

/// A directed relationship between two nodes.
#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

/// The kinds of relationship we record. `Imports` is populated in a later slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum EdgeKind {
    /// A file defines a symbol.
    Defines,
    /// A definition calls another definition.
    Calls,
    /// A file imports another module/file.
    Imports,
}

/// An unresolved call site: a caller definition invoking a callee *by name*.
/// `graph::GraphBatch::build` resolves the name to one or more definitions.
#[derive(Debug, Clone)]
pub struct CallRef {
    pub caller_id: String,
    pub callee_name: String,
    /// File the call occurs in (used to prefer same-file resolution).
    pub file: String,
}

/// The unit of work written to a [`Store`]: everything extracted in one pass.
#[derive(Debug, Default, Serialize)]
pub struct GraphBatch {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl GraphBatch {
    /// Stable id for a definition node.
    pub fn def_id(file: &str, name: &str, start_line: usize) -> String {
        format!("{file}#{name}@{start_line}")
    }

    /// Build a batch from discovered files, their definitions, and call sites.
    ///
    /// Nodes: one `File` per file, one `Definition` per symbol. Edges: a
    /// `Defines` edge file→def, and `Calls` edges resolved name-based. Call
    /// resolution prefers a same-file definition of that name; otherwise it
    /// links to every repo-wide definition with the name (imprecise by design —
    /// type-aware resolution is future work). Self-loops are dropped.
    pub fn build(files: &[String], symbols: &[Symbol], calls: &[CallRef]) -> GraphBatch {
        let mut batch = GraphBatch::default();

        for file in files {
            batch.nodes.push(Node {
                id: file.clone(),
                kind: NodeKind::File,
                name: file.clone(),
                file: file.clone(),
                symbol_kind: None,
                start_line: 0,
                end_line: 0,
            });
        }

        for s in symbols {
            let id = Self::def_id(&s.file, &s.name, s.start_line);
            batch.edges.push(Edge {
                from: s.file.clone(),
                to: id.clone(),
                kind: EdgeKind::Defines,
            });
            batch.nodes.push(Node {
                id,
                kind: NodeKind::Definition,
                name: s.name.clone(),
                file: s.file.clone(),
                symbol_kind: Some(s.kind),
                start_line: s.start_line,
                end_line: s.end_line,
            });
        }

        // Resolve call sites to Def→Def `Calls` edges.
        let mut by_name: HashMap<&str, Vec<&Symbol>> = HashMap::new();
        for s in symbols {
            by_name.entry(s.name.as_str()).or_default().push(s);
        }

        let mut seen: HashSet<(String, String)> = HashSet::new();
        for call in calls {
            let Some(candidates) = by_name.get(call.callee_name.as_str()) else {
                continue;
            };
            let same_file: Vec<&Symbol> = candidates
                .iter()
                .copied()
                .filter(|s| s.file == call.file)
                .collect();
            let targets: Vec<&Symbol> = if same_file.is_empty() {
                candidates.clone()
            } else {
                same_file
            };

            for callee in targets {
                let callee_id = Self::def_id(&callee.file, &callee.name, callee.start_line);
                if callee_id == call.caller_id {
                    continue; // drop self-loops
                }
                if seen.insert((call.caller_id.clone(), callee_id.clone())) {
                    batch.edges.push(Edge {
                        from: call.caller_id.clone(),
                        to: callee_id,
                        kind: EdgeKind::Calls,
                    });
                }
            }
        }

        batch
    }
}

/// The storage seam. The concrete implementation (LadybugDB via `lbug`) lives
/// in `store.rs`; keeping callers behind this trait isolates the FFI/cmake
/// boundary and keeps the backend swappable.
#[allow(dead_code)]
pub trait Store {
    /// Create node/edge tables and indexes if absent.
    fn init_schema(&mut self) -> anyhow::Result<()>;
    /// Upsert every node and edge in `batch`.
    fn write_batch(&mut self, batch: &GraphBatch) -> anyhow::Result<()>;
    /// Drop a file's nodes and edges (used by incremental re-indexing).
    fn remove_file(&mut self, rel_path: &str) -> anyhow::Result<()>;
}
