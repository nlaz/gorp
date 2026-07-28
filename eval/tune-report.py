#!/usr/bin/env python3
"""Compare §9.5 tuning-sweep conditions against their references.

Usage:
  python3 eval/tune-report.py

MaxSim knobs (mp48/mp96/bl75/bl50) compare against maxsim2 (pool auto,
blend 1.0). SIF knobs (sifa2/sifa4/sifc) compare against sif-maxsim2
(a=1e-3, no centering). Deltas per corpus × mode × kind.
"""

import json
from pathlib import Path

HERE = Path(__file__).parent
DATA = HERE / "data"
GROUPS = [
    ("maxsim2", ["mp48", "mp96", "bl75", "bl50"]),
    ("sif-maxsim2", ["sifa2", "sifa4", "sifc"]),
]
CORPORA = ["vscode", "wikipedia", "linux"]


def load(corpus, cond):
    p = DATA / f"lever-{corpus}-{cond}.json"
    if not p.exists():
        return {}
    return {(r["mode"], r["kind"]): r for r in json.loads(p.read_text())}


def main():
    for corpus in CORPORA:
        printed = False
        for ref_name, conds in GROUPS:
            ref = load(corpus, ref_name)
            if not ref:
                continue
            for cond in conds:
                rows = load(corpus, cond)
                if not rows:
                    continue
                if not printed:
                    print(f"\n== {corpus} ==")
                    print(f"{'condition':<10} {'vs':<12} {'mode':<9} {'kind':<11} "
                          f"{'R@5':>6} {'Δ':>7} {'MRR':>7} {'Δ':>7}")
                    printed = True
                for (mode, kind), r in sorted(rows.items()):
                    b = ref.get((mode, kind))
                    if not b:
                        continue
                    print(f"{cond:<10} {ref_name:<12} {mode:<9} {kind:<11} "
                          f"{r['recall@5']:>6.2f} {r['recall@5'] - b['recall@5']:>+7.2f} "
                          f"{r['mrr@10']:>7.3f} {r['mrr@10'] - b['mrr@10']:>+7.3f}")


if __name__ == "__main__":
    main()
