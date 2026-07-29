#!/usr/bin/env bash
# Score the currently-built embedding table against the LLM-generated query
# sets, in exactly the conditions that produced eval/data/lever-<corpus>-base.json
# (modes bm25,semantic,hybrid; no lever flags) so the two are comparable.
#
# The binary must already be built with the table under test — the model is
# selected at build time via ESE_MODEL_URL/ESE_TOKENIZER_URL. Indexes are
# rebuilt because a table swap changes emb.bin's dims and meaning.
#
# Usage: eval/run-model-ab.sh <tag>     # writes eval/data/<tag>-<corpus>.json
set -euo pipefail
cd "$(dirname "$0")/.."
tag="${1:?usage: run-model-ab.sh <tag>}"
SG=target/release/semgrep
export SEMGREP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-$tag-cache"

for corpus in vscode wikipedia linux; do
  dir=bench/corpora/$corpus
  queries=eval/data/$corpus.jsonl
  [ -d "$dir" ] && [ -f "$queries" ] || { echo "skip $corpus"; continue; }
  out=eval/data/$tag-$corpus.json
  [ -f "$out" ] && { echo "== $corpus: skip, exists ($out)"; continue; }

  echo "== $corpus: rebuild index =="
  /usr/bin/time -p $SG index "$dir" 2>&1 | tail -4

  echo "== $corpus: score =="
  python3 eval/run_eval.py "$queries" "$dir" --modes bm25,semantic,hybrid --out "$out"
done
echo "model A/B campaign complete: $tag"
