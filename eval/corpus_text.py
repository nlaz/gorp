#!/usr/bin/env python3
"""Read a corpus file the way gorp does.

Ground truth is a line span. A line span only means something if the harness
and the engine agree on what a line is and on which files exist — and they did
not. Two disagreements, both found by the gold-span validator on real corpora:

**1. What counts as a line break.** Python's `str.splitlines()` breaks on
`\\x0b \\x0c \\x1c \\x1d \\x1e \\x85 \\u2028 \\u2029` as well as `\\n`. Rust's
`str::lines()`, which `corpus/chunk.rs:59` uses, breaks on `\\n` alone. So on
any file containing one of those characters the two number the lines
differently from that point on, and a gold span recorded under one convention
addresses different text under the other. Measured share of files where they
disagree:

    jekyll 4.19%   etcd 1.99%   vscode 1.04%   commons-lang 0.84%
    linux  0.27%   tokio 0.00%

Form feeds (`\\x0c`) are the common case — GNU C style uses them as page
separators, which is why the kernel has 232 of them.

**2. Which files exist at all.** `corpus/mod.rs:82` skips any file with a NUL
byte in its first 8192 bytes. A gold span in such a file is unfindable by
construction: every condition misses it, forever, and it reads as a uniform
accuracy loss rather than a bookkeeping error. commons-lang has an OSS-Fuzz
test like this, and two generated queries pointed straight at it.

Both rules are mirrored here rather than approximated. If the engine's rules
change, this module is the one place to change with them.
"""

from pathlib import Path

# corpus/mod.rs:84 — only the first 8 KiB are sniffed, so a NUL later in the
# file does not disqualify it. Matching the window matters: a whole-file scan
# would call files unindexable that gorp indexes perfectly well.
SNIFF_BYTES = 8192


def is_indexable(path):
    """True if gorp's walker would read this file as text."""
    try:
        with open(path, "rb") as f:
            return 0 not in f.read(SNIFF_BYTES)
    except OSError:
        return False


def read_text(path):
    """The file's text, or None if gorp would skip it.

    Mirrors `corpus::read_text`: NUL in the first 8 KiB means binary, and
    invalid UTF-8 is replaced lossily rather than bailing.
    """
    try:
        data = Path(path).read_bytes()
    except OSError:
        return None
    if 0 in data[:SNIFF_BYTES]:
        return None
    return data.decode("utf-8", errors="replace")


def split_lines(text):
    """Rust `str::lines()` semantics, which is what line numbers count.

    Splits on `\\n` only, strips one trailing `\\r`, and yields no trailing
    empty element for text ending in a newline. Line N is `split_lines(t)[N-1]`.
    """
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [ln[:-1] if ln.endswith("\r") else ln for ln in lines]


def read_lines(path):
    """(lines, ok). `ok` is False when gorp would skip the file — callers
    must distinguish "no lines" from "this file is not in the index"."""
    text = read_text(path)
    if text is None:
        return [], False
    return split_lines(text), True


def span(lines, start_line, end_line):
    """The text of a 1-based inclusive line span."""
    return "\n".join(lines[start_line - 1:end_line])
