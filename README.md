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
- [x] **Slice 3 — semantic (flagship).** `fastembed` (local, offline) embeddings
      stored as `FLOAT[384]` on `Def`; `search <text>` runs brute-force cosine
      KNN via the built-in `array_cosine_similarity` — **no extension, fully
      offline, one engine**. Verified on sieve (finds code by meaning).
      *(HNSW extension = scale-only; auto vector→graph "hybrid" combine = next.)*
- [x] **Slice 4 — incremental.** `watch <path> --db` (`notify`): re-parses only
      the changed file, rebuilds resolution in memory, and rewrites just the
      sub-graph incident to it (incoming + outgoing edges) — never a full
      reindex. Verified live: adding a function updates `who-calls` instantly.
      *(per-change in-memory rebuild + event debouncing: optimize later.)*
- [x] **Slice 5 — graph intelligence.** `analyze` computes **PageRank**
      importance + **Louvain** communities in Rust over the call graph (the
      `algo` extension loads over the network → would break offline), storing
      `pagerank`/`community` on each `Def`. `important` + `communities` commands.
      Verified on sieve (recovered the auth/cache/proxy/semantic-cache modules).
- [x] **Slice 6 — MCP server.** `serve --db` exposes `search`, `who_calls`,
      `call_chain`, `important` over rmcp/stdio. Verified with a full JSON-RPC
      session (initialize → tools/list → tools/call). *(communities/impact tools
      are easy follow-ons.)*
- [x] **Slice 7 — web UI.** `ui --db` serves a **no-Docker, offline** browser UI
      (page + JS embedded in the binary): a force-directed call graph colored by
      Louvain community, sized by PageRank, with a semantic-search highlight and
      click-to-see callers/callees. Verified: serves the page + `/api/graph`
      (185 nodes / 364 edges, each with community + pagerank).
- [x] **Slice 8 — live serve.** `serve --watch <repo>` / `ui --watch <repo>`:
      one process indexes, watches, and serves — a background thread patches the
      same in-process store the server queries (no second process, no lock
      conflict). Verified live: an edit propagated to `/api/graph` (2→3 nodes)
      with no restart.
- [x] **Slice 9 — scale.** (a) walker prunes heavy dirs (node_modules/target/
      dist/…) + skips files > 512 KB; (b) a fresh index bulk-loads via CSV `COPY`
      instead of per-row `MERGE`; (c) `--embed` caches by def id, so re-index
      skips already-embedded defs (full re-embed → instant). Index a big monorepo
      or a parent folder of repos without the file/embedding count exploding.
- [x] **Slice 10 — HNSW vector search.** The MCP/UI servers build an in-memory
      HNSW index (pure-Rust `hnsw_rs`) from the stored embeddings at startup, so
      semantic search is ~O(log n) instead of brute-force O(n); results join
      metadata back from the graph DB, and the live watcher adds new vectors.
      Falls back to brute-force when no index. Verified: HNSW results match
      brute-force on sieve. (CLI one-shot `search` stays brute-force.)
- [x] **Slice 11 — workspace / multi-repo.** `index <repoA> <repoB> … --db ws.db`
      indexes several repos into one graph, path-qualified by repo
      (`repoA/src/…`) and resolved together so **cross-repo call edges** form;
      queries span the workspace and show `repo/file:line`. Verified: a call in
      repo B to a function defined in repo A links across.
- [x] **Slice 12 — scope-aware resolution.** Ambiguous names resolve locally
      (same-file → imported → globally-unique → same-repo, capped); only unique
      names cross repos. Killed the cross-repo false-edge noise a token test
      exposed at workspace scale.
- [x] **Slice 13 — type-aware resolution.** Methods carry their owner type
      (Rust `impl` / Go receiver / TS+JS+Python `class`). Method calls resolve by
      **receiver type**: `self`/`this` → enclosing type; typed params + Go
      receivers → the param/receiver type; local `let`/`:=`/`new` constructors →
      the inferred type; qualified `Type::new()` → the qualifier. Verified across
      Rust/Go/TS fixtures (two types sharing a method name → the right one wins).
      Complete for the static languages; Python stays name-based for dynamic cases.

## Build & use

```bash
cargo build --release
B=./target/release/codegraph

$B index <repo> --db graph.db --embed         # parse → graph → embeddings
$B index <repoA> <repoB> --db ws.db --embed   # multi-repo workspace (cross-repo edges)
$B analyze --db graph.db                       # PageRank + Louvain communities
$B search "rate limiting logic" --db graph.db  # find by meaning
$B who-calls parseAuth --db graph.db
$B call-chain handleRequest --db graph.db --depth 3
$B important --db graph.db                      # most-depended-on code
$B communities --db graph.db                    # module clusters
$B watch <repo> --db graph.db                   # keep fresh as you edit
$B ui --db graph.db                             # explore in a browser → http://127.0.0.1:7700
```

## Use from an AI agent (MCP)

Point any MCP client at `codegraph serve`. For Claude Code:

```bash
claude mcp add codegraph -- /abs/path/to/codegraph serve --db /abs/path/to/graph.db
```

Tools exposed: `search` (by meaning), `who_calls`, `call_chain`, `important`.

For a **live** index that updates as you edit, add `--watch <repo>` (and
`--embed` to keep semantic search fresh) — one process serves *and* watches:

```bash
claude mcp add codegraph -- /abs/codegraph serve --db /abs/graph.db --watch /abs/repo --embed
```

## Prerequisites

- `cmake` (`lbug` compiles LadybugDB's C++ core) — `brew install cmake`
- `search`/`--embed` download the embedding model once, then run fully offline.

## License

MIT
