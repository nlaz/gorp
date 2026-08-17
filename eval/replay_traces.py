#!/usr/bin/env python3
"""Replay tiered agent traces against real gold, and report by tier.

This is the free gate. `run_eval.py` scores queries we generated — 10-15
words, carrying the gold file's own identifier ~70% of the time — and §21-§23
established three times over that it cannot referee a ranking change, because
real agent queries are ~5 words and do not look like that. The instrument
that can is a replay of real harvested traffic against real gold, and it used
to live only in the bench repo, which meant gating an engine change required
a second checkout and a campaign's leftovers.

    python3 eval/replay_traces.py --trees ../gorp-bench/data/locbench/repos/trees
    python3 eval/replay_traces.py --tier blind --mode semantic --limit 500
    python3 eval/replay_traces.py --baseline base.json --out cand.json

**Report per tier, never pooled.** blind / guess / golden are different
retrieval problems: a golden query hands the engine the answer's name and a
blind one shares no vocabulary with it at all (§19.2b measured 13% against
50% on that contrast). A pooled number moves when the *mix* moves, so a
corpus refresh reads as an engine change. The tier split is the whole reason
this format exists.

The metric is **rank of the first gold file** in the returned hits, 0 when no
gold file came back. Files and not functions, deliberately: a function metric
needs both `rank_func` and `rank_func_ovl` to mean anything (§24.1 — they
differ by 14.2pp because chunks are 32 lines and the median gold function is
12), and computing both needs the gold span, which a harvested trace does not
carry. The bench repo's `guessplay.py` does that job on the full arm matrix.
This one answers the cheaper question completely rather than the expensive
one partially.

## Trees

A trace names a repo and a base commit; searching it needs that tree on disk.
`--trees DIR` points at a directory of `<owner>__<repo>__<sha12>/` checkouts —
gorp-bench already materializes exactly that under
`data/locbench/repos/trees`, so the normal invocation borrows them rather
than cloning 5 GB a second time. Instances with no tree are skipped and
counted, never silently dropped.
"""

import argparse
import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import traces  # noqa: E402
from run_eval import _bootstrap_ci, _score, _sign_test  # noqa: E402

GORP = Path(__file__).resolve().parents[1] / "target/release/gorp"
MODES = ("bm25", "semantic", "hybrid")


def tree_for(row, trees):
    """The checkout to search for this row, or None.

    Matches gorp-bench's `repo_key` naming (`owner__repo__sha12`) and falls
    back to any directory that starts with the instance's owner__repo, so a
    tree checked out at a different commit still answers rather than the
    whole instance vanishing from the report.
    """
    t = row["target"]
    repo, sha = (t.get("repo") or "").replace("/", "__"), (t.get("sha") or "")[:12]
    if not repo:
        return None
    exact = trees / f"{repo}__{sha}"
    if exact.is_dir():
        return exact
    for cand in sorted(trees.glob(f"{repo}__*")):
        if cand.is_dir():
            return cand
    return None


def rank_of_gold(hits, gold_files):
    """1-based rank of the first hit in a gold file, or 0 for a miss.

    Paths compare by suffix as well as equality: gorp prints paths relative
    to the scope it was given, and a scoped search therefore returns
    `db_manager.py` where gold says `backend/services/db_manager.py`. Equality
    alone scored those as misses — the same asymmetry `scoring.py` documents
    for rg.
    """
    for i, h in enumerate(hits, 1):
        p = h.get("path") or ""
        for g in gold_files:
            if p == g or g.endswith("/" + p) or p.endswith("/" + g):
                return i
    return 0


def run_one(binary, tree, query, mode, k, cache_dir):
    """One ranked search. Returns hits, or None if the engine failed.

    A crash is not a miss: exit codes outside {0, 1} mean the tool broke, and
    counting that as "gold not found" is how a broken binary scores 0.00
    across a whole set and looks like a measurement (`run_eval.py` makes the
    same distinction for the same reason).
    """
    cmd = [str(binary), query, str(tree), "--json", "-k", str(k), "--mode", mode]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=120,
                           env={"GORP_CACHE_DIR": str(cache_dir), "HOME": str(Path.home()),
                                "PATH": "/usr/bin:/bin", "GORP_NO_HINTS": "1",
                                # Caching is opt-in (2026-08-16); the replay
                                # wants query 2..n of a tree served warm.
                                "GORP_AUTO_INDEX": "1"})
    except subprocess.TimeoutExpired:
        return None
    if p.returncode not in (0, 1):
        return None
    hits = []
    for line in p.stdout.splitlines():
        line = line.strip()
        if line:
            try:
                hits.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return hits


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--queries", type=Path, default=HERE / "queries/traces-v1.jsonl")
    ap.add_argument("--trees", type=Path, required=True,
                    help="directory of <owner>__<repo>__<sha12>/ checkouts")
    ap.add_argument("--mode", default="hybrid", choices=MODES)
    ap.add_argument("--tier", choices=traces.TIERS, help="score one tier only")
    ap.add_argument("--repo", help="score one target repo only (owner/name), "
                                   "which is how you iterate against a single "
                                   "checked-out tree")
    ap.add_argument("-k", type=int, default=5)
    ap.add_argument("--limit", type=int, help="first N rows (after tier filter)")
    ap.add_argument("--binary", type=Path, default=GORP)
    ap.add_argument("--cache-dir", type=Path,
                    help="index cache (default: a temp dir, so a replay never "
                         "poisons ordinary use)")
    ap.add_argument("--out", type=Path, help="write per-row results as JSON")
    ap.add_argument("--baseline", type=Path, help="compare against an --out file")
    ap.add_argument("--resamples", type=int, default=2000)
    args = ap.parse_args()

    if not args.binary.exists():
        sys.exit(f"no binary at {args.binary} — cargo build --release")

    rows = traces.load(args.queries)
    if args.tier:
        rows = [r for r in rows if r["tier"] == args.tier]
    if args.repo:
        rows = [r for r in rows if (r["target"].get("repo") or "") == args.repo]
    if args.limit:
        rows = rows[:args.limit]

    import tempfile
    tmp = None
    if args.cache_dir:
        cache = args.cache_dir
    else:
        tmp = tempfile.TemporaryDirectory(prefix="gorp-replay-cache-")
        cache = Path(tmp.name)

    scored, skipped = [], defaultdict(int)
    for n, row in enumerate(rows, 1):
        tree = tree_for(row, args.trees)
        if tree is None:
            skipped["no-tree"] += 1
            continue
        hits = run_one(args.binary, tree, row["query"], args.mode, args.k, cache)
        if hits is None:
            skipped["engine-error"] += 1
            continue
        scored.append({
            "id": row["id"], "tier": row["tier"],
            "rank": rank_of_gold(hits, row["gold"]["files"]),
        })
        if n % 100 == 0:
            print(f"\r  {n}/{len(rows)}", end="", file=sys.stderr, flush=True)
    print("\r", end="", file=sys.stderr)

    by_tier = defaultdict(list)
    for r in scored:
        by_tier[r["tier"]].append(r)

    print(f"{args.queries.name}  mode={args.mode}  k={args.k}  "
          f"{len(scored)}/{len(rows)} scored")
    if skipped:
        # Named, not netted out: "no tree" is a coverage gap and
        # "engine error" is a bug, and a single skipped count hides which.
        print("  skipped: " + "  ".join(f"{k}={v}" for k, v in sorted(skipped.items())))
    print(f"\n  {'tier':8} {'n':>6} {'recall@1':>9} {'recall@5':>9} {'mrr@10':>8}")
    for tier in traces.TIERS:
        rs = by_tier.get(tier) or []
        if not rs:
            continue
        n = len(rs)
        r1 = sum(_score(r["rank"], "recall@1") for r in rs) / n
        r5 = sum(_score(r["rank"], "recall@5") for r in rs) / n
        mrr = sum(_score(r["rank"], "mrr@10") for r in rs) / n
        print(f"  {tier:8} {n:6d} {r1:9.3f} {r5:9.3f} {mrr:8.3f}")

    if args.baseline:
        base = {r["id"]: r for r in json.loads(args.baseline.read_text())["rows"]}
        print(f"\n  vs {args.baseline.name} (paired, {args.resamples} resamples)")
        for tier in traces.TIERS:
            rs = [r for r in by_tier.get(tier) or [] if r["id"] in base]
            if not rs:
                continue
            a = [r["rank"] for r in rs]
            b = [base[r["id"]]["rank"] for r in rs]
            pt, lo, hi = _bootstrap_ci(a, b, "recall@5", args.resamples)
            p = _sign_test(a, b, "recall@5")
            flag = "" if lo <= 0 <= hi else "  *"
            print(f"  {tier:8} {len(rs):6d}  Δrecall@5 {pt:+.3f} "
                  f"[{lo:+.3f}, {hi:+.3f}]  sign p={p:.3f}{flag}")

    if args.out:
        args.out.write_text(json.dumps({
            "queries": str(args.queries), "mode": args.mode, "k": args.k,
            "skipped": dict(skipped), "rows": scored,
        }, indent=1))
        print(f"\nwrote {args.out}")
    if tmp:
        tmp.cleanup()


if __name__ == "__main__":
    main()
