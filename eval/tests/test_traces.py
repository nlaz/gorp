"""The tier rule decides which stratum every trace lands in, so it gets the
same scrutiny as the scorers: a rule that drifts silently re-labels history.
"""

import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import traces  # noqa: E402


GOLD = {
    "files": ["uxarray/grid/coordinates.py"],
    "funcs": ["uxarray/grid/coordinates.py:_construct_face_centroids"],
}


# --- the three tiers ---------------------------------------------------------

def test_naming_a_gold_function_is_golden():
    assert traces.tier_of("_construct_face_centroids is slow", GOLD) == "golden"


def test_naming_only_the_path_is_a_guess():
    # `coordinates` is the gold file's stem, not a gold identifier: the agent
    # has located the file without naming what it wants inside it.
    assert traces.tier_of("centroid coordinates helper", GOLD) == "guess"


def test_sharing_neither_is_blind():
    assert traces.tier_of("why is the mesh so slow to load", GOLD) == "blind"


def test_golden_beats_guess_when_a_query_carries_both():
    q = "_construct_face_centroids in coordinates"
    assert traces.tier_of(q, GOLD) == "golden"


# --- what must NOT count -----------------------------------------------------

def test_a_substring_of_an_identifier_does_not_name_it():
    """`centroid` appears inside `_construct_face_centroids` as a substring.

    Whole-word matching only: a query that types a fragment guessed at the
    vocabulary, which is the guess tier's definition, not the golden one.
    """
    assert traces.tier_of("centroid", GOLD) != "golden"


def test_case_matters_for_identifiers():
    gold = {"files": ["g.py"], "funcs": ["g.py:Grid.face_lon"]}
    assert traces.tier_of("Grid face_lon", gold) == "golden"
    assert traces.tier_of("grid", gold) != "golden"


def test_trivial_qualname_parts_do_not_make_a_query_golden():
    """Otherwise every query containing `get` names half the corpus."""
    gold = {"files": ["a/b.py"], "funcs": ["a/b.py:Thing.get"]}
    assert traces.tier_of("get the thing", gold) != "golden"


def test_universal_path_segments_do_not_make_a_query_a_guess():
    gold = {"files": ["src/lib/retry.rs"], "funcs": []}
    assert traces.tier_of("src lib", gold) == "blind"
    assert traces.tier_of("retry", gold) == "guess"


def test_extensions_are_not_path_vocabulary():
    gold = {"files": ["net/retry.rs"], "funcs": []}
    assert traces.tier_of("rs", gold) == "blind"


def test_gold_with_nothing_in_it_raises_rather_than_guessing():
    with pytest.raises(ValueError):
        traces.tier_of("anything", {"files": [], "funcs": []})


# --- row validation ----------------------------------------------------------

def _row(**over):
    row = {
        "id": "abc123",
        "query": "_construct_face_centroids is slow",
        "tier": "golden",
        "provenance": {"source": "harvested", "harness": "locbench",
                       "run_id": "r1", "condition": "desc-v8", "tool": "semgrep",
                       "kind": "guess_ranked"},
        "target": {"repo": "UXARRAY/uxarray", "sha": "fe4cae13"},
        "gold": dict(GOLD),
    }
    row.update(over)
    return row


def test_a_well_formed_row_validates():
    assert traces.validate_row(_row()) == []


def test_a_stamped_tier_that_disagrees_is_an_error():
    errs = traces.validate_row(_row(tier="blind"))
    assert any("recomputes to 'golden'" in e for e in errs)


def test_missing_fields_are_reported_together():
    row = _row()
    del row["gold"], row["target"]
    errs = traces.validate_row(row)
    assert len(errs) == 2


def test_an_unknown_provenance_source_is_rejected():
    errs = traces.validate_row(_row(provenance={"source": "invented"}))
    assert any("provenance.source" in e for e in errs)


def test_recompute_can_be_switched_off_for_speed():
    assert traces.validate_row(_row(tier="blind"), recompute=False) == []


# --- files -------------------------------------------------------------------

def test_load_rejects_duplicate_ids(tmp_path):
    p = tmp_path / "t.jsonl"
    p.write_text("\n".join(json.dumps(_row()) for _ in range(2)))
    with pytest.raises(ValueError, match="duplicate id"):
        traces.load(p)


def test_load_reports_every_problem_not_just_the_first(tmp_path):
    p = tmp_path / "t.jsonl"
    p.write_text("\n".join([
        json.dumps(_row(id="a", tier="blind")),
        json.dumps(_row(id="b", tier="guess")),
    ]))
    with pytest.raises(ValueError) as e:
        traces.load(p)
    assert "2 problem(s)" in str(e.value)


def test_counts_zero_fills_every_tier(tmp_path):
    rows = [_row(id="a")]
    assert traces.counts(rows) == {"blind": 0, "guess": 0, "golden": 1}


def test_the_checked_in_trace_sets_are_valid():
    """Every traces-*.jsonl in eval/queries/ must load, tiers recomputed.

    This is the guard that makes the cross-repo contract real: gorp-bench
    writes these files, this repo scores them, and nothing else joins the two.
    """
    qdir = Path(__file__).resolve().parents[1] / "queries"
    found = sorted(qdir.glob("traces-*.jsonl"))
    for path in found:
        rows = traces.load(path)
        assert rows, f"{path.name} is empty"
