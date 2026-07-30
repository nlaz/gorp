#!/usr/bin/env python3
"""Generate retrieval eval queries from a corpus using the claude CLI.

Samples random chunks, asks Claude for (a) a natural-language query a
developer/reader would type to find that chunk and (b) a paraphrase that
avoids the chunk's own identifiers/keywords. Ground truth = the source
chunk's file + line span.

Usage:
  python3 eval/generate.py bench/corpora/linux --n 200 --out eval/queries/linux.jsonl

Requires `claude` on PATH (headless: claude -p). Spot-check the output file
before trusting scores; drop rows where Claude's query is off-target.
"""

import argparse
import hashlib
import sys
import json
import random
import subprocess
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import corpus_text  # noqa: E402
import validate_queries  # noqa: E402

CODE_EXT = {".c", ".h", ".rs", ".ts", ".js", ".py", ".go", ".java", ".cpp", ".md", ".txt"}
WINDOW = 30

# The prompt deliberately does NOT contain the file path.
#
# It used to open "Below is a chunk of a file from a corpus ({path}, lines
# {start}-{end})", and semgrep's tokenizer does path augmentation — so the
# generator was shown the document identifier and the scorer indexes the
# document identifier. Measured on the sets that prompt produced (RESEARCH.md
# §13.1): 32.7% of linux `direct` queries contain the gold file's stem and
# 48.2% a directory segment.
#
# The part that matters is that `paraphrase` leaked path segments at 17.1%,
# slightly MORE than `direct`'s 16.1%, despite being the pole whose whole
# purpose is to withhold the answer's vocabulary. The instruction to avoid the
# chunk's identifiers left the path as the one piece of the answer the model
# could still see, and it reached for it.
#
# Line numbers are gone for the same reason: they are metadata about where the
# answer lives, not about what it says.
PROMPT = """You are generating search-quality eval data. Below is a chunk of text
from a corpus.

---
{chunk}
---

Return STRICT JSON (no markdown fence) with exactly these keys:
{{"direct": "<a natural-language query (5-15 words) a person would type into a code/document search engine to find exactly this chunk>",
  "paraphrase": "<a query for the same content that deliberately avoids the distinctive identifiers, function names, or rare words appearing in the chunk>"}}

Write both queries using ONLY what the chunk itself says. Do not guess at or
refer to the file name, the directory it might live in, or its position in a
project — you have not been shown them, and a query that names them would be
scoring the eval's bookkeeping rather than the search.

The queries must be answerable by this chunk specifically, not by the whole file."""


def eligible(p: Path) -> bool:
    # extension in the allowlist, or extensionless wikiextractor output (wiki_00)
    return p.suffix in CODE_EXT or (p.suffix == "" and p.name.startswith("wiki_"))


def sample_symbols(root: Path, n: int, seed: int, min_lines: int = 4):
    """Sample whole SYMBOLS (functions/classes) instead of line windows.

    Ground truth is then the symbol's span, which is neutral to how the
    engine chunks — the flaw that made the window-sampled sets unable to
    referee a chunking change (RESEARCH.md §11.4). Carries strata along
    (has_doc, size, language) so results can be cut without re-running.
    """
    import symbols as symmod
    files = [
        p for p in root.rglob("*")
        if p.is_file() and p.suffix in symmod.EXT_LANG
        and 0 < p.stat().st_size < 2_000_000 and ".semgrep" not in p.parts
    ]
    rng = random.Random(seed)
    rng.shuffle(files)
    out = []
    for p in files:
        if len(out) >= n:
            break
        # Never sample a file semgrep's walker skips: a gold span in one can
        # never be returned, so the row would be a permanent miss for every
        # condition and read as an accuracy result.
        text = corpus_text.read_text(p)
        if text is None:
            continue
        lines = corpus_text.split_lines(text)
        syms = [s for s in symmod.extract(p, text) if s["n_lines"] >= min_lines]
        if not syms:
            continue
        s = rng.choice(syms)
        body = "\n".join(lines[s["start_line"] - 1:s["end_line"]])
        if sum(len(l.strip()) for l in body.split("\n")) < 120:
            continue
        out.append({
            "file": str(p.relative_to(root)),
            "start_line": s["start_line"],
            "end_line": s["end_line"],
            "chunk": body,
            # strata — recorded up front so analysis never needs a re-run
            "symbol": s["name"],
            "symbol_kind": s["kind"],
            "lang": symmod.EXT_LANG.get(p.suffix),
            "has_doc": s["has_doc"],
            "n_lines": s["n_lines"],
        })
    return out


def sample_chunks(root: Path, n: int, seed: int):
    files = [
        p for p in root.rglob("*")
        if p.is_file() and eligible(p) and 0 < p.stat().st_size < 2_000_000
        and ".semgrep" not in p.parts
    ]
    rng = random.Random(seed)
    rng.shuffle(files)
    out = []
    for p in files:
        if len(out) >= n:
            break
        try:
            lines = p.read_text(errors="replace").splitlines()
        except OSError:
            continue
        if len(lines) < 10:
            continue
        start = rng.randint(0, max(0, len(lines) - WINDOW))
        chunk = lines[start:start + WINDOW]
        if sum(len(l.strip()) for l in chunk) < 200:  # skip near-empty windows
            continue
        out.append({
            "file": str(p.relative_to(root)),
            "start_line": start + 1,
            "end_line": start + len(chunk),
            "chunk": "\n".join(chunk),
        })
    return out


def ask_claude(prompt: str) -> dict | None:
    proc = subprocess.run(
        ["claude", "-p", "--output-format", "text", prompt],
        capture_output=True, text=True, timeout=120,
    )
    text = proc.stdout.strip()
    # tolerate accidental fences
    if text.startswith("```"):
        text = text.strip("`").lstrip("json").strip()
    try:
        d = json.loads(text)
        return d if {"direct", "paraphrase"} <= d.keys() else None
    except json.JSONDecodeError:
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus", type=Path)
    ap.add_argument("--n", type=int, default=200)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--workers", type=int, default=8)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--anchor", choices=("symbol", "window"), default="symbol",
                    help="symbol: truth is a function/class span (chunking-neutral, "
                         "default). window: legacy fixed-line sampling — biased "
                         "toward window chunking, kept only to reproduce old sets.")
    ap.add_argument("--locked-frac", type=float, default=0.3,
                    help="fraction held out as a locked split; report dev while "
                         "iterating, open locked only at decision time")
    args = ap.parse_args()

    args.out.parent.mkdir(parents=True, exist_ok=True)
    chunks = (sample_symbols(args.corpus, args.n, args.seed)
              if args.anchor == "symbol"
              else sample_chunks(args.corpus, args.n, args.seed))
    print(f"sampled {len(chunks)} chunks; querying claude ({args.workers} workers)…")
    from concurrent.futures import ThreadPoolExecutor
    import threading
    lock = threading.Lock()
    written = 0

    def one(c):
        # chunk only — see the note on PROMPT for why path/start/end are not
        # passed, and what happened when they were.
        q = ask_claude(PROMPT.format(chunk=c["chunk"]))
        return (c, q)

    with open(args.out, "w") as f, ThreadPoolExecutor(max_workers=args.workers) as pool:
        for i, (c, q) in enumerate(pool.map(one, chunks)):
            if not q:
                continue
            with lock:
                # Deterministic dev/locked assignment by content hash, so the
                # split survives regeneration and cannot be gamed by reordering.
                split = ("locked"
                         if (int(hashlib.sha256(
                             (c["file"] + str(c["start_line"])).encode()
                         ).hexdigest()[:8], 16) % 1000) / 1000.0 < args.locked_frac
                         else "dev")
                for kind in ("direct", "paraphrase"):
                    row = {
                        "query": q[kind], "kind": kind, "file": c["file"],
                        "start_line": c["start_line"], "end_line": c["end_line"],
                        "split": split,
                        # Content fingerprint of the gold span. This is the only
                        # check that catches an EDITED gold file — the path and
                        # the line count survive an edit, so every other
                        # validation passes while the span silently describes
                        # something else. See eval/validate_queries.py.
                        "gold_sha": validate_queries.span_sha(c["chunk"]),
                    }
                    # strata, when the symbol sampler produced them
                    for k in ("symbol", "symbol_kind", "lang", "has_doc", "n_lines"):
                        if k in c:
                            row[k] = c[k]
                    f.write(json.dumps(row) + "\n")
                f.flush()
                written += 1
            if (i + 1) % 20 == 0:
                print(f"  {i + 1}/{len(chunks)} chunks → {written * 2} queries", flush=True)
    print(f"wrote {written * 2} queries to {args.out}")


if __name__ == "__main__":
    main()
