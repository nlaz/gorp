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
  python3 eval/run_eval.py eval/queries/linux.jsonl bench/corpora/linux \
      [--modes bm25,semantic,hybrid,rg] [--k 10] [--slack 10] [--no-index]
"""

import argparse
import hashlib
import json
import math
import random
import re
import subprocess
import time
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
import corpus_text  # noqa: E402
import leakage  # noqa: E402
from leakage import STOPWORDS, content_tokens, identifiers  # noqa: E402
from validate_queries import format_problems, validate  # noqa: E402


def semgrep_search(query, corpus, mode, k, no_index, extra=()):
    cmd = [str(SEMGREP), "--mode", mode, "--json", "-k", str(k)]
    if no_index:
        cmd.append("--no-index")
    cmd += list(extra) + [query, str(corpus)]
    # errors="replace" for the same reason as rg_run: hit lines carry corpus
    # bytes, and one accented character must not end the run.
    proc = subprocess.run(cmd, capture_output=True, text=True,
                          errors="replace", timeout=600)
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
    # errors="replace": rg echoes the matched line's bytes, and a corpus with
    # a Latin-1 identifier in it (commons-lang has several) makes strict UTF-8
    # decoding raise mid-run. Killing a 400-query scoring run over one accented
    # character in one matched line is not a stricter measurement, it is no
    # measurement — and the engine itself decodes lossily for the same reason
    # (corpus/mod.rs: "never bail on mixed-encoding trees").
    # Determinism is load-bearing, not cosmetic. ripgrep parallelizes its
    # directory walk and emits results as workers finish, so by default the
    # same pattern over the same corpus returns hits in a different order from
    # run to run — measured: 6 runs of one pattern over etcd produced 2
    # distinct top-10 orderings. Since a rank is a position in that list,
    # every rg and rg-strong figure this harness produced before this carried
    # run-to-run variance from thread scheduling, and no rg result was
    # reproducible. It also made the rg-oracle ceiling appear to lose to
    # rg_strong on queries where it had merely re-run the same pattern and
    # been handed a different order.
    #
    # rg's own `--sort path` fixes it by forcing a SINGLE-THREADED walk, which
    # costs 3.8-5.9x on the kernel. Sorting the parallel output here instead
    # gives byte-identical results at parallel speed — verified against
    # `--sort path` on patterns from 8 to 107,361 matches. On a corpus that
    # size the difference is 12.0s vs 2.1s per scan, which is what makes a
    # kernel-scale run finishable at all.
    proc = subprocess.run(
        [RG, "--no-heading", "-n", "-m", "3", *flags, pattern, str(corpus)],
        capture_output=True, text=True, errors="replace", timeout=600)
    raw = []
    for line in proc.stdout.splitlines():
        m = re.match(r"(.+?):(\d+):", line)
        if m:
            raw.append((m.group(1), int(m.group(2))))
    # Sort by path COMPONENTS, not by the raw string, because that is what rg
    # does and the two disagree whenever one directory name is a prefix of a
    # sibling's. In tokio: rg orders `tokio/` before `tokio-macros/`, while a
    # string sort puts `tokio-macros/...` first, since '-' (0x2d) sorts below
    # '/' (0x2f). That moved the first hit from rank 7 to rank 7 of a
    # different file — a silently different answer, on 3 of 27 spot-checked
    # (corpus, pattern) pairs.
    raw.sort(key=lambda pn: (Path(pn[0]).parts, pn[1]))
    hits = [(str(Path(p).relative_to(corpus)) if p.startswith(str(corpus)) else p, n, n)
            for p, n in raw[:k]]
    return hits


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
    # linux/vscode/wikipedia/cosqa have <=1 content word. Both fire on real
    # agent queries (eval/queries/replay-agent.jsonl and its exact-mode twin),
    # where 181/1223 (15%) have <=1 — 6% of ranked queries and 21% of exact
    # ones, 5 of which have no content word at all.
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


# ---------------------------------------------------------------------------
# checkpointing
#
# A full run over the kernel takes hours: --sort path costs 3.7-4.4x there
# (§13.5), and rg-oracle issues up to ORACLE_MAX_TOKENS + ~5 scans per query.
# Any interruption used to throw away every scan already paid for, which is
# how the kernel and CoSQA oracle runs failed three times without producing a
# single number.
#
# The unit of work is one (query index, mode) pair. Each result is appended to
# a JSONL file as soon as it is known, so a killed run resumes where it
# stopped. The header pins the query fingerprint, mode list and corpus: a
# checkpoint from a different run is REFUSED rather than silently mixed in,
# because half a table from one condition and half from another is worse than
# no table.
# ---------------------------------------------------------------------------

NOT_DONE = object()


class Checkpoint:
    def __init__(self, path, queries_fp, modes, corpus):
        self.path = Path(path) if path else None
        self.done = {}
        self.fh = None
        if self.path is None:
            return
        header = {"queries_fp": queries_fp, "modes": modes, "corpus": corpus}
        if self.path.exists():
            lines = self.path.read_text().splitlines()
            if lines:
                got = json.loads(lines[0])
                if got != header:
                    raise SystemExit(
                        f"checkpoint {self.path} is from a different run:\n"
                        f"  it has:   {got}\n"
                        f"  this run: {header}\n"
                        f"Delete it or pass a different --checkpoint.")
                for ln in lines[1:]:
                    try:
                        r = json.loads(ln)
                    except json.JSONDecodeError:
                        continue      # a torn last line from a hard kill
                    self.done[(r["i"], r["mode"])] = r["rank"]
        else:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self.path.write_text(json.dumps(header) + "\n")
        self.fh = self.path.open("a")

    def get(self, i, mode):
        return self.done.get((i, mode), NOT_DONE)

    def put(self, i, mode, rank):
        if self.fh is None:
            return
        self.fh.write(json.dumps({"i": i, "mode": mode, "rank": rank}) + "\n")
        # flush per result: the whole point is surviving a kill -9, and a
        # buffered write loses exactly the work we are trying to keep.
        self.fh.flush()

    def close(self):
        if self.fh is not None:
            self.fh.close()
            self.fh = None


def rg_oracle(query, corpus, k, truth, slack):
    """A strict UPPER BOUND on ripgrep, not a baseline.

    §12.2 corrected the legacy `rg` baseline's tokenizer and the published
    "30× gap" collapsed to ~2.9×. But `rg-strong` is still a hand-tuned
    heuristic — two identifiers, longest first, then some fallbacks — so the
    honest question §12 left open is how much of the remaining gap is the
    engine and how much is still the baseline's query planning.

    This answers it by removing the planning entirely: try EVERY content token
    as its own pattern and keep the best rank any of them achieves. No agent
    could run this — choosing the winning token requires already knowing the
    answer — so it is a ceiling, and it must be reported as one. If semgrep's
    margin survives it, the margin is the engine. If it does not, §12.2's
    correction did not go far enough and the claim is baseline-shaped.

    Cost is the whole design problem: ~12 tokens x 400 queries x a 1.15 GB
    kernel scan is hours. The pruning below is exact rather than approximate.
    A hit only counts if it lands in the gold file on a line overlapping the
    gold span (+/- slack), so a token that does not appear ANYWHERE in that
    window cannot produce a correct hit at any rank, and scanning the corpus
    for it cannot change the oracle's answer. Reading one gold window is free
    next to a corpus scan, and it typically leaves 1-3 tokens instead of 12.

    (The obvious alternative — one `rg --json -e tok1 -e tok2 ...` scan — was
    rejected: rg reports the matched *text* rather than which pattern matched,
    and `-m 1` caps output at the first matching line per file, so a token
    matching later in a file already matched by another token becomes
    invisible. An upper bound that under-credits is not an upper bound.)
    """
    window = _gold_window(corpus, truth, slack)
    if window is None:
        return []

    toks = content_tokens(query)
    # Identifiers first, then the rest longest-first: same rarity proxy as
    # rg_strong, so the two are ordered comparably when both are capped.
    idents = set(identifiers(toks))
    ordered = identifiers(toks) + sorted(
        (t for t in toks if t not in idents), key=len, reverse=True)
    # A token absent from the gold window cannot produce a hit `correct()`
    # accepts, so scanning for it cannot change the answer. Exact, not
    # heuristic — see test_pruning_cannot_discard_a_token_that_would_have_scored.
    live = [re.escape(t) for t in ordered[:ORACLE_MAX_TOKENS]
            if t.lower() in window]

    # Everything rg_strong would try, unpruned. Without this the oracle is NOT
    # an upper bound, and it was not: measured over 1,374 real queries it lost
    # to rg_strong on 53 of them (3.9%). The cause is that rg_strong tries
    # CONJUNCTIVE patterns (`A.*B`, requiring both tokens on one line) which
    # are strictly more selective than either token alone, so gold can rank
    # better under them than under any single token. A single-token vocabulary
    # cannot express that, so it cannot bound it.
    #
    # The fixture test asserting the bound passed anyway, because the fixtures
    # never exercised a conjunctive win. The per-query check across four real
    # corpora is what caught it.
    candidates = _dedupe(rg_strong_attempts(query) + live)

    best_rank, best_hits = None, []
    for pat in candidates:
        hits = rg_run(pat, corpus, k, flags=("-i",))
        r = next((i + 1 for i, h in enumerate(hits) if correct(h, truth, slack)), None)
        if r is not None and (best_rank is None or r < best_rank):
            best_rank, best_hits = r, hits
    return best_hits


ORACLE_MAX_TOKENS = 12


def _gold_window(corpus, truth, slack):
    """Lowercased text of the gold span widened by `slack`, or None.

    This is the pruning oracle: a token absent from here can never produce a
    hit that `correct()` accepts, whatever the corpus does.
    """
    lines, ok = corpus_text.read_lines(Path(corpus) / truth["file"])
    if not ok:
        return None
    lo = max(0, truth["start_line"] - 1 - slack)
    hi = min(len(lines), truth["end_line"] + slack)
    return "\n".join(lines[lo:hi]).lower()


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

def provenance(args, queries_fp, n_rows):
    """What produced this number, recorded with it.

    Result files used to carry a mode, a kind, four metrics and the per-query
    ranks — and nothing about the run. Six months later "kernel direct R@5 =
    0.92" cannot be checked, because which binary, which corpus tree and which
    query file produced it are all unrecoverable. §13.7 is the cost of that:
    a number that would not reproduce, with no way to tell whether the engine,
    the corpus or the harness had moved.

    Everything here is cheap and none of it is guessed. A field that cannot be
    determined is recorded as None rather than filled in.
    """
    def _digest(corpus):
        mf = Path(__file__).resolve().parents[1] / "bench" / "corpora" / "MANIFEST.json"
        if not mf.exists():
            return None
        try:
            return json.loads(mf.read_text()).get(Path(corpus).name, {}).get("tree_digest")
        except (json.JSONDecodeError, OSError):
            return None

    return {
        "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "queries": str(args.queries),
        "queries_fp": queries_fp,
        "n_queries": n_rows,
        "corpus": str(args.corpus),
        "corpus_digest": _digest(args.corpus),
        "modes": args.modes,
        "k": args.k,
        "slack": args.slack,
        "where": args.where or None,
        "stratify": args.stratify or None,
        "extra": args.extra or None,
        "no_index": args.no_index,
        **binary_provenance(),
    }


def binary_provenance():
    """Which binary, and which commit. Split out because every harness needs it.

    `eval/sim/` records the same block, and two implementations of "which
    binary produced this" is exactly one too many — the failure it guards
    against (§13.7) is a number whose provenance turns out to be wrong, and a
    second copy that drifts is a new way to be wrong about it.
    """
    def _git_head():
        try:
            r = subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                               cwd=Path(__file__).parent, capture_output=True,
                               text=True, timeout=10)
            return r.stdout.strip() or None
        except (OSError, subprocess.SubprocessError):
            return None

    binst = SEMGREP.stat() if SEMGREP.exists() else None
    return {
        "binary": str(SEMGREP),
        "binary_mtime": time.strftime("%Y-%m-%dT%H:%M:%SZ",
                                      time.gmtime(binst.st_mtime)) if binst else None,
        "binary_bytes": binst.st_size if binst else None,
        "git_head": _git_head(),
    }


def check_index_freshness(corpus, modes, no_index, allow_stale):
    """Refuse to score semgrep modes against an index older than the binary.

    `eval/locbench/run.py:220` has had this guard for a while — "a rebuilt
    binary may embed different dims (e.g. a swapped embedding table), and
    reusing that index makes every query bail on the dims check, which would
    look like a catastrophic accuracy result rather than the mechanical
    mismatch it is." run_eval.py never had it. This is the same guard, in the
    place that produces the published numbers.

    Honest note on why it was added: a kernel rerun scored bm25/semantic/hybrid
    against an index a day older than the binary, and its results differed from
    the published ones while VS Code's (whose index postdated the binary)
    matched to three decimals. That looked like the explanation. It was not —
    rebuilding the kernel index and rescoring reproduced all 1,194 ranks
    EXACTLY, so the staleness had changed nothing and the discrepancy has
    another cause (see §13.7). The guard stays because the hazard it covers is
    real and locbench documents it, not because it explained that measurement.

    Only semgrep modes read the index — rg conditions are unaffected, which is
    why a run that is invalid for one column can be perfectly valid for
    another.
    """
    if no_index:
        return
    sem_modes = [m for m in modes.split(",") if not m.startswith("rg")]
    if not sem_modes:
        return
    meta = Path(corpus) / ".semgrep" / "meta.json"
    if not meta.exists():
        return                      # no local index: the cache path decides
    if meta.stat().st_mtime >= SEMGREP.stat().st_mtime:
        return
    msg = (f"index at {meta.parent} is OLDER than {SEMGREP}\n"
           f"  index:  {time.ctime(meta.stat().st_mtime)}\n"
           f"  binary: {time.ctime(SEMGREP.stat().st_mtime)}\n"
           f"  modes reading it: {', '.join(sem_modes)}\n"
           f"Rebuild with `{SEMGREP} index {corpus}` — a binary built since the "
           f"index may embed different dims, and the mismatch reads as an "
           f"accuracy result rather than the mechanical problem it is.")
    if not allow_stale:
        raise SystemExit(msg + "\n(--allow-stale to score anyway)")
    print("WARNING: " + msg)


QUERY_DIRS = (HERE / "queries", HERE / "data")


def resolve_queries(spec):
    """Accept a path, or a bare set name resolved against eval/queries then
    eval/data. The sets moved into eval/queries when they were checked into
    git; a bare name keeps callers from having to care which."""
    p = Path(spec)
    if p.exists():
        return p
    if p.parent == Path("."):
        for d in QUERY_DIRS:
            for cand in (d / p.name, d / f"{p.name}.jsonl"):
                if cand.exists():
                    return cand
    raise SystemExit(
        f"no such query set: {spec} (looked in {', '.join(str(d) for d in QUERY_DIRS)})")


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
    ap.add_argument("--checkpoint", type=Path, default=None,
                    help="append each (query, mode) result here and resume from "
                         "it on restart — a kernel or oracle run takes hours and "
                         "an interruption otherwise discards every scan paid for")
    args = ap.parse_args()
    extra = tuple(args.extra.split()) if args.extra else ()

    args.queries = resolve_queries(args.queries)
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

    check_index_freshness(args.corpus, args.modes, args.no_index, args.allow_stale)

    rows = apply_where(rows, args.where)
    if not rows:
        raise SystemExit(f"--where {args.where!r} matched no rows")

    # The fingerprint covers the filter and the strata as well as the file, so
    # a filtered run can never be --baseline-compared against an unfiltered one.
    queries_fp = hashlib.sha256(
        (raw + "\0" + args.where + "\0" + args.stratify).encode()).hexdigest()[:16]

    run_meta = provenance(args, queries_fp, len(rows))
    lk = leakage.summarize(rows, args.corpus)
    print()
    print(leakage.format_summary(lk, f"({args.queries.name})"))

    strata = [s for s in args.stratify.split(",") if s]
    modes = args.modes.split(",")
    # metric accumulators: (mode, cell) -> list of first-correct ranks (None = miss)
    ranks = defaultdict(list)

    ckpt = Checkpoint(args.checkpoint, queries_fp, args.modes, str(args.corpus))
    if ckpt.done:
        print(f"resuming from {args.checkpoint}: {len(ckpt.done)} "
              f"(query, mode) pairs already scored")

    for i, truth in enumerate(rows):
        cell = cell_of(truth, strata, args.corpus)
        for mode in modes:
            cached = ckpt.get(i, mode)
            if cached is not NOT_DONE:
                ranks[(mode, cell)].append(cached)
                continue
            if mode == "rg":
                hits = rg_agent_style(truth["query"], args.corpus, args.k)
            elif mode == "rg-strong":
                hits = rg_strong(truth["query"], args.corpus, args.k)
            elif mode == "rg-oracle":
                # Takes `truth`, unlike every other condition — that is what
                # makes it a ceiling rather than a baseline.
                hits = rg_oracle(truth["query"], args.corpus, args.k, truth, args.slack)
            else:
                hits = semgrep_search(truth["query"], args.corpus, mode, args.k, args.no_index, extra)
            rank = next((r + 1 for r, h in enumerate(hits) if correct(h, truth, args.slack)), None)
            ckpt.put(i, mode, rank)
            ranks[(mode, cell)].append(rank)
        if (i + 1) % 25 == 0:
            print(f"  {i + 1}/{len(rows)} queries", flush=True)
    ckpt.close()

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
                        "queries_fp": queries_fp,
                        # Embedded per row rather than kept in a sidecar: a
                        # sidecar gets separated from its data, which is the
                        # failure this is fixing. Consumers key on
                        # (mode, kind) and ignore extra fields.
                        "run": run_meta})
    if any(m == "rg-oracle" for m in modes):
        print("\n* rg-oracle is a CEILING, not a baseline: it tries every query\n"
              "  token as its own pattern and keeps whichever scored best, which\n"
              "  requires already knowing the answer. No agent can run it. Read it\n"
              "  as 'the most ripgrep could possibly do here', and compare\n"
              "  semgrep's margin against it, not against rg-strong alone.")
    if args.compare_modes:
        a, b = args.compare_modes.split(",")
        compare_modes(results, a.strip(), b.strip(), args.bootstrap)
    if args.baseline:
        compare(results, args.baseline, args.bootstrap)

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        for r in results:
            r["leakage"] = lk.get(r["kind"].split("|")[0])
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
