#!/usr/bin/env python3
"""Re-cut an existing result file into blind / named strata (RESEARCH.md §15).

Zero scan cost: result rows store per-query `ranks` in row order within each
(mode, kind) cell, so ranks join back to the query rows positionally. This
re-reads the queries, computes the §15.3 strict-blind predicate per row, and
re-aggregates each cell into its blind and named substrata — a first
empirical read of the blind-search thesis from measurements already paid for.

    python3 eval/blind_cut.py eval/results/lever-cosqa-preproc-split-sif.json \
        eval/queries/cosqa-1200.jsonl eval/data/cosqa/corpus \
        --compare semantic,bm25

Rows without a `symbol` field (CoSQA's whole-file anchor) get a best-effort
one: the first `def NAME` in a .py gold. That only *adds* hits, so a row this
misses reads as blind-optimistic; the direction of any error is reported.
"""

import argparse
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import leakage  # noqa: E402
from run_eval import _bootstrap_ci, _score, _sign_test  # noqa: E402

METRICS = ["recall@1", "recall@5", "recall@10", "mrr@10"]
_DEF = re.compile(r"^\s*def\s+(\w+)", re.M)


def load_rows(queries_fp):
    return [json.loads(l) for l in Path(queries_fp).read_text().splitlines() if l.strip()]


def blind_flags(rows, corpus):
    """Per query-row: True (blind) / False (named) / None (gold unreadable)."""
    flags, guessed = [], 0
    for row in rows:
        gold = leakage._gold_text(row, corpus)
        if gold is None:
            flags.append(None)
            continue
        if "symbol" not in row and row["file"].endswith(".py"):
            m = _DEF.search(gold)
            if m:
                row = {**row, "symbol": m.group(1)}
                guessed += 1
        flags.append(leakage.is_blind(row, gold))
    return flags, guessed


def cut(results, rows, flags):
    """-> {(mode, kind): {"blind": [ranks], "named": [ranks]}}."""
    by_kind = {}
    for i, r in enumerate(rows):
        by_kind.setdefault(r.get("kind", "?"), []).append(i)
    out = {}
    for cell in results:
        key = (cell["mode"], cell["kind"])
        idxs = by_kind.get(cell["kind"])
        if idxs is None or len(idxs) != len(cell["ranks"]):
            raise SystemExit(
                f"cannot join {key}: {len(cell['ranks'])} ranks vs "
                f"{len(idxs or [])} query rows — was the run filtered?")
        strata = {"blind": [], "named": []}
        for qi, rank in zip(idxs, cell["ranks"]):
            if flags[qi] is None:
                continue
            strata["blind" if flags[qi] else "named"].append(rank)
        out[key] = strata
    return out


def agg(ranks):
    return {m: sum(_score(r, m) for r in ranks) / len(ranks) if ranks else None
            for m in METRICS}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results", type=Path)
    ap.add_argument("queries", type=Path)
    ap.add_argument("corpus", type=Path)
    ap.add_argument("--compare", default="",
                    help="two modes, e.g. 'semantic,bm25': paired delta per stratum")
    ap.add_argument("--bootstrap", type=int, default=2000)
    args = ap.parse_args()

    results = json.loads(args.results.read_text())
    rows = load_rows(args.queries)
    flags, guessed = blind_flags(rows, args.corpus)
    n_blind = sum(1 for f in flags if f is True)
    n_named = sum(1 for f in flags if f is False)
    n_skip = sum(1 for f in flags if f is None)
    print(f"{args.queries.name}: {n_blind} blind / {n_named} named"
          + (f" / {n_skip} unreadable-gold skipped" if n_skip else "")
          + (f" ({guessed} symbols guessed from `def` — misses read blind-optimistic)"
             if guessed else ""))

    cells = cut(results, rows, flags)
    print(f"\n{'mode':<10} {'kind':<11} {'stratum':<7} {'n':>5} "
          + " ".join(f"{m:>9}" for m in METRICS))
    for (mode, kind), strata in sorted(cells.items()):
        for name in ("blind", "named"):
            a = agg(strata[name])
            vals = " ".join(
                f"{a[m]:>9.3f}" if a[m] is not None else f"{'--':>9}" for m in METRICS)
            print(f"{mode:<10} {kind:<11} {name:<7} {len(strata[name]):>5} {vals}")

    if args.compare:
        ma, mb = args.compare.split(",")
        for kind in sorted({k for (_, k) in cells}):
            if (ma, kind) not in cells or (mb, kind) not in cells:
                continue
            print(f"\npaired {ma} - {mb}, kind={kind}:")
            for name in ("blind", "named"):
                a, b = cells[(ma, kind)][name], cells[(mb, kind)][name]
                if len(a) != len(b) or not a:
                    continue
                for m in METRICS:
                    d, lo, hi = _bootstrap_ci(a, b, m, args.bootstrap)
                    st = _sign_test(a, b, m)
                    stx = f"  w{st[0]}/l{st[1]} p={st[2]:.4f}" if st else ""
                    print(f"  {name:<7} {m:>9}: d={d:+.3f} CI[{lo:+.3f},{hi:+.3f}]{stx}")


if __name__ == "__main__":
    main()
