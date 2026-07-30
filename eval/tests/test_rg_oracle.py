"""Tests for the ripgrep ceiling.

`rg-oracle` exists to try to falsify this project's headline claim. §12.2
already found the original `rg` baseline was a strawman and the "30× gap"
was really ~2.9×; `rg-strong` replaced it but is still a hand-tuned heuristic
(two identifiers, longest first). The oracle removes the query planning
entirely and keeps whichever token happened to score best.

The invariant that makes it meaningful is `rank(rg-oracle) <= rank(rg-strong)`
— an upper bound that can be beaten by the thing it bounds is not an upper
bound, it is a bug. Most of the tests below defend the pruning, which is the
only part that could quietly violate it.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import run_eval  # noqa: E402


def corpus(tmp_path, files):
    for rel, body in files.items():
        p = tmp_path / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body)
    return tmp_path


def truth(file="src/a.py", start=1, end=3):
    return {"file": file, "start_line": start, "end_line": end}


# --- the pruning window ------------------------------------------------------

def test_the_gold_window_is_the_span_widened_by_slack(tmp_path):
    c = corpus(tmp_path, {"src/a.py": "one\ntwo\nthree\nfour\nfive\nsix\n"})
    w = run_eval._gold_window(c, truth(start=3, end=3), slack=1)
    assert "two" in w and "three" in w and "four" in w
    assert "one" not in w and "five" not in w


def test_the_gold_window_is_lowercased(tmp_path):
    # rg runs with -i, so the token test must be case-insensitive too or the
    # pruning would drop tokens that would in fact have matched.
    c = corpus(tmp_path, {"src/a.py": "ParseConfig\n"})
    assert "parseconfig" in run_eval._gold_window(c, truth(start=1, end=1), slack=0)


def test_the_gold_window_clamps_at_both_ends(tmp_path):
    c = corpus(tmp_path, {"src/a.py": "only\n"})
    assert run_eval._gold_window(c, truth(start=1, end=1), slack=100) == "only"


def test_a_missing_gold_file_yields_no_window(tmp_path):
    assert run_eval._gold_window(tmp_path, truth(file="gone.py"), slack=0) is None


def test_a_missing_gold_file_makes_the_oracle_return_no_hits(tmp_path):
    # It cannot bound anything if it cannot read the answer. Returning [] is
    # right; guessing would silently invent a ceiling.
    c = corpus(tmp_path, {"src/a.py": "x\n"})
    assert run_eval.rg_oracle("anything", c, 10, truth(file="gone.py"), 0) == []


# --- the ceiling property ----------------------------------------------------

def test_the_oracle_finds_a_token_rg_strong_never_tries(tmp_path):
    """The whole point, in one case.

    rg_strong only ever greps for its top-2 identifiers, then the phrase, then
    the two longest tokens. Here both identifiers appear in 30 decoy files, so
    every attempt it makes returns decoys and it scores a miss. The winning
    token, `zebra`, is a plain lowercase word — deliberately NOT an identifier
    (see leakage.identifiers and §12.1), so rg_strong never reaches for it.
    """
    files = {f"decoy/d{i}.py": "parse_config handler_thing\n" for i in range(30)}
    files["src/a.py"] = "zebra\n"
    c = corpus(tmp_path, files)
    t = truth(start=1, end=1)
    q = "parse_config handler_thing zebra"

    strong = run_eval.rg_strong(q, c, 10)
    oracle = run_eval.rg_oracle(q, c, 10, t, 0)

    def rank(hits):
        return next((i + 1 for i, h in enumerate(hits) if run_eval.correct(h, t, 0)), None)

    assert rank(strong) is None, "fixture no longer exercises the gap"
    assert rank(oracle) == 1


def test_a_conjunctive_pattern_can_beat_every_single_token(tmp_path):
    """The case that broke the bound, kept as the reason the fix exists.

    `alpha` and `beta` each appear in 20 decoy files, so neither alone ranks
    gold in the top 10. `alpha.*beta` — which rg_strong tries and a
    single-token vocabulary cannot express — matches only the gold line. An
    oracle built from single tokens alone therefore LOST to rg_strong here,
    on 53 of 1,374 real queries before this was fixed.
    """
    files = {f"decoy/a{i}.py": "alpha only\n" for i in range(20)}
    files.update({f"decoy/b{i}.py": "beta only\n" for i in range(20)})
    files["src/a.py"] = "alpha and beta together\n"
    c = corpus(tmp_path, files)
    t = truth(start=1, end=1)
    q = "alpha beta"

    def rank(hits):
        return next((i + 1 for i, h in enumerate(hits) if run_eval.correct(h, t, 0)), None)

    rs = rank(run_eval.rg_strong(q, c, 10))
    ro = rank(run_eval.rg_oracle(q, c, 10, t, 0))
    assert rs is not None, "fixture no longer exercises the conjunctive win"
    assert ro is not None and ro <= rs


def test_the_oracle_tries_everything_rg_strong_tries(tmp_path):
    # The structural guarantee behind the bound: the candidate set is a
    # superset of rg_strong's attempts, so the oracle cannot do worse.
    c = corpus(tmp_path, {"src/a.py": "parse_config handler value\n"})
    seen = []
    orig = run_eval.rg_run
    run_eval.rg_run = lambda p, corp, k, flags=(): (seen.append(p), [])[1]
    try:
        run_eval.rg_oracle("parse_config handler value", c, 10, truth(start=1, end=1), 0)
    finally:
        run_eval.rg_run = orig
    for pat in run_eval.rg_strong_attempts("parse_config handler value"):
        assert pat in seen, pat


def test_the_oracle_rank_is_never_worse_than_rg_strong(tmp_path):
    """The upper-bound invariant, over a spread of query shapes."""
    files = {f"noise/n{i}.py": f"filler helper value {i}\n" for i in range(12)}
    files["src/a.py"] = "def parse_config(handler):\n    return handler.value\n"
    files["src/b.py"] = "helper value\n"
    c = corpus(tmp_path, files)
    t = truth(start=1, end=2)

    def rank(hits):
        return next((i + 1 for i, h in enumerate(hits) if run_eval.correct(h, t, 0)), None)

    for q in ["parse_config handler", "handler value", "the helper for value",
              "parse_config", "value", "nonexistent_token_here",
              "parse_config nonexistent_token_here"]:
        rs = rank(run_eval.rg_strong(q, c, 10))
        ro = rank(run_eval.rg_oracle(q, c, 10, t, 0))
        if rs is not None:
            assert ro is not None and ro <= rs, f"{q!r}: oracle {ro} > strong {rs}"


def test_pruning_cannot_discard_a_token_that_would_have_scored(tmp_path):
    """The pruning is exact, not heuristic — this is where that is checked.

    A token absent from the gold window cannot produce a hit `correct()`
    accepts, so skipping its scan cannot change the answer. Verified by
    comparing against an unpruned oracle over the same corpus.
    """
    files = {"src/a.py": "alpha beta\n", "other/b.py": "gamma delta\n"}
    c = corpus(tmp_path, files)
    t = truth(start=1, end=1)

    def unpruned(query):
        best = None
        for tok in run_eval.content_tokens(query):
            hits = run_eval.rg_run(__import__("re").escape(tok), c, 10, flags=("-i",))
            r = next((i + 1 for i, h in enumerate(hits) if run_eval.correct(h, t, 0)), None)
            if r is not None and (best is None or r < best):
                best = r
        return best

    for q in ["alpha gamma", "gamma delta", "alpha beta gamma delta", "delta"]:
        hits = run_eval.rg_oracle(q, c, 10, t, 0)
        pruned = next((i + 1 for i, h in enumerate(hits) if run_eval.correct(h, t, 0)), None)
        assert pruned == unpruned(q), q


# --- token selection ---------------------------------------------------------

def test_the_token_budget_is_capped(tmp_path):
    c = corpus(tmp_path, {"src/a.py": " ".join(f"tok_{i}" for i in range(40)) + "\n"})
    q = " ".join(f"tok_{i}" for i in range(40))
    calls = []
    orig = run_eval.rg_run
    run_eval.rg_run = lambda p, corp, k, flags=(): (calls.append(p), orig(p, corp, k, flags))[1]
    try:
        run_eval.rg_oracle(q, c, 10, truth(start=1, end=1), 0)
    finally:
        run_eval.rg_run = orig
    # The cap applies to the single-token additions; rg_strong's own attempts
    # are always included, because bounding them requires trying them.
    n_strong = len(run_eval.rg_strong_attempts(q))
    assert len(calls) <= run_eval.ORACLE_MAX_TOKENS + n_strong


def test_stopwords_are_not_scanned_for_on_their_own(tmp_path):
    # Grepping for "the" alone would match half the corpus and cost a full
    # scan. (It may still appear inside rg_strong's escaped exact phrase.)
    c = corpus(tmp_path, {"src/a.py": "the value of the thing\n"})
    calls = []
    orig = run_eval.rg_run
    run_eval.rg_run = lambda p, corp, k, flags=(): (calls.append(p), [])[1]
    try:
        run_eval.rg_oracle("the value of the thing", c, 10, truth(start=1, end=1), 0)
    finally:
        run_eval.rg_run = orig
    assert "the" not in calls and "of" not in calls


def test_a_query_with_no_live_tokens_still_tries_rg_strongs_attempts(tmp_path):
    # Pruning removes single-token scans that cannot score. It must NOT remove
    # rg_strong's attempts: the oracle bounds rg_strong, and it cannot bound
    # what it never runs.
    c = corpus(tmp_path, {"src/a.py": "alpha\n"})
    calls = []
    orig = run_eval.rg_run
    run_eval.rg_run = lambda p, corp, k, flags=(): (calls.append(p), [])[1]
    try:
        got = run_eval.rg_oracle("zeta eta theta", c, 10, truth(start=1, end=1), 0)
    finally:
        run_eval.rg_run = orig
    assert got == []
    assert calls == run_eval.rg_strong_attempts("zeta eta theta")


def test_token_order_is_deterministic():
    # Same query must produce the same scans every run, or two invocations of
    # the "same" condition differ.
    q = "parse_config handler value someOther thing"
    a = run_eval.content_tokens(q)
    b = run_eval.content_tokens(q)
    assert a == b
    assert run_eval.identifiers(a) == run_eval.identifiers(b)
