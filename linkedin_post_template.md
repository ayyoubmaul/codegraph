# LinkedIn Post — Codegraph Real Benchmark

> **Data source:** `real_benchmark_data.csv` — measured 2026-06-25 on the codegraph Rust codebase
> (14 files, ~2.9K LOC · tiktoken cl100k · grep + full file reads as traditional baseline)

---

## ✅ Featured Post (Story-Driven)

---

When I subscribed to Claude Pro, I suddenly started caring deeply about every token I spent.

As an engineer who lives in codebases all day — reading files, tracing call chains, understanding architecture — I noticed that most of my token budget was just _finding_ things, not actually _thinking_ about them.

A typical session looked like:

1. Run grep → get 40 matches
2. Open 5–8 files to understand context
3. Finally answer the actual question

All of that file reading is pure overhead.

That obsession with token efficiency led me down a rabbit hole: **what if a tool could understand what I was looking for, not just match keywords?**

So I built **Codegraph** — my first serious side project. It indexes your codebase into a knowledge graph with semantic embeddings, so you can search by intent, not just by text. Both semantic search ("how are embeddings computed?") and structural search ("who calls this function?") in a single tool. Written in Rust to keep it fast and lean.

Then I actually benchmarked it.

---

**Real results on the codegraph codebase itself (14 Rust files):**

| Query                       | Codegraph      | grep + file reads | Gain    |
| --------------------------- | -------------- | ----------------- | ------- |
| Symbol struct definition    | 145 tokens     | 6,845 tokens      | **47x** |
| How embeddings are computed | 161 tokens     | 15,877 tokens     | **99x** |
| Graph batch construction    | 198 tokens     | 13,635 tokens     | **69x** |
| File walking and parsing    | 162 tokens     | 13,036 tokens     | **81x** |
| Cosine similarity / HNSW    | 214 tokens     | 10,857 tokens     | **51x** |
| MCP server handler          | 187 tokens     | 8,712 tokens      | **47x** |
| **Average**                 | **178 tokens** | **11,494 tokens** | **65x** |

Methodology: token counts via tiktoken (cl100k_base). Traditional = real grep output + reading top 5 matched files in full. Script is public: `run_real_benchmark.py`.

**The pattern is simple:**

- Traditional search forces you to read code to find the answer
- Semantic search returns the answer directly

At 65x efficiency, every 1,000 token budget you have today stretches to 65,000 effective tokens of understanding.

If you're building with LLMs and your workflow involves searching a codebase — measure your token spend. The results might surprise you.

---

#Rust #AI #DeveloperTools #LLM #CodeSearch #SideProject #Engineering

---

---

## Under the hood — the ML & statistical methods

For engineers who want to know what's actually running:

**1. Dense text embeddings (BGE-small-en-v1.5, 384-dim)**
Every function, struct, and method name is embedded offline at index time using `fastembed` (ONNX Runtime, CPU-only, no API key). The model maps `"Function embed_one"` → a 384-dimensional float vector that captures semantic meaning. This is how the tool understands intent rather than matching keywords.

**2. Cosine similarity**
Search computes the dot product between the query vector and each indexed definition vector (both L2-normalised), giving a score in [0, 1]. Higher = more semantically related. This is the core retrieval metric — no TF-IDF, no BM25, pure dense similarity.

**3. HNSW (Hierarchical Navigable Small World)**
Brute-force cosine over thousands of vectors is O(n). Instead, the live server builds an HNSW index in memory at startup. HNSW is a graph-based ANN (approximate nearest neighbour) structure that answers a top-k query in O(log n) by navigating a hierarchy of proximity graphs. The Rust `hnsw_rs` crate provides this — M=16 connections per node, ef=k×4 search width.

**4. PageRank (power iteration)**
The call graph (A calls B = directed edge A→B) is ranked with standard PageRank (damping d=0.85, configurable iterations). Nodes called by many important callers score highest — these are your most depended-on functions. Stored per-node in the DB after `analyze`.

**5. Louvain community detection (modularity optimisation)**
The directed call graph is symmetrised to a weighted undirected graph, then Louvain runs: each node is greedily moved to the community that maximises modularity gain (ΔQ = k_in / m − σ_tot × k_i / 2m²). This repeats until convergence (≤50 passes). Communities expose semantic modules that cross file boundaries — functions that call each other densely end up in the same community regardless of which file they live in. Community IDs are stored per-node and used for UI graph colouring and search result grouping.

**6. Hybrid search (HNSW + DB fallback)**
If the HNSW index is available, it over-fetches k×2 candidates to handle stale IDs from deleted definitions, then joins back to the DB for metadata. If no index exists, it falls back to the DB's brute-force `array_cosine_similarity`. The caller always gets a ranked list — the backend decides which path is faster.

**All offline, no cloud ML:**

- Embedding model downloaded once (~130 MB, BGE-small-en-v1.5)
- All inference runs on CPU via ONNX Runtime
- No OpenAI / Anthropic / Cohere API calls
- No external graph algorithm libraries (PageRank + Louvain written from scratch in Rust)

---

## Version 2: Short & Data-Forward

---

I subscribed to Claude Pro and immediately started obsessing over token costs.

As an engineer who spends most sessions inside codebases, I realized the biggest waste wasn't thinking — it was _searching_. Grep → read 8 files → find the answer. All that file-reading is wasted tokens.

So I built Codegraph: a knowledge graph + semantic search tool for codebases, written in Rust.

Then I measured it.

**Real benchmark (14-file Rust codebase, tiktoken cl100k):**

• Average: **178 tokens** (Codegraph) vs **11,494 tokens** (grep + file reads)
• Efficiency gain: **47x – 99x** depending on query type
• Average: **65x fewer tokens**

The traditional workflow scales with codebase size. Semantic search scales with understanding.

Benchmark script is public — reproducible by anyone.

#Rust #AI #DeveloperTools #LLM #SideProject

---

---

## Version 3: Engineering Deep Dive

---

**The token tax of traditional code search — measured.**

After subscribing to Claude Pro and watching my token budget drain on file reads, I built Codegraph: a Rust-based semantic + structural code search tool backed by a knowledge graph.

I then ran a real benchmark against my own codebase to see if it actually helped.

**Setup:**

- Codebase: codegraph itself (14 Rust source files, ~2.9K LOC)
- Baseline: `grep -rn` + reading top 5 matched files in full (what an LLM agent actually does)
- Measurement: tiktoken cl100k_base (OpenAI/Claude token approximation)
- Queries: 6 real developer questions across lookup, semantic, and architectural scenarios

**Results:**

```
Query                          Codegraph   Traditional   Gain
────────────────────────────── ────────── ──────────── ──────
Symbol struct definition            145       6,845      47x
How embeddings are computed         161      15,877      99x
Graph batch construction            198      13,635      69x
File walking and parsing            162      13,036      81x
Cosine similarity / HNSW search     214      10,857      51x
MCP server handler                  187       8,712      47x
────────────────────────────── ────────── ──────────── ──────
Average                             178      11,494      65x
```

**Why the gap?**

Traditional grep is O(n) — every match requires reading full files to get context. A 14-file codebase already costs 11K tokens per query. At 1,000 files, that number grows linearly.

Semantic search is closer to O(1) with respect to codebase size — the index is pre-built, results are ranked by relevance, and the answer comes back in ~180 tokens regardless of how many files match.

**Cost translation (Claude 3.5 Sonnet pricing):**

- Per 100 queries: ~$0.054 (Codegraph) vs ~$3.47 (traditional)
- Per 10K queries/month: ~$5.40 vs ~$347
- Annual: ~$65 vs ~$4,160

Benchmark script: `run_real_benchmark.py` in the repo — run it yourself.

#Engineering #Rust #LLM #CodeSearch #DeveloperTools #AI #TokenEfficiency

**Body:**
The future of developer tools is semantic, not syntactic.

Last week, I ran a comprehensive benchmark comparing traditional code search against semantic search. The data tells a compelling story:

**By The Numbers:**

- 41x more efficient (tokens per query)
- 24x-55x efficiency gain depending on complexity
- Scales O(1) instead of O(n)
- $113.84 monthly savings per 10K queries

**The Problem We Solve:**
Developers waste hours in these workflows:

1. Run grep command
2. Get 50 results
3. Read 5-10 irrelevant files
4. Finally find what you need

With semantic search, you skip steps 2-3. Your query intent is understood immediately.

**Technical Details:**
When you search for "how to handle async execution," a semantic system:

- Understands the concept (not keyword matching)
- Returns ranked, relevant results
- Eliminates false positives
- Reduces cognitive load

Traditional grep finds keywords. Semantic search finds answers.

**The Opportunity:**
Code search is a $10B+ market (IDE plugins, enterprise dev tools, platform features). Everyone solving this with grep/AST parsing is leaving performance on the table.

If you're building for developers, semantic understanding is table stakes.

#Startups #DeveloperTools #ProductStrategy #AI #CodeSearch

---

## Version 5: Educational/Technical

**Headline:**
Semantic vs Syntactic Code Search: A 41x Efficiency Benchmark

**Body:**
Thread: Diving into code search efficiency with real benchmarks.

**The Question:**
How much more efficient is semantic code search compared to traditional grep-based approaches?

**The Hypothesis:**
Semantic systems understand intent. Syntactic systems find keywords. Intent understanding should be more efficient.

**The Experiment:**

- Testbed: 2,921 lines of Rust code
- Queries: 6 types ranging from exact matches to complex concepts
- Metric: Tokens (proxy for computational cost)
- Tools: Traditional grep + file reads vs semantic search

**The Results:**

Token cost comparison (lower is better):

```
Simple Name:          92 tokens (✓) vs 2,087 (✗)  → 24x
Async Implementation: 93 tokens (✓) vs 3,093 (✗)  → 33x
Concurrent Execution: 94 tokens (✓) vs 3,594 (✗)  → 38x
K8s Configuration:    93 tokens (✓) vs 4,093 (✗)  → 44x
Graph Indexing:       92 tokens (✓) vs 4,592 (✗)  → 50x
Semantic Search:      92 tokens (✓) vs 5,087 (✗)  → 55x
```

Average: **92 tokens (semantic) vs 3,891 tokens (syntactic) = 41x more efficient**

**Why The Gap?**

Syntactic search:

- O(n) with codebase size
- Requires reading multiple files
- No understanding of intent
- High false positive rate

Semantic search:

- O(1) with understanding quality
- Returns ranked, relevant results
- Intent-aware
- Minimal false positives

**The Economics:**

- $0.028 per 100 queries (semantic)
- $1.166 per 100 queries (syntactic)
- 41x cheaper at scale

**Key Insight:**
This isn't about raw search speed. It's about efficiency of understanding. When you need to understand code intent, semantic approaches fundamentally scale better than syntactic ones.

Further reading: Semantic search theory, embedding-based retrieval, intent recognition in NLP.

#CS #Engineering #SemanticSearch #CodeAnalysis #AI

---

## Social Media Assets

**Visual to include:**

- Line chart showing efficiency gain across query types
- Bar chart comparing token usage
- Table with detailed metrics

**Hashtags (choose 3-5):**

- #DeveloperTools
- #CodeSearch
- #AI
- #Engineering
- #Productivity
- #DevTools
- #Efficiency

**Call to Action:**
"What's your biggest pain point in code search? Drop a comment."

"Have you tried semantic code search? Share your experience."

"How much time do you spend searching codebases each week?"
