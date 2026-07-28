#!/usr/bin/env bash
# Score the §9 retrieval levers (PRF, SIF, MaxSim) against the LLM-generated
# ground-truth query sets. Rebuilds each corpus index normal → scores base /
# prf / maxsim → rebuilds --sif → scores sif / sif+maxsim → restores normal.
# Results: eval/data/lever-<corpus>-<condition>.json
set -euo pipefail
cd "$(dirname "$0")/.."
SG=target/release/semgrep
run_if_missing() {
  # first arg after the command list is --out <file>; grep it out
  out=$(printf '%s\n' "$@" | awk '/--out/{getline; print}')
  [ -n "$out" ] && [ -f "$out" ] && { echo "   (skip, exists: $out)"; return 0; }
  "$@"
}
export SEMGREP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-lever-cache"

for corpus in vscode wikipedia linux; do
  dir=bench/corpora/$corpus
  queries=eval/data/$corpus.jsonl
  [ -d "$dir" ] && [ -f "$queries" ] || { echo "skip $corpus"; continue; }

  echo "== $corpus: rebuild normal index =="
  $SG index "$dir" 2>/dev/null

  echo "== $corpus: base =="
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes bm25,semantic,hybrid \
    --out eval/data/lever-$corpus-base.json
  echo "== $corpus: prf =="
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes bm25,hybrid \
    --extra='--prf 8' --out eval/data/lever-$corpus-prf.json
  echo "== $corpus: maxsim =="
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra=--maxsim --out eval/data/lever-$corpus-maxsim.json

  echo "== $corpus: rebuild --sif =="
  $SG index "$dir" --sif 2>/dev/null
  echo "== $corpus: sif =="
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --out eval/data/lever-$corpus-sif.json
  echo "== $corpus: sif+maxsim =="
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra=--maxsim --out eval/data/lever-$corpus-sif-maxsim.json

  echo "== $corpus: restore normal index =="
  $SG index "$dir" 2>/dev/null
done
echo "lever campaign complete"
