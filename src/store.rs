//! LadybugDB-backed implementation of the graph [`Store`], plus structural
//! queries (`who_calls`, `call_chain`).
//!
//! Holds a `Database` and opens a short-lived `Connection` per operation: a
//! `Connection<'a>` borrows the `Database`, so storing both in one struct would
//! be self-referential. Writes run inside a single transaction using prepared
//! statements (LadybugDB serializes writers — we parallelize the parse stage,
//! batch the write stage).

use std::path::Path;
use std::sync::Mutex;

use lbug::{Connection, Database, LogicalType, SystemConfig, Value};

use crate::graph::{EdgeKind, GraphBatch, NodeKind, Store};

/// Per-query wall-clock cap for *agent-facing* read queries (search, who_calls,
/// call_chain, important). Bounds a pathological query so an MCP/UI tool call
/// fails fast instead of hanging the agent. Background reads (analyze, vector
/// build) use the untimed [`LadybugStore::connect`].
const READ_TIMEOUT_MS: u64 = 10_000;

pub struct LadybugStore {
    db: Database,
    /// Serializes *writers* among themselves (LadybugDB allows a single writer
    /// at a time). Readers never take this — they run concurrently against the
    /// shared `Database`, which gives them committed-snapshot reads. This is the
    /// whole point of dropping the old `Arc<Mutex<LadybugStore>>`: a long write
    /// (initial index, batch embed, reanalyze) no longer blocks queries.
    write_lock: Mutex<()>,
}

/// A definition row returned by a query.
#[derive(Debug, Clone)]
pub struct DefHit {
    pub name: String,
    pub file: String,
    pub start_line: i64,
}

/// One definition in a structural outline (`file`-ordered).
#[derive(Debug, Clone)]
pub struct OutlineRow {
    pub file: String,
    pub kind: String,
    pub name: String,
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
        let store = Self {
            db,
            write_lock: Mutex::new(()),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn connect(&self) -> anyhow::Result<Connection<'_>> {
        Connection::new(&self.db).map_err(|e| anyhow::anyhow!("lbug connect: {e}"))
    }

    /// A connection for an agent-facing read query, capped at [`READ_TIMEOUT_MS`]
    /// so a slow/pathological query returns an error rather than hanging.
    fn read_conn(&self) -> anyhow::Result<Connection<'_>> {
        let conn = self.connect()?;
        conn.set_query_timeout(READ_TIMEOUT_MS);
        Ok(conn)
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
    pub fn who_calls(&self, name: &str, repo: Option<&str>) -> anyhow::Result<Vec<DefHit>> {
        let conn = self.read_conn()?;
        let mut stmt = conn
            .prepare(
                "MATCH (caller:Def)-[:Calls]->(callee:Def {name: $name}) \
                 WHERE caller.file STARTS WITH $prefix \
                 RETURN DISTINCT caller.name, caller.file, caller.start_line \
                 ORDER BY caller.file, caller.start_line",
            )
            .map_err(|e| anyhow::anyhow!("lbug prepare who_calls: {e}"))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![
                    ("name", Value::String(name.to_string())),
                    ("prefix", Value::String(repo_prefix(repo))),
                ],
            )
            .map_err(|e| anyhow::anyhow!("lbug who_calls: {e}"))?;
        Ok(result.filter_map(row_to_hit).collect())
    }

    /// Definitions transitively reachable from `name` via `Calls`, up to `depth`
    /// hops (clamped to 1..=10).
    pub fn call_chain(&self, name: &str, depth: u8, repo: Option<&str>) -> anyhow::Result<Vec<DefHit>> {
        let depth = depth.clamp(1, 10);
        let conn = self.read_conn()?;
        // `depth` is a validated integer, safe to interpolate; `name` is a param.
        let query = format!(
            "MATCH (:Def {{name: $name}})-[:Calls*1..{depth}]->(d:Def) \
             WHERE d.file STARTS WITH $prefix \
             RETURN DISTINCT d.name, d.file, d.start_line \
             ORDER BY d.file, d.start_line"
        );
        let mut stmt = conn
            .prepare(&query)
            .map_err(|e| anyhow::anyhow!("lbug prepare call_chain: {e}"))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![
                    ("name", Value::String(name.to_string())),
                    ("prefix", Value::String(repo_prefix(repo))),
                ],
            )
            .map_err(|e| anyhow::anyhow!("lbug call_chain: {e}"))?;
        Ok(result.filter_map(row_to_hit).collect())
    }

    /// Store an embedding vector for each `(def_id, vector)` pair.
    pub fn set_embeddings(&self, items: &[(String, Vec<f32>)]) -> anyhow::Result<()> {
        let _w = self.write_lock.lock().unwrap();
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
    pub fn semantic_search(
        &self,
        query: &[f32],
        k: usize,
        repo: Option<&str>,
    ) -> anyhow::Result<Vec<(DefHit, f32)>> {
        let k = k.clamp(1, 100);
        let conn = self.read_conn()?;
        let q = Value::Array(
            LogicalType::Float,
            query.iter().map(|f| Value::Float(*f)).collect(),
        );
        // With a repo prefix this scans only that repo's embedded defs, so the
        // brute-force cosine stays cheap even on a big multi-repo workspace.
        let mut stmt = conn
            .prepare(&format!(
                "MATCH (d:Def) WHERE d.embedding IS NOT NULL AND d.file STARTS WITH $prefix \
                 RETURN d.name, d.file, d.start_line, \
                 array_cosine_similarity(d.embedding, $q) AS sim \
                 ORDER BY sim DESC LIMIT {k}"
            ))
            .map_err(|e| anyhow::anyhow!("lbug prepare semantic_search: {e}"))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("q", q), ("prefix", Value::String(repo_prefix(repo)))],
            )
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
    pub fn set_analysis(&self, items: &[(String, f64, i64)]) -> anyhow::Result<()> {
        let _w = self.write_lock.lock().unwrap();
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
    pub fn top_important(
        &self,
        k: usize,
        repo: Option<&str>,
    ) -> anyhow::Result<Vec<(DefHit, f32)>> {
        let k = k.clamp(1, 200);
        let conn = self.read_conn()?;
        let mut stmt = conn
            .prepare(&format!(
                "MATCH (d:Def) WHERE d.pagerank IS NOT NULL AND d.file STARTS WITH $prefix \
                 RETURN d.name, d.file, d.start_line, d.pagerank \
                 ORDER BY d.pagerank DESC LIMIT {k}"
            ))
            .map_err(|e| anyhow::anyhow!("lbug prepare top_important: {e}"))?;
        let result = conn
            .execute(&mut stmt, vec![("prefix", Value::String(repo_prefix(repo)))])
            .map_err(|e| anyhow::anyhow!("lbug top_important: {e}"))?;
        Ok(result.filter_map(row_to_hit_score).collect())
    }

    /// The repos currently indexed (first path segment of each def's file),
    /// each with its definition count, ordered by count desc. Lets an agent see
    /// what's queryable — the index spans the whole workspace regardless of any
    /// client's working directory.
    pub fn repos(&self) -> anyhow::Result<Vec<(String, u64)>> {
        let conn = self.connect()?;
        let result = conn
            .query("MATCH (d:Def) RETURN d.file, count(*)")
            .map_err(|e| anyhow::anyhow!("lbug repos: {e}"))?;
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for row in result {
            let mut it = row.into_iter();
            let Some(Value::String(file)) = it.next() else {
                continue;
            };
            let c = match it.next() {
                Some(Value::Int64(n)) => n.max(0) as u64,
                _ => 0,
            };
            let repo = file.split('/').next().unwrap_or("").to_string();
            *counts.entry(repo).or_default() += c;
        }
        let mut out: Vec<(String, u64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }

    /// A structural outline: every definition (class/function/…) in scope,
    /// ordered by file then line, capped at `limit`. Optionally scoped to one
    /// repo. Lets an agent see a repo's shape in one call instead of reading
    /// every directory.
    pub fn outline(&self, repo: Option<&str>, limit: usize) -> anyhow::Result<Vec<OutlineRow>> {
        let limit = limit.clamp(1, 5000);
        let conn = self.read_conn()?;
        let mut stmt = conn
            .prepare(&format!(
                "MATCH (d:Def) WHERE d.file STARTS WITH $prefix \
                 RETURN d.file, d.kind, d.name, d.start_line \
                 ORDER BY d.file, d.start_line LIMIT {limit}"
            ))
            .map_err(|e| anyhow::anyhow!("lbug prepare outline: {e}"))?;
        let result = conn
            .execute(&mut stmt, vec![("prefix", Value::String(repo_prefix(repo)))])
            .map_err(|e| anyhow::anyhow!("lbug outline: {e}"))?;
        Ok(result
            .filter_map(|row| {
                let mut it = row.into_iter();
                let file = as_string(it.next()?)?;
                let kind = as_string(it.next()?)?;
                let name = as_string(it.next()?)?;
                let start_line = match it.next()? {
                    Value::Int64(n) => n,
                    _ => 0,
                };
                Some(OutlineRow {
                    file,
                    kind,
                    name,
                    start_line,
                })
            })
            .collect())
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
        let _w = self.write_lock.lock().unwrap();
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

    /// Every stored embedding as `(def_id, vector)` — used to build the HNSW
    /// vector index in memory.
    pub fn all_embeddings(&self) -> anyhow::Result<Vec<(String, Vec<f32>)>> {
        let conn = self.connect()?;
        let result = conn
            .query("MATCH (d:Def) WHERE d.embedding IS NOT NULL RETURN d.id, d.embedding")
            .map_err(|e| anyhow::anyhow!("lbug all_embeddings: {e}"))?;
        Ok(result
            .filter_map(|row| {
                let mut it = row.into_iter();
                let id = match it.next()? {
                    Value::String(s) => s,
                    _ => return None,
                };
                let vec = match it.next()? {
                    Value::Array(_, elems) | Value::List(_, elems) => elems
                        .into_iter()
                        .filter_map(|v| match v {
                            Value::Float(f) => Some(f),
                            Value::Double(d) => Some(d as f32),
                            _ => None,
                        })
                        .collect(),
                    _ => return None,
                };
                Some((id, vec))
            })
            .collect())
    }

    /// Fetch metadata for a set of def ids (for joining HNSW results).
    pub fn def_hits_by_ids(
        &self,
        ids: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, DefHit>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let conn = self.read_conn()?;
        let list = Value::List(
            LogicalType::String,
            ids.iter().map(|s| Value::String(s.clone())).collect(),
        );
        let mut stmt = conn
            .prepare("MATCH (d:Def) WHERE d.id IN $ids RETURN d.id, d.name, d.file, d.start_line")
            .map_err(|e| anyhow::anyhow!("lbug prepare def_hits_by_ids: {e}"))?;
        let result = conn
            .execute(&mut stmt, vec![("ids", list)])
            .map_err(|e| anyhow::anyhow!("lbug def_hits_by_ids: {e}"))?;
        Ok(result
            .filter_map(|row| {
                let mut it = row.into_iter();
                let id = match it.next()? {
                    Value::String(s) => s,
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
                Some((
                    id,
                    DefHit {
                        name,
                        file,
                        start_line,
                    },
                ))
            })
            .collect())
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
    fn init_schema(&self) -> anyhow::Result<()> {
        let _w = self.write_lock.lock().unwrap();
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

    fn write_batch(&self, batch: &GraphBatch) -> anyhow::Result<()> {
        let _w = self.write_lock.lock().unwrap();
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

    fn remove_file(&self, rel_path: &str) -> anyhow::Result<()> {
        let _w = self.write_lock.lock().unwrap();
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

/// Path prefix for a repo-scoped query. `None` → `""` (every string starts with
/// the empty string, so the filter matches all defs). `Some(repo)` → `"repo/"`;
/// the trailing slash keeps `api` from also matching `api-client`. Works
/// for sub-paths too (`Some("my-repo/pkg")` → `"my-repo/pkg/"`).
fn repo_prefix(repo: Option<&str>) -> String {
    match repo {
        None => String::new(),
        Some(r) => {
            let r = r.trim_matches('/');
            if r.is_empty() {
                String::new()
            } else {
                format!("{r}/")
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, GraphBatch, Node, NodeKind};
    use crate::symbol::SymbolKind;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// A batch of `n` defs in one file, every def calling `fn0` — so
    /// `who_calls("fn0")` returns `n - 1`.
    fn fixture(n: usize) -> (GraphBatch, Vec<String>) {
        let file = "repo/a.rs".to_string();
        let mut nodes = vec![Node {
            id: file.clone(),
            kind: NodeKind::File,
            name: file.clone(),
            file: file.clone(),
            symbol_kind: None,
            start_line: 0,
            end_line: 0,
        }];
        let mut edges = Vec::new();
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let name = format!("fn{i}");
            let id = GraphBatch::def_id(&file, &name, i);
            ids.push(id.clone());
            nodes.push(Node {
                id: id.clone(),
                kind: NodeKind::Definition,
                name,
                file: file.clone(),
                symbol_kind: Some(SymbolKind::Function),
                start_line: i,
                end_line: i,
            });
            edges.push(Edge { from: file.clone(), to: id.clone(), kind: EdgeKind::Defines });
        }
        let callee = ids[0].clone();
        for id in ids.iter().skip(1) {
            edges.push(Edge { from: id.clone(), to: callee.clone(), kind: EdgeKind::Calls });
        }
        (GraphBatch { nodes, edges }, ids)
    }

    /// The core guarantee of the lock-free-reads design: while a writer hammers
    /// the store, reads keep returning promptly and consistently — they are not
    /// serialized behind the writer. Regression guard for the MCP timeout issue.
    #[test]
    fn reads_stay_responsive_under_concurrent_writes() {
        let n = 400;
        let (batch, ids) = fixture(n);
        let dir = std::env::temp_dir().join(format!(
            "codegraph-test-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let store = Arc::new(LadybugStore::open(&dir).expect("open db"));
        store.write_batch(&batch).expect("seed write");

        let analysis: Vec<(String, f64, i64)> =
            ids.iter().map(|id| (id.clone(), 1.0 / n as f64, 0)).collect();

        // Writer thread: repeated set_analysis + write_batch, each taking the
        // internal write lock. ~20 iterations keeps the test ~1–2s.
        let done = Arc::new(AtomicBool::new(false));
        let (w_store, w_done, w_batch, w_analysis) =
            (store.clone(), done.clone(), batch, analysis);
        let writer = std::thread::spawn(move || {
            for _ in 0..20 {
                w_store.set_analysis(&w_analysis).expect("set_analysis");
                w_store.write_batch(&w_batch).expect("write_batch");
            }
            w_done.store(true, Ordering::SeqCst);
        });

        // Reader: query repeatedly until the writer finishes, recording how many
        // reads landed *while the writer was still running* and the slowest one.
        let mut reads_during_write = 0usize;
        let mut max_latency = Duration::ZERO;
        let mut total_reads = 0usize;
        while !done.load(Ordering::SeqCst) {
            let t = Instant::now();
            let callers = store.who_calls("fn0", None).expect("who_calls");
            let count = store.def_count().expect("def_count");
            let dt = t.elapsed();
            max_latency = max_latency.max(dt);
            assert_eq!(callers.len(), n - 1, "every fn except fn0 calls fn0");
            assert_eq!(count, n as u64, "all defs present");
            reads_during_write += 1;
            total_reads += 1;
        }
        writer.join().expect("writer thread");
        // A few more reads after the writer is done, for good measure.
        for _ in 0..5 {
            assert_eq!(store.who_calls("fn0", None).expect("who_calls").len(), n - 1);
            total_reads += 1;
        }

        let _ = std::fs::remove_dir_all(&dir);

        // Reads overlapped the writer (not serialized behind the whole loop)...
        assert!(
            reads_during_write >= 5,
            "expected reads to interleave with writes, got {reads_during_write}"
        );
        // ...and no single read came anywhere near the 10s read timeout.
        assert!(
            max_latency < Duration::from_secs(5),
            "slow read under contention: {max_latency:?}"
        );
        eprintln!(
            "concurrency ok: {total_reads} reads ({reads_during_write} during writes), \
             max read latency {max_latency:?}"
        );
    }
}
