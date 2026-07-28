#!/usr/bin/env bash
# Fine-grained MaxSim pool curve: 32/64/128 (24/48/96 already scored).
set -euo pipefail
cd "$(dirname "$0")/.."
SG=target/release/semgrep
export SEMGREP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-lever-cache"
for corpus in vscode wikipedia linux; do
  dir=bench/corpora/$corpus
  [ -d "$dir" ] || continue
  $SG index "$dir" 2>/dev/null   # ensure normal (non-sif) index
  for p in 32 64 128; do
    out=eval/data/lever-$corpus-mp$p.json
    [ -f "$out" ] && continue
    echo "== $corpus pool $p =="
    python3 eval/run_eval.py eval/data/$corpus.jsonl "$dir" --modes semantic,hybrid \
      --extra="--maxsim --maxsim-pool $p" --out "$out" | tail -1
  done
done
echo "pool sweep complete"
