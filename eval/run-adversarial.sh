#!/usr/bin/env bash
# Adversarial pass on our own eval: how much of the reported semgrep-vs-rg
# gap is the engine, and how much is a weak ripgrep baseline?
#
# The legacy `rg` condition tokenizes with [a-zA-Z0-9]+ — no underscore — so
# `blkg_rwstat_add` is shredded before ripgrep sees it, and it then picks the
# two LONGEST words as a proxy for "rarest", which on a typical direct query
# means "function"/"choosing" rather than the identifier. `rg-strong` tries
# identifier-shaped tokens first, which is what a competent agent does.
#
# Scoring both is the point. If the gap collapses, our headline claim was
# measuring our own baseline.
set -uo pipefail
cd "$(dirname "$0")/.."
export SEMGREP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-adversarial-cache"

for corpus in vscode linux wikipedia; do
  d=bench/corpora/$corpus; q=eval/queries/$corpus.jsonl
  [ -d "$d" ] && [ -f "$q" ] || { echo "skip $corpus"; continue; }
  echo "===== $corpus ====="
  python3 eval/run_eval.py "$q" "$d" \
      --modes hybrid,rg,rg-strong \
      --compare-modes hybrid,rg-strong \
      --out eval/data/adv-$corpus.json
done
echo "adversarial pass complete"
