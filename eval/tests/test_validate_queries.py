"""Tests for the gold-span validator and the strata/filter plumbing.

The validator exists because of a failure mode this harness has already been
bitten by once: something is wrong, nothing raises, every row scores 0, and
the output looks like a measurement. `run_eval.py` learned it for a mismatched
embedding width; a query set that has drifted from its corpus produces the
identical symptom for a different reason.

A validator that passes on good data and also passes on bad data is worthless,
so every check below is paired with the mutation it is supposed to catch.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import run_eval  # noqa: E402
import validate_queries as vq  # noqa: E402


def corpus_with(tmp_path, body="one\ntwo\nthree\nfour\nfive\n", rel="src/a.py"):
    p = tmp_path / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(body)
    return tmp_path


def row(**kw):
    r = {"query": "q", "kind": "direct", "file": "src/a.py",
         "start_line": 2, "end_line": 4}
    r.update(kw)
    return r


# --- the happy path ----------------------------------------------------------

def test_a_matching_set_has_no_problems(tmp_path):
    problems, stats = vq.validate([row()], corpus_with(tmp_path))
    assert problems == []
    assert stats["n_rows"] == 1 and stats["n_files"] == 1


def test_a_span_ending_exactly_at_eof_is_valid(tmp_path):
    # Off-by-one bait: 5 lines, end_line 5 is the last line, not past it.
    assert vq.validate([row(start_line=1, end_line=5)], corpus_with(tmp_path))[0] == []


# --- the mutations it must catch ---------------------------------------------

def test_a_missing_gold_file_is_a_problem(tmp_path):
    problems, _ = vq.validate([row(file="src/gone.py")], corpus_with(tmp_path))
    assert [k for _, k, _ in problems] == ["missing-file"]


def test_a_span_past_eof_is_a_problem(tmp_path):
    problems, _ = vq.validate([row(start_line=4, end_line=99)], corpus_with(tmp_path))
    assert [k for _, k, _ in problems] == ["span-past-eof"]


def test_a_zero_or_negative_start_line_is_a_problem(tmp_path):
    problems, _ = vq.validate([row(start_line=0, end_line=2)], corpus_with(tmp_path))
    assert [k for _, k, _ in problems] == ["span-before-start"]


def test_an_inverted_span_is_a_problem(tmp_path):
    problems, _ = vq.validate([row(start_line=4, end_line=2)], corpus_with(tmp_path))
    assert [k for _, k, _ in problems] == ["inverted-span"]


def test_a_non_integer_span_is_a_problem_not_a_crash(tmp_path):
    problems, _ = vq.validate([row(start_line="2", end_line=4)], corpus_with(tmp_path))
    assert [k for _, k, _ in problems] == ["bad-span-type"]


def test_a_row_without_a_file_field_is_a_problem(tmp_path):
    r = row()
    del r["file"]
    problems, _ = vq.validate([r], corpus_with(tmp_path))
    assert [k for _, k, _ in problems] == ["no-file-field"]


# --- gold_sha: the only check that catches an EDITED file --------------------

def test_gold_sha_matches_when_the_content_is_unchanged(tmp_path):
    c = corpus_with(tmp_path)
    sha = vq.span_sha("two\nthree\nfour")
    assert vq.validate([row(gold_sha=sha)], c)[0] == []


def test_gold_sha_catches_an_edit_that_preserves_path_and_line_count(tmp_path):
    # The realistic drift: the file still exists and is still long enough, so
    # every other check passes and the gold span is silently about something
    # else. This is the case the validator exists for.
    c = corpus_with(tmp_path)
    sha = vq.span_sha("two\nthree\nfour")
    (c / "src/a.py").write_text("one\nTWO\nTHREE\nFOUR\nfive\n")
    problems, _ = vq.validate([row(gold_sha=sha)], c)
    assert [k for _, k, _ in problems] == ["gold-content-changed"]


def test_a_row_without_gold_sha_is_unverifiable_not_failed(tmp_path):
    problems, stats = vq.validate([row()], corpus_with(tmp_path))
    assert problems == []
    assert stats["n_sha_checked"] == 0
    # Reported honestly: this set cannot detect an edited file at all.
    assert stats["n_unverifiable"] == 1


def test_span_sha_is_insensitive_to_line_endings():
    # A checkout that flips CRLF is not content drift.
    assert vq.span_sha("a\r\nb") == vq.span_sha("a\nb")


def test_no_sha_mode_reports_everything_as_unverifiable(tmp_path):
    _, stats = vq.validate([row(gold_sha="deadbeef")], corpus_with(tmp_path),
                           check_sha=False)
    assert stats["n_sha_checked"] == 0 and stats["n_unverifiable"] == 1


# --- --where -----------------------------------------------------------------

def test_where_filters_rows():
    rows = [row(split="dev"), row(split="locked"), row(split="dev")]
    assert len(run_eval.apply_where(rows, "split=dev")) == 2


def test_where_with_no_clause_returns_everything():
    rows = [row(), row()]
    assert run_eval.apply_where(rows, "") is rows


def test_where_compares_as_strings_so_bools_and_ints_work():
    rows = [row(has_doc=True), row(has_doc=False)]
    assert len(run_eval.apply_where(rows, "has_doc=True")) == 1


def test_where_on_an_unknown_field_is_an_error_not_an_empty_result():
    # Silently returning zero rows would read as "this stratum is empty",
    # which is a finding. A typo is not a finding.
    try:
        run_eval.apply_where([row()], "spilt=dev")
    except SystemExit as e:
        assert "spilt" in str(e)
    else:
        raise AssertionError("expected SystemExit")


def test_a_malformed_where_clause_is_an_error():
    try:
        run_eval.apply_where([row()], "split")
    except SystemExit:
        pass
    else:
        raise AssertionError("expected SystemExit")


# --- --stratify --------------------------------------------------------------

def test_the_default_cell_is_just_the_kind():
    assert run_eval.cell_of(row(), []) == "direct"


def test_stratify_on_a_row_field_splits_the_cell():
    assert run_eval.cell_of(row(lang="c"), ["lang"]) == "direct|lang=c"


def test_stratify_on_several_fields_composes():
    assert run_eval.cell_of(row(lang="c", has_doc=True), ["lang", "has_doc"]) == \
        "direct|lang=c|has_doc=True"


def test_stratify_on_a_computed_leakage_field_works():
    # The path-leakage question needs this: `has_identifier` is on no row,
    # it is derived from the query.
    assert run_eval.cell_of(row(query="parse_config"), ["has_identifier"]) == \
        "direct|has_identifier=True"
    assert run_eval.cell_of(row(query="a helper"), ["has_identifier"]) == \
        "direct|has_identifier=False"


def test_stratify_on_an_unknown_field_is_an_error():
    # A stratum that lumps every row into `None` looks like a breakdown and
    # is not one.
    try:
        run_eval.cell_of(row(), ["nonesuch"])
    except SystemExit as e:
        assert "nonesuch" in str(e)
    else:
        raise AssertionError("expected SystemExit")


def test_strata_partition_the_rows_exactly():
    rows = [row(lang="c"), row(lang="rust"), row(lang="c")]
    cells = [run_eval.cell_of(r, ["lang"]) for r in rows]
    assert len(cells) == len(rows)
    assert sorted(set(cells)) == ["direct|lang=c", "direct|lang=rust"]


# --- the blind gate (RESEARCH.md §15.3) --------------------------------------

def blind_corpus(tmp_path):
    return corpus_with(
        tmp_path,
        body="def flush(self):\n    # drain the pending buffer\n"
             "    self._ring_buffer.clear()\n    return True\n",
        rel="src/a.py")


def test_a_genuinely_blind_row_passes(tmp_path):
    r = row(kind="blind", query="force queued output to disk",
            symbol="flush", start_line=1, end_line=4)
    problems, stats = vq.validate([r], blind_corpus(tmp_path))
    assert problems == []
    assert stats["n_blind_checked"] == 1


def test_a_blind_row_naming_the_lowercase_symbol_is_refused(tmp_path):
    r = row(kind="blind", query="flush the buffer", symbol="flush",
            start_line=1, end_line=4)
    problems, _ = vq.validate([r], blind_corpus(tmp_path))
    assert "blind-violation" in [p[1] for p in problems]
    assert "flush" in problems[0][2]


def test_a_blind_row_naming_a_gold_identifier_is_refused(tmp_path):
    r = row(kind="blind", query="clear the _ring_buffer here", symbol="flush",
            start_line=1, end_line=4)
    problems, _ = vq.validate([r], blind_corpus(tmp_path))
    assert "blind-violation" in [p[1] for p in problems]


def test_overlap_above_the_row_cap_is_refused(tmp_path):
    # No identifier named, but every content token appears in the gold.
    r = row(kind="blind", query="drain pending buffer", symbol="flush",
            start_line=1, end_line=4)
    problems, _ = vq.validate([r], blind_corpus(tmp_path))
    assert "blind-violation" in [p[1] for p in problems]
    assert "row cap" in problems[0][2]


def test_paraphrase_rows_are_never_gated(tmp_path):
    # The historical kinds keep validating exactly as before, however leaky.
    r = row(kind="paraphrase", query="flush the _ring_buffer", symbol="flush",
            start_line=1, end_line=4)
    problems, stats = vq.validate([r], blind_corpus(tmp_path))
    assert problems == []
    assert stats["n_blind_checked"] == 0


def test_where_falls_back_to_computed_leakage_fields(tmp_path):
    corpus = blind_corpus(tmp_path)
    rows = [row(query="force queued output to disk", symbol="flush",
                start_line=1, end_line=4),
            row(query="flush the pending buffer", symbol="flush",
                start_line=1, end_line=4)]
    kept = run_eval.apply_where(rows, "is_blind=True", corpus)
    assert len(kept) == 1 and kept[0]["query"].startswith("force")


def test_where_on_an_unknown_field_still_errors_with_both_lists(tmp_path):
    try:
        run_eval.apply_where([row()], "nonesuch=1", blind_corpus(tmp_path))
    except SystemExit as e:
        assert "nonesuch" in str(e)
    else:
        raise AssertionError("expected SystemExit")
