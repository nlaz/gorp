#!/usr/bin/env bash
# Score the §9.4 re-wire: MaxSim moved PRE-fusion (semantic list reranked
# before RRF). Conditions: maxsim2 (normal index), sif-maxsim2 (--sif index).
# Baselines from run-levers.sh remain valid (flag-off paths untouched).
set -euo pipefail
cd "$(dirname "$0")/.."
SG=target/release/semgrep
run_if_missing() {
  out=$(printf '%s\n' "$@" | awk '/--out/{getline; print}')
  [ -n "$out" ] && [ -f "$out" ] && { echo "   (skip, exists: $out)"; return 0; }
  "$@"
}
export SEMGREP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-lever-cache"

for corpus in vscode wikipedia linux; do
  dir=bench/corpora/$corpus
  queries=eval/data/$corpus.jsonl
  [ -d "$dir" ] && [ -f "$queries" ] || { echo "skip $corpus"; continue; }

  echo "== $corpus: maxsim2 (pre-fusion, normal index) =="
  $SG index "$dir" 2>/dev/null
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra=--maxsim --out eval/data/lever-$corpus-maxsim2.json

  echo "== $corpus: sif-maxsim2 =="
  $SG index "$dir" --sif 2>/dev/null
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra=--maxsim --out eval/data/lever-$corpus-sif-maxsim2.json

  echo "== $corpus: restore normal index =="
  $SG index "$dir" 2>/dev/null
done
echo "lever v2 campaign complete"
