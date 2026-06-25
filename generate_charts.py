import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
import numpy as np

# ── Real measurements from run_real_benchmark.py (2026-06-25) ──────────────
# Codebase: codegraph repo, 14 Rust files
# Codegraph: query sent to binary, output captured → tokenised (cl100k_base)
# Traditional: grep -rn patterns → top 5 matched files read in full → tokenised
scenarios_short = ['Symbol\nLookup', 'Embedding\nCompute', 'Graph\nBatch',
                   'File\nParsing', 'Vector\nSearch', 'MCP\nServer']
scenarios_long  = ['Symbol struct definition', 'How embeddings are computed',
                   'Graph batch construction', 'File walking & parsing',
                   'Cosine similarity / HNSW', 'MCP server handler']

codegraph   = [145,   161,   198,   162,   214,   187]
traditional = [6845, 15877, 13635, 13036, 10857,  8712]
efficiency  = [47.2,  98.6,  68.9,  80.5,  50.7,  46.6]

CG_COLOR   = '#10a37f'
TRAD_COLOR = '#d41159'
EFF_COLOR  = '#f5a623'

# Claude 3.5 Sonnet: $3/M input tokens, $15/M output tokens
# We treat all tokens as "output" (conservative / worst-case)
cost_per_token = 15 / 1_000_000
cg_cost   = [c * cost_per_token * 100 for c in codegraph]     # per 100 queries
trad_cost = [t * cost_per_token * 100 for t in traditional]

avg_cg   = int(np.mean(codegraph))
avg_trad = int(np.mean(traditional))
avg_eff  = round(np.mean(efficiency), 1)

# ── Layout: 3 panels ────────────────────────────────────────────────────────
fig = plt.figure(figsize=(18, 13), facecolor='#fafafa')
gs  = fig.add_gridspec(2, 2, hspace=0.45, wspace=0.35,
                        left=0.07, right=0.97, top=0.88, bottom=0.08)

ax_top   = fig.add_subplot(gs[0, :])   # full-width horizontal bar chart
ax_eff   = fig.add_subplot(gs[1, 0])   # efficiency gains
ax_cost  = fig.add_subplot(gs[1, 1])   # cost per 100 queries

fig.suptitle('Codegraph  vs  Traditional Code Search\nReal Token Benchmark · 14-file Rust codebase · tiktoken cl100k · 2026-06-25',
             fontsize=15, fontweight='bold', color='#222222', y=0.97)

# ── Panel 1: Horizontal grouped bars ─────────────────────────────────────────
y      = np.arange(len(scenarios_long))
height = 0.35

h1 = ax_top.barh(y + height/2, traditional, height, label='Traditional (grep + read files)',
                 color=TRAD_COLOR, alpha=0.85)
h2 = ax_top.barh(y - height/2, codegraph,   height, label='Codegraph (semantic search)',
                 color=CG_COLOR,  alpha=0.85)

ax_top.set_yticks(y)
ax_top.set_yticklabels(scenarios_long, fontsize=11)
ax_top.set_xlabel('Tokens per query  (lower is better)', fontsize=11, fontweight='bold')
ax_top.set_title('Token consumption per query', fontsize=13, fontweight='bold', pad=10)
ax_top.legend(fontsize=11, loc='lower right')
ax_top.grid(axis='x', alpha=0.3, linestyle='--')
ax_top.spines[['top', 'right']].set_visible(False)
ax_top.set_xlim(0, max(traditional) * 1.18)

# Labels on bars
for bar in h1:
    w = bar.get_width()
    ax_top.text(w + 150, bar.get_y() + bar.get_height()/2,
                f'{int(w):,}', va='center', fontsize=9, color='#333333')
for bar in h2:
    w = bar.get_width()
    ax_top.text(w + 150, bar.get_y() + bar.get_height()/2,
                f'{int(w)}', va='center', fontsize=9, color='#333333')

# Avg annotations
ax_top.axvline(avg_cg,   linestyle=':', linewidth=1.5, color=CG_COLOR,   alpha=0.7)
ax_top.axvline(avg_trad, linestyle=':', linewidth=1.5, color=TRAD_COLOR, alpha=0.7)
ax_top.text(avg_cg   + 100, -0.7, f'avg {avg_cg}', color=CG_COLOR,   fontsize=9, fontstyle='italic')
ax_top.text(avg_trad + 100, -0.7, f'avg {avg_trad:,}', color=TRAD_COLOR, fontsize=9, fontstyle='italic')

# ── Panel 2: Efficiency gain bars ────────────────────────────────────────────
x    = np.arange(len(scenarios_short))
bars = ax_eff.bar(x, efficiency, color=EFF_COLOR, alpha=0.85, width=0.6, edgecolor='white')

ax_eff.set_xticks(x)
ax_eff.set_xticklabels(scenarios_short, fontsize=9)
ax_eff.set_ylabel('Efficiency gain  (× times fewer tokens)', fontsize=10, fontweight='bold')
ax_eff.set_title('Efficiency gain per scenario', fontsize=12, fontweight='bold')
ax_eff.set_ylim(0, max(efficiency) * 1.22)
ax_eff.grid(axis='y', alpha=0.3, linestyle='--')
ax_eff.spines[['top', 'right']].set_visible(False)
ax_eff.axhline(avg_eff, linestyle='--', linewidth=1.5, color='#888888')
ax_eff.text(len(scenarios_short) - 0.5, avg_eff + 1.5,
            f'avg {avg_eff}×', fontsize=9, color='#555555', ha='right', fontstyle='italic')

for bar, eff in zip(bars, efficiency):
    ax_eff.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 1.2,
                f'{eff}×', ha='center', fontsize=10, fontweight='bold', color='#333333')

# ── Panel 3: Cost per 100 queries ─────────────────────────────────────────────
bar_w = 0.35
b1 = ax_cost.bar(x - bar_w/2, trad_cost, bar_w, label='Traditional', color=TRAD_COLOR, alpha=0.85)
b2 = ax_cost.bar(x + bar_w/2, cg_cost,   bar_w, label='Codegraph',   color=CG_COLOR,   alpha=0.85)

ax_cost.set_xticks(x)
ax_cost.set_xticklabels(scenarios_short, fontsize=9)
ax_cost.set_ylabel('Cost USD  (per 100 queries)', fontsize=10, fontweight='bold')
ax_cost.set_title('Estimated cost @ Claude 3.5 Sonnet pricing\n($15 / 1M tokens)', fontsize=11, fontweight='bold')
ax_cost.legend(fontsize=10)
ax_cost.grid(axis='y', alpha=0.3, linestyle='--')
ax_cost.spines[['top', 'right']].set_visible(False)
ax_cost.yaxis.set_major_formatter(plt.FuncFormatter(lambda v, _: f'${v:.2f}'))

for bar in b1:
    h = bar.get_height()
    ax_cost.text(bar.get_x() + bar.get_width()/2, h + 0.003,
                 f'${h:.2f}', ha='center', fontsize=8, color='#333333')
for bar in b2:
    h = bar.get_height()
    ax_cost.text(bar.get_x() + bar.get_width()/2, h + 0.003,
                 f'${h:.3f}', ha='center', fontsize=8, color='#333333')

# ── Save ─────────────────────────────────────────────────────────────────────
out = '/Users/hadidya/gitrepos/codegraph/benchmark_chart.svg'
fig.savefig(out, dpi=150, bbox_inches='tight', facecolor=fig.get_facecolor())
print(f"Saved → {out}")

# ── Summary card (benchmark_summary.png) ─────────────────────────────────────
fig2, ax2 = plt.subplots(figsize=(12, 6), facecolor='#ffffff')
ax2.axis('off')

rows = [
    ('Scenario', 'Codegraph (tokens)', 'Traditional (tokens)', 'Gain'),
    *[(sl, f'{cg:,}', f'{tr:,}', f'{ef}×')
      for sl, cg, tr, ef in zip(scenarios_long, codegraph, traditional, efficiency)],
    ('Average', f'{avg_cg}', f'{avg_trad:,}', f'{avg_eff}×'),
]
col_x = [0.01, 0.40, 0.60, 0.82]
row_h = 0.86 / len(rows)

for r, row in enumerate(rows):
    y_r = 0.96 - r * row_h
    is_header = r == 0
    is_avg    = r == len(rows) - 1
    bg = '#222222' if is_header else ('#e8f8f2' if is_avg else ('#f5f5f5' if r % 2 else '#ffffff'))
    fc = 'white' if is_header else '#222222'
    fw = 'bold' if (is_header or is_avg) else 'normal'
    rect = mpatches.FancyBboxPatch((0, y_r - row_h * 0.85), 1, row_h * 0.9,
                                    boxstyle='square,pad=0', linewidth=0,
                                    facecolor=bg, transform=ax2.transAxes, clip_on=False)
    ax2.add_patch(rect)
    for cx, text in zip(col_x, row):
        color = EFF_COLOR if (not is_header and col_x.index(cx) == 3) else fc
        ax2.text(cx, y_r - row_h * 0.35, text, transform=ax2.transAxes,
                 fontsize=10.5, fontweight=fw, color=color, va='center')

ax2.set_title('Codegraph vs Traditional Code Search — Real Token Benchmark\n'
              '14 Rust files · grep + full file reads · tiktoken cl100k_base · 2026-06-25',
              fontsize=12, fontweight='bold', pad=14, color='#222222')

out2 = '/Users/hadidya/gitrepos/codegraph/benchmark_summary.png'
fig2.savefig(out2, dpi=150, bbox_inches='tight')
print(f"Saved → {out2}")

