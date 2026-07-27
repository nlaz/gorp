#!/usr/bin/env python3
"""Score retrieval quality: recall@1/5/10 and MRR@10 per tool/mode.

A hit is correct if it lands in the ground-truth file with a line span
overlapping the truth window (± --slack lines).

Conditions:
  semgrep:bm25 / semgrep:semantic / semgrep:hybrid  — `semgrep --json -k 10` (uses .semgrep if present)
  rg:agent-style                        — how an agent uses ripgrep for an NL
      intent: try the exact phrase, then AND the two rarest content words,
      then OR them; first 10 file:line hits count. This fallback is the
      honest baseline, not a strawman: it mirrors common agent behavior.

Usage:
  python3 eval/run_eval.py eval/data/linux.jsonl bench/corpora/linux \
      [--modes bm25,semantic,hybrid,rg] [--k 10] [--slack 10] [--no-index]
"""

import argparse
import json
import re
import subprocess
from collections import defaultdict
from pathlib import Path

import os
HERE = Path(__file__).parent
SEMGREP = Path(os.environ.get("SEMGREP_BIN", HERE.parent / "target/release/semgrep"))
RG = "/opt/homebrew/bin/rg"

STOPWORDS = set("""a an and are as at be by code does file find for from how in is
it its of on or that the this to what when where which who why with""".split())


def semgrep_search(query, corpus, mode, k, no_index, extra=()):
    cmd = [str(SEMGREP), "--mode", mode, "--json", "-k", str(k)]
    if no_index:
        cmd.append("--no-index")
    cmd += list(extra) + [query, str(corpus)]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    hits = []
    for line in proc.stdout.splitlines():
        try:
            h = json.loads(line)
            hits.append((h["path"], h["start_line"], h["end_line"]))
        except (json.JSONDecodeError, KeyError):
            continue
    return hits


def rg_run(pattern, corpus, k, flags=()):
    proc = subprocess.run(
        [RG, "--no-heading", "-n", "-m", "3", *flags, pattern, str(corpus)],
        capture_output=True, text=True, timeout=600)
    hits = []
    for line in proc.stdout.splitlines():
        m = re.match(r"(.+?):(\d+):", line)
        if m:
            hits.append((str(Path(m.group(1)).relative_to(corpus))
                         if m.group(1).startswith(str(corpus)) else m.group(1),
                         int(m.group(2)), int(m.group(2))))
        if len(hits) >= k:
            break
    return hits[:k]


def rg_agent_style(query, corpus, k):
    words = [w for w in re.findall(r"[a-zA-Z0-9]+", query.lower()) if w not in STOPWORDS]
    rare = sorted(words, key=len, reverse=True)[:2]
    for attempt in ([query], [".*".join(rare)], ["|".join(rare)] if len(rare) > 1 else []):
        hits = rg_run(attempt[0], corpus, k, flags=("-i",))
        if hits:
            return hits
    return []


def correct(hit, truth, slack):
    path, s, e = hit
    return (path == truth["file"]
            and s <= truth["end_line"] + slack
            and e >= truth["start_line"] - slack)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("queries", type=Path)
    ap.add_argument("corpus", type=Path)
    ap.add_argument("--modes", default="bm25,semantic,hybrid,rg")
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--slack", type=int, default=10)
    ap.add_argument("--no-index", action="store_true")
    ap.add_argument("--extra", default="",
                    help="extra semgrep CLI args, e.g. '--sem-weight 0.3 --no-diversify'")
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()
    extra = tuple(args.extra.split()) if args.extra else ()

    rows = [json.loads(l) for l in args.queries.read_text().splitlines() if l.strip()]
    modes = args.modes.split(",")
    # metric accumulators: (mode, kind) -> list of first-correct ranks (None = miss)
    ranks = defaultdict(list)

    for i, truth in enumerate(rows):
        for mode in modes:
            if mode == "rg":
                hits = rg_agent_style(truth["query"], args.corpus, args.k)
            else:
                hits = semgrep_search(truth["query"], args.corpus, mode, args.k, args.no_index, extra)
            rank = next((r + 1 for r, h in enumerate(hits) if correct(h, truth, args.slack)), None)
            ranks[(mode, truth["kind"])].append(rank)
        if (i + 1) % 25 == 0:
            print(f"  {i + 1}/{len(rows)} queries")

    print(f"\n{'condition':<24} {'n':>4} {'R@1':>6} {'R@5':>6} {'R@10':>6} {'MRR@10':>7}")
    results = []
    for (mode, kind), rs in sorted(ranks.items()):
        n = len(rs)
        r1 = sum(1 for r in rs if r == 1) / n
        r5 = sum(1 for r in rs if r and r <= 5) / n
        r10 = sum(1 for r in rs if r and r <= 10) / n
        mrr = sum(1 / r for r in rs if r) / n
        print(f"{mode + ':' + kind:<24} {n:>4} {r1:>6.2f} {r5:>6.2f} {r10:>6.2f} {mrr:>7.3f}")
        results.append({"mode": mode, "kind": kind, "n": n,
                        "recall@1": round(r1, 3), "recall@5": round(r5, 3),
                        "recall@10": round(r10, 3), "mrr@10": round(mrr, 3)})
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(results, indent=2))
        print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
