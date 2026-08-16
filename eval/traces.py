"""Tiered agent traces: one schema for every query we score, and the rule
that sorts a query into `blind`, `guess`, or `golden`.

## Why one schema

Queries reached this project in two shapes that could not be scored together.
Generated sets (`eval/queries/<corpus>.jsonl`) carry a `kind` — `direct`,
`paraphrase`, `blind`, `blind_long` — and a gold *span*. Harvested agent
invocations (`eval/queries/guesses-*.jsonl`) carry an argv, a scope and an
instance id, with gold living in a benchmark dataset somewhere else. The
first went through `run_eval.py`, the second through a 700-line replayer in
the bench repo, and nothing put a real agent's query next to a generated one
on the same axis.

A trace row is that axis. It says what was typed, what the right answer was,
where the query came from, and — the part that makes the sets comparable —
**how much of the answer the query already contained.**

## The tiers

    golden   the query names a gold identifier
    guess    it names none, but shares the gold's path vocabulary
    blind    it shares neither

This is graded context removal (CORE-Bench's idea, §15's ladder) applied to
real traffic instead of to prompts. The point of separating them is that they
are different retrieval problems and the engine is good at different ones:
§19.2b measured a *blind description* finding the gold 13% of the time
against a *blind name*'s 50%. A pooled number over a mixed set moves when the
mix moves, which is how a corpus change can look like an engine change.

**The tier is computed, never authored.** A hand-labeled tier is a claim that
rots the first time the gold moves; a computed one is a function of (query,
gold) that anyone can re-derive — which is exactly what
`validate_queries.py --traces` does, and why a stamped tier that disagrees
with a recomputed one is an error rather than a preference.

## Where the pieces run

Tiering needs gold, and harvested rows get theirs from a benchmark dataset
that lives with the harness. So `gorp-bench` builds and publishes these files
(`harness/common/publish_traces.py`), and they are **checked in here**,
beside the query sets they join. That direction matters: the harness needs
agent runs and costs money, this repo needs neither, and an engine change
must be gateable without either.

Both repos import this module — gorp-bench through
`harness/common/gorp_repo.py` — so there is exactly one tier rule rather than
two that agree today.
"""

import json
import re
from pathlib import Path

#: Ordered least- to most-informed. Report in this order; a table that puts
#: `golden` first invites reading the easy column as the headline.
TIERS = ("blind", "guess", "golden")

#: Identifier fragments too short or too common to count as naming anything.
#: `leaf_names` already drops <=2 characters; this catches the rest of the
#: noise that qualnames carry.
_TRIVIAL = {"get", "set", "run", "new", "init", "main", "self", "cls", "test"}


def leaf_names(gold_functions):
    """`['path/to/f.py:Class.method']` -> `{'Class', 'method'}`.

    Qualnames, not paths: a query that types the file name has *located* the
    file, which is what the path term below measures, and counting it as an
    identifier hit would collapse the two signals into one.

    Lifted from the harness's `stratify.py`, which uses the same rule to tier
    *issues* by whether they name the gold. Same question asked of a query.
    """
    names = set()
    for g in gold_functions or []:
        qual = g.split(":", 1)[1] if ":" in g else g
        for part in qual.split("."):
            part = part.strip()
            if len(part) > 2 and part.lower() not in _TRIVIAL:
                names.add(part)
    return names


def path_tokens(gold_files):
    """Vocabulary a query could borrow from the gold's *location*.

    Directory segments and file stems, minus extensions. `src/net/retry.rs`
    contributes `net` and `retry` but not `src` (universal) or `rs`.
    """
    out = set()
    for f in gold_files or []:
        for seg in Path(f).parts:
            stem = Path(seg).stem
            if len(stem) > 2 and stem.lower() not in {"src", "lib", "test", "tests"}:
                out.add(stem)
    return out


def _mentions(text, words):
    """Whole-word, case-sensitive-identifier match, the `stratify.py` rule.

    Case-sensitive because `Grid` and `grid` are different identifiers, and a
    query that typed the wrong case did not name the symbol; the path term
    lowercases separately, where casing carries no meaning.
    """
    for w in words:
        if re.search(rf"(?<![A-Za-z0-9_]){re.escape(w)}(?![A-Za-z0-9_])", text or ""):
            return True
    return False


def tier_of(query, gold):
    """`blind` | `guess` | `golden` for one query against one gold spec.

    `gold` is a trace row's gold object: `funcs` (qualnames) and `files`.
    A row with neither cannot be tiered and raises — silently returning
    `blind` would manufacture the stratum §19.5 cares most about.
    """
    funcs, files = gold.get("funcs") or [], gold.get("files") or []
    if not funcs and not files:
        raise ValueError("gold has neither funcs nor files; cannot tier")
    if _mentions(query, leaf_names(funcs)):
        return "golden"
    low = (query or "").lower()
    if _mentions(low, {p.lower() for p in path_tokens(files)}):
        return "guess"
    return "blind"


# --- the row -----------------------------------------------------------------

REQUIRED = ("id", "query", "tier", "provenance", "target", "gold")
SOURCES = ("harvested", "generated", "cosqa")


def validate_row(row, i=0, recompute=True):
    """Structural problems with one row, as a list of strings (empty == fine).

    `recompute=True` re-derives the tier and reports a mismatch. That check is
    the whole reason the tier is a function rather than a label: the two repos
    that write and read these files can only drift apart silently if nobody
    recomputes.
    """
    errs = []
    where = f"row {i}"
    for k in REQUIRED:
        if k not in row:
            errs.append(f"{where}: missing {k!r}")
    if errs:
        return errs
    if row["tier"] not in TIERS:
        errs.append(f"{where}: tier {row['tier']!r} not in {TIERS}")
    src = (row.get("provenance") or {}).get("source")
    if src not in SOURCES:
        errs.append(f"{where}: provenance.source {src!r} not in {SOURCES}")
    if not (row.get("query") or "").strip():
        errs.append(f"{where}: empty query")
    gold = row.get("gold") or {}
    if not (gold.get("funcs") or gold.get("files")):
        errs.append(f"{where}: gold has neither funcs nor files")
    elif recompute and row["tier"] in TIERS:
        try:
            want = tier_of(row["query"], gold)
        except ValueError as e:
            errs.append(f"{where}: {e}")
        else:
            if want != row["tier"]:
                errs.append(
                    f"{where}: tier is {row['tier']!r} but recomputes to {want!r} "
                    f"— query {row['query']!r}"
                )
    return errs


def load(path, recompute=True):
    """Read a traces file, or raise with every problem found at once.

    All problems, not the first: a set built by a script in another repo
    usually fails the same way in many rows, and fixing them one run at a
    time is how a five-minute repair becomes an afternoon.
    """
    rows, errs, seen = [], [], set()
    with open(path) as f:
        for i, line in enumerate(f):
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError as e:
                errs.append(f"row {i}: bad JSON: {e}")
                continue
            errs.extend(validate_row(row, i, recompute))
            if row.get("id") in seen:
                errs.append(f"row {i}: duplicate id {row['id']!r}")
            seen.add(row.get("id"))
            rows.append(row)
    if errs:
        raise ValueError(
            f"{path}: {len(errs)} problem(s)\n  " + "\n  ".join(errs[:40])
            + ("\n  ..." if len(errs) > 40 else "")
        )
    return rows


def counts(rows):
    """`{tier: n}` over every tier, including the empty ones.

    Zero-filled deliberately: a stratum that vanished from a set is a
    property of that set worth seeing, and a dict that omits it reads as
    though the question was never asked.
    """
    out = {t: 0 for t in TIERS}
    for r in rows:
        out[r["tier"]] = out.get(r["tier"], 0) + 1
    return out
