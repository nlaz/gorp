#!/usr/bin/env bash
# The first blind-search campaign (RESEARCH.md §15, predictions in §15.5,
# committed before this ran). Scores the generated <corpus>-blind.jsonl sets:
# blind and blind_long cells under the baseline engine set, the §14 champion
# index, and the rg-oracle ceiling — plus the same sets' direct cells as the
# in-set L0 regression anchor over identical golds.
#
#   eval/blind.sh                 every corpus
#   eval/blind.sh tokio etcd      only those
#
# Results: eval/results/lever-<corpus>-blind-<tag>.json (diff.py-compatible).
set -euo pipefail
cd "$(dirname "$0")/.."

SG=target/release/gorp
CORPORA="${*:-tokio etcd commons-lang jekyll vscode linux}"

export GORP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-blind-cache"

# tag|index flags|modes|where|compare
CONDITIONS=$(cat <<'EOF'
base||bm25,semantic,hybrid,rg-strong|kind=blind|semantic,bm25
base-long||bm25,semantic|kind=blind_long|semantic,bm25
base-direct||bm25,semantic|kind=direct|semantic,bm25
champion|--embed-preproc split --sif|semantic|kind=blind|
champion-long|--embed-preproc split --sif|semantic|kind=blind_long|
champion-direct|--embed-preproc split --sif|semantic|kind=direct|
oracle||rg-oracle|kind=blind|
EOF
)

for corpus in $CORPORA; do
  dir=bench/corpora/$corpus
  queries=eval/queries/$corpus-blind.jsonl
  [ -d "$dir" ] && [ -f "$queries" ] || { echo "skip $corpus (no corpus or blind set)"; continue; }

  last_index=unset
  while IFS='|' read -r tag index_flags modes where compare; do
    [ -z "$tag" ] && continue
    out=eval/results/lever-$corpus-blind-$tag.json
    if [ -f "$out" ]; then
      echo "== $corpus/$tag (skip, exists)"
      continue
    fi
    if [ "$index_flags" != "$last_index" ]; then
      echo "== $corpus: index ${index_flags:-(default)}"
      # shellcheck disable=SC2086
      $SG index "$dir" $index_flags 2>/dev/null
      last_index=$index_flags
    fi
    echo "== $corpus/$tag"
    python3 eval/run_eval.py "$queries" "$dir" --modes "$modes" --where "$where" \
      ${compare:+--compare-modes "$compare"} \
      --checkpoint "eval/data/ckpt-blind-$corpus-$tag.jsonl" --out "$out" \
      || exit 1
  done <<< "$CONDITIONS"

  if [ "$last_index" != unset ] && [ "$last_index" != "" ]; then
    echo "== $corpus: restore default index"
    $SG index "$dir" 2>/dev/null
  fi
done
echo "blind campaign complete"
