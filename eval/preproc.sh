#!/usr/bin/env bash
# Score the §14 embed-preprocessing lever: prose-rendered chunk/query text vs
# the raw doc_text baseline, semantic mode, across the pre-registered corpora
# (RESEARCH.md §14.3 — predictions committed before this ran).
#
#   eval/preproc.sh                every corpus, every condition
#   eval/preproc.sh tokio etcd     only those corpora
#
# Results land in eval/results/lever-<corpus>-preproc-<tag>.json — the lever
# campaign's naming, so the existing comparator reads them unchanged:
#   eval/diff.py --base preproc-none --cand preproc-split preproc-split-nokw
#
# bm25 is scored under `none` (the bar) and again under `split` (a tripwire:
# its pipeline does not touch the lever, so the two must agree exactly).
set -euo pipefail
cd "$(dirname "$0")/.."

SG=target/release/gorp
CORPORA="${*:-tokio etcd vscode cosqa linux}"

# Isolated, like levers.sh: the harness must never poison ordinary use.
export GORP_CACHE_DIR="${TMPDIR:-/tmp}/gorp-preproc-cache"

# tag|index flags|modes
CONDITIONS=$(cat <<'EOF'
none||semantic,bm25
split|--embed-preproc split|semantic,bm25
split-whole|--embed-preproc split-whole|semantic
split-nokw|--embed-preproc split-nokw|semantic
split-sif|--embed-preproc split --sif|semantic
EOF
)

corpus_dir() {
  case "$1" in
    cosqa) echo eval/data/cosqa/corpus ;;
    *) echo bench/corpora/"$1" ;;
  esac
}

queries_for() {
  case "$1" in
    cosqa) echo eval/queries/cosqa-1200.jsonl ;;
    *) echo eval/queries/"$1".jsonl ;;
  esac
}

for corpus in $CORPORA; do
  dir=$(corpus_dir "$corpus")
  queries=$(queries_for "$corpus")
  [ -d "$dir" ] && [ -f "$queries" ] || { echo "skip $corpus (no corpus or queries)"; continue; }

  while IFS='|' read -r tag index_flags modes; do
    [ -z "$tag" ] && continue
    out=eval/results/lever-$corpus-preproc-$tag.json
    if [ -f "$out" ]; then
      echo "== $corpus/$tag (skip, exists)"
      continue
    fi
    echo "== $corpus: index ${index_flags:-(default)}"
    # shellcheck disable=SC2086
    $SG index "$dir" $index_flags 2>/dev/null
    echo "== $corpus/$tag"
    python3 eval/run_eval.py "$queries" "$dir" --modes "$modes" --out "$out"
  done <<< "$CONDITIONS"

  # Never leave a corpus on an experimental index.
  echo "== $corpus: restore default index"
  $SG index "$dir" 2>/dev/null
done
echo "preproc campaign complete"
