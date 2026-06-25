//! Graph intelligence: PageRank importance + Louvain community detection,
//! computed in Rust over the call graph.
//!
//! LadybugDB's `algo` extension loads over the network (`INSTALL`), which would
//! break the offline requirement — so we pull the edge list out and run these
//! well-understood algorithms locally (no external algo dependency to rot).

use std::collections::HashMap;

use crate::store::LadybugStore;

/// Load the call graph, compute PageRank + communities, and store them back.
/// Returns `(num_defs, num_communities)`.
///
/// Takes `&LadybugStore` (not `&mut`) and holds no store lock across the heavy
/// part: the edge list is read with concurrent-read connections, PageRank and
/// Louvain run purely in memory with **no lock at all** (seconds, on a big
/// workspace), and only the final `set_analysis` write briefly serializes
/// against other writers. So a periodic re-analyze never blocks live queries.
pub fn run(store: &LadybugStore, iters: usize) -> anyhow::Result<(usize, usize)> {
    // --- read phase (concurrent-safe reads) ---
    let ids = store.def_ids()?;
    let raw_edges = store.call_edges()?;

    // --- compute phase (lock-free, pure CPU) ---
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, id) in ids.iter().enumerate() {
        index.insert(id.as_str(), i);
    }
    let n = ids.len();
    let edges: Vec<(usize, usize)> = raw_edges
        .iter()
        .filter_map(|(a, b)| Some((*index.get(a.as_str())?, *index.get(b.as_str())?)))
        .collect();

    let pr = pagerank(n, &edges, 0.85, iters.max(1));
    let comm = louvain(n, &edges);
    let num_comm = comm.iter().copied().max().map(|m| m + 1).unwrap_or(0);

    // --- write phase (brief writer-serialized transaction) ---
    let items: Vec<(String, f64, i64)> = (0..n)
        .map(|i| (ids[i].clone(), pr[i], comm[i] as i64))
        .collect();
    store.set_analysis(&items)?;

    Ok((n, num_comm))
}

/// Power-iteration PageRank over the directed call graph (A→B = A calls B), so
/// nodes called by many important callers score high (most-depended-on code).
fn pagerank(n: usize, edges: &[(usize, usize)], damping: f64, iters: usize) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let nf = n as f64;
    let mut out_deg = vec![0usize; n];
    for &(a, _) in edges {
        out_deg[a] += 1;
    }
    let mut pr = vec![1.0 / nf; n];
    for _ in 0..iters {
        // Mass from dangling (no-out) nodes is redistributed uniformly.
        let dangling: f64 = (0..n).filter(|&v| out_deg[v] == 0).map(|v| pr[v]).sum();
        let base = (1.0 - damping) / nf + damping * dangling / nf;
        let mut next = vec![base; n];
        for &(a, b) in edges {
            next[b] += damping * pr[a] / out_deg[a] as f64;
        }
        pr = next;
    }
    pr
}

/// Louvain community detection (modularity local-moving, single level) over the
/// call graph symmetrized to a weighted undirected graph.
fn louvain(n: usize, directed: &[(usize, usize)]) -> Vec<usize> {
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut k = vec![0.0f64; n];
    let mut m2 = 0.0f64;
    for &(a, b) in directed {
        if a == b {
            continue;
        }
        adj[a].push((b, 1.0));
        adj[b].push((a, 1.0));
        k[a] += 1.0;
        k[b] += 1.0;
        m2 += 2.0;
    }
    if m2 == 0.0 {
        return (0..n).collect();
    }
    let m = m2 / 2.0;

    let mut comm: Vec<usize> = (0..n).collect();
    let mut sigma_tot = k.clone();

    let mut improved = true;
    let mut passes = 0;
    while improved && passes < 50 {
        improved = false;
        passes += 1;
        for i in 0..n {
            let ci = comm[i];
            sigma_tot[ci] -= k[i];

            // Total edge weight from i into each neighbouring community.
            let mut wcomm: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &adj[i] {
                if j != i {
                    *wcomm.entry(comm[j]).or_default() += w;
                }
            }

            // Pick the community with the best modularity gain; stay otherwise.
            let inv = k[i] / (2.0 * m);
            let mut best_c = ci;
            let mut best_gain = wcomm.get(&ci).copied().unwrap_or(0.0) - sigma_tot[ci] * inv;
            for (&c, &kin) in &wcomm {
                let gain = kin - sigma_tot[c] * inv;
                if gain > best_gain {
                    best_gain = gain;
                    best_c = c;
                }
            }

            sigma_tot[best_c] += k[i];
            comm[i] = best_c;
            if best_c != ci {
                improved = true;
            }
        }
    }

    relabel(&comm)
}

/// Map arbitrary community ids to contiguous `0..k`.
fn relabel(comm: &[usize]) -> Vec<usize> {
    let mut map: HashMap<usize, usize> = HashMap::new();
    comm.iter()
        .map(|&c| {
            let next = map.len();
            *map.entry(c).or_insert(next)
        })
        .collect()
}
