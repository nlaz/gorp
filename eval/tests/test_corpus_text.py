"""Tests for reading a corpus file the way gorp does.

Ground truth is a line span, so the harness and the engine have to agree on
what a line is and on which files exist. They did not, and both disagreements
were found by the gold-span validator on real corpora rather than by reading
the code:

  - `str.splitlines()` breaks on \\x0b \\x0c \\x1c \\x1d \\x1e \\x85 \\u2028
    \\u2029 as well as \\n; Rust's `str::lines()` (corpus/chunk.rs:59) breaks
    on \\n alone. 0.27%-4.19% of files in the seven corpora contain one.
  - `corpus/mod.rs:82` skips any file with a NUL byte in its first 8 KiB.

These pin both rules against the engine's. If the engine's rules change, these
fail, which is the point.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import corpus_text  # noqa: E402


# --- split_lines: Rust str::lines() semantics --------------------------------

def test_a_trailing_newline_does_not_make_an_extra_empty_line():
    # "a\nb\n".split("\n") is ["a", "b", ""] — the empty string is not a line,
    # and counting it would put every end-of-file span one past the end.
    assert corpus_text.split_lines("a\nb\n") == ["a", "b"]


def test_text_without_a_trailing_newline_keeps_its_last_line():
    assert corpus_text.split_lines("a\nb") == ["a", "b"]


def test_a_carriage_return_is_stripped():
    assert corpus_text.split_lines("a\r\nb\r\n") == ["a", "b"]


def test_the_empty_string_has_no_lines():
    assert corpus_text.split_lines("") == []


def test_a_lone_newline_is_one_empty_line():
    assert corpus_text.split_lines("\n") == [""]


def test_a_form_feed_is_not_a_line_break():
    # The common real case: GNU C style uses \x0c as a page separator, which is
    # why the kernel has 232 files containing one. splitlines() would return
    # three lines here and shift every line number after it by one.
    assert corpus_text.split_lines("a\x0cb\nc") == ["a\x0cb", "c"]
    assert len("a\x0cb\nc".splitlines()) == 3      # what we are NOT doing


def test_a_group_separator_is_not_a_line_break():
    # \x1d, found in commons-lang's OSS-Fuzz test — the file that first
    # surfaced this.
    assert corpus_text.split_lines("a\x1db\nc") == ["a\x1db", "c"]


def test_unicode_line_separators_are_not_line_breaks():
    for ch in (" ", " ", "\x85", "\x0b", "\x1c", "\x1e"):
        assert corpus_text.split_lines(f"a{ch}b") == [f"a{ch}b"], ch


# --- span --------------------------------------------------------------------

def test_a_span_is_one_based_and_inclusive():
    lines = corpus_text.split_lines("one\ntwo\nthree\nfour\n")
    assert corpus_text.span(lines, 2, 3) == "two\nthree"


def test_a_single_line_span():
    lines = corpus_text.split_lines("one\ntwo\n")
    assert corpus_text.span(lines, 1, 1) == "one"


def test_a_span_covering_the_whole_file():
    lines = corpus_text.split_lines("one\ntwo\n")
    assert corpus_text.span(lines, 1, 2) == "one\ntwo"


# --- is_indexable / read_text: corpus::read_text semantics -------------------

def test_a_plain_text_file_is_indexable(tmp_path):
    p = tmp_path / "a.py"
    p.write_text("def f():\n    pass\n")
    assert corpus_text.is_indexable(p)
    assert corpus_text.read_text(p) == "def f():\n    pass\n"


def test_a_nul_byte_in_the_first_8k_makes_a_file_unindexable(tmp_path):
    p = tmp_path / "a.java"
    p.write_bytes(b"class A {\x00}\n")
    assert not corpus_text.is_indexable(p)
    assert corpus_text.read_text(p) is None


def test_a_nul_byte_past_the_sniff_window_does_not(tmp_path):
    # corpus/mod.rs sniffs only the first 8192 bytes. Scanning the whole file
    # would call files unindexable that gorp indexes perfectly well.
    p = tmp_path / "a.java"
    p.write_bytes(b"x" * (corpus_text.SNIFF_BYTES + 10) + b"\x00")
    assert corpus_text.is_indexable(p)
    assert corpus_text.read_text(p) is not None


def test_a_nul_at_the_last_byte_of_the_window_still_counts(tmp_path):
    p = tmp_path / "a.java"
    p.write_bytes(b"x" * (corpus_text.SNIFF_BYTES - 1) + b"\x00")
    assert not corpus_text.is_indexable(p)


def test_invalid_utf8_is_replaced_rather_than_failing(tmp_path):
    # "never bail on mixed-encoding trees" — corpus/mod.rs
    p = tmp_path / "a.c"
    p.write_bytes(b"caf\xe9\n")
    assert corpus_text.read_text(p) is not None


def test_a_missing_file_is_not_indexable(tmp_path):
    assert not corpus_text.is_indexable(tmp_path / "gone.py")
    assert corpus_text.read_text(tmp_path / "gone.py") is None


# --- read_lines: the ok flag ------------------------------------------------

def test_read_lines_reports_unindexable_separately_from_empty(tmp_path):
    # The distinction that matters: an empty file has no lines and IS indexed;
    # a binary file has no lines and is NOT. Collapsing them turns "this row
    # can never be found" into "this row scored zero", which reads as an
    # accuracy result.
    empty = tmp_path / "empty.py"
    empty.write_text("")
    binary = tmp_path / "bin.py"
    binary.write_bytes(b"\x00")

    assert corpus_text.read_lines(empty) == ([], True)
    assert corpus_text.read_lines(binary) == ([], False)
