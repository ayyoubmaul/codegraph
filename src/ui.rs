//! Built-in web UI: a local HTTP server that renders the code graph (no Docker,
//! no CDN, fully offline — the page and its JS are embedded in the binary).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::embed::Embedder;
use crate::graph::GraphBatch;
use crate::store::LadybugStore;

struct AppState {
    store: Arc<Mutex<LadybugStore>>,
    embedder: Arc<Mutex<Option<Embedder>>>,
    vindex: crate::vector::SharedVector,
}

/// Open the database and serve the web UI until interrupted. With `watch`, also
/// index that repo and keep it live in a background thread sharing the store.
pub async fn serve(
    db: &Path,
    port: u16,
    watch: &[PathBuf],
    embed: bool,
    reanalyze: Option<u64>,
) -> anyhow::Result<()> {
    let store = LadybugStore::open(db)?;
    // Without --watch, build the vector index now from the existing db.
    let vindex_built = if watch.is_empty() {
        crate::vector::build_from_store(&store)?
    } else {
        None
    };

    let store: Arc<Mutex<LadybugStore>> = Arc::new(Mutex::new(store));
    let embedder: Arc<Mutex<Option<Embedder>>> = Arc::new(Mutex::new(None));
    crate::embed::warm(embedder.clone());
    let vindex: crate::vector::SharedVector = Arc::new(Mutex::new(vindex_built));

    // With --watch, index + watch on a background thread so the UI serves now.
    if !watch.is_empty() {
        let (s2, e2, v2, repos) = (store.clone(), embedder.clone(), vindex.clone(), watch.to_vec());
        std::thread::spawn(move || {
            if let Err(e) = crate::watch::index_and_watch(&repos, s2, e2, v2, embed, reanalyze) {
                eprintln!("codegraph: index/watch stopped: {e}");
            }
        });
    }

    let state = Arc::new(AppState {
        store,
        embedder,
        vindex,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/graph", get(graph))
        .route("/api/search", get(search))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("codegraph UI → http://127.0.0.1:{port}  (Ctrl-C to stop)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("ui_index.html"))
}

/// The call graph: definition nodes (with community + pagerank) that take part
/// in at least one call edge, and the edges between them.
async fn graph(State(state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, AppError> {
    let store = state.store.lock().await;
    let nodes = store.graph_nodes()?;
    let edges = store.call_edges()?;

    let mut incident: HashSet<&str> = HashSet::new();
    for (a, b) in &edges {
        incident.insert(a.as_str());
        incident.insert(b.as_str());
    }
    let nodes: Vec<_> = nodes
        .iter()
        .filter(|n| incident.contains(n.id.as_str()))
        .collect();
    let edges: Vec<_> = edges
        .iter()
        .map(|(a, b)| json!({ "source": a, "target": b }))
        .collect();

    Ok(Json(json!({ "nodes": nodes, "edges": edges })))
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default)]
    k: Option<usize>,
}

async fn search(
    State(state): State<Arc<AppState>>,
    Query(p): Query<SearchParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let query_vec = {
        let mut guard = state.embedder.lock().await;
        if guard.is_none() {
            *guard = Some(Embedder::new()?);
        }
        guard.as_mut().unwrap().embed_one(&p.q)?
    };
    let hits = {
        let store = state.store.lock().await;
        let vindex = state.vindex.lock().await;
        crate::vector::hybrid_search(&store, vindex.as_ref(), &query_vec, p.k.unwrap_or(15))?
    };
    let hits: Vec<_> = hits
        .iter()
        .map(|(h, sim)| {
            json!({
                "id": GraphBatch::def_id(&h.file, &h.name, h.start_line.max(0) as usize),
                "name": h.name,
                "file": h.file,
                "line": h.start_line,
                "sim": sim,
            })
        })
        .collect();
    Ok(Json(json!({ "hits": hits })))
}

/// Maps any handler error to a 500 with the message.
struct AppError(anyhow::Error);

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}
