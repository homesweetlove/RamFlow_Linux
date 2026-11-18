#!/usr/bin/env python3
"""Chart for a study directory: per-block p95 of each arm, per feature. usage: chart.py DIR"""
import csv, json, statistics, sys
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

d = sys.argv[1]
rows = list(csv.DictReader(open(f"{d}/trials.csv")))
arms = ["off", "on", "auto", "auto15"]
label = {"off": "RamFlow 꺼짐", "on": "목록 지정", "auto": "자동 (기본 기준)", "auto15": "자동 (15% 기준)"}
color = {"off": "#6b6a66", "on": "#2a78d6", "auto": "#eb6834", "auto15": "#1baf7a"}
mark = {"off": "o", "on": "s", "auto": "^", "auto15": "D"}
plt.rcParams.update({"font.family": ["Noto Sans CJK KR", "NanumGothic", "DejaVu Sans"], "axes.unicode_minus": False})
fig, axes = plt.subplots(1, 2, figsize=(11, 4.4), facecolor="#fcfcfb")
for ax, (metric, title) in zip(axes, (("search", "검색 (행 1,000개 다시 그리기)"), ("sort", "정렬 버튼"))):
    ax.set_facecolor("#fcfcfb")
    for si, arm in enumerate(arms):
        pts = sorted((int(r["block"]), float(r["p95Ms"])) for r in rows if r["condition"] == arm and r["metric"] == metric)
        med = statistics.median(v for _, v in pts)
        ax.scatter([si + (b - 3) * 0.06 for b, _ in pts], [v for _, v in pts], s=36, color=color[arm], marker=mark[arm], zorder=3, edgecolor="#fcfcfb", linewidth=1)
        ax.hlines(med, si - 0.3, si + 0.3, color=color[arm], linewidth=2, zorder=2)
        ax.text(si + 0.33, med, f"{med:.0f}", ha="left", va="center", fontsize=10, color="#0b0b0b")
    ax.set_xticks(range(len(arms)), [label[a] for a in arms], fontsize=9)
    ax.set_ylim(0, None)
    ax.set_ylabel("p95 지연 (ms) — 낮을수록 좋음", fontsize=9, color="#52514e")
    ax.set_title(title, fontsize=11, loc="left", color="#0b0b0b")
    ax.grid(axis="y", color="#e4e3df", linewidth=0.8); ax.set_axisbelow(True)
    for sp in ("top", "right", "left"): ax.spines[sp].set_visible(False)
    ax.spines["bottom"].set_color("#c3c2b7"); ax.tick_params(length=0, colors="#52514e")
fig.suptitle("진짜 크롬, 배경 작업 8개(코어 2개) — 블록별 p95 (점)과 중앙값 (선)", x=0.01, ha="left", fontsize=12)
fig.tight_layout()
fig.savefig(f"{d}/chart.png", dpi=160)
