#!/usr/bin/env python3
"""Compare retrieval-eval result sets.

Replaces lever-report.py, tune-report.py and model-ab-report.py, which were three
copies of the same twenty lines: load `eval/data/<tag>-<corpus>.json`, align rows
on (mode, kind), print deltas. They differed only in which tags they hard-coded,
so a fourth campaign meant a fourth script.

    eval/diff.py --base base --cand maxsim
    eval/diff.py --base base --cand prf maxsim sif        # several candidates
    eval/diff.py --base maxsim2 --cand mp48 bl75 --metric recall@5
    eval/diff.py --list                                   # what is on disk

Result files are named either `lever-<corpus>-<tag>.json` (the lever campaign) or
`<tag>-<corpus>.json` (model A/B); both layouts are tried.
"""

import argparse
import json
import re
from pathlib import Path

DATA = Path(__file__).parent / "data"
CORPORA = ["vscode", "wikipedia", "linux"]
METRICS = ["recall@1", "recall@5", "recall@10", "mrr@10"]


def path_for(tag, corpus):
    """Both naming conventions, so one tool reads every campaign's output."""
    for name in (f"lever-{corpus}-{tag}.json", f"{tag}-{corpus}.json"):
        p = DATA / name
        if p.exists():
            return p
    return None


def load(tag, corpus):
    """-> {(mode, kind): row}, empty when the condition was never run."""
    p = path_for(tag, corpus)
    if p is None:
        return {}
    return {(r["mode"], r["kind"]): r for r in json.loads(p.read_text())}


def available_tags():
    tags = set()
    for p in DATA.glob("*.json"):
        m = re.fullmatch(r"lever-(\w+)-(.+)", p.stem)
        if m and m.group(1) in CORPORA:
            tags.add(m.group(2))
            continue
        m = re.fullmatch(r"(.+)-(\w+)", p.stem)
        if m and m.group(2) in CORPORA:
            tags.add(m.group(1))
    return sorted(tags)


def fmt_delta(value):
    if value is None:
        return "     ·"
    # Explicit sign: a wall of unsigned numbers hides which way a lever moved.
    return f"{value:+6.3f}"


def compare(base_tag, cand_tag, metric, corpora):
    """Print one candidate against the base. Returns rows compared."""
    print(f"\n=== {cand_tag} vs {base_tag} · {metric} ===")
    header = f"{'corpus':<10} {'mode':<9} {'kind':<11} {'base':>7} {'cand':>7} {'delta':>7}"
    print(header)
    print("-" * len(header))

    compared = 0
    deltas = []
    for corpus in corpora:
        base, cand = load(base_tag, corpus), load(cand_tag, corpus)
        if not base or not cand:
            missing = base_tag if not base else cand_tag
            print(f"{corpus:<10} (no {missing} results)")
            continue
        for key in sorted(base.keys() & cand.keys()):
            mode, kind = key
            b, c = base[key].get(metric), cand[key].get(metric)
            if b is None or c is None:
                continue
            delta = c - b
            deltas.append(delta)
            compared += 1
            print(
                f"{corpus:<10} {mode:<9} {kind:<11} {b:>7.3f} {c:>7.3f} {fmt_delta(delta)}"
            )

    if deltas:
        mean = sum(deltas) / len(deltas)
        wins = sum(1 for d in deltas if d > 0)
        losses = sum(1 for d in deltas if d < 0)
        # Mean and win/loss together, because a lever that wins narrowly on many
        # cells and loses badly on one is a different thing from the reverse, and
        # either number alone hides that.
        print(f"\n{'':<32} mean {fmt_delta(mean)}   {wins} up / {losses} down / "
              f"{len(deltas) - wins - losses} flat")
    return compared


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", help="condition to compare against")
    ap.add_argument("--cand", nargs="+", help="one or more candidate conditions")
    ap.add_argument("--metric", default="recall@5", choices=METRICS)
    ap.add_argument("--corpora", default=",".join(CORPORA))
    ap.add_argument("--list", action="store_true", help="list conditions on disk")
    args = ap.parse_args()

    if args.list:
        tags = available_tags()
        print("conditions in eval/data:")
        for t in tags:
            have = [c for c in CORPORA if path_for(t, c)]
            print(f"  {t:<16} {' '.join(have)}")
        if not tags:
            print("  (none — run eval/levers.sh first)")
        return

    if not args.base or not args.cand:
        ap.error("--base and --cand are required (or use --list)")

    corpora = [c.strip() for c in args.corpora.split(",") if c.strip()]
    total = 0
    for cand in args.cand:
        total += compare(args.base, cand, args.metric, corpora)
    if total == 0:
        print("\nnothing compared — check `eval/diff.py --list`")


if __name__ == "__main__":
    main()
