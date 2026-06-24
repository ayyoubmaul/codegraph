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

/// The kinds of relationship we record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A file defines a symbol.
    Defines,
    /// A definition calls another definition.
    Calls,
    /// A file imports another file.
    Imports,
}

/// An unresolved call site: a caller definition invoking a callee *by name*.
#[derive(Debug, Clone)]
pub struct CallRef {
    pub caller_id: String,
    pub callee_name: String,
    /// File the call occurs in (used to prefer same-file/imported resolution).
    pub file: String,
    /// Was this a method-style call (`x.foo()`) vs a plain call (`foo()`)?
    pub is_method: bool,
}

/// An import statement: `file` imports module/path `source` (raw, unresolved).
#[derive(Debug, Clone)]
pub struct ImportRef {
    pub file: String,
    pub source: String,
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

    /// Build a batch from discovered files, definitions, call sites, and imports.
    ///
    /// Nodes: one `File` per file, one `Definition` per symbol. Edges: `Defines`
    /// (file→def), `Imports` (file→file, resolved for relative JS/TS sources),
    /// and `Calls` (def→def). Call resolution is receiver-aware (method calls
    /// prefer `Method` defs, plain calls prefer non-method) and locality-scoped
    /// (same-file → imported files → repo-wide). Imprecise by design — true
    /// type inference is future work. Self-loops are dropped.
    pub fn build(
        files: &[String],
        symbols: &[Symbol],
        calls: &[CallRef],
        imports: &[ImportRef],
    ) -> GraphBatch {
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

        // Imports: resolve relative (JS/TS) sources to File nodes.
        let file_set: HashSet<&str> = files.iter().map(String::as_str).collect();
        let mut imports_by_file: HashMap<&str, HashSet<String>> = HashMap::new();
        let mut import_seen: HashSet<(String, String)> = HashSet::new();
        for imp in imports {
            let Some(target) = resolve_relative_import(&imp.file, &imp.source, &file_set) else {
                continue;
            };
            if target == imp.file {
                continue;
            }
            imports_by_file
                .entry(imp.file.as_str())
                .or_default()
                .insert(target.clone());
            if import_seen.insert((imp.file.clone(), target.clone())) {
                batch.edges.push(Edge {
                    from: imp.file.clone(),
                    to: target,
                    kind: EdgeKind::Imports,
                });
            }
        }

        // Calls: name index, then receiver-aware + locality-scoped resolution.
        let mut by_name: HashMap<&str, Vec<&Symbol>> = HashMap::new();
        for s in symbols {
            by_name.entry(s.name.as_str()).or_default().push(s);
        }

        let mut seen: HashSet<(String, String)> = HashSet::new();
        for call in calls {
            let Some(candidates) = by_name.get(call.callee_name.as_str()) else {
                continue;
            };

            // Receiver/kind preference; fall back to all if it empties the set.
            let mut pool: Vec<&Symbol> = candidates
                .iter()
                .copied()
                .filter(|s| {
                    if call.is_method {
                        s.kind == SymbolKind::Method
                    } else {
                        s.kind != SymbolKind::Method
                    }
                })
                .collect();
            if pool.is_empty() {
                pool = candidates.clone();
            }

            // Locality tiers: same-file → imported files → repo-wide.
            let same_file: Vec<&Symbol> =
                pool.iter().copied().filter(|s| s.file == call.file).collect();
            let targets: Vec<&Symbol> = if !same_file.is_empty() {
                same_file
            } else if let Some(imported) = imports_by_file.get(call.file.as_str()) {
                let in_imports: Vec<&Symbol> = pool
                    .iter()
                    .copied()
                    .filter(|s| imported.contains(s.file.as_str()))
                    .collect();
                if in_imports.is_empty() { pool } else { in_imports }
            } else {
                pool
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

/// Resolve a relative import (`./x`, `../lib/x`) against the importing file's
/// directory, probing common extensions and index files. Returns the matched
/// repo-relative path, or `None` for non-relative or unresolved imports.
fn resolve_relative_import(
    importing_file: &str,
    source: &str,
    files: &HashSet<&str>,
) -> Option<String> {
    if !(source.starts_with("./") || source.starts_with("../")) {
        return None;
    }
    let dir = importing_file
        .rfind('/')
        .map(|i| &importing_file[..i])
        .unwrap_or("");
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for seg in source.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let base = parts.join("/");

    if files.contains(base.as_str()) {
        return Some(base);
    }
    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "py"] {
        let cand = format!("{base}.{ext}");
        if files.contains(cand.as_str()) {
            return Some(cand);
        }
    }
    for idx in ["index.ts", "index.tsx", "index.js", "index.jsx"] {
        let cand = format!("{base}/{idx}");
        if files.contains(cand.as_str()) {
            return Some(cand);
        }
    }
    None
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
