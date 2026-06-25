//! MCP server exposing codegraph's structural + semantic queries over stdio,
//! so AI agents (Claude Code, Codex, …) can query the code graph directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use tokio::sync::Mutex;

use crate::embed::Embedder;
use crate::store::{DefHit, LadybugStore, OutlineRow};

#[derive(Clone)]
pub struct CodegraphServer {
    /// Shared without a Rust lock — reads run concurrently with the background
    /// watcher's writes (LadybugDB serializes writers internally).
    store: Arc<LadybugStore>,
    /// Dedicated to embedding *query* strings. Kept separate from the watcher's
    /// batch embedder so a long batch embed never blocks a query.
    embedder: Arc<Mutex<Option<Embedder>>>,
    vindex: crate::vector::SharedVector,
    // Consumed by the `#[tool_handler]` macro expansion.
    #[allow(dead_code)]
    tool_router: ToolRouter<CodegraphServer>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SearchArgs {
    /// Natural-language description of the code you're looking for.
    query: String,
    /// Max number of results (default 8).
    #[serde(default)]
    k: Option<usize>,
    /// Optional: restrict results to one repo in the workspace, by its name
    /// (e.g. "my-repo"). Omit to search the whole workspace.
    #[serde(default)]
    repo: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NameArgs {
    /// The exact symbol name (function/method/type).
    name: String,
    /// Optional: restrict to callers in one repo (by name, e.g. "api").
    #[serde(default)]
    repo: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct CallChainArgs {
    /// The symbol name to start from.
    name: String,
    /// Max hops to traverse (default 3).
    #[serde(default)]
    depth: Option<u8>,
    /// Optional: restrict reachable defs to one repo (by name).
    #[serde(default)]
    repo: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct TopKArgs {
    /// Number of results (default 10).
    #[serde(default)]
    k: Option<usize>,
    /// Optional: rank importance within a single repo (by name, e.g.
    /// "my-repo") instead of across the whole workspace.
    #[serde(default)]
    repo: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct IndexRepoArgs {
    /// Absolute path to a repo/directory to index into the live graph.
    path: String,
    /// Also compute embeddings for semantic search (default true).
    #[serde(default)]
    embed: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct OutlineArgs {
    /// Optional: outline just one repo in the workspace (by name).
    #[serde(default)]
    repo: Option<String>,
    /// Max definitions to list (default 300).
    #[serde(default)]
    limit: Option<usize>,
}

#[tool_router]
impl CodegraphServer {
    pub fn new(store: LadybugStore) -> anyhow::Result<Self> {
        let vindex = crate::vector::build_from_store(&store)?;
        let embedder = Arc::new(Mutex::new(None));
        crate::embed::warm(embedder.clone());
        Ok(Self {
            store: Arc::new(store),
            embedder,
            vindex: Arc::new(tokio::sync::RwLock::new(vindex)),
            tool_router: Self::tool_router(),
        })
    }

    /// Build from an already-shared store + embedder + vector index, so a
    /// background watcher can patch the same state this server queries.
    pub fn with_shared(
        store: Arc<LadybugStore>,
        embedder: Arc<Mutex<Option<Embedder>>>,
        vindex: crate::vector::SharedVector,
    ) -> Self {
        crate::embed::warm(embedder.clone());
        Self {
            store,
            embedder,
            vindex,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Semantic search: find definitions by meaning, not just name (e.g. 'rate limiting logic'). Returns ranked name, location, and similarity. Pass `repo` to scope to one repo in the workspace."
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let query_vec = {
            let mut guard = self.embedder.lock().await;
            if guard.is_none() {
                *guard = Some(Embedder::new().map_err(internal)?);
            }
            guard
                .as_mut()
                .unwrap()
                .embed_one(&args.query)
                .map_err(internal)?
        };
        let hits = {
            let vindex = self.vindex.read().await;
            crate::vector::hybrid_search(
                &self.store,
                vindex.as_ref(),
                &query_vec,
                args.k.unwrap_or(8),
                args.repo.as_deref(),
            )
            .map_err(internal)?
        };
        let mut out = String::new();
        for (h, sim) in &hits {
            out.push_str(&format!("{sim:.3}  {}  {}:{}\n", h.name, h.file, h.start_line));
        }
        if out.is_empty() {
            out.push_str("no results (is the database indexed with `--embed`?)");
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Who calls this symbol? Direct callers of the named definition.")]
    async fn who_calls(
        &self,
        Parameters(args): Parameters<NameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let hits = self
            .store
            .who_calls(&args.name, args.repo.as_deref())
            .map_err(internal)?;
        Ok(CallToolResult::success(vec![Content::text(format_hits(
            &hits, &args.name, "callers",
        ))]))
    }

    #[tool(
        description = "Call chain: definitions transitively reachable from a symbol via calls (what it ends up invoking)."
    )]
    async fn call_chain(
        &self,
        Parameters(args): Parameters<CallChainArgs>,
    ) -> Result<CallToolResult, McpError> {
        let hits = self
            .store
            .call_chain(&args.name, args.depth.unwrap_or(3), args.repo.as_deref())
            .map_err(internal)?;
        Ok(CallToolResult::success(vec![Content::text(format_hits(
            &hits, &args.name, "reachable",
        ))]))
    }

    #[tool(
        description = "Most important (most-depended-on) definitions by PageRank. Requires `analyze` to have been run on the database. Pass `repo` to rank within one repo instead of the whole workspace."
    )]
    async fn important(
        &self,
        Parameters(args): Parameters<TopKArgs>,
    ) -> Result<CallToolResult, McpError> {
        let hits = self
            .store
            .top_important(args.k.unwrap_or(10), args.repo.as_deref())
            .map_err(internal)?;
        let mut out = String::new();
        for (h, pr) in &hits {
            out.push_str(&format!("{pr:.4}  {}  {}:{}\n", h.name, h.file, h.start_line));
        }
        if out.is_empty() {
            out.push_str("no analysis yet (run `analyze` first)");
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "List the repos indexed in this codegraph workspace, with definition counts. The index spans the whole workspace regardless of your current working directory — call this to check whether a repo is queryable BEFORE treating it as external (e.g. before fetching from GitHub or grepping)."
    )]
    async fn repos(&self) -> Result<CallToolResult, McpError> {
        let rows = self.store.repos().map_err(internal)?;
        let mut out = String::new();
        for (repo, n) in &rows {
            out.push_str(&format!("{repo}  ({n} defs)\n"));
        }
        if out.is_empty() {
            out.push_str("no repos indexed yet (index a workspace with `index`)");
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Index a repo into the live graph on demand — for a repo that ISN'T in the workspace yet (e.g. a freshly cloned dependency). Give an absolute path; afterwards query it like any other repo via repo=<name>. This is the only way to add a repo while the server is running (a separate `codegraph index` process can't — single-writer lock)."
    )]
    async fn index_repo(
        &self,
        Parameters(args): Parameters<IndexRepoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = std::path::PathBuf::from(&args.path);
        if !path.is_dir() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "not a directory: {} — give an absolute path to a repo.",
                args.path
            ))]));
        }
        let repo = crate::walk::repo_name(&path);
        let store = self.store.clone();
        let vindex = self.vindex.clone();
        let embed = args.embed.unwrap_or(true);
        // Indexing is CPU + blocking I/O — run off the async runtime.
        let res = tokio::task::spawn_blocking(move || {
            crate::watch::index_path(&path, &store, embed, Some(&vindex))
        })
        .await
        .map_err(|e| internal(anyhow::anyhow!("index task: {e}")))?;
        let (files, defs) = res.map_err(internal)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "indexed {repo}: {files} files, {defs} defs — now queryable with \
             repo=\"{repo}\" (important/PageRank fills in on the next analyze)."
        ))]))
    }

    #[tool(
        description = "Structural outline: every class/function grouped by file, in file order. Use this to map a repo's shape in one call instead of reading files one by one. Pass `repo` to scope to one repo, and `limit` to cap the count."
    )]
    async fn outline(
        &self,
        Parameters(args): Parameters<OutlineArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(300);
        let rows = self
            .store
            .outline(args.repo.as_deref(), limit)
            .map_err(internal)?;
        let mut out = format_outline(&rows);
        if rows.len() >= limit {
            out.push_str(&format!(
                "\n… truncated at {limit} defs — narrow with `repo` or raise `limit`.\n"
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[tool_handler]
impl ServerHandler for CodegraphServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(
            "codegraph is a structural + semantic index of the indexed codebase(s). \
             Prefer it over reading or grepping files when exploring code:\n\
             • The index spans the WHOLE workspace of indexed repos, independent of \
             your current working directory. A repo outside your cwd may still be \
             indexed — do NOT assume it's 'external' and fetch it from GitHub/grep. \
             Call `repos` to see exactly which repos are queryable.\n\
             • Understand a repo's shape: call `outline` (it lists every \
             class/function per file in one call). Read individual files only after \
             it points you somewhere — don't walk the tree by hand.\n\
             • Find where something lives: use `search` (by meaning) instead of \
             grepping for names.\n\
             • Trace relationships: `who_calls` and `call_chain`; `important` ranks \
             the most-depended-on code.\n\
             • Multi-repo workspace: pass the optional `repo` arg (a repo name, e.g. \
             'my-repo' or 'shared-lib') to scope any tool to one repo.\n\
             Scope note: only code definitions in Rust/Python/Go/TS-JS are indexed — \
             config/data/docs (.toml, .sql, .yaml, .md, …) are not, so read those \
             files directly."
                .into(),
        );
        info
    }
}

fn internal(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// Group outline rows (already file-ordered) into a per-file structural map.
fn format_outline(rows: &[OutlineRow]) -> String {
    if rows.is_empty() {
        return "no definitions in scope (is the database indexed?)".into();
    }
    let mut out = String::new();
    let mut files = 0;
    let mut i = 0;
    while i < rows.len() {
        let file = &rows[i].file;
        let start = i;
        while i < rows.len() && &rows[i].file == file {
            i += 1;
        }
        let group = &rows[start..i];
        out.push_str(&format!("\n{file}  ({} defs)\n", group.len()));
        for r in group {
            out.push_str(&format!(
                "  {:<9} {}  :{}\n",
                r.kind.to_lowercase(),
                r.name,
                r.start_line
            ));
        }
        files += 1;
    }
    format!("{} definitions across {files} files:\n{out}", rows.len())
}

fn format_hits(hits: &[DefHit], name: &str, label: &str) -> String {
    if hits.is_empty() {
        return format!("no {label} for `{name}`");
    }
    let mut out = format!("{} {label} of `{name}`:\n", hits.len());
    for h in hits {
        out.push_str(&format!("  {}  {}:{}\n", h.name, h.file, h.start_line));
    }
    out
}

/// Open the database and serve the MCP protocol over stdio until the client
/// disconnects.
pub async fn serve(db: &Path) -> anyhow::Result<()> {
    let store = LadybugStore::open(db)?;
    let service = CodegraphServer::new(store)?.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve over stdio while a background thread watches `repo` and keeps the
/// shared store fresh. The initial index completes before serving begins.
pub async fn serve_watch(
    db: &Path,
    repos: &[PathBuf],
    embed: bool,
    reanalyze: Option<u64>,
) -> anyhow::Result<()> {
    let store: Arc<LadybugStore> = Arc::new(LadybugStore::open(db)?);
    let vindex: crate::vector::SharedVector = Arc::new(tokio::sync::RwLock::new(None));

    // Two embedders: the server embeds *query* strings, the watcher embeds
    // *definition batches*. Separate so a long batch embed (initial index of a
    // big workspace) never blocks a query embed.
    let query_embedder: Arc<Mutex<Option<Embedder>>> = Arc::new(Mutex::new(None));
    let watch_embedder: Arc<Mutex<Option<Embedder>>> = Arc::new(Mutex::new(None));

    // Index + watch on a background thread so the MCP server starts answering
    // immediately (no startup index → no handshake timeout). Tools return
    // partial results until the initial index completes, then full + live.
    let (s2, v2, repos) = (store.clone(), vindex.clone(), repos.to_vec());
    std::thread::spawn(move || {
        if let Err(e) = crate::watch::index_and_watch(&repos, s2, watch_embedder, v2, embed, reanalyze) {
            eprintln!("codegraph: index/watch stopped: {e}");
        }
    });

    let service = CodegraphServer::with_shared(store, query_embedder, vindex)
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Serve the MCP protocol over **HTTP** (streamable-http) so *multiple* clients
/// (e.g. opencode and Claude Code) can use one codegraph at once. Because
/// LadybugDB allows only a single writer process, the usual one-`serve`-process-
/// per-client (stdio) model can't share a live, watched workspace — the second
/// process can't open the locked DB. Here a single process holds the DB and
/// every connecting client gets a handle to the *same* shared store / vector
/// index / embedder, so they all see the same live index with no lock conflict.
pub async fn serve_http(
    db: &Path,
    repos: &[PathBuf],
    embed: bool,
    reanalyze: Option<u64>,
    addr: &str,
) -> anyhow::Result<()> {
    let store: Arc<LadybugStore> = Arc::new(LadybugStore::open(db)?);
    let vindex: crate::vector::SharedVector = Arc::new(tokio::sync::RwLock::new(None));
    let query_embedder: Arc<Mutex<Option<Embedder>>> = Arc::new(Mutex::new(None));
    crate::embed::warm(query_embedder.clone());

    if repos.is_empty() {
        // No --watch: build the vector index once from the existing db.
        *vindex.write().await = crate::vector::build_from_store(&store)?;
    } else {
        // --watch: index + watch on a background thread (its own batch embedder).
        let watch_embedder: Arc<Mutex<Option<Embedder>>> = Arc::new(Mutex::new(None));
        let (s2, v2, repos) = (store.clone(), vindex.clone(), repos.to_vec());
        std::thread::spawn(move || {
            if let Err(e) =
                crate::watch::index_and_watch(&repos, s2, watch_embedder, v2, embed, reanalyze)
            {
                eprintln!("codegraph: index/watch stopped: {e}");
            }
        });
    }

    // Each connecting client is one session; the factory hands it a server that
    // shares the same store/embedder/vindex Arcs — one process, one DB lock.
    let service = StreamableHttpService::new(
        move || {
            Ok(CodegraphServer::with_shared(
                store.clone(),
                query_embedder.clone(),
                vindex.clone(),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
    eprintln!("codegraph MCP (HTTP) → http://{addr}/mcp  (connect multiple clients here)");
    axum::serve(listener, app).await?;
    Ok(())
}
