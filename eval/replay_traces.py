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


def run_one(binary, tree, query, mode, k, cache_dir, daemon=False):
    """One ranked search. Returns (hits, served_by_daemon), or None if the
    engine failed.

    A crash is not a miss: exit codes outside {0, 1} mean the tool broke, and
    counting that as "gold not found" is how a broken binary scores 0.00
    across a whole set and looks like a measurement (`run_eval.py` makes the
    same distinction for the same reason).

    GORP_DAEMON is always pinned. A daemon-featured binary spawns a per-repo
    daemon on the second search of a scope, and an *unset* variable here would
    let the baseline arm quietly turn into the daemon arm mid-replay.
    """
    cmd = [str(binary), query, str(tree), "--json", "-k", str(k), "--mode", mode]
    env = {"GORP_CACHE_DIR": str(cache_dir), "HOME": str(Path.home()),
           "PATH": "/usr/bin:/bin", "GORP_NO_HINTS": "1"}
    if daemon:
        # --stats is the provenance channel: `daemon=true` on the stats line
        # is how each row proves which arm actually served it. Auto-index
        # stays on for the rows the daemon declines (~18% of harvested
        # queries carry a `|` and split into phrases the daemon won't take):
        # cold == warm is enforced, so a warm fallback returns the same ranks
        # a cold one would and only the wall clock changes.
        cmd.append("--stats")
        env["GORP_DAEMON"] = "1"
        env["GORP_AUTO_INDEX"] = "1"
    else:
        # Caching is opt-in (2026-08-16); the replay wants query 2..n of a
        # tree served warm.
        env["GORP_AUTO_INDEX"] = "1"
        env["GORP_DAEMON"] = "0"
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=120, env=env)
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
    return hits, ("daemon=true" in p.stderr)


class DaemonPool:
    """One pre-warmed `gorp daemon` at a time, following the replay from tree
    to tree (rows arrive grouped by tree in daemon mode).

    Pre-warmed, because a filling daemon declines every query — its manifest
    diff reads as "not ready" — and a replay that races the fill measures the
    fallback path while calling it the daemon arm. Warmth is read from the
    daemon's own stderr ("daemon warm"), which costs nothing, instead of
    probe queries, which each pay a cold stream while the fill runs.
    """

    def __init__(self, binary, cache_dir, warm_secs):
        self.binary, self.cache, self.warm_secs = binary, cache_dir, warm_secs
        self.tree, self.proc, self.log = None, None, None
        self.warm_failures = 0

    def ensure(self, tree):
        import tempfile
        import time
        if self.tree == tree:
            return
        self.stop()
        self.tree = tree
        self.log = tempfile.NamedTemporaryFile(prefix="gorp-daemon-", suffix=".log",
                                               delete=False, mode="w")
        self.proc = subprocess.Popen(
            [str(self.binary), "daemon", str(tree)],
            stdout=subprocess.DEVNULL, stderr=self.log,
            env={"GORP_CACHE_DIR": str(self.cache), "HOME": str(Path.home()),
                 "PATH": "/usr/bin:/bin"})
        deadline = time.monotonic() + self.warm_secs
        logpath = Path(self.log.name)
        while time.monotonic() < deadline:
            text = logpath.read_text(errors="replace")
            if "daemon warm" in text or "already serves" in text:
                return
            if self.proc.poll() is not None:
                print(f"\n  daemon for {tree.name} exited early:\n{text[-500:]}",
                      file=sys.stderr)
                self.warm_failures += 1
                return
            time.sleep(0.25)
        print(f"\n  daemon for {tree.name} not warm after {self.warm_secs}s — "
              f"its rows will fall back to the local path", file=sys.stderr)
        self.warm_failures += 1

    def stop(self):
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        if self.log is not None:
            self.log.close()
            Path(self.log.name).unlink(missing_ok=True)
        self.tree, self.proc, self.log = None, None, None


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
    ap.add_argument("--daemon", action="store_true",
                    help="the daemon arm: pre-warm a `gorp daemon` per tree "
                         "and serve queries through it (needs a binary built "
                         "with --features daemon). Rows are grouped by tree, "
                         "and each row records whether the daemon actually "
                         "answered it — the arm verifies itself.")
    ap.add_argument("--daemon-warm-secs", type=int, default=900,
                    help="how long to wait for a tree's daemon fill")
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

    pairs = []
    scored, skipped = [], defaultdict(int)
    for row in rows:
        tree = tree_for(row, args.trees)
        if tree is None:
            skipped["no-tree"] += 1
            continue
        pairs.append((row, tree))
    if args.daemon:
        # Grouped by tree so one daemon serves each tree's whole run of rows;
        # stable within a tree, so --limit stays reproducible.
        pairs.sort(key=lambda p: str(p[1]))

    pool = DaemonPool(args.binary, cache, args.daemon_warm_secs) if args.daemon else None
    served = 0
    for n, (row, tree) in enumerate(pairs, 1):
        if pool is not None:
            pool.ensure(tree)
        out = run_one(args.binary, tree, row["query"], args.mode, args.k, cache,
                      daemon=args.daemon)
        if out is None:
            skipped["engine-error"] += 1
            continue
        hits, via_daemon = out
        served += via_daemon
        scored.append({
            "id": row["id"], "tier": row["tier"],
            "rank": rank_of_gold(hits, row["gold"]["files"]),
            **({"daemon": via_daemon} if args.daemon else {}),
        })
        if n % 100 == 0:
            print(f"\r  {n}/{len(pairs)}", end="", file=sys.stderr, flush=True)
    if pool is not None:
        pool.stop()
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
    if args.daemon:
        # The arm verifying itself: a row the daemon declined answered locally
        # (identical engine, so its rank is still valid), but a low fraction
        # means this run measured mostly the fallback and should say so.
        note = f" ({pool.warm_failures} tree(s) never warmed)" if pool.warm_failures else ""
        print(f"  daemon served {served}/{len(scored)} rows{note}")
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

    # The per-row file writes before any comparison prints: an hour of replay
    # must never be lost to a formatting bug in the analysis below (it was,
    # once).
    if args.out:
        args.out.write_text(json.dumps({
            "queries": str(args.queries), "mode": args.mode, "k": args.k,
            "skipped": dict(skipped), "rows": scored,
        }, indent=1))
        print(f"\nwrote {args.out}")

    if args.baseline:
        base = {r["id"]: r for r in json.loads(args.baseline.read_text())["rows"]}
        print(f"\n  vs {args.baseline.name} (paired, {args.resamples} resamples)")
        for tier in traces.TIERS:
            rs = [r for r in by_tier.get(tier) or [] if r["id"] in base]
            # In daemon mode, compare only the rows the daemon actually
            # served: the rest ran the identical local engine in both arms,
            # and diluting the contrast 4:1 with guaranteed ties is how a
            # real difference reads as noise.
            if args.daemon:
                rs = [r for r in rs if r.get("daemon")]
            if not rs:
                continue
            a = [r["rank"] for r in rs]
            b = [base[r["id"]]["rank"] for r in rs]
            pt, lo, hi = _bootstrap_ci(a, b, "recall@5", args.resamples)
            wins, losses, p = _sign_test(a, b, "recall@5")
            flag = "" if lo <= 0 <= hi else "  *"
            print(f"  {tier:8} {len(rs):6d}  Δrecall@5 {pt:+.3f} "
                  f"[{lo:+.3f}, {hi:+.3f}]  sign {wins}W/{losses}L p={p:.3f}{flag}")
    if tmp:
        tmp.cleanup()


if __name__ == "__main__":
    main()
