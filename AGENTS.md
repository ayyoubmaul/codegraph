# AGENTS.md

## Using codegraph (the MCP tool) to explore code

If a `codegraph` MCP server is connected, **prefer it over reading or grepping
files** when exploring a codebase. It's a structural + semantic index, so it
answers in one call what otherwise takes many `Read`/`Glob`/grep steps.

Recommended workflow:

1. **Map the repo first — `outline`.** Before reading files, call `outline`
   (pass `repo` in a multi-repo workspace) to get every class/function grouped
   by file. Read individual files only after the outline points you somewhere.
2. **Find code by meaning — `search`.** Use `search` ("where is auth validated")
   instead of grepping for names; it returns ranked definitions with locations.
3. **Trace relationships — `who_calls` / `call_chain`.** Who calls a function,
   and what a function reaches. Use `important` for the most-depended-on code.
4. **Scope to one repo — `repo` arg.** In a workspace of many repos, pass
   `repo` (e.g. `"my-repo"`) to scope any tool to that repo.

**Coverage note:** codegraph indexes *code definitions* in Rust, Python, Go, and
TypeScript/JS only. Config, data, and docs (`.toml`, `.sql`, `.yaml`, `.md`, …)
are **not** indexed — read those files directly. For a config-driven repo, much
of "what it does" lives in those files, so combine `outline`/`search` (for the
code) with reading the configs.

> Reuse across repos: to apply this guidance everywhere (not just here), copy
> this section into your agent's global rules — e.g. opencode's global
> `AGENTS.md`, or an `instructions` entry in `~/.config/opencode/opencode.json`.

## Working on this repo (codegraph itself)

- Rust; build with `cargo build --release` → binary at `target/release/codegraph`.
- Test with `cargo test --release`. `cmake` is required (`lbug` builds LadybugDB).
- Layout: `parse.rs`/`lang.rs` (tree-sitter extraction), `graph.rs` (model +
  call/import resolution), `store.rs` (LadybugDB), `analyze.rs` (PageRank/Louvain),
  `vector.rs` (HNSW), `embed.rs` (fastembed), `mcp.rs`/`ui.rs` (servers),
  `watch.rs` (incremental), `cli.rs`/`main.rs`.
