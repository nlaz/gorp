#!/usr/bin/env python3
"""Generate retrieval eval queries from a corpus using the claude CLI.

Samples random chunks, asks Claude for (a) a natural-language query a
developer/reader would type to find that chunk and (b) a paraphrase that
avoids the chunk's own identifiers/keywords. Ground truth = the source
chunk's file + line span.

Usage:
  python3 eval/generate.py bench/corpora/linux --n 200 --out eval/data/linux.jsonl

Requires `claude` on PATH (headless: claude -p). Spot-check the output file
before trusting scores; drop rows where Claude's query is off-target.
"""

import argparse
import json
import random
import subprocess
from pathlib import Path

CODE_EXT = {".c", ".h", ".rs", ".ts", ".js", ".py", ".go", ".java", ".cpp", ".md", ".txt"}
WINDOW = 30

PROMPT = """You are generating search-quality eval data. Below is a chunk of a file
from a corpus ({path}, lines {start}-{end}).

---
{chunk}
---

Return STRICT JSON (no markdown fence) with exactly these keys:
{{"direct": "<a natural-language query (5-15 words) a person would type into a code/document search engine to find exactly this chunk>",
  "paraphrase": "<a query for the same content that deliberately avoids the distinctive identifiers, function names, or rare words appearing in the chunk>"}}

The queries must be answerable by this chunk specifically, not by the whole file."""


def eligible(p: Path) -> bool:
    # extension in the allowlist, or extensionless wikiextractor output (wiki_00)
    return p.suffix in CODE_EXT or (p.suffix == "" and p.name.startswith("wiki_"))


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
    args = ap.parse_args()

    args.out.parent.mkdir(parents=True, exist_ok=True)
    chunks = sample_chunks(args.corpus, args.n, args.seed)
    print(f"sampled {len(chunks)} chunks; querying claude ({args.workers} workers)…")
    from concurrent.futures import ThreadPoolExecutor
    import threading
    lock = threading.Lock()
    written = 0

    def one(c):
        q = ask_claude(PROMPT.format(
            path=c["file"], start=c["start_line"], end=c["end_line"], chunk=c["chunk"]))
        return (c, q)

    with open(args.out, "w") as f, ThreadPoolExecutor(max_workers=args.workers) as pool:
        for i, (c, q) in enumerate(pool.map(one, chunks)):
            if not q:
                continue
            with lock:
                for kind in ("direct", "paraphrase"):
                    f.write(json.dumps({
                        "query": q[kind], "kind": kind, "file": c["file"],
                        "start_line": c["start_line"], "end_line": c["end_line"],
                    }) + "\n")
                f.flush()
                written += 1
            if (i + 1) % 20 == 0:
                print(f"  {i + 1}/{len(chunks)} chunks → {written * 2} queries", flush=True)
    print(f"wrote {written * 2} queries to {args.out}")


if __name__ == "__main__":
    main()
