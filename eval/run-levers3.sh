#!/usr/bin/env bash
# Tune the §9.5 knobs: MaxSim head size (mp48/mp96), MaxSim blend (bl75/bl50),
# SIF smoothing a (sifa2=1e-2, sifa4=1e-4), SIF centering (sifc).
# References: lever-*-maxsim2.json (pool auto/blend 1.0/a 1e-3, no centering)
# and lever-*-sif-maxsim2.json from run-levers2.sh.
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

  echo "== $corpus: normal index (maxsim pool/blend sweeps) =="
  $SG index "$dir" 2>/dev/null
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra='--maxsim --maxsim-pool 48' --out eval/data/lever-$corpus-mp48.json
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra='--maxsim --maxsim-pool 96' --out eval/data/lever-$corpus-mp96.json
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra='--maxsim --maxsim-blend 0.75' --out eval/data/lever-$corpus-bl75.json
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra='--maxsim --maxsim-blend 0.5' --out eval/data/lever-$corpus-bl50.json

  echo "== $corpus: sif a=1e-2 =="
  $SG index "$dir" --sif --sif-a 0.01 2>/dev/null
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra=--maxsim --out eval/data/lever-$corpus-sifa2.json
  echo "== $corpus: sif a=1e-4 =="
  $SG index "$dir" --sif --sif-a 0.0001 2>/dev/null
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra=--maxsim --out eval/data/lever-$corpus-sifa4.json
  echo "== $corpus: sif centered (a=1e-3) =="
  $SG index "$dir" --sif --sif-center 2>/dev/null
  run_if_missing python3 eval/run_eval.py "$queries" "$dir" --modes semantic,hybrid \
    --extra=--maxsim --out eval/data/lever-$corpus-sifc.json

  echo "== $corpus: restore normal index =="
  $SG index "$dir" 2>/dev/null
done
echo "tuning sweep complete"
