#!/usr/bin/env python3
"""Compare §9 lever conditions from eval/data/lever-*.json.

Usage:
  python3 eval/lever-report.py

Per corpus, prints R@5 / MRR@10 for direct and paraphrase queries, per
condition and mode, with deltas against the base condition's same mode.
"""

import json
from pathlib import Path

HERE = Path(__file__).parent
DATA = HERE / "data"
CONDS = ["base", "prf", "maxsim", "sif", "sif-maxsim"]
CORPORA = ["vscode", "wikipedia", "linux"]


def load(corpus, cond):
    p = DATA / f"lever-{corpus}-{cond}.json"
    if not p.exists():
        return {}
    out = {}
    for row in json.loads(p.read_text()):
        out[(row["mode"], row["kind"])] = row
    return out


def main():
    for corpus in CORPORA:
        base = load(corpus, "base")
        if not base:
            continue
        print(f"\n== {corpus} ==")
        hdr = f"{'condition':<22} {'kind':<11} {'R@5':>6} {'Δbase':>7} {'MRR@10':>7} {'Δbase':>7}"
        print(hdr)
        print("-" * len(hdr))
        for cond in CONDS:
            rows = base if cond == "base" else load(corpus, cond)
            for (mode, kind), r in sorted(rows.items()):
                b = base.get((mode, kind))
                d_r5 = r["recall@5"] - b["recall@5"] if b and cond != "base" else None
                d_mrr = r["mrr@10"] - b["mrr@10"] if b and cond != "base" else None
                fmt = lambda x: f"{x:+.2f}" if x is not None else "     —"
                print(
                    f"{cond + ':' + mode:<22} {kind:<11} {r['recall@5']:>6.2f} "
                    f"{fmt(d_r5):>7} {r['mrr@10']:>7.3f} {fmt(d_mrr):>7}"
                )


if __name__ == "__main__":
    main()
