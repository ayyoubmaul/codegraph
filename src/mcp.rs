//! MCP server exposing codegraph's structural + semantic queries over stdio,
//! so AI agents (Claude Code, Codex, …) can query the code graph directly.

use std::path::Path;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use tokio::sync::Mutex;

use crate::embed::Embedder;
use crate::store::{DefHit, LadybugStore};

#[derive(Clone)]
pub struct CodegraphServer {
    store: Arc<Mutex<LadybugStore>>,
    embedder: Arc<Mutex<Option<Embedder>>>,
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
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct NameArgs {
    /// The exact symbol name (function/method/type).
    name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct CallChainArgs {
    /// The symbol name to start from.
    name: String,
    /// Max hops to traverse (default 3).
    #[serde(default)]
    depth: Option<u8>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct TopKArgs {
    /// Number of results (default 10).
    #[serde(default)]
    k: Option<usize>,
}

#[tool_router]
impl CodegraphServer {
    pub fn new(store: LadybugStore) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            embedder: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// Build from an already-shared store + embedder, so a background watcher
    /// can patch the same store this server queries.
    pub fn with_shared(
        store: Arc<Mutex<LadybugStore>>,
        embedder: Arc<Mutex<Option<Embedder>>>,
    ) -> Self {
        Self {
            store,
            embedder,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Semantic search: find definitions by meaning, not just name (e.g. 'rate limiting logic'). Returns ranked name, location, and similarity."
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
            let store = self.store.lock().await;
            store
                .semantic_search(&query_vec, args.k.unwrap_or(8))
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
        let hits = {
            let store = self.store.lock().await;
            store.who_calls(&args.name).map_err(internal)?
        };
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
        let hits = {
            let store = self.store.lock().await;
            store
                .call_chain(&args.name, args.depth.unwrap_or(3))
                .map_err(internal)?
        };
        Ok(CallToolResult::success(vec![Content::text(format_hits(
            &hits, &args.name, "reachable",
        ))]))
    }

    #[tool(
        description = "Most important (most-depended-on) definitions by PageRank. Requires `analyze` to have been run on the database."
    )]
    async fn important(
        &self,
        Parameters(args): Parameters<TopKArgs>,
    ) -> Result<CallToolResult, McpError> {
        let hits = {
            let store = self.store.lock().await;
            store.top_important(args.k.unwrap_or(10)).map_err(internal)?
        };
        let mut out = String::new();
        for (h, pr) in &hits {
            out.push_str(&format!("{pr:.4}  {}  {}:{}\n", h.name, h.file, h.start_line));
        }
        if out.is_empty() {
            out.push_str("no analysis yet (run `analyze` first)");
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
            "codegraph: a structural + semantic code graph. Tools: search (find code by \
             meaning), who_calls, call_chain, important (PageRank)."
                .into(),
        );
        info
    }
}

fn internal(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
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
    let service = CodegraphServer::new(store).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve over stdio while a background thread watches `repo` and keeps the
/// shared store fresh. The initial index completes before serving begins.
pub async fn serve_watch(db: &Path, repo: &Path, embed: bool) -> anyhow::Result<()> {
    let mut store = LadybugStore::open(db)?;
    let mut embedder: Option<Embedder> = None;
    let (root, cache) = crate::watch::index_once_owned(repo, &mut store, &mut embedder, embed)?;

    let store: Arc<Mutex<LadybugStore>> = Arc::new(Mutex::new(store));
    let embedder: Arc<Mutex<Option<Embedder>>> = Arc::new(Mutex::new(embedder));

    let (s2, e2) = (store.clone(), embedder.clone());
    std::thread::spawn(move || {
        if let Err(e) = crate::watch::watch_only(root, cache, s2, e2, embed) {
            eprintln!("codegraph: watcher stopped: {e}");
        }
    });

    let service = CodegraphServer::with_shared(store, embedder)
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
