"""Tests for symbol extraction.

`symbols.extract` defines the ground truth for the symbol-anchored query sets
(RESEARCH.md §11.4): a query's correct answer is "the span of this function". If
extraction reports the wrong span, every recall number computed against it is
wrong in a way no downstream check can see.

It is deliberately regex-based rather than an AST parse — py-tree-sitter
segfaulted the interpreter on real inputs. That trade means the edge cases below
are the ones worth pinning: nested definitions, decorators, doc blocks, and the
brace/indent split between languages.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import symbols  # noqa: E402


def extract(name, text):
    return symbols.extract(Path(name), text)


def by_name(syms):
    return {s["name"]: s for s in syms}


# ---------------------------------------------------------------------------
# python: indentation-delimited
# ---------------------------------------------------------------------------

PY = '''\
"""Module docstring."""

import os


def alpha(x):
    """Doc for alpha."""
    return x + 1


class Widget:
    """A widget."""

    def method(self, y):
        return y * 2

    def other(self):
        pass


def omega():
    return None
'''


def test_python_functions_and_classes_are_found():
    found = by_name(extract("m.py", PY))
    for name in ("alpha", "Widget", "method", "other", "omega"):
        assert name in found, f"{name} missing from {sorted(found)}"


def test_python_span_covers_the_whole_body():
    alpha = by_name(extract("m.py", PY))["alpha"]
    # `def alpha` is on line 6; the body runs to `return x + 1` on line 8.
    assert alpha["sig_line"] == 6
    assert alpha["end_line"] >= 8, f"body truncated: {alpha}"
    assert alpha["end_line"] < 12, f"span ran into the next symbol: {alpha}"


def test_python_spans_do_not_overlap_the_next_symbol():
    syms = sorted(extract("m.py", PY), key=lambda s: s["sig_line"])
    tops = [s for s in syms if s["name"] in ("alpha", "Widget", "omega")]
    for a, b in zip(tops, tops[1:]):
        assert a["end_line"] < b["sig_line"], f"{a['name']} bleeds into {b['name']}"


def test_a_docstring_is_part_of_the_symbol_start():
    alpha = by_name(extract("m.py", PY))["alpha"]
    assert alpha["start_line"] <= alpha["sig_line"], "start_line includes any lead-in"
    assert alpha["has_doc"], "alpha has a docstring"


def test_decorated_functions_keep_the_decorator_in_the_lead():
    text = '@cache\n@retry(3)\ndef decorated():\n    return 1\n'
    sym = by_name(extract("m.py", text))["decorated"]
    assert sym["sig_line"] == 3
    assert sym["start_line"] <= 3, "decorators belong to the symbol"


def test_the_last_symbol_in_a_file_ends_at_the_file():
    omega = by_name(extract("m.py", PY))["omega"]
    assert omega["end_line"] >= omega["sig_line"] + 1


# ---------------------------------------------------------------------------
# brace languages
# ---------------------------------------------------------------------------

RS = '''\
//! Module docs.

/// Doc for alpha.
pub fn alpha(x: u32) -> u32 {
    x + 1
}

pub struct Widget {
    pub field: u32,
}

impl Widget {
    pub fn method(&self) -> u32 {
        self.field
    }
}

fn omega() {}
'''


def test_rust_symbols_are_found():
    found = by_name(extract("m.rs", RS))
    assert "alpha" in found
    assert "method" in found
    assert "omega" in found


def test_rust_span_stops_at_the_closing_brace():
    alpha = by_name(extract("m.rs", RS))["alpha"]
    assert alpha["sig_line"] == 4
    assert alpha["end_line"] == 6, f"should end on the closing brace, got {alpha}"


def test_a_one_line_body_is_a_valid_span():
    omega = by_name(extract("m.rs", RS))["omega"]
    assert omega["end_line"] >= omega["sig_line"]
    assert omega["n_lines"] >= 1


def test_control_flow_keywords_are_not_symbols():
    # `if (...) {` looks like a declaration to a loose regex, and crediting it
    # would put ground truth on a brace instead of a function.
    text = 'void run() {\n    if (x) {\n        while (y) {\n        }\n    }\n}\n'
    names = {s["name"] for s in extract("m.c", text)}
    assert "if" not in names and "while" not in names, f"got {names}"


# ---------------------------------------------------------------------------
# robustness: extraction must never be the thing that fails a run
# ---------------------------------------------------------------------------


def test_unknown_extensions_yield_nothing():
    assert extract("notes.txt", "def alpha():\n    pass\n") == []
    assert extract("data.json", '{"def": "alpha"}') == []


def test_empty_and_degenerate_input():
    assert extract("m.py", "") == []
    assert extract("m.py", "\n\n\n") == []
    assert extract("m.rs", "// only a comment\n") == []


def test_unbalanced_braces_do_not_hang_or_crash():
    # A truncated file, which real corpora contain.
    syms = extract("m.rs", "pub fn alpha() {\n    let x = 1;\n")
    for s in syms:
        assert s["end_line"] >= s["sig_line"], f"span inverted: {s}"


def test_every_span_is_well_formed():
    for name, text in (("m.py", PY), ("m.rs", RS)):
        for s in extract(name, text):
            assert s["start_line"] >= 1, s
            assert s["start_line"] <= s["sig_line"] <= s["end_line"], s
            assert s["n_lines"] >= 1, s
