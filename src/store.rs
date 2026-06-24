//! LadybugDB-backed implementation of the graph [`Store`].
//!
//! Holds a `Database` and opens a short-lived `Connection` per operation: a
//! `Connection<'a>` borrows the `Database`, so storing both in one struct would
//! be self-referential. Writes run inside a single transaction using prepared
//! statements (LadybugDB serializes writers — the parse stage is what we
//! parallelize, the write stage is batched).

use std::path::Path;

use lbug::{Connection, Database, SystemConfig, Value};

use crate::graph::{EdgeKind, GraphBatch, NodeKind, Store};

pub struct LadybugStore {
    db: Database,
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

    /// `(files, defs, defines_edges)` currently stored — used to verify a write.
    pub fn summary(&self) -> anyhow::Result<(u64, u64, u64)> {
        Ok((
            self.count("MATCH (:File) RETURN count(*)")?,
            self.count("MATCH (:Def) RETURN count(*)")?,
            self.count("MATCH (:File)-[r:Defines]->(:Def) RETURN count(r)")?,
        ))
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
             file STRING, start_line INT64, end_line INT64, PRIMARY KEY(id))",
            "CREATE REL TABLE IF NOT EXISTS Defines(FROM File TO Def)",
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
            if matches!(edge.kind, EdgeKind::Defines) {
                conn.execute(
                    &mut defines_stmt,
                    vec![
                        ("file", Value::String(edge.from.clone())),
                        ("id", Value::String(edge.to.clone())),
                    ],
                )
                .map_err(|e| anyhow::anyhow!("lbug insert defines: {e}"))?;
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
