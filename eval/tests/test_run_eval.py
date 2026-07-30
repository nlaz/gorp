"""Tests for the retrieval scorer's hit predicate.

`correct()` decides whether a returned chunk counts as finding the ground-truth
span, and therefore decides every recall@k and MRR number in RESEARCH.md. It is
six lines of interval arithmetic with a slack parameter, which is exactly the
shape of thing that is off by one in one direction and nobody notices.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import run_eval  # noqa: E402


def truth(start=10, end=20, file="src/a.py"):
    return {"file": file, "start_line": start, "end_line": end}


def hit(start, end, file="src/a.py"):
    return (file, start, end)


def test_a_chunk_containing_the_truth_is_correct():
    assert run_eval.correct(hit(1, 40), truth(), slack=0)


def test_a_chunk_inside_the_truth_is_correct():
    assert run_eval.correct(hit(12, 15), truth(), slack=0)


def test_partial_overlap_counts():
    # Overlap, not containment: a chunking scheme that splits the target must
    # still be credited, or the metric is measuring chunk alignment.
    assert run_eval.correct(hit(1, 12), truth(), slack=0), "overlaps the start"
    assert run_eval.correct(hit(18, 30), truth(), slack=0), "overlaps the end"


def test_the_wrong_file_is_never_correct():
    assert not run_eval.correct(hit(10, 20, "src/b.py"), truth(), slack=0)
    # Not even with unlimited slack.
    assert not run_eval.correct(hit(10, 20, "src/b.py"), truth(), slack=1000)


def test_disjoint_spans_are_not_correct():
    assert not run_eval.correct(hit(1, 9), truth(), slack=0), "ends before the truth"
    assert not run_eval.correct(hit(21, 30), truth(), slack=0), "starts after the truth"


def test_touching_at_a_single_line_counts():
    # A chunk ending exactly on the truth's first line does overlap it.
    assert run_eval.correct(hit(1, 10), truth(), slack=0)
    assert run_eval.correct(hit(20, 30), truth(), slack=0)


def test_slack_widens_the_window_symmetrically():
    # Nine lines short of the truth: rejected at slack 0, accepted at slack 1.
    assert not run_eval.correct(hit(1, 9), truth(), slack=0)
    assert run_eval.correct(hit(1, 9), truth(), slack=1)

    assert not run_eval.correct(hit(21, 30), truth(), slack=0)
    assert run_eval.correct(hit(21, 30), truth(), slack=1)


def test_slack_boundary_is_inclusive():
    # Exactly `slack` lines away is inside the window; one more is outside.
    assert run_eval.correct(hit(1, 5), truth(), slack=5)
    assert not run_eval.correct(hit(1, 4), truth(), slack=5)


def test_a_single_line_truth_is_matchable():
    point = truth(start=10, end=10)
    assert run_eval.correct(hit(10, 10), point, slack=0)
    assert run_eval.correct(hit(5, 15), point, slack=0)
    assert not run_eval.correct(hit(11, 15), point, slack=0)
