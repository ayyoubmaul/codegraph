"""
Real Codegraph Benchmark
========================
Measures ACTUAL token usage of:
  - Codegraph semantic search (runs queries against a DB copy)
  - Traditional grep (runs real grep, simulates what an LLM would consume)

Outputs: real_benchmark_data.csv + console summary
"""

import subprocess
import shutil
import csv
import os
import time
import tiktoken

CODEGRAPH_BIN = "./target/release/codegraph"
GRAPH_DB = "./graph.db"
BENCH_DB = "./benchmark_run.db"
SEARCH_ROOT = "."   # grep search root (the codegraph repo itself)
K = 5               # top-k results for codegraph

# Tokenizer (cl100k = GPT-4 / Claude approx)
enc = tiktoken.get_encoding("cl100k_base")

def count_tokens(text: str) -> int:
    return len(enc.encode(text))

# ---------------------------------------------------------------------------
# Queries: (display_name, codegraph_query, grep_patterns)
# All queries are against the codegraph Rust codebase itself.
# ---------------------------------------------------------------------------
QUERIES = [
    (
        "Symbol struct definition",
        "what is Symbol struct",
        ["Symbol"],
    ),
    (
        "Vector embedding computation",
        "how are embeddings computed",
        ["embed", "fastembed", "Embedder"],
    ),
    (
        "Graph batch construction",
        "how is a graph batch built",
        ["GraphBatch", "batch", "build"],
    ),
    (
        "File walking and parsing",
        "how files are walked and parsed",
        ["walk", "parse", "WalkDir", "tree_sitter"],
    ),
    (
        "Vector similarity search",
        "cosine similarity nearest neighbour search",
        ["cosine", "hnsw", "hybrid_search", "similarity"],
    ),
    (
        "MCP server handler",
        "MCP server tool handler implementation",
        ["mcp", "serve", "tool_call", "handler"],
    ),
]

# ---------------------------------------------------------------------------
# Codegraph: copy DB and run search
# ---------------------------------------------------------------------------
def copy_db():
    if os.path.exists(BENCH_DB):
        os.remove(BENCH_DB)
    # Remove any stale bench WAL first
    bench_wal = BENCH_DB + ".wal"
    if os.path.exists(bench_wal):
        os.remove(bench_wal)
    shutil.copy2(GRAPH_DB, BENCH_DB)
    # Do NOT copy the WAL — it may be mid-transaction (corrupted).
    # LadybugDB will open in read-only mode using just the committed main file.

def run_codegraph_search(query: str) -> tuple[str, float]:
    """Returns (raw output text, elapsed seconds)"""
    t0 = time.perf_counter()
    result = subprocess.run(
        [CODEGRAPH_BIN, "search", query, "--db", BENCH_DB, "--k", str(K)],
        capture_output=True, text=True, timeout=30
    )
    elapsed = time.perf_counter() - t0
    output = result.stdout + result.stderr
    return output.strip(), elapsed

# ---------------------------------------------------------------------------
# Traditional grep: run real grep, simulate LLM reading top file hits
# ---------------------------------------------------------------------------
def run_grep(patterns: list[str]) -> tuple[str, int]:
    """
    Run grep for each pattern, collect unique matched files.
    Returns (all grep output, number of unique files matched).
    """
    all_output_lines = []
    matched_files = set()

    for pattern in patterns:
        result = subprocess.run(
            ["grep", "-rn", "--include=*.rs", "-l", pattern, SEARCH_ROOT],
            capture_output=True, text=True, timeout=30
        )
        files = [f.strip() for f in result.stdout.strip().splitlines() if f.strip()]
        matched_files.update(files)

        # Also capture grep -rn output (line matches) for token counting
        result2 = subprocess.run(
            ["grep", "-rn", "--include=*.rs", pattern, SEARCH_ROOT],
            capture_output=True, text=True, timeout=30
        )
        all_output_lines.extend(result2.stdout.strip().splitlines())

    grep_output = "\n".join(all_output_lines)
    return grep_output, list(matched_files)

def read_top_files(files: list[str], max_files: int = 5) -> str:
    """Simulate LLM reading up to max_files matched files in full."""
    content_parts = []
    for f in files[:max_files]:
        try:
            with open(f, "r", errors="replace") as fh:
                content_parts.append(f"# FILE: {f}\n" + fh.read())
        except Exception:
            pass
    return "\n\n".join(content_parts)

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
def main():
    print("=" * 60)
    print("REAL CODEGRAPH BENCHMARK")
    print("=" * 60)

    # Copy the DB once (avoids lock conflict with serve process)
    print("\n[setup] Copying graph.db → benchmark_run.db ...")
    try:
        copy_db()
        print("[setup] DB copy OK")
    except Exception as e:
        print(f"[setup] ERROR copying DB: {e}")
        print("        The serve process holds graph.db.")
        print("        Stop it first with: kill $(lsof -t graph.db)")
        return

    results = []
    print()

    for display, cg_query, grep_patterns in QUERIES:
        print(f"--- {display} ---")

        # --- Codegraph ---
        cg_output, cg_elapsed = run_codegraph_search(cg_query)
        cg_tokens = count_tokens(cg_query + "\n" + cg_output)
        print(f"  Codegraph  : {cg_tokens:>5} tokens  ({cg_elapsed:.2f}s)")
        if not cg_output or "Error" in cg_output:
            print(f"  [WARN] codegraph output: {cg_output[:120]}")

        # --- Traditional grep ---
        grep_output, matched_files = run_grep(grep_patterns)
        # Token cost = grep output lines + reading up to 5 files in full
        file_content = read_top_files(matched_files)
        trad_text = grep_output + "\n\n" + file_content
        trad_tokens = count_tokens(trad_text)
        print(f"  Traditional: {trad_tokens:>5} tokens  ({len(matched_files)} files matched, read {min(5, len(matched_files))})")

        if cg_tokens > 0 and trad_tokens > 0:
            ratio = trad_tokens / cg_tokens
            print(f"  Efficiency : {ratio:.1f}x")
        else:
            ratio = 0
            print(f"  Efficiency : N/A (zero tokens)")

        results.append({
            "Scenario": display,
            "Codegraph Query": cg_query,
            "Grep Patterns": "|".join(grep_patterns),
            "Codegraph Tokens": cg_tokens,
            "Codegraph Output Preview": cg_output[:200].replace("\n", " "),
            "Traditional Tokens": trad_tokens,
            "Files Matched": len(matched_files),
            "Files Read": min(5, len(matched_files)),
            "Efficiency Gain": f"{ratio:.1f}x",
            "Codegraph Time (s)": f"{cg_elapsed:.3f}",
        })
        print()

    # --- Summary ---
    print("=" * 60)
    print("SUMMARY")
    print("=" * 60)
    valid = [r for r in results if int(r["Codegraph Tokens"]) > 0 and int(r["Traditional Tokens"]) > 0]
    if valid:
        avg_cg = sum(int(r["Codegraph Tokens"]) for r in valid) / len(valid)
        avg_trad = sum(int(r["Traditional Tokens"]) for r in valid) / len(valid)
        avg_ratio = avg_trad / avg_cg if avg_cg else 0
        print(f"  Avg Codegraph tokens   : {avg_cg:.0f}")
        print(f"  Avg Traditional tokens : {avg_trad:.0f}")
        print(f"  Avg efficiency gain    : {avg_ratio:.1f}x")

    # --- Write CSV ---
    out_csv = "real_benchmark_data.csv"
    fieldnames = list(results[0].keys()) if results else []
    with open(out_csv, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fieldnames)
        w.writeheader()
        w.writerows(results)
    print(f"\n  Saved → {out_csv}")

    # Cleanup bench DB
    for path in [BENCH_DB, BENCH_DB + ".wal"]:
        if os.path.exists(path):
            os.remove(path)

if __name__ == "__main__":
    main()
