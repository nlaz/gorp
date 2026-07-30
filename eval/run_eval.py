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
import hashlib
import json
import math
import random
import re
import subprocess
from collections import defaultdict
from pathlib import Path

import os
import sys
HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))
SEMGREP = Path(os.environ.get("SEMGREP_BIN", HERE.parent / "target/release/semgrep"))
RG = "/opt/homebrew/bin/rg"

# The identifier predicate and the stopword list are shared with leakage.py on
# purpose: the leakage percentage printed above every results table must be
# the same predicate that decides the rg-strong baseline, or the two drift and
# §12.1's audit stops meaning anything.
import leakage  # noqa: E402
from leakage import STOPWORDS, content_tokens, identifiers  # noqa: E402
from validate_queries import format_problems, validate  # noqa: E402


def semgrep_search(query, corpus, mode, k, no_index, extra=()):
    cmd = [str(SEMGREP), "--mode", mode, "--json", "-k", str(k)]
    if no_index:
        cmd.append("--no-index")
    cmd += list(extra) + [query, str(corpus)]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    # Exit 2 is "something went wrong", as distinct from 1 = "no match". A binary
    # that cannot answer must stop the run, not score zero: this silently
    # reported 0.00 across 400 queries when handed a binary whose embedding
    # width did not match the index, and the result looked like a measurement.
    if proc.returncode not in (0, 1):
        raise RuntimeError(
            f"semgrep failed (exit {proc.returncode}) on {mode!r} query {query!r}\n"
            f"  cmd: {' '.join(cmd)}\n"
            f"  stderr: {proc.stderr.strip()[:400]}"
        )
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
    """Legacy baseline. Kept for comparability with published numbers — but
    it is weaker than a competent agent, and knowingly so:

    `[a-zA-Z0-9]+` excludes `_`, so `blkg_rwstat_add` is shredded to
    blkg/rwstat/add before ripgrep sees it, and "rarest" is approximated by
    *longest*, which on a typical direct query picks generic words like
    "function"/"choosing" over the identifier. Use `rg-strong` for the fair
    comparison; scoring both is the only way to know how much of our
    reported gap was the baseline rather than the engine.
    """
    words = [w for w in re.findall(r"[a-zA-Z0-9]+", query.lower()) if w not in STOPWORDS]
    rare = sorted(words, key=len, reverse=True)[:2]
    # The attempt list used to be built inline as
    #   ([query], [".*".join(rare)], ["|".join(rare)] if len(rare) > 1 else [])
    # and indexed with attempt[0], so a query with <=1 content word raised
    # IndexError once the first two patterns missed, and a query with none at
    # all grepped for "" — which matches every line and scores as a hit.
    # Neither could affect a published number: 0 of the 2,398 queries in
    # linux/vscode/wikipedia/cosqa have <=1 content word. Both fire constantly
    # on real agent queries, where 195/1194 (16%) have <=1 and 6 have none.
    attempts = [query]
    if rare:
        attempts.append(".*".join(rare))
    if len(rare) > 1:
        attempts.append("|".join(rare))
    for pat in _dedupe(attempts):
        hits = rg_run(pat, corpus, k, flags=("-i",))
        if hits:
            return hits
    return []


def _dedupe(patterns):
    """Drop empties and repeats, preserving order.

    Cannot change a result — the same pattern returns the same hits, and the
    first occurrence already decided it. It removes wasted full-corpus scans:
    a one-word query like `init` produced the pattern twice, and §12.4 puts
    the cost of a kernel scan at ~3 s.
    """
    seen, out = set(), []
    for p in patterns:
        if p and p not in seen:
            seen.add(p)
            out.append(p)
    return out


def rg_strong(query, corpus, k):
    """What a competent agent actually does with ripgrep.

    Identifiers survive tokenization, and any identifier-shaped token in the
    query is tried FIRST — if the question contains `parse_config`, grepping
    for it is the obvious move and it is nearly free to get right.
    """
    for pat in rg_strong_attempts(query):
        hits = rg_run(pat, corpus, k, flags=("-i",))
        if hits:
            return hits
    return []


def rg_strong_attempts(query):
    """The ordered patterns `rg_strong` tries. Split out so the baseline is
    testable without invoking ripgrep, and pinned byte-for-byte against a
    fixture captured before `identifiers()` moved to leakage.py — if this
    list moves, §12.1's and §12.2's published percentages move with it.

    A *true* identifier is snake_case or camelCase; see `leakage.identifiers`
    for why long lowercase words are deliberately excluded.
    """
    toks = content_tokens(query)
    idents = identifiers(toks)
    rare = sorted(toks, key=len, reverse=True)[:2]

    attempts = []
    attempts += [re.escape(i) for i in idents[:2]]      # the obvious symbol
    attempts.append(re.escape(query))                    # exact phrase
    if len(rare) > 1:
        attempts.append(".*".join(re.escape(r) for r in rare))   # both on a line
        attempts.append("|".join(re.escape(r) for r in rare))    # either
    # An empty pattern matches every line, so a blank query would score hits
    # it did not earn. No published query is empty; real agent queries include
    # several (see the note in rg_agent_style).
    return _dedupe(attempts)


def correct(hit, truth, slack):
    path, s, e = hit
    return (path == truth["file"]
            and s <= truth["end_line"] + slack
            and e >= truth["start_line"] - slack)


# ---------------------------------------------------------------------------
# strata and filtering
#
# generate.py has written `split`, `symbol`, `symbol_kind`, `lang`, `has_doc`
# and `n_lines` onto every row since the symbol-anchored sampler landed, and
# this scorer read none of them — the dev/locked discipline §11.5 asked for
# was unenforceable because nothing could filter on it. Rather than special-
# casing `split`, any row field works, and so does any computed leakage field,
# which is what the path-leakage question needs.
# ---------------------------------------------------------------------------

def apply_where(rows, where):
    """Keep rows matching every `k=v` clause. Values compare as strings so
    `has_doc=True` and `n_lines=12` work without type declarations."""
    if not where:
        return rows
    clauses = []
    for c in where.split(","):
        if "=" not in c:
            raise SystemExit(f"--where clause {c!r} is not k=v")
        k, v = c.split("=", 1)
        clauses.append((k.strip(), v.strip()))
    for k, _ in clauses:
        if not any(k in r for r in rows):
            raise SystemExit(
                f"--where field {k!r} is on no row; available: "
                f"{sorted(set().union(*(r.keys() for r in rows)))}")
    return [r for r in rows
            if all(str(r.get(k)) == v for k, v in clauses)]


def cell_of(row, strata, corpus=None):
    """The results-table cell a row belongs to: its kind, plus any strata.

    An absent field is an error rather than a silent bucket. A stratum that
    quietly lumps every row into `None` produces a table that looks like a
    breakdown and is not one.
    """
    parts = [str(row.get("kind", "?"))]
    lk = None
    for s in strata:
        if s in row:
            parts.append(f"{s}={row[s]}")
            continue
        if lk is None:
            lk = leakage.leakage(row, corpus)
        if s in lk:
            parts.append(f"{s}={lk[s]}")
            continue
        raise SystemExit(
            f"--stratify field {s!r} is neither a row field nor a leakage field "
            f"({sorted(lk)})")
    return "|".join(parts)


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
    ap.add_argument("--baseline", type=Path, default=None,
                    help="a previous --out file; report PAIRED deltas with "
                         "bootstrap CIs and a sign test instead of bare numbers")
    ap.add_argument("--bootstrap", type=int, default=2000,
                    help="bootstrap resamples for the delta CI")
    ap.add_argument("--compare-modes", default="",
                    help="paired stats between two modes in THIS run, "
                         "e.g. 'hybrid,rg' — the semgrep-vs-ripgrep claim")
    ap.add_argument("--stratify", default="",
                    help="break the table down by row fields or computed "
                         "leakage fields, e.g. 'has_identifier' or 'lang,has_doc'")
    ap.add_argument("--where", default="",
                    help="keep only matching rows, e.g. 'split=dev'")
    ap.add_argument("--allow-stale", action="store_true",
                    help="score even if gold spans no longer match the corpus")
    args = ap.parse_args()
    extra = tuple(args.extra.split()) if args.extra else ()

    raw = args.queries.read_text()
    rows = [json.loads(l) for l in raw.splitlines() if l.strip()]
    if not rows:
        raise SystemExit(f"{args.queries} has no rows — nothing to score")

    # A stale query set does not fail loudly, it scores 0 and looks like a
    # measurement. Check before spending an hour of scans on it.
    problems, vstats = validate(rows, args.corpus)
    if problems:
        print(f"query set does not match the corpus: {vstats['n_problems']} problems")
        print(format_problems(problems))
        if not args.allow_stale:
            raise SystemExit(
                "refusing to score a drifted query set (--allow-stale to override)")
        print("--allow-stale: scoring anyway; affected rows will read as misses")
    if vstats["n_unverifiable"]:
        print(f"note: {vstats['n_unverifiable']}/{vstats['n_rows']} rows carry no "
              f"gold_sha — an edited gold file would not be detected for those")

    rows = apply_where(rows, args.where)
    if not rows:
        raise SystemExit(f"--where {args.where!r} matched no rows")

    # The fingerprint covers the filter and the strata as well as the file, so
    # a filtered run can never be --baseline-compared against an unfiltered one.
    queries_fp = hashlib.sha256(
        (raw + "\0" + args.where + "\0" + args.stratify).encode()).hexdigest()[:16]

    lk = leakage.summarize(rows, args.corpus)
    print()
    print(leakage.format_summary(lk, f"({args.queries.name})"))

    strata = [s for s in args.stratify.split(",") if s]
    modes = args.modes.split(",")
    # metric accumulators: (mode, cell) -> list of first-correct ranks (None = miss)
    ranks = defaultdict(list)

    for i, truth in enumerate(rows):
        cell = cell_of(truth, strata, args.corpus)
        for mode in modes:
            if mode == "rg":
                hits = rg_agent_style(truth["query"], args.corpus, args.k)
            elif mode == "rg-strong":
                hits = rg_strong(truth["query"], args.corpus, args.k)
            else:
                hits = semgrep_search(truth["query"], args.corpus, mode, args.k, args.no_index, extra)
            rank = next((r + 1 for r, h in enumerate(hits) if correct(h, truth, args.slack)), None)
            ranks[(mode, cell)].append(rank)
        if (i + 1) % 25 == 0:
            print(f"  {i + 1}/{len(rows)} queries")

    assert sum(len(v) for v in ranks.values()) == len(rows) * len(modes), \
        "strata dropped rows — every query must land in exactly one cell"

    width = max([24] + [len(f"{m}:{c}") for (m, c) in ranks])
    print(f"\n{'condition':<{width}} {'n':>4} {'R@1':>6} {'R@5':>6} {'R@10':>6} {'MRR@10':>7}")
    results = []
    for (mode, kind), rs in sorted(ranks.items()):
        n = len(rs)
        r1 = sum(1 for r in rs if r == 1) / n
        r5 = sum(1 for r in rs if r and r <= 5) / n
        r10 = sum(1 for r in rs if r and r <= 10) / n
        mrr = sum(1 / r for r in rs if r) / n
        print(f"{mode + ':' + kind:<{width}} {n:>4} {r1:>6.2f} {r5:>6.2f} {r10:>6.2f} {mrr:>7.3f}")
        results.append({"mode": mode, "kind": kind, "n": n,
                        "recall@1": round(r1, 3), "recall@5": round(r5, 3),
                        "recall@10": round(r10, 3), "mrr@10": round(mrr, 3),
                        # Per-query first-correct rank (null = miss). Required
                        # for paired statistics; aggregates alone cannot say
                        # whether a 0.01 delta is signal.
                        "ranks": rs,
                        "queries_fp": queries_fp})
    if args.compare_modes:
        a, b = args.compare_modes.split(",")
        compare_modes(results, a.strip(), b.strip(), args.bootstrap)
    if args.baseline:
        compare(results, args.baseline, args.bootstrap)

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(results, indent=2))
        print(f"wrote {args.out}")


# ---------------------------------------------------------------------------
# paired statistics
#
# Every retrieval delta this project has argued about has been 1-5 points on
# ~200 queries per cell, which is inside the noise. Point estimates alone
# invited reading +0.010 as an improvement; these functions make a comparison
# state its uncertainty, and refuse to call a win when the CI spans zero.
# ---------------------------------------------------------------------------

METRICS = ("recall@1", "recall@5", "recall@10", "mrr@10")


def _score(rank, metric):
    """Per-query contribution, so a metric is just a mean over queries."""
    if metric == "mrr@10":
        return 1.0 / rank if rank else 0.0
    k = int(metric.split("@")[1])
    return 1.0 if (rank and rank <= k) else 0.0


def _bootstrap_ci(a, b, metric, n_resamples, seed=1):
    """Percentile CI for mean(cand) - mean(base), resampling QUERIES (the
    unit of independence) rather than scores."""
    rng = random.Random(seed)
    n = len(a)
    sa = [_score(r, metric) for r in a]
    sb = [_score(r, metric) for r in b]
    point = sum(sa) / n - sum(sb) / n
    deltas = []
    for _ in range(n_resamples):
        idx = [rng.randrange(n) for _ in range(n)]
        deltas.append(sum(sa[i] for i in idx) / n - sum(sb[i] for i in idx) / n)
    deltas.sort()
    lo = deltas[int(0.025 * n_resamples)]
    hi = deltas[min(int(0.975 * n_resamples), n_resamples - 1)]
    return point, lo, hi


def _sign_test(a, b, metric):
    """Exact two-sided sign test over queries where the two disagree."""
    if metric == "mrr@10":
        return None
    k = int(metric.split("@")[1])
    hit = lambda r: bool(r and r <= k)
    wins = sum(1 for x, y in zip(a, b) if hit(x) and not hit(y))
    loss = sum(1 for x, y in zip(a, b) if hit(y) and not hit(x))
    n = wins + loss
    if n == 0:
        return wins, loss, 1.0
    tail = sum(math.comb(n, i) for i in range(0, min(wins, loss) + 1))
    return wins, loss, min(1.0, 2 * tail / 2 ** n)


def compare_modes(results, a, b, n_resamples):
    """Paired stats between two modes scored on the SAME queries in one run.

    Every query was answered by both engines, so this is a paired design with
    no run-to-run variance at all — the cleanest form of the comparison the
    project actually makes: ranked search vs ripgrep.
    """
    by = {(r["mode"], r["kind"]): r for r in results}
    kinds = sorted({k for (_, k) in by})
    print(f"\n=== {a} vs {b}, paired on the same queries "
          f"(bootstrap {n_resamples}x, 95% CI) ===")
    print(f"{'kind':<12} {'metric':<10} {b:>7} {a:>7} {'delta':>8} "
          f"{'95% CI':>19} {'w-l':>9} {'p':>7}  verdict")
    for kind in kinds:
        ra, rb = by.get((a, kind)), by.get((b, kind))
        if not ra or not rb or "ranks" not in ra or "ranks" not in rb:
            continue
        for m in METRICS:
            point, lo, hi = _bootstrap_ci(ra["ranks"], rb["ranks"], m, n_resamples)
            st = _sign_test(ra["ranks"], rb["ranks"], m)
            wl = f"{st[0]}-{st[1]}" if st else ""
            pv = f"{st[2]:.4f}" if st else ""
            verdict = "WIN" if lo > 0 else ("LOSS" if hi < 0 else "inconclusive")
            print(f"{kind:<12} {m:<10} {rb[m]:>7.3f} {ra[m]:>7.3f} {point:>+8.3f} "
                  f"[{lo:>+6.3f},{hi:>+6.3f}] {wl:>9} {pv:>7}  {verdict}")


def compare(results, baseline_path, n_resamples):
    base_rows = json.loads(Path(baseline_path).read_text())
    base = {(r["mode"], r["kind"]): r for r in base_rows}
    print(f"\n=== PAIRED vs {baseline_path.name} "
          f"(bootstrap {n_resamples}x, 95% CI; sign test over disagreements) ===")
    print(f"{'condition':<22} {'metric':<10} {'base':>6} {'cand':>6} {'delta':>7} "
          f"{'95% CI':>17} {'w-l':>7} {'p':>6}  verdict")
    for r in results:
        key = (r["mode"], r["kind"])
        b = base.get(key)
        if not b:
            continue
        if "ranks" not in b:
            print(f"{r['mode'] + ':' + r['kind']:<22} (baseline predates per-query "
                  f"ranks — regenerate it for paired stats)")
            continue
        if b.get("queries_fp") and b["queries_fp"] != r.get("queries_fp"):
            print(f"{r['mode'] + ':' + r['kind']:<22} SKIP: baseline used a different "
                  f"query set ({b['queries_fp']} != {r.get('queries_fp')})")
            continue
        for m in METRICS:
            point, lo, hi = _bootstrap_ci(r["ranks"], b["ranks"], m, n_resamples)
            st = _sign_test(r["ranks"], b["ranks"], m)
            wl = f"{st[0]}-{st[1]}" if st else ""
            p = f"{st[2]:.3f}" if st else ""
            if lo > 0:
                verdict = "WIN"
            elif hi < 0:
                verdict = "LOSS"
            else:
                verdict = "inconclusive"
            print(f"{r['mode'] + ':' + r['kind']:<22} {m:<10} "
                  f"{b[m]:>6.3f} {r[m]:>6.3f} {point:>+7.3f} "
                  f"[{lo:>+6.3f},{hi:>+6.3f}] {wl:>7} {p:>6}  {verdict}")


if __name__ == "__main__":
    main()
