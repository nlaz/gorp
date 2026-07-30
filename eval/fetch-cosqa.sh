#!/usr/bin/env bash
# CoSQA: real web-search queries (Bing logs) paired with Python functions,
# human-labelled for relevance. BEIR-style: three configs — corpus, queries,
# and qrels (`default`) joining them by id.
#
# Why this matters for us: every query set we have is LLM-generated *from* the
# chunk it should retrieve, which leaks the answer's vocabulary into the
# question — 66-70% of our "direct" code queries contain the gold identifier
# verbatim (RESEARCH.md §12). CoSQA queries were typed by people who had never
# seen the code, so they carry none of that leakage and none of our
# generator's stylistic tells. Compare:
#
#   ours (direct): "blkg_rwstat_add inline function choosing percpu counter..."
#   CoSQA:         "python code to write bool value 1"
#
# The whole corpus is written out, not just the labelled functions, so
# retrieval faces realistic distractors rather than a handful of candidates.
#
# Output: eval/data/cosqa/{corpus/,queries.jsonl} in the shape run_eval.py
# expects — one file per function, ground truth = that whole file.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=eval/data/cosqa

if [ -f "$OUT/queries.jsonl" ]; then
  echo "already fetched: $(wc -l < "$OUT/queries.jsonl") queries, \
$(find "$OUT/corpus" -name '*.py' | wc -l) functions"
  exit 0
fi
mkdir -p "$OUT"

uv run --with datasets python - "$OUT" <<'PY'
import json, sys
from pathlib import Path
from datasets import load_dataset

out = Path(sys.argv[1])
corpus_dir = out / "corpus"
corpus_dir.mkdir(parents=True, exist_ok=True)

corpus = load_dataset("CoIR-Retrieval/cosqa", "corpus", split="corpus")
queries = load_dataset("CoIR-Retrieval/cosqa", "queries", split="queries")
qrels = load_dataset("CoIR-Retrieval/cosqa", split="train")

qtext = {r["_id"]: r["text"] for r in queries}

# Write the FULL corpus: distractors are the point. Retrieval over only the
# labelled functions would be a much easier task than the real one.
names, nlines = {}, {}
for i, r in enumerate(corpus):
    name = f"{r['_id']}.py"
    (corpus_dir / name).write_text(r["text"])
    names[r["_id"]] = name
    nlines[r["_id"]] = r["text"].count("\n") + 1
print(f"wrote {len(names)} functions to {corpus_dir}")

rows, skipped = [], 0
for r in qrels:
    if int(r["score"]) != 1:
        continue
    q, d = r["query-id"], r["corpus-id"]
    if q not in qtext or d not in names:
        skipped += 1
        continue
    rows.append({
        "query": qtext[q].strip(),
        "kind": "real",            # neither "direct" nor "paraphrase": a human wrote it
        "file": names[d],
        "start_line": 1,
        "end_line": nlines[d],
    })

(out / "queries.jsonl").write_text("\n".join(json.dumps(r) for r in rows) + "\n")
print(f"wrote {len(rows)} relevance-labelled real queries "
      f"({skipped} skipped for missing ids)")
print("sample queries:")
for r in rows[:5]:
    print(f"   {r['query'][:70]!r} -> {r['file']}")
PY
