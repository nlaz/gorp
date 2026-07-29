#!/usr/bin/env python3
"""Diff two retrieval-eval result sets produced by run_eval.py --out.

Both sides must have been scored in the same conditions (same query sets,
same modes); this just aligns on (mode, kind) and prints the delta.

Usage:
  python3 eval/model-ab-report.py --base lever --cand codemodel \
      [--corpora vscode,wikipedia,linux] [--metric recall@5]
"""
import argparse
import json
from pathlib import Path

DATA = Path(__file__).parent / "data"
METRICS = ["recall@1", "recall@5", "recall@10", "mrr@10"]


def load(tag, corpus):
    """lever-<corpus>-base.json is the baseline naming; <tag>-<corpus>.json the candidate."""
    for name in (f"{tag}-{corpus}-base.json", f"{tag}-{corpus}.json"):
        p = DATA / name
        if p.exists():
            return {(r["mode"], r["kind"]): r for r in json.loads(p.read_text())}
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="lever")
    ap.add_argument("--cand", default="codemodel")
    ap.add_argument("--corpora", default="linux,vscode,wikipedia")
    ap.add_argument("--metric", default="recall@5", choices=METRICS + ["all"])
    args = ap.parse_args()

    metrics = METRICS if args.metric == "all" else [args.metric]
    for corpus in args.corpora.split(","):
        base, cand = load(args.base, corpus), load(args.cand, corpus)
        if not base or not cand:
            print(f"\n## {corpus}: missing ({'base' if not base else 'cand'})")
            continue
        print(f"\n## {corpus}")
        for m in metrics:
            print(f"\n{m}")
            print(f"{'mode':<10} {'kind':<12} {'base':>7} {'cand':>7} {'delta':>8}")
            for key in sorted(set(base) & set(cand)):
                b, c = base[key].get(m), cand[key].get(m)
                if b is None or c is None:
                    continue
                d = c - b
                mark = "  +" if d > 0.005 else ("  -" if d < -0.005 else "   ")
                print(f"{key[0]:<10} {key[1]:<12} {b:>7.3f} {c:>7.3f} {d:>+8.3f}{mark}")


if __name__ == "__main__":
    main()
