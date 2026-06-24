//! LadybugDB-backed implementation of the graph [`Store`], plus structural
//! queries (`who_calls`, `call_chain`).
//!
//! Holds a `Database` and opens a short-lived `Connection` per operation: a
//! `Connection<'a>` borrows the `Database`, so storing both in one struct would
//! be self-referential. Writes run inside a single transaction using prepared
//! statements (LadybugDB serializes writers — we parallelize the parse stage,
//! batch the write stage).

use std::path::Path;

use lbug::{Connection, Database, LogicalType, SystemConfig, Value};

use crate::graph::{EdgeKind, GraphBatch, NodeKind, Store};

pub struct LadybugStore {
    db: Database,
}

/// A definition row returned by a query.
#[derive(Debug)]
pub struct DefHit {
    pub name: String,
    pub file: String,
    pub start_line: i64,
}

/// A definition node with metadata, for the UI graph.
#[derive(serde::Serialize)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub file: String,
    pub kind: String,
    pub community: Option<i64>,
    pub pagerank: Option<f64>,
}

impl LadybugStore {
    /// Open (or create) a LadybugDB database at `path` and ensure the schema.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("open LadybugDB at {}: {e}", path.display()))?;
        let mut store = Self { db };
        store.init_schema()?;
        Ok(store)
    }

    fn connect(&self) -> anyhow::Result<Connection<'_>> {
        Connection::new(&self.db).map_err(|e| anyhow::anyhow!("lbug connect: {e}"))
    }

    /// `(files, defs, defines, calls, imports)` currently stored.
    pub fn summary(&self) -> anyhow::Result<(u64, u64, u64, u64, u64)> {
        Ok((
            self.count("MATCH (:File) RETURN count(*)")?,
            self.count("MATCH (:Def) RETURN count(*)")?,
            self.count("MATCH (:File)-[r:Defines]->(:Def) RETURN count(r)")?,
            self.count("MATCH (:Def)-[r:Calls]->(:Def) RETURN count(r)")?,
            self.count("MATCH (:File)-[r:Imports]->(:File) RETURN count(r)")?,
        ))
    }

    /// Direct callers of any definition named `name`.
    pub fn who_calls(&self, name: &str) -> anyhow::Result<Vec<DefHit>> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "MATCH (caller:Def)-[:Calls]->(callee:Def {name: $name}) \
                 RETURN DISTINCT caller.name, caller.file, caller.start_line \
                 ORDER BY caller.file, caller.start_line",
            )
            .map_err(|e| anyhow::anyhow!("lbug prepare who_calls: {e}"))?;
        let result = conn
            .execute(&mut stmt, vec![("name", Value::String(name.to_string()))])
            .map_err(|e| anyhow::anyhow!("lbug who_calls: {e}"))?;
        Ok(result.filter_map(row_to_hit).collect())
    }

    /// Definitions transitively reachable from `name` via `Calls`, up to `depth`
    /// hops (clamped to 1..=10).
    pub fn call_chain(&self, name: &str, depth: u8) -> anyhow::Result<Vec<DefHit>> {
        let depth = depth.clamp(1, 10);
        let conn = self.connect()?;
        // `depth` is a validated integer, safe to interpolate; `name` is a param.
        let query = format!(
            "MATCH (:Def {{name: $name}})-[:Calls*1..{depth}]->(d:Def) \
             RETURN DISTINCT d.name, d.file, d.start_line \
             ORDER BY d.file, d.start_line"
        );
        let mut stmt = conn
            .prepare(&query)
            .map_err(|e| anyhow::anyhow!("lbug prepare call_chain: {e}"))?;
        let result = conn
            .execute(&mut stmt, vec![("name", Value::String(name.to_string()))])
            .map_err(|e| anyhow::anyhow!("lbug call_chain: {e}"))?;
        Ok(result.filter_map(row_to_hit).collect())
    }

    /// Store an embedding vector for each `(def_id, vector)` pair.
    pub fn set_embeddings(&mut self, items: &[(String, Vec<f32>)]) -> anyhow::Result<()> {
        let conn = self.connect()?;
        conn.query("BEGIN TRANSACTION")
            .map_err(|e| anyhow::anyhow!("lbug begin: {e}"))?;
        let mut stmt = conn
            .prepare("MATCH (d:Def {id: $id}) SET d.embedding = $vec")
            .map_err(|e| anyhow::anyhow!("lbug prepare set_embedding: {e}"))?;
        for (id, vec) in items {
            let arr = Value::Array(
                LogicalType::Float,
                vec.iter().map(|f| Value::Float(*f)).collect(),
            );
            conn.execute(
                &mut stmt,
                vec![("id", Value::String(id.clone())), ("vec", arr)],
            )
            .map_err(|e| anyhow::anyhow!("lbug set_embedding `{id}`: {e}"))?;
        }
        conn.query("COMMIT")
            .map_err(|e| anyhow::anyhow!("lbug commit: {e}"))?;
        Ok(())
    }

    /// Brute-force cosine KNN over stored embeddings: the `k` definitions most
    /// similar to `query`, each with its similarity score.
    pub fn semantic_search(&self, query: &[f32], k: usize) -> anyhow::Result<Vec<(DefHit, f32)>> {
        let k = k.clamp(1, 100);
        let conn = self.connect()?;
        let q = Value::Array(
            LogicalType::Float,
            query.iter().map(|f| Value::Float(*f)).collect(),
        );
        let mut stmt = conn
            .prepare(&format!(
                "MATCH (d:Def) WHERE d.embedding IS NOT NULL \
                 RETURN d.name, d.file, d.start_line, \
                 array_cosine_similarity(d.embedding, $q) AS sim \
                 ORDER BY sim DESC LIMIT {k}"
            ))
            .map_err(|e| anyhow::anyhow!("lbug prepare semantic_search: {e}"))?;
        let result = conn
            .execute(&mut stmt, vec![("q", q)])
            .map_err(|e| anyhow::anyhow!("lbug semantic_search: {e}"))?;
        Ok(result.filter_map(row_to_hit_score).collect())
    }

    /// All definition ids (so isolated defs are included in analysis).
    pub fn def_ids(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.connect()?;
        let result = conn
            .query("MATCH (d:Def) RETURN d.id")
            .map_err(|e| anyhow::anyhow!("lbug def_ids: {e}"))?;
        Ok(result
            .filter_map(|row| match row.into_iter().next()? {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect())
    }

    /// All `Calls` edges as `(caller_id, callee_id)` pairs.
    pub fn call_edges(&self) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.connect()?;
        let result = conn
            .query("MATCH (a:Def)-[:Calls]->(b:Def) RETURN a.id, b.id")
            .map_err(|e| anyhow::anyhow!("lbug call_edges: {e}"))?;
        Ok(result
            .filter_map(|row| {
                let mut it = row.into_iter();
                let a = match it.next()? {
                    Value::String(s) => s,
                    _ => return None,
                };
                let b = match it.next()? {
                    Value::String(s) => s,
                    _ => return None,
                };
                Some((a, b))
            })
            .collect())
    }

    /// Store `(def_id, pagerank, community)` analysis results.
    pub fn set_analysis(&mut self, items: &[(String, f64, i64)]) -> anyhow::Result<()> {
        let conn = self.connect()?;
        conn.query("BEGIN TRANSACTION")
            .map_err(|e| anyhow::anyhow!("lbug begin: {e}"))?;
        let mut stmt = conn
            .prepare("MATCH (d:Def {id: $id}) SET d.pagerank = $pr, d.community = $c")
            .map_err(|e| anyhow::anyhow!("lbug prepare set_analysis: {e}"))?;
        for (id, pr, c) in items {
            conn.execute(
                &mut stmt,
                vec![
                    ("id", Value::String(id.clone())),
                    ("pr", Value::Double(*pr)),
                    ("c", Value::Int64(*c)),
                ],
            )
            .map_err(|e| anyhow::anyhow!("lbug set_analysis `{id}`: {e}"))?;
        }
        conn.query("COMMIT")
            .map_err(|e| anyhow::anyhow!("lbug commit: {e}"))?;
        Ok(())
    }

    /// The `k` most important definitions by PageRank (with their score).
    pub fn top_important(&self, k: usize) -> anyhow::Result<Vec<(DefHit, f32)>> {
        let k = k.clamp(1, 200);
        let conn = self.connect()?;
        let mut result = conn
            .query(&format!(
                "MATCH (d:Def) WHERE d.pagerank IS NOT NULL \
                 RETURN d.name, d.file, d.start_line, d.pagerank \
                 ORDER BY d.pagerank DESC LIMIT {k}"
            ))
            .map_err(|e| anyhow::anyhow!("lbug top_important: {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = result.next() {
            if let Some(hit) = row_to_hit_score(row) {
                out.push(hit);
            }
        }
        Ok(out)
    }

    /// Every analyzed definition as `(community, def, pagerank)`, ordered by
    /// community then importance.
    pub fn community_members(&self) -> anyhow::Result<Vec<(i64, DefHit, f64)>> {
        let conn = self.connect()?;
        let result = conn
            .query(
                "MATCH (d:Def) WHERE d.community IS NOT NULL \
                 RETURN d.community, d.name, d.file, d.start_line, d.pagerank \
                 ORDER BY d.community, d.pagerank DESC",
            )
            .map_err(|e| anyhow::anyhow!("lbug community_members: {e}"))?;
        Ok(result
            .filter_map(|row| {
                let mut it = row.into_iter();
                let community = match it.next()? {
                    Value::Int64(n) => n,
                    _ => return None,
                };
                let name = match it.next()? {
                    Value::String(s) => s,
                    _ => return None,
                };
                let file = match it.next()? {
                    Value::String(s) => s,
                    _ => return None,
                };
                let start_line = match it.next()? {
                    Value::Int64(n) => n,
                    _ => 0,
                };
                let pr = match it.next()? {
                    Value::Double(f) => f,
                    Value::Float(f) => f as f64,
                    _ => 0.0,
                };
                Some((
                    community,
                    DefHit {
                        name,
                        file,
                        start_line,
                    },
                    pr,
                ))
            })
            .collect())
    }

    /// Number of definition nodes (used to detect a fresh database).
    pub fn def_count(&self) -> anyhow::Result<u64> {
        self.count("MATCH (:Def) RETURN count(*)")
    }

    /// Ids of definitions that already have an embedding (so re-indexing can
    /// skip them — a def's embed-text is fixed by its id).
    pub fn embedded_ids(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        let conn = self.connect()?;
        let result = conn
            .query("MATCH (d:Def) WHERE d.embedding IS NOT NULL RETURN d.id")
            .map_err(|e| anyhow::anyhow!("lbug embedded_ids: {e}"))?;
        Ok(result
            .filter_map(|row| match row.into_iter().next()? {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect())
    }

    /// Bulk-load a batch into EMPTY tables via CSV `COPY` — the fast path for a
    /// fresh index. Writes temp CSVs under `tmp_dir`. `COPY` runs once per table,
    /// so callers use this only when the database is empty.
    pub fn bulk_load(&self, batch: &GraphBatch, tmp_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(tmp_dir)?;
        let mut file_csv = String::new();
        let mut def_csv = String::new();
        let mut defines_csv = String::new();
        let mut calls_csv = String::new();
        let mut imports_csv = String::new();

        for n in &batch.nodes {
            match n.kind {
                NodeKind::File => {
                    file_csv.push_str(&csv_field(&n.id));
                    file_csv.push('\n');
                }
                NodeKind::Definition => {
                    let kind = n.symbol_kind.map(|k| format!("{k:?}")).unwrap_or_default();
                    def_csv.push_str(&format!(
                        "{},{},{},{},{},{}\n",
                        csv_field(&n.id),
                        csv_field(&n.name),
                        csv_field(&kind),
                        csv_field(&n.file),
                        n.start_line,
                        n.end_line
                    ));
                }
            }
        }
        for e in &batch.edges {
            let line = format!("{},{}\n", csv_field(&e.from), csv_field(&e.to));
            match e.kind {
                EdgeKind::Defines => defines_csv.push_str(&line),
                EdgeKind::Calls => calls_csv.push_str(&line),
                EdgeKind::Imports => imports_csv.push_str(&line),
            }
        }

        self.copy_csv(tmp_dir, "file.csv", &file_csv, "COPY File FROM '{path}'")?;
        self.copy_csv(
            tmp_dir,
            "def.csv",
            &def_csv,
            "COPY Def (id, name, kind, file, start_line, end_line) FROM '{path}'",
        )?;
        self.copy_csv(tmp_dir, "defines.csv", &defines_csv, "COPY Defines FROM '{path}'")?;
        self.copy_csv(tmp_dir, "calls.csv", &calls_csv, "COPY Calls FROM '{path}'")?;
        self.copy_csv(tmp_dir, "imports.csv", &imports_csv, "COPY Imports FROM '{path}'")?;
        Ok(())
    }

    fn copy_csv(&self, tmp: &Path, name: &str, content: &str, tmpl: &str) -> anyhow::Result<()> {
        if content.is_empty() {
            return Ok(());
        }
        let path = tmp.join(name);
        std::fs::write(&path, content)?;
        let query = tmpl.replace("{path}", &path.to_string_lossy());
        let conn = self.connect()?;
        conn.query(&query)
            .map_err(|e| anyhow::anyhow!("lbug bulk `{query}`: {e}"))?;
        Ok(())
    }

    /// All definitions with their metadata (for the UI graph).
    pub fn graph_nodes(&self) -> anyhow::Result<Vec<GraphNode>> {
        let conn = self.connect()?;
        let result = conn
            .query("MATCH (d:Def) RETURN d.id, d.name, d.file, d.kind, d.community, d.pagerank")
            .map_err(|e| anyhow::anyhow!("lbug graph_nodes: {e}"))?;
        Ok(result
            .filter_map(|row| {
                let mut it = row.into_iter();
                let id = as_string(it.next()?)?;
                let name = as_string(it.next()?)?;
                let file = as_string(it.next()?)?;
                let kind = as_string(it.next()?)?;
                let community = match it.next()? {
                    Value::Int64(n) => Some(n),
                    _ => None,
                };
                let pagerank = match it.next()? {
                    Value::Double(f) => Some(f),
                    Value::Float(f) => Some(f as f64),
                    _ => None,
                };
                Some(GraphNode {
                    id,
                    name,
                    file,
                    kind,
                    community,
                    pagerank,
                })
            })
            .collect())
    }

    fn count(&self, query: &str) -> anyhow::Result<u64> {
        let conn = self.connect()?;
        let mut result = conn
            .query(query)
            .map_err(|e| anyhow::anyhow!("lbug query `{query}`: {e}"))?;
        match result.next().and_then(|row| row.into_iter().next()) {
            Some(Value::Int64(n)) => Ok(n.max(0) as u64),
            other => anyhow::bail!("unexpected count result for `{query}`: {other:?}"),
        }
    }
}

impl Store for LadybugStore {
    fn init_schema(&mut self) -> anyhow::Result<()> {
        let conn = self.connect()?;
        for ddl in [
            "CREATE NODE TABLE IF NOT EXISTS File(path STRING, PRIMARY KEY(path))",
            "CREATE NODE TABLE IF NOT EXISTS Def(id STRING, name STRING, kind STRING, \
             file STRING, start_line INT64, end_line INT64, embedding FLOAT[384], \
             pagerank DOUBLE, community INT64, PRIMARY KEY(id))",
            "CREATE REL TABLE IF NOT EXISTS Defines(FROM File TO Def)",
            "CREATE REL TABLE IF NOT EXISTS Calls(FROM Def TO Def)",
            "CREATE REL TABLE IF NOT EXISTS Imports(FROM File TO File)",
        ] {
            conn.query(ddl)
                .map_err(|e| anyhow::anyhow!("lbug schema `{ddl}`: {e}"))?;
        }
        Ok(())
    }

    fn write_batch(&mut self, batch: &GraphBatch) -> anyhow::Result<()> {
        let conn = self.connect()?;
        conn.query("BEGIN TRANSACTION")
            .map_err(|e| anyhow::anyhow!("lbug begin: {e}"))?;

        let mut file_stmt = conn
            .prepare("MERGE (:File {path: $path})")
            .map_err(|e| anyhow::anyhow!("lbug prepare file: {e}"))?;
        let mut def_stmt = conn
            .prepare(
                "MERGE (d:Def {id: $id}) SET d.name = $name, d.kind = $kind, \
                 d.file = $file, d.start_line = $sl, d.end_line = $el",
            )
            .map_err(|e| anyhow::anyhow!("lbug prepare def: {e}"))?;
        let mut defines_stmt = conn
            .prepare("MATCH (f:File {path: $file}), (d:Def {id: $id}) MERGE (f)-[:Defines]->(d)")
            .map_err(|e| anyhow::anyhow!("lbug prepare defines: {e}"))?;
        let mut calls_stmt = conn
            .prepare("MATCH (a:Def {id: $from}), (b:Def {id: $to}) MERGE (a)-[:Calls]->(b)")
            .map_err(|e| anyhow::anyhow!("lbug prepare calls: {e}"))?;
        let mut imports_stmt = conn
            .prepare("MATCH (a:File {path: $from}), (b:File {path: $to}) MERGE (a)-[:Imports]->(b)")
            .map_err(|e| anyhow::anyhow!("lbug prepare imports: {e}"))?;

        for node in &batch.nodes {
            match node.kind {
                NodeKind::File => {
                    conn.execute(&mut file_stmt, vec![("path", Value::String(node.id.clone()))])
                        .map_err(|e| anyhow::anyhow!("lbug insert file `{}`: {e}", node.id))?;
                }
                NodeKind::Definition => {
                    let kind = node.symbol_kind.map(|k| format!("{k:?}")).unwrap_or_default();
                    conn.execute(
                        &mut def_stmt,
                        vec![
                            ("id", Value::String(node.id.clone())),
                            ("name", Value::String(node.name.clone())),
                            ("kind", Value::String(kind)),
                            ("file", Value::String(node.file.clone())),
                            ("sl", Value::Int64(node.start_line as i64)),
                            ("el", Value::Int64(node.end_line as i64)),
                        ],
                    )
                    .map_err(|e| anyhow::anyhow!("lbug insert def `{}`: {e}", node.id))?;
                }
            }
        }

        for edge in &batch.edges {
            match edge.kind {
                EdgeKind::Defines => {
                    conn.execute(
                        &mut defines_stmt,
                        vec![
                            ("file", Value::String(edge.from.clone())),
                            ("id", Value::String(edge.to.clone())),
                        ],
                    )
                    .map_err(|e| anyhow::anyhow!("lbug insert defines: {e}"))?;
                }
                EdgeKind::Calls => {
                    conn.execute(
                        &mut calls_stmt,
                        vec![
                            ("from", Value::String(edge.from.clone())),
                            ("to", Value::String(edge.to.clone())),
                        ],
                    )
                    .map_err(|e| anyhow::anyhow!("lbug insert calls: {e}"))?;
                }
                EdgeKind::Imports => {
                    conn.execute(
                        &mut imports_stmt,
                        vec![
                            ("from", Value::String(edge.from.clone())),
                            ("to", Value::String(edge.to.clone())),
                        ],
                    )
                    .map_err(|e| anyhow::anyhow!("lbug insert imports: {e}"))?;
                }
            }
        }

        conn.query("COMMIT")
            .map_err(|e| anyhow::anyhow!("lbug commit: {e}"))?;
        Ok(())
    }

    fn remove_file(&mut self, rel_path: &str) -> anyhow::Result<()> {
        let conn = self.connect()?;
        let mut del_defs = conn
            .prepare("MATCH (:File {path: $path})-[:Defines]->(d:Def) DETACH DELETE d")
            .map_err(|e| anyhow::anyhow!("lbug prepare del defs: {e}"))?;
        conn.execute(&mut del_defs, vec![("path", Value::String(rel_path.to_string()))])
            .map_err(|e| anyhow::anyhow!("lbug del defs: {e}"))?;
        let mut del_file = conn
            .prepare("MATCH (f:File {path: $path}) DETACH DELETE f")
            .map_err(|e| anyhow::anyhow!("lbug prepare del file: {e}"))?;
        conn.execute(&mut del_file, vec![("path", Value::String(rel_path.to_string()))])
            .map_err(|e| anyhow::anyhow!("lbug del file: {e}"))?;
        Ok(())
    }
}

fn row_to_hit(row: Vec<Value>) -> Option<DefHit> {
    let mut it = row.into_iter();
    let name = match it.next()? {
        Value::String(s) => s,
        _ => return None,
    };
    let file = match it.next()? {
        Value::String(s) => s,
        _ => return None,
    };
    let start_line = match it.next()? {
        Value::Int64(n) => n,
        _ => 0,
    };
    Some(DefHit {
        name,
        file,
        start_line,
    })
}

/// CSV-quote a field (wrap in quotes, double internal quotes) for `COPY`.
fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn as_string(v: Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s),
        _ => None,
    }
}

fn row_to_hit_score(row: Vec<Value>) -> Option<(DefHit, f32)> {
    let mut it = row.into_iter();
    let name = match it.next()? {
        Value::String(s) => s,
        _ => return None,
    };
    let file = match it.next()? {
        Value::String(s) => s,
        _ => return None,
    };
    let start_line = match it.next()? {
        Value::Int64(n) => n,
        _ => 0,
    };
    let sim = match it.next()? {
        Value::Float(f) => f,
        Value::Double(f) => f as f32,
        _ => 0.0,
    };
    Some((
        DefHit {
            name,
            file,
            start_line,
        },
        sim,
    ))
}
