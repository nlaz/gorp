#!/usr/bin/env python3
"""Does this query set still describe this corpus?

A query set records ground truth as `file` + `start_line`..`end_line`. Nothing
ties those coordinates to the corpus on disk, and the corpora are not pinned:
`bench/fetch-corpora.sh` clones vscode at `--depth 1` of HEAD and then deletes
`.git`, so a refetch silently produces a different tree while the query set
goes on claiming line 42 of a file that has moved.

The failure mode is the dangerous kind. A gold file that no longer exists does
not raise — every hit simply fails `correct()`, the row scores 0, and the run
reports a number. `run_eval.py` already learned this lesson once for a
different cause: a binary whose embedding width did not match the index
"silently reported 0.00 across 400 queries and the result looked like a
measurement." Same shape, different origin.

So: validate before scoring, and refuse to score a set that has drifted.

Checks, cheapest first:
  1. the gold file exists under the corpus
  2. 1 <= start_line <= end_line <= len(lines)
  3. if the row carries `gold_sha` (written by generate.py), the sha256 of the
     gold span's text still matches what was there at generation time
  4. rows whose `kind` claims blindness (RESEARCH.md §15.3) actually are:
     zero gold-identifier hits and bounded token overlap, plus the set-mean
     overlap cap over all blind rows. A "blind" set that isn't is refused the
     same way a drifted one is — blindness is a property of the data, not an
     instruction the generator once received.

Check 3 is the only one that catches an *edited* file, which is the common
case for a live repo — the path and the line count survive, the content does
not. Sets generated before `gold_sha` existed skip it rather than fail: a
missing check is reported as unverifiable, not as a pass.
"""

import hashlib
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import corpus_text  # noqa: E402
import leakage  # noqa: E402


def span_sha(text):
    """sha256 of a gold span. Newline-normalized so a checkout that flips
    line endings does not read as content drift."""
    return hashlib.sha256(text.replace("\r\n", "\n").encode()).hexdigest()[:16]


def validate(rows, corpus, check_sha=True):
    """Return (problems, stats). A problem is (index, kind, detail)."""
    corpus = Path(corpus)
    problems = []
    cache = {}
    n_sha_checked = 0
    blind_overlaps = []

    for i, row in enumerate(rows):
        rel = row.get("file")
        if rel is None:
            problems.append((i, "no-file-field", repr(row)[:80]))
            continue
        if rel not in cache:
            p = corpus / rel
            if not p.exists():
                cache[rel] = ("missing", None)
            else:
                lines, ok = corpus_text.read_lines(p)
                # A file semgrep's walker skips is not a missing file and not
                # a passing row: the gold span in it can never be returned by
                # any condition, so it is a permanent, uniform miss that reads
                # as an accuracy result. Name it.
                cache[rel] = ("ok", lines) if ok else ("unindexable", None)
        kind, lines = cache[rel]
        if kind == "missing":
            problems.append((i, "missing-file", rel))
            continue
        if kind == "unindexable":
            problems.append((i, "unindexable-gold",
                             f"{rel} (NUL byte in first 8 KiB — semgrep's walker "
                             f"skips it, so this row can never be found)"))
            continue

        s, e = row.get("start_line"), row.get("end_line")
        if not isinstance(s, int) or not isinstance(e, int):
            problems.append((i, "bad-span-type", f"{rel}:{s}-{e}"))
            continue
        if s < 1:
            problems.append((i, "span-before-start", f"{rel}:{s}"))
            continue
        if e < s:
            problems.append((i, "inverted-span", f"{rel}:{s}-{e}"))
            continue
        if e > len(lines):
            problems.append((i, "span-past-eof", f"{rel}:{s}-{e} (file has {len(lines)})"))
            continue

        if check_sha and "gold_sha" in row:
            n_sha_checked += 1
            got = span_sha(corpus_text.span(lines, s, e))
            if got != row["gold_sha"]:
                problems.append((i, "gold-content-changed",
                                 f"{rel}:{s}-{e} ({row['gold_sha']} -> {got})"))

        if row.get("kind") in leakage.BLIND_KINDS:
            gold = corpus_text.span(lines, s, e)
            toks = leakage.content_tokens(row["query"])
            hits = leakage.gold_identifier_hits(toks, gold, row.get("symbol"))
            ov = leakage._overlap(toks, leakage.content_tokens(gold))
            blind_overlaps.append(ov)
            if hits:
                problems.append((i, "blind-violation",
                                 f"{rel}:{s}-{e} query names gold identifiers: "
                                 f"{sorted(set(hits))}"))
            elif ov > leakage.BLIND_ROW_OVERLAP_CAP:
                problems.append((i, "blind-violation",
                                 f"{rel}:{s}-{e} gold_token_overlap {ov:.2f} > "
                                 f"{leakage.BLIND_ROW_OVERLAP_CAP} row cap"))

    if blind_overlaps:
        mean = sum(blind_overlaps) / len(blind_overlaps)
        if mean > leakage.BLIND_SET_OVERLAP_CAP:
            problems.append((-1, "blind-set-overlap",
                             f"mean gold_token_overlap over {len(blind_overlaps)} blind "
                             f"rows is {mean:.2f} > {leakage.BLIND_SET_OVERLAP_CAP}"))

    stats = {
        "n_rows": len(rows),
        "n_files": len(cache),
        "n_sha_checked": n_sha_checked,
        "n_blind_checked": len(blind_overlaps),
        "n_problems": len(problems),
        # An honest report of what was NOT verified. A set with no gold_sha
        # passes every check here and still cannot detect an edited file.
        "n_unverifiable": len(rows) - n_sha_checked if check_sha else len(rows),
    }
    return problems, stats


def format_problems(problems, limit=10):
    by_kind = {}
    for _, kind, _ in problems:
        by_kind[kind] = by_kind.get(kind, 0) + 1
    lines = [f"  {kind}: {n}" for kind, n in sorted(by_kind.items())]
    for i, kind, detail in problems[:limit]:
        lines.append(f"    row {i}: {kind}: {detail}")
    if len(problems) > limit:
        lines.append(f"    ... and {len(problems) - limit} more")
    return "\n".join(lines)


def main():
    import argparse
    import sys

    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("queries", type=Path)
    ap.add_argument("corpus", type=Path)
    ap.add_argument("--no-sha", action="store_true", help="skip the content check")
    a = ap.parse_args()

    rows = [json.loads(l) for l in a.queries.read_text().splitlines() if l.strip()]
    if not rows:
        print(f"ERROR: {a.queries} has no rows")
        sys.exit(2)
    problems, stats = validate(rows, a.corpus, check_sha=not a.no_sha)
    print(f"{a.queries.name} vs {a.corpus}: {stats['n_rows']} rows, "
          f"{stats['n_files']} files, {stats['n_sha_checked']} content-verified, "
          f"{stats['n_problems']} problems")
    if stats["n_unverifiable"]:
        print(f"  note: {stats['n_unverifiable']} rows carry no gold_sha — an edited "
              f"file would not be detected for those")
    if problems:
        print(format_problems(problems, limit=20))
        sys.exit(1)


if __name__ == "__main__":
    main()
