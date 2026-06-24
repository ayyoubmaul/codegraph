# codegraph

**Structural + semantic codebase memory for AI coding agents — over MCP.**

Indexes a codebase into an embedded, on-disk knowledge graph and exposes it to
AI agents via the Model Context Protocol, so an agent can ask *structural* and
*semantic* questions about your code in milliseconds instead of grepping
file-by-file.

## Thesis

Inspired by [`a structural-only index`](https://github.com/upstream/a structural-only index),
but it deliberately beats three of its tradeoffs:

> **Everything a structural-only index does structurally — _plus_ retrieval by
> meaning, that _never goes stale_, on-disk in _low RAM_, fully offline, one
> cross-platform binary.**

| a structural-only index | codegraph |
| --- | --- |
| Structural only (no semantic search) | **Hybrid**: structural graph **+** local semantic embeddings |
| Batch reindex → goes stale | **Incremental**: file-watch, re-parse only what changed |
| In-memory SQLite (RAM-bound) | **On-disk** embedded graph (low, predictable RAM) |
| C (fast, but unsafe + hard to extend) | **Rust** (same speed class, memory-safe, easy to extend) |

## Stack (locked)

| Concern | Choice | Why |
| --- | --- | --- |
| Language | **Rust** | C-class speed + memory safety; native crates (no cgo) → clean static cross-compile to mac/Linux/Windows |
| Graph + vector store | **LadybugDB** (`lbug`), embedded | MIT-licensed, actively-maintained Kùzu successor: on-disk columnar property-graph, Cypher, native vector + full-text index — graph **and** embeddings in one engine. Accessed behind a `Store` trait, so the FFI boundary is isolated and the backend stays swappable (fallback: SQLite + sqlite-vec). |
| Parsing | **tree-sitter** | Fast incremental parsing; cheap to add languages |
| Embeddings | **fastembed** (ONNX, CPU) | Local, offline, no API keys |
| Incremental | **notify** + content hashing | Re-parse only changed files |
| MCP server | **rmcp** (official Rust SDK) | stdio tools agents connect to |
| CLI | **clap** | `index` · `serve` · `watch` |

## Status

Vertical slices, each one builds and runs:

- [x] **Slice 1 — parse pipeline.** gitignore-aware walk → tree-sitter →
      symbol extraction, parallelized with rayon. Languages: Rust, Python,
      Go, TypeScript/JS. `codegraph index <path>`.
- [x] **Slice 2a — graph model + store seam.** `GraphBatch` (nodes/edges) +
      `Store` trait. `index` now reports node/edge counts. *(cmake-free)*
- [x] **Slice 2b — LadybugDB store.** `LadybugStore` persists the batch via
      `lbug` (schema + `MERGE` writes in a transaction) with a Cypher count
      read-back. `index --db <path>`; idempotent, on-disk.
- [x] **Slice 2c — call edges + queries.** Extract `Calls` edges (name-based
      resolution, same-file preferred) and Cypher-backed `who-calls` /
      `call-chain` commands. *(`Imports` edges + type-aware resolution: later.)*
- [x] **Slice 2d — imports + sharper resolution.** `Imports` edges (relative
      JS/TS resolution) + receiver-aware, import-scoped call resolution
      (same-file → imported → repo-wide). *(Go/Rust module-path imports and true
      type inference still future.)*
- [ ] **Slice 3 — semantic (flagship of the bet).** Embed symbols with
      `fastembed` (local, offline) → LadybugDB `vector` extension (HNSW); hybrid
      query: vector recall by *meaning* → structural graph expansion.
- [ ] **Slice 4 — incremental.** `notify` watcher re-parses only changed files
      and patches the graph in-place (never stale).
- [ ] **Slice 5 — graph intelligence.** Community detection (**Louvain**) +
      importance (**PageRank**) via LadybugDB's `algo` extension; store a
      `community` / `rank` on each `Def`. Surfaces module clusters,
      architectural boundaries, and most-depended-on code.
- [ ] **Slice 6 — MCP server.** Expose `search`, `who-calls`, `call-chain`,
      `definition`, `neighbors`, `impact-of`, `communities` over rmcp/stdio.

## Build

```bash
cargo build --release
./target/release/codegraph index .          # summary
./target/release/codegraph index . --json    # every symbol as JSON
```

Prerequisites:

- `cmake` (`lbug` compiles LadybugDB's C++ core) — `brew install cmake` ✓ installed
- The semantic slice downloads the embedding model once, then runs fully offline.

## License

MIT
