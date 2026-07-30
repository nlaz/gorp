#!/usr/bin/env python3
"""How much of the answer does a query already contain?

RESEARCH.md §12.3 established that our self-written query sets sit at two
poles neither of which is where users are: `direct` hands the tool the gold
identifier (66-70% of the time), `paraphrase` deliberately strips it (2%).
§12.5's conclusion was that no quality claim should be read without knowing
which pole produced it. This module makes that operational — `run_eval.py`
prints these numbers above every results table, so a recall figure cannot be
quoted without its leakage beside it.

It also measures a leak §12 did not: **path leakage**. `generate.py` used to
pass the file path into the generator prompt, and semgrep's tokenizer does
path augmentation, so the generator saw the document identifier and the
scorer indexes the document identifier. Measured on the sets on disk before
that prompt was fixed:

    set        kind        basename   stem    dir-segment
    linux      direct          1.5%   32.7%         48.2%
    linux      paraphrase      0.0%    0.0%         25.1%
    vscode     direct          0.0%   22.5%         46.0%
    vscode     paraphrase      0.0%    0.0%         26.5%
    wikipedia  both            0.0%    0.0%          0.0%

The honest caveat, which the headline number must carry: in C a file stem and
an identifier prefix are frequently the same token (`blkg-rwstat.c` <->
`blkg_rwstat_add`), so `stem_in_query` partly re-measures §12.1's identifier
leakage rather than isolating a new one. `path_seg_not_in_gold` is the clean
number — a path segment present in the query but absent from the gold chunk
text cannot be explained away as identifier overlap.
"""

import re
from pathlib import Path

# Shared with run_eval.py's ripgrep baselines. Imported there rather than
# duplicated: the leakage percentage we PRINT and the identifier predicate
# that DECIDES the rg-strong baseline must be the same function, or the two
# numbers drift apart and §12.1's audit stops meaning anything.
STOPWORDS = set("""a an and are as at be by code does file find for from how in is
it its of on or that the this to what when where which who why with""".split())

_TOKEN = re.compile(r"[A-Za-z0-9_]+")
_CAMEL = re.compile(r"[a-z][A-Z]")

# A path segment shorter than this is too collision-prone to call leakage:
# "io", "os", "net" appear in ordinary English and in half the corpus.
MIN_SEGMENT = 4


def content_tokens(text):
    """Identifier-shaped tokens, stopwords dropped, case preserved."""
    return [w for w in _TOKEN.findall(text) if w.lower() not in STOPWORDS]


def identifiers(tokens):
    """The tokens that are *true* identifiers: snake_case or camelCase.

    §12.1's own self-correction is load-bearing here. An earlier version of
    that measurement counted long lowercase words like `workaround` as
    identifiers and reported 93% leakage for paraphrase queries — wrong, and
    corrected in the doc. A long English word is not an identifier, and
    conflating the two overstates what the rg-strong baseline can do.

    Returned longest-first, which is `rg_strong`'s rarity proxy.
    """
    return sorted((w for w in tokens if "_" in w or _CAMEL.search(w)),
                  key=len, reverse=True)


def _gold_text(row, corpus):
    """The gold span's text, or None if it cannot be read.

    Unreadable gold is not silently treated as empty: an empty string would
    make `path_seg_not_in_gold` fire for every segment and manufacture
    leakage that isn't there. Callers get None and must skip the row.
    """
    try:
        lines = (Path(corpus) / row["file"]).read_text(errors="replace").splitlines()
    except OSError:
        return None
    return "\n".join(lines[row["start_line"] - 1:row["end_line"]])


def leakage(row, corpus=None):
    """Per-row leakage signals. `corpus` is optional; the path-vs-gold
    comparison is simply absent without it rather than guessed at."""
    q = row["query"]
    ql = q.lower()
    toks = content_tokens(q)
    f = Path(row["file"])
    segs = [s for s in f.parts[:-1] if len(s) >= MIN_SEGMENT]

    out = {
        "has_identifier": bool(identifiers(toks)),
        "n_words": len(q.split()),
        "basename_in_query": f.name.lower() in ql,
        "stem_in_query": len(f.stem) >= MIN_SEGMENT and f.stem.lower() in ql,
        "path_seg_in_query": any(s.lower() in ql for s in segs),
        "path_seg_not_in_gold": False,
    }

    if corpus is not None:
        gold = _gold_text(row, corpus)
        if gold is not None:
            gold_l = gold.lower()
            # A segment the query carries that the gold text itself does NOT.
            # This is the isolation: overlap that identifier leakage cannot
            # explain, because the token is absent from the answer's body.
            out["path_seg_not_in_gold"] = any(
                s.lower() in ql and s.lower() not in gold_l for s in segs)
            out["gold_token_overlap"] = _overlap(toks, content_tokens(gold))
    return out


def _overlap(q_toks, gold_toks):
    """Fraction of the query's DISTINCT content tokens present in the gold.

    Distinct, not raw count: a query repeating `config` five times leaks one
    token, not five, and counting occurrences would inflate long queries.
    """
    if not q_toks:
        return 0.0
    qs = {t.lower() for t in q_toks}
    gs = {t.lower() for t in gold_toks}
    return len(qs & gs) / len(qs)


def summarize(rows, corpus=None):
    """Aggregate by `kind`, in the shape run_eval.py prints and stores."""
    by = {}
    for row in rows:
        by.setdefault(row.get("kind", "?"), []).append(leakage(row, corpus))
    out = {}
    for kind, ls in sorted(by.items()):
        n = len(ls)
        words = sorted(l["n_words"] for l in ls)
        ov = [l["gold_token_overlap"] for l in ls if "gold_token_overlap" in l]
        out[kind] = {
            "n": n,
            "identifier_pct": sum(l["has_identifier"] for l in ls) / n,
            "median_words": words[n // 2],
            "basename_pct": sum(l["basename_in_query"] for l in ls) / n,
            "stem_pct": sum(l["stem_in_query"] for l in ls) / n,
            "path_seg_pct": sum(l["path_seg_in_query"] for l in ls) / n,
            "path_seg_not_in_gold_pct": sum(l["path_seg_not_in_gold"] for l in ls) / n,
            "gold_token_overlap": (sum(ov) / len(ov)) if ov else None,
        }
    return out


HEADER = (f"{'kind':<12} {'n':>5} {'ident%':>7} {'medwords':>9} {'goldtok%':>9} "
          f"{'base%':>6} {'stem%':>6} {'pathseg%':>9} {'pathseg!gold%':>14}")


def format_summary(summary, label=""):
    lines = [f"=== query-set leakage {label} ===", HEADER]
    for kind, s in summary.items():
        ov = f"{s['gold_token_overlap']:>9.1%}" if s["gold_token_overlap"] is not None else f"{'--':>9}"
        lines.append(
            f"{kind:<12} {s['n']:>5} {s['identifier_pct']:>7.1%} {s['median_words']:>9} {ov} "
            f"{s['basename_pct']:>6.1%} {s['stem_pct']:>6.1%} {s['path_seg_pct']:>9.1%} "
            f"{s['path_seg_not_in_gold_pct']:>14.1%}")
    return "\n".join(lines)


if __name__ == "__main__":
    import argparse
    import json

    ap = argparse.ArgumentParser(description="print a query set's leakage profile")
    ap.add_argument("queries", type=Path)
    ap.add_argument("corpus", type=Path, nargs="?", default=None,
                    help="optional; enables gold-token overlap and path_seg_not_in_gold")
    a = ap.parse_args()
    rows = [json.loads(l) for l in a.queries.read_text().splitlines() if l.strip()]
    print(format_summary(summarize(rows, a.corpus), f"({a.queries.name})"))
