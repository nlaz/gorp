#!/usr/bin/env bash
# Score the §9 retrieval levers — PRF, SIF, MaxSim, and their knobs — against the
# ground-truth query sets. Results land in eval/data/lever-<corpus>-<tag>.json and
# are compared with eval/diff.py.
#
# Replaces run-levers.sh, run-levers2.sh, run-levers3.sh and run-pool-sweep.sh,
# which were four near-copies of this loop with a condition list edited in place
# and the same run_if_missing helper pasted into each.
#
#   eval/levers.sh                 every condition
#   eval/levers.sh base prf        only those
#   eval/levers.sh --list          what is available
#
# Conditions are (tag, index flags, modes, search flags). Index flags decide the
# build, so conditions are grouped by them and the index is rebuilt once per
# group rather than once per condition.
set -euo pipefail
cd "$(dirname "$0")/.."

SG=target/release/semgrep
CORPORA="${CORPORA:-vscode wikipedia linux}"

# A cache of its own. The harness sweeps chunk parameters, and until the entry
# key included them (see FIXES.md #10) that contaminated whatever was measured
# next — including, if this pointed at the default location, ordinary use.
export SEMGREP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-lever-cache"

# tag|index flags|modes|search flags
CONDITIONS=$(cat <<'EOF'
base||bm25,semantic,hybrid|
prf||bm25,hybrid|--prf 8
maxsim||semantic,hybrid|--maxsim
mp32||semantic,hybrid|--maxsim --maxsim-pool 32
mp48||semantic,hybrid|--maxsim --maxsim-pool 48
mp64||semantic,hybrid|--maxsim --maxsim-pool 64
mp96||semantic,hybrid|--maxsim --maxsim-pool 96
mp128||semantic,hybrid|--maxsim --maxsim-pool 128
bl50||semantic,hybrid|--maxsim --maxsim-blend 0.50
bl75||semantic,hybrid|--maxsim --maxsim-blend 0.75
sif|--sif|semantic,hybrid|
sif-maxsim|--sif|semantic,hybrid|--maxsim
sifa2|--sif --sif-a 0.01|semantic,hybrid|--maxsim
sifa4|--sif --sif-a 0.0001|semantic,hybrid|--maxsim
sifc|--sif --sif-center|semantic,hybrid|--maxsim
EOF
)

if [ "${1:-}" = --list ]; then
  printf '%s\n' "$CONDITIONS" | cut -d'|' -f1
  exit 0
fi

wanted() {
  [ $# -eq 0 ] && return 0
  local tag=$1; shift
  for want in "$@"; do [ "$tag" = "$want" ] && return 0; done
  return 1
}

for corpus in $CORPORA; do
  dir=bench/corpora/$corpus
  queries=eval/data/$corpus.jsonl
  [ -d "$dir" ] && [ -f "$queries" ] || { echo "skip $corpus (no corpus or queries)"; continue; }

  # Group by index flags so a corpus is reindexed once per distinct build, not
  # once per condition. Indexing the linux corpus takes ~45 s.
  last_index=unset
  while IFS='|' read -r tag index_flags modes search_flags; do
    [ -z "$tag" ] && continue
    wanted "$tag" "$@" || continue
    out=eval/data/lever-$corpus-$tag.json
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
    if [ -n "$search_flags" ]; then
      python3 eval/run_eval.py "$queries" "$dir" --modes "$modes" \
        --extra="$search_flags" --out "$out"
    else
      python3 eval/run_eval.py "$queries" "$dir" --modes "$modes" --out "$out"
    fi
  done <<< "$CONDITIONS"

  # Leave the corpus on a default index, so a later run does not silently
  # measure against whatever the last condition happened to build.
  if [ "$last_index" != unset ] && [ "$last_index" != "" ]; then
    echo "== $corpus: restore default index"
    $SG index "$dir" 2>/dev/null
  fi
done
echo "lever campaign complete"
