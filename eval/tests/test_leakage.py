"""Tests for the leakage gate.

These numbers decide how every recall figure is *read*. RESEARCH.md §12.1
records that an earlier version of this measurement counted long lowercase
words like `workaround` as identifiers and reported 93% leakage for
paraphrase queries — wrong, and corrected in the doc, with the note "the
audit needed auditing." The tests below pin the corrected predicate so it
cannot regress silently.

The other job here is the fixture pin: `identifiers()` moved out of
`run_eval.rg_strong` into `leakage.py` so the printed percentage and the
baseline's behaviour come from one function. That refactor was required to
change nothing, and `test_rg_strong_attempts_match_the_prerefactor_fixture`
is the proof.
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import leakage  # noqa: E402
import run_eval  # noqa: E402

FIXTURES = Path(__file__).parent / "fixtures"


# --- the identifier predicate ------------------------------------------------

def test_snake_case_is_an_identifier():
    assert leakage.identifiers(["parse_config"]) == ["parse_config"]


def test_camel_case_is_an_identifier():
    assert leakage.identifiers(["parseConfig"]) == ["parseConfig"]


def test_a_long_lowercase_word_is_not_an_identifier():
    # §12.1's own self-correction. `workaround` is a word, not a symbol;
    # counting it inflated paraphrase leakage from 2% to 93%.
    assert leakage.identifiers(["workaround", "statistic", "percpu"]) == []


def test_screaming_snake_case_is_an_identifier():
    assert leakage.identifiers(["MAX_BUFFER_SIZE"]) == ["MAX_BUFFER_SIZE"]


def test_all_caps_without_an_underscore_is_not_an_identifier():
    # No case transition and no underscore — indistinguishable from an
    # ordinary acronym like HTTP, which is not a symbol reference.
    assert leakage.identifiers(["HTTP", "JSON"]) == []


def test_identifiers_are_returned_longest_first():
    # rg_strong's rarity proxy is length, and it takes only the first two.
    got = leakage.identifiers(["a_b", "long_identifier_name", "mid_name"])
    assert got == ["long_identifier_name", "mid_name", "a_b"]


def test_stopwords_are_dropped_from_content_tokens():
    assert leakage.content_tokens("find the config in this file") == ["config"]


def test_content_tokens_preserve_case_but_stopword_match_is_insensitive():
    assert leakage.content_tokens("The Config") == ["Config"]


def test_content_tokens_keep_underscores():
    # The §12.1 defect in one assertion: the legacy tokenizer used
    # [a-zA-Z0-9]+ and shredded this into three useless words.
    assert leakage.content_tokens("blkg_rwstat_add") == ["blkg_rwstat_add"]


# --- gold-token overlap ------------------------------------------------------

def test_overlap_counts_distinct_tokens_not_occurrences():
    # A query repeating `config` five times leaks one token, not five.
    # Counting occurrences would inflate long queries for free.
    assert leakage._overlap(["config"] * 5, ["config"]) == 1.0


def test_overlap_is_over_whole_tokens_not_substrings():
    # `conf` appearing inside `config` is not the query's token being present.
    assert leakage._overlap(["conf"], ["config"]) == 0.0


def test_overlap_is_case_insensitive():
    assert leakage._overlap(["Config"], ["config"]) == 1.0


def test_overlap_of_an_empty_query_is_zero_not_an_error():
    assert leakage._overlap([], ["config"]) == 0.0


# --- path leakage ------------------------------------------------------------

def row(query, file="block/blk-cgroup-rwstat.h", start=1, end=3):
    return {"query": query, "kind": "direct", "file": file,
            "start_line": start, "end_line": end}


def test_basename_and_stem_are_distinguished():
    # The stem leaks without the extension; conflating them would report the
    # far rarer basename case as if it were the common one.
    l = leakage.leakage(row("blk-cgroup-rwstat.h helper"))
    assert l["basename_in_query"] and l["stem_in_query"]
    l = leakage.leakage(row("blk-cgroup-rwstat helper"))
    assert not l["basename_in_query"] and l["stem_in_query"]


def test_a_directory_segment_in_the_query_is_path_leakage():
    assert leakage.leakage(row("block layer helper"))["path_seg_in_query"]


def test_short_path_segments_do_not_count_as_leakage():
    # "io" and "os" appear in ordinary English and in half of any corpus;
    # crediting them would report leakage everywhere and mean nothing.
    assert not leakage.leakage(row("write a value", file="io/os/thing.c"))["path_seg_in_query"]


def test_path_seg_not_in_gold_needs_the_corpus(tmp_path):
    # Without a corpus the comparison is absent, not guessed at.
    assert leakage.leakage(row("block layer"))["path_seg_not_in_gold"] is False


def test_path_seg_not_in_gold_fires_when_the_segment_is_absent_from_gold(tmp_path):
    (tmp_path / "block").mkdir()
    (tmp_path / "block" / "a.c").write_text("int helper(void) { return 1; }\n")
    r = {"query": "block helper", "kind": "direct", "file": "block/a.c",
         "start_line": 1, "end_line": 1}
    assert leakage.leakage(r, tmp_path)["path_seg_not_in_gold"]


def test_path_seg_not_in_gold_is_false_when_the_gold_text_contains_it(tmp_path):
    # This is the isolation that matters: if the gold body itself says
    # "block", the query carrying "block" is ordinary vocabulary overlap,
    # not the eval feeding the tokenizer the document identifier.
    (tmp_path / "block").mkdir()
    (tmp_path / "block" / "a.c").write_text("/* the block layer */\nint helper(void);\n")
    r = {"query": "block helper", "kind": "direct", "file": "block/a.c",
         "start_line": 1, "end_line": 2}
    assert not leakage.leakage(r, tmp_path)["path_seg_not_in_gold"]


def test_unreadable_gold_does_not_manufacture_leakage(tmp_path):
    # An empty string would make every segment "absent from gold" and report
    # 100% path leakage on a corpus that simply isn't there.
    r = {"query": "block helper", "kind": "direct", "file": "block/missing.c",
         "start_line": 1, "end_line": 1}
    l = leakage.leakage(r, tmp_path)
    assert not l["path_seg_not_in_gold"]
    assert "gold_token_overlap" not in l


# --- summary -----------------------------------------------------------------

def test_summary_groups_by_kind_and_counts_add_up():
    rows = [row("parse_config here"), row("a helper for counters"),
            {**row("other"), "kind": "paraphrase"}]
    s = leakage.summarize(rows)
    assert s["direct"]["n"] == 2 and s["paraphrase"]["n"] == 1
    assert sum(v["n"] for v in s.values()) == len(rows)


def test_summary_identifier_pct_matches_the_predicate():
    rows = [row("parse_config here"), row("a helper for counters")]
    assert leakage.summarize(rows)["direct"]["identifier_pct"] == 0.5


def test_format_summary_renders_without_a_corpus():
    out = leakage.format_summary(leakage.summarize([row("x_y")]))
    assert "query-set leakage" in out and "direct" in out


# --- the refactor pin --------------------------------------------------------

def _attempts_via(fn, query):
    """Capture the ordered patterns a baseline tries, without running rg."""
    seen = []
    orig = run_eval.rg_run
    run_eval.rg_run = lambda pattern, corpus, k, flags=(): (seen.append(pattern), [])[1]
    try:
        fn(query, Path("/nonexistent"), 10)
    finally:
        run_eval.rg_run = orig
    return seen


def test_rg_strong_attempts_match_the_prerefactor_fixture():
    """Pinned against attempts captured BEFORE identifiers() moved to
    leakage.py. If this fails, the rg-strong baseline changed, and §12.2's
    published fair-gap numbers (kernel direct R@5 0.32, VS Code 0.355)
    changed with it. Stop and reconcile — do not re-record the fixture."""
    golden = json.loads((FIXTURES / "rg_baseline_attempts.json").read_text())
    assert golden, "fixture is empty"
    for g in golden:
        # The fixture predates two guards, neither of which can change a
        # result: an empty pattern matches every line and was never a
        # legitimate attempt, and a repeated pattern returns what its first
        # occurrence already returned.
        want = run_eval._dedupe(g["rg_strong_attempts"])
        assert _attempts_via(run_eval.rg_strong, g["query"]) == want, g["query"]


def test_legacy_rg_attempts_match_the_prerefactor_fixture():
    golden = json.loads((FIXTURES / "rg_baseline_attempts.json").read_text())
    for g in golden:
        want = run_eval._dedupe(g["rg_attempts"])
        assert _attempts_via(run_eval.rg_agent_style, g["query"]) == want, g["query"]


def test_dedupe_preserves_order_and_drops_empties_and_repeats():
    assert run_eval._dedupe(["a", "", "b", "a", "c", "b"]) == ["a", "b", "c"]


def test_rg_strong_and_leakage_agree_on_what_an_identifier_is():
    # The point of the refactor. If these ever diverge, the leakage table
    # describes a baseline that no longer exists.
    q = "parse_config workaround parseOther"
    idents = leakage.identifiers(leakage.content_tokens(q))
    attempts = run_eval.rg_strong_attempts(q)
    assert attempts[:len(idents[:2])] == [__import__("re").escape(i) for i in idents[:2]]


# --- degenerate queries: the 16% of real agent queries that used to crash ----

def test_a_single_word_query_does_not_crash_the_legacy_baseline():
    # Was IndexError: the inline attempt tuple indexed [0] on an empty list.
    # 0 of 2,398 published queries hit this; 195/1194 real agent queries do.
    assert _attempts_via(run_eval.rg_agent_style, "init") == ["init"]


def test_an_all_stopword_query_greps_for_nothing_rather_than_everything():
    # An empty pattern matches every line, scoring hits it did not earn.
    assert _attempts_via(run_eval.rg_agent_style, "the of and") == ["the of and"]
    assert _attempts_via(run_eval.rg_strong, "the of and") == [
        __import__("re").escape("the of and")]


def test_an_empty_query_produces_no_attempts_at_all():
    assert _attempts_via(run_eval.rg_strong, "") == []
    assert _attempts_via(run_eval.rg_agent_style, "") == []
