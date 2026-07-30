#!/usr/bin/env python3
"""Symbol extraction for eval ground truth.

Why: `generate.py` used to sample fixed WINDOW-line chunks and call that span
the truth, which makes the eval structurally unable to referee a *chunking*
change — its ground truth is one of the strategies under test (RESEARCH.md
§11.4). Anchoring truth to a symbol (a function or class) is neutral: any
chunking scheme is scored on whether it surfaces the symbol.

Why regex and not an AST: this was first built on py-tree-sitter, which
**segfaulted the interpreter** on real inputs (django's
`related_descriptors.py`, reproducibly, at `node.start_point` and
`node.children`). The same traversal over the same bytes worked outside the
module, so it was a binding lifetime bug rather than anything in the grammar
or the traversal. A hard crash cannot be caught and skipped by a harness, so
that path was removed rather than left as an opt-in trap. If AST-accurate
spans are ever needed, do it out-of-process (a subprocess, or ctags), not by
linking a parser into the harness.

The extractor below is deliberately conservative: it under-counts (multi-line
signatures, macro-heavy C) rather than mis-locating. For ground truth that is
the right failure direction — a missed symbol costs sample size, a wrong span
costs correctness. Audited at 0 control-flow false positives over 979 kernel
symbols.

Usage:
    from symbols import extract
    for sym in extract(Path("foo.py")):
        sym["name"], sym["start_line"], sym["end_line"], sym["has_doc"]
"""

from __future__ import annotations

import re
from pathlib import Path

EXT_LANG = {
    ".py": "python", ".pyi": "python",
    ".js": "javascript", ".jsx": "javascript", ".mjs": "javascript",
    ".ts": "typescript", ".tsx": "typescript",
    ".rs": "rust", ".go": "go",
    ".c": "c", ".h": "c", ".cc": "c", ".cpp": "c", ".hpp": "c",
    ".java": "java", ".rb": "ruby",
}

BRACE_LANGS = {"javascript", "typescript", "rust", "go", "c", "java"}

# Control-flow keywords look exactly like a call site at the start of a line;
# without this, `if (x) {` is sampled as a function named "if".
NOT_NAMES = {"if", "for", "while", "switch", "return", "sizeof", "catch",
             "do", "else", "case", "typedef", "defined", "assert"}

DECL = {
    "python": r"^(\s*)(?:async\s+)?(?:def|class)\s+(\w+)",
    "rust": r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)",
    "go": r"^(\s*)func\s+(?:\([^)]*\)\s*)?(\w+)",
    # `def self.foo` and `def Klass.foo` are singleton methods; the receiver
    # prefix is skipped so the name is `foo`. Without it, 29 of jekyll's 1,060
    # symbols (3%) were named `self` — the span was right, so ground truth was
    # unaffected, but the `symbol` stratum was reporting a keyword as a method.
    "ruby": r"^(\s*)def\s+(?:self\.|[A-Z]\w*\.)?(\w+)",
    "javascript": r"^(\s*)(?:export\s+)?(?:async\s+)?(?:function\s+(\w+)"
                  r"|(?:const|let)\s+(\w+)\s*=\s*(?:async\s*)?\()",
    "typescript": r"^(\s*)(?:export\s+)?(?:async\s+)?(?:function\s+(\w+)"
                  r"|(?:const|let)\s+(\w+)\s*=\s*(?:async\s*)?\()",
    "java": r"^(\s*)(?:(?:public|private|protected|static|final|abstract|"
            r"synchronized)\s+)+[\w<>\[\],.\s]+\s+(\w+)\s*\(",
    # greedy prefix so the LAST identifier before "(" is the function name
    "c": r"^(\s*)(?:[A-Za-z_]\w*[\s\*&]+)+(\w+)\s*\([^;]*\)\s*\{?\s*$",
}
DECL_RE = {k: re.compile(v) for k, v in DECL.items()}

# Shared comment-prefix table — "Rule B" from the §11.2 measurement, which
# pulled in 0% unrelated code across python/c/typescript/rust/ruby/java where
# the simpler walk-to-blank-line rule dragged in the previous method's body
# 36-55% of the time in brace languages.
COMMENTISH = ("///", "//", "#[", "#", "/*", "*/", "*", "@", "--", ";;", '"""', "'''")
MAX_LEAD = 20
DOCSTRING_RE = re.compile(r'^\s*[ru]?("""|\'\'\')')


def _is_commentish(line: str) -> bool:
    t = line.strip()
    return bool(t) and (t.startswith(COMMENTISH) or t.endswith("*/"))


def _lead_start(lines: list[str], sig: int) -> int:
    """Rule B: walk up over comment-ish lines, tolerating one blank gap."""
    start, i, used_gap = sig, sig, False
    while i > 0 and sig - start < MAX_LEAD:
        prev = i - 1
        if not lines[prev].strip():
            if used_gap or prev == 0:
                break
            used_gap, i = True, prev
            continue
        if not _is_commentish(lines[prev]):
            break
        start = i = prev
        used_gap = False
    return start


def _end_brace(lines: list[str], i: int) -> int | None:
    """End of a brace-delimited body, or None if this was a bare declaration."""
    depth, opened, j = 0, False, i
    while j < len(lines) and j - i < 2000:
        depth += lines[j].count("{") - lines[j].count("}")
        opened = opened or "{" in lines[j]
        if opened and depth <= 0:
            return j
        j += 1
    return None


def _end_indent(lines: list[str], i: int, indent: int) -> int:
    j = i + 1
    while j < len(lines):
        ln = lines[j]
        if ln.strip() and (len(ln) - len(ln.lstrip())) <= indent:
            break
        j += 1
    return j - 1


def extract(path: Path, text: str | None = None) -> list[dict]:
    """Symbols in `path` as {name, kind, start_line, sig_line, end_line,
    n_lines, has_doc}. Lines are 1-based inclusive; `start_line` includes any
    leading doc block, `sig_line` is the declaration itself."""
    lang = EXT_LANG.get(path.suffix)
    rx = DECL_RE.get(lang)
    if rx is None:
        return []
    if text is None:
        try:
            text = path.read_text(errors="replace")
        except OSError:
            return []

    lines = text.split("\n")
    brace = lang in BRACE_LANGS
    out = []
    for i, line in enumerate(lines):
        m = rx.match(line)
        if not m:
            continue
        name = next((g for g in m.groups()[1:] if g), None)
        if name in NOT_NAMES:
            continue
        if brace:
            end = _end_brace(lines, i)
            if end is None:          # a prototype/declaration, not a body
                continue
        else:
            end = _end_indent(lines, i, len(m.group(1)))
        lead = _lead_start(lines, i)
        out.append({
            "name": name,
            "kind": f"{lang}-decl",
            "start_line": lead + 1,
            "sig_line": i + 1,
            "end_line": end + 1,
            "n_lines": end - i + 1,
            # A docstring inside the body, or a doc block above it — the
            # stratum where a prose-trained embedder can actually read the
            # chunk (RESEARCH.md §11.2).
            "has_doc": lead < i or any(
                DOCSTRING_RE.match(l) for l in lines[i + 1:i + 3]),
        })
    return out


if __name__ == "__main__":
    import sys
    for f in sys.argv[1:]:
        p = Path(f)
        syms = extract(p)
        print(f"\n{p} -> {len(syms)} symbols")
        for s in syms[:12]:
            print(f"  {str(s['name']):<30} {s['kind']:<16} "
                  f"{s['start_line']:>5}-{s['end_line']:<5} "
                  f"{s['n_lines']:>4}L doc={s['has_doc']}")
