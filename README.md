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
- [ ] **Slice 2b — LadybugDB store.** Persist the batch via `lbug`; extract
      `CALLS`/`IMPORTS` edges; Cypher-backed `who_calls` / `call_chain`.
      *(needs `brew install cmake`)*
- [ ] **Slice 3 — semantic.** Embed symbols with fastembed → Kùzu HNSW;
      hybrid query (vector recall → graph expansion).
- [ ] **Slice 4 — incremental.** `notify` watcher patches the graph in-place.
- [ ] **Slice 5 — MCP server.** Expose `search`, `who_calls`, `call_chain`,
      `definition`, `neighbors`, `impact_of` over rmcp/stdio.

## Build

```bash
cargo build --release
./target/release/codegraph index .          # summary
./target/release/codegraph index . --json    # every symbol as JSON
```

Prerequisites for the upcoming graph/semantic slices:

- `cmake` (Kùzu compiles a C++ core) — `brew install cmake`
- First run downloads the embedding model once, then runs fully offline.

## License

MIT
