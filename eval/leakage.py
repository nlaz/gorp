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
pass the file path into the generator prompt, and gorp's tokenizer does
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
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import corpus_text  # noqa: E402

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

    Definition FROZEN for historical comparability — every recorded
    `identifier_pct` means this. It is query-side and gold-agnostic, so a
    single lowercase gold symbol (`flush`) is invisible to it; blindness
    (§15.3) is decided by the gold-aware `gold_identifier_hits` instead.
    """
    return sorted((w for w in tokens if "_" in w or _CAMEL.search(w)),
                  key=len, reverse=True)


# --- strict-blind predicate (RESEARCH.md §15.3) ---------------------------

BLIND_KINDS = {"blind", "blind_long", "symptom"}

# The §16 agentic-guess kinds. NOT blind-gated on purpose: a guess is allowed
# — expected — to contain identifiers, including wrong ones; the wrong-guess
# case is precisely what ranked search is supposed to rescue.
GUESS_KINDS = {"guess_ranked", "guess_exact", "guess_rg", "guess_ladder", "guess"}

# Light stemming, symbol-match only: "flushing" still names `flush`.
_SUFFIXES = ("ing", "ed", "es", "s", "er")

# Per-row and set-mean caps on gold_token_overlap for blind rows. Calibrated
# in §15's Phase 0 against existing distributions (paraphrase ~0.10-0.115,
# real CoSQA users 0.42), then frozen.
BLIND_ROW_OVERLAP_CAP = 0.5
BLIND_SET_OVERLAP_CAP = 0.25


def _subtokens(word):
    """snake_case / camelCase subtokens of one identifier, lowercased."""
    out = []
    for part in word.split("_"):
        if not part:
            continue
        for m in re.finditer(r"[A-Z]+(?![a-z])|[A-Z][a-z0-9]*|[a-z0-9]+", part):
            out.append(m.group(0).lower())
    return out


def _stem_match(a, b):
    """a names b under light suffixing, either direction."""
    if a == b:
        return True
    return any(a == b + s or b == a + s for s in _SUFFIXES)


_COMMENT_LINE = re.compile(r"^\s*(#|//|/\*|\*|;|--|'''|\"\"\")")


def _comment_prose(gold_text):
    """Lowercased bare tokens from the gold's comment/docstring lines.

    "Plain prose" for the §15.3 subtoken guard: a bare variable named
    `rwstat` is code, not prose — only what a comment *says* attests a word
    as ordinary vocabulary. Docstring interiors count via the triple-quote
    block scan; identifier-shaped tokens never count wherever they appear.
    """
    out = set()
    in_doc = False
    for line in gold_text.splitlines():
        stripped = line.strip()
        is_comment = bool(_COMMENT_LINE.match(stripped))
        if '"""' in stripped or "'''" in stripped:
            is_comment = True
            if stripped.count('"""') % 2 == 1 or stripped.count("'''") % 2 == 1:
                in_doc = not in_doc
        elif in_doc:
            is_comment = True
        if is_comment:
            for t in content_tokens(stripped):
                if "_" not in t and not _CAMEL.search(t):
                    out.add(t.lower())
    return out


def gold_identifier_hits(q_toks, gold_text, symbol=None):
    """Query tokens that name an identifier OF THE GOLD. Empty == blind.

    (a) the token equals a snake/camel identifier token of the gold span;
    (b) it equals the gold's own symbol name (the clause `identifiers()`
        cannot express — closes the lowercase-symbol leak), incl. light
        suffixing;
    (c) it equals a subtoken of the symbol (len >= MIN_SEGMENT, not a
        stopword) that the gold's comments/docstrings do not use as an
        ordinary word — `rwstat` is caught, a comment's `read` passes.
    """
    gold_ids = {g.lower() for g in identifiers(content_tokens(gold_text))}
    hits = []
    sym = (symbol or "").lower()
    sym_subs = set()
    if symbol:
        prose_toks = _comment_prose(gold_text)
        sym_subs = {
            s for s in _subtokens(symbol)
            if len(s) >= MIN_SEGMENT and s not in STOPWORDS and s not in prose_toks
        }
    for t in q_toks:
        tl = t.lower()
        if tl in gold_ids:
            hits.append(t)
        elif sym and _stem_match(tl, sym):
            hits.append(t)
        elif tl in sym_subs:
            hits.append(t)
    return hits


def is_blind(row, gold_text):
    """§15.3: no gold-identifier hits, and bounded token overlap."""
    toks = content_tokens(row["query"])
    if gold_identifier_hits(toks, gold_text, row.get("symbol")):
        return False
    return _overlap(toks, content_tokens(gold_text)) <= BLIND_ROW_OVERLAP_CAP


def _gold_text(row, corpus):
    """The gold span's text, or None if it cannot be read.

    Unreadable gold is not silently treated as empty: an empty string would
    make `path_seg_not_in_gold` fire for every segment and manufacture
    leakage that isn't there. Callers get None and must skip the row.
    """
    lines, ok = corpus_text.read_lines(Path(corpus) / row["file"])
    if not ok:
        return None
    return corpus_text.span(lines, row["start_line"], row["end_line"])


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
            hits = gold_identifier_hits(toks, gold, row.get("symbol"))
            out["gold_id_hits"] = len(hits)
            out["is_blind"] = (
                not hits and out["gold_token_overlap"] <= BLIND_ROW_OVERLAP_CAP)
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
        gid = [l["gold_id_hits"] for l in ls if "gold_id_hits" in l]
        out[kind] = {
            "n": n,
            "identifier_pct": sum(l["has_identifier"] for l in ls) / n,
            "median_words": words[n // 2],
            "basename_pct": sum(l["basename_in_query"] for l in ls) / n,
            "stem_pct": sum(l["stem_in_query"] for l in ls) / n,
            "path_seg_pct": sum(l["path_seg_in_query"] for l in ls) / n,
            "path_seg_not_in_gold_pct": sum(l["path_seg_not_in_gold"] for l in ls) / n,
            "gold_token_overlap": (sum(ov) / len(ov)) if ov else None,
            # §15.3, gold-aware: rows whose query names a gold identifier.
            # A new column — historical fields keep their exact meaning.
            "gold_id_pct": (sum(1 for g in gid if g) / len(gid)) if gid else None,
        }
    return out


HEADER = (f"{'kind':<12} {'n':>5} {'ident%':>7} {'goldid%':>8} {'medwords':>9} {'goldtok%':>9} "
          f"{'base%':>6} {'stem%':>6} {'pathseg%':>9} {'pathseg!gold%':>14}")


def format_summary(summary, label=""):
    lines = [f"=== query-set leakage {label} ===", HEADER]
    for kind, s in summary.items():
        ov = f"{s['gold_token_overlap']:>9.1%}" if s["gold_token_overlap"] is not None else f"{'--':>9}"
        gid = (f"{s['gold_id_pct']:>8.1%}"
               if s.get("gold_id_pct") is not None else f"{'--':>8}")
        lines.append(
            f"{kind:<12} {s['n']:>5} {s['identifier_pct']:>7.1%} {gid} {s['median_words']:>9} {ov} "
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
