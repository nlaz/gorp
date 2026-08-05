#!/usr/bin/env bash
# Score the §20 pruning ladder and the character budget — predictions in
# RESEARCH.md §20.3, committed before this ran.
#
#   eval/prune.sh                  every corpus, every arm
#   eval/prune.sh vscode tokio     only those corpora
#
# Results land in eval/results/lever-<corpus>-prune-<tag>.json — the lever
# campaign's naming, so the existing comparator reads them unchanged:
#   python3 eval/diff.py --base prune-kw --cand prune-lex prune-decl prune-soft
#
# Three groups of arms, one prediction each:
#   tiers        the ladder itself (§20.3 predictions 1-4)
#   path-*       PathRender at the two tiers where it can matter (prediction 5)
#   *-sif        the tiers crossed with SIF (prediction 6)
#   budget       chars-800 against lines-32 at a fixed rendering (prediction 7)
#
# `split-nokw` is scored first as the incumbent: every tier delta is read
# against it, and it is the condition §14.4 published.
#
# Output tags carry the query-side policy: `qsym` is the shipped one, where a
# tier's pruning also applies to the query where a query can mirror it (§20.6).
# The `prune-qasym-*` files on disk are the run-2 experiment that removed that
# mirroring and lost on every corpus; they are kept for the comparison.
#
# `lexsym`/`uniqsym` are the §20.7 arms: the same documents as `lex`/`uniq`,
# with the low-signal table (and dedupe) mirrored onto the query. They exist to
# tell "the stoplist removes signal" apart from "the stoplist was asymmetric".
set -euo pipefail
cd "$(dirname "$0")/.."

SG=target/release/semgrep
CORPORA="${*:-tokio etcd vscode cosqa linux}"

# Isolated, like levers.sh and preproc.sh: the harness must never poison
# ordinary use. The prune variants are not part of the cache entry key.
export SEMGREP_CACHE_DIR="${TMPDIR:-/tmp}/semgrep-prune-cache"

# tag|index flags|queryset|modes|where
CONDITIONS=$(cat <<'EOF'
nokw|--embed-preproc split-nokw||semantic,bm25|
kw|--embed-preproc prune-kw||semantic,bm25|
lex|--embed-preproc prune-lex||semantic|
decl|--embed-preproc prune-decl||semantic|
soft|--embed-preproc prune-soft||semantic|
uniq|--embed-preproc prune-uniq||semantic|
lex-pdedupe|--embed-preproc prune-lex --chunk-path dedupe||semantic|
lex-ptail|--embed-preproc prune-lex --chunk-path tail||semantic|
lex-pscaled|--embed-preproc prune-lex --chunk-path scaled||semantic|
decl-pdedupe|--embed-preproc prune-decl --chunk-path dedupe||semantic|
decl-ptail|--embed-preproc prune-decl --chunk-path tail||semantic|
decl-pscaled|--embed-preproc prune-decl --chunk-path scaled||semantic|
kw-sif|--embed-preproc prune-kw --sif||semantic|
lex-sif|--embed-preproc prune-lex --sif||semantic|
lex-blind|--embed-preproc prune-lex|-blind|semantic|kind=blind
decl-blind|--embed-preproc prune-decl|-blind|semantic|kind=blind
lexsym|--embed-preproc prune-lex-sym||semantic|
uniqsym|--embed-preproc prune-uniq-sym||semantic|
lexsym-sif|--embed-preproc prune-lex-sym --sif||semantic|
EOF
)

# The budget arm is a *chunking* change, so it is scored only where chunking
# can differ. cosqa is one short Python function per file: a 32-line window and
# an 800-character budget cut it into the same single chunk, and a null there
# would be reporting the corpus rather than the lever (§20.3).
# The sweep (§20.9). 800 is parity with today's median 32-line window; 1600 and
# 2400 walk toward the ~2,000 non-whitespace characters the external controlled
# study found optimal (arXiv 2605.04763). Rendering is held at `prune-kw` across
# all three so the only thing moving is chunk size.
BUDGET_CONDITIONS=$(cat <<'EOF'
budget800|--embed-preproc prune-kw --chunk-budget 800||semantic|
budget1600|--embed-preproc prune-kw --chunk-budget 1600||semantic|
budget2400|--embed-preproc prune-kw --chunk-budget 2400||semantic|
EOF
)
BUDGET_CORPORA="tokio etcd vscode linux"

corpus_dir() {
  case "$1" in
    cosqa) echo eval/data/cosqa/corpus ;;
    *) echo bench/corpora/"$1" ;;
  esac
}

queries_for() {
  # $1 corpus, $2 queryset suffix ("" or "-blind")
  #
  # cosqa has exactly one query set and every row is `kind=real` — there is no
  # blind cut of it. Name a path that does not exist so the condition takes the
  # "no such query set" skip, rather than reaching run_eval and having
  # `--where kind=blind` match zero rows, which is a nonzero exit and under
  # `set -e` takes the rest of the campaign (including linux) with it.
  case "$1" in
    cosqa) [ -z "$2" ] && echo eval/queries/cosqa-1200.jsonl || echo /nonexistent ;;
    *) echo eval/queries/"$1$2".jsonl ;;
  esac
}

run_condition() {
  local corpus="$1" dir="$2" tag index_flags qsuffix modes where
  IFS='|' read -r tag index_flags qsuffix modes where <<< "$3"
  [ -z "$tag" ] && return 0

  local queries out
  queries=$(queries_for "$corpus" "$qsuffix")
  out=eval/results/lever-$corpus-prune-qsym-$tag.json
  if [ ! -f "$queries" ]; then
    echo "== $corpus/$tag (skip, no $queries)"
    return 0
  fi
  if [ -f "$out" ]; then
    echo "== $corpus/$tag (skip, exists)"
    return 0
  fi

  echo "== $corpus: index $index_flags"
  # shellcheck disable=SC2086
  $SG index "$dir" $index_flags 2>/dev/null
  echo "== $corpus/$tag"
  # A budgeted index must be *searched* with the same budget: `cache::discover`
  # keys entries by chunk params, so without it the query misses its own index
  # and silently streams cold — scoring a different chunking than the one the
  # arm is about. Only the chunking flags carry over; the rendering comes back
  # out of meta.json by design.
  local search_flags=""
  case "$index_flags" in
    *"--chunk-budget"*)
      search_flags="--chunk-budget ${index_flags##*--chunk-budget }"
      search_flags="--chunk-budget ${search_flags##*--chunk-budget }"
      ;;
  esac
  python3 eval/run_eval.py "$queries" "$dir" --modes "$modes" \
    ${where:+--where "$where"} \
    ${search_flags:+--extra "$search_flags"} \
    --out "$out"
}

for corpus in $CORPORA; do
  dir=$(corpus_dir "$corpus")
  if [ ! -d "$dir" ]; then
    echo "skip $corpus (no corpus at $dir)"
    continue
  fi

  if [ "${BUDGET_ONLY:-0}" != "1" ]; then
    while IFS= read -r line; do
      [ -z "$line" ] && continue
      run_condition "$corpus" "$dir" "$line"
    done <<< "$CONDITIONS"
  fi

  case " $BUDGET_CORPORA " in
    *" $corpus "*)
      while IFS= read -r line; do
        [ -z "$line" ] && continue
        run_condition "$corpus" "$dir" "$line"
      done <<< "$BUDGET_CONDITIONS"
      ;;
    *) echo "== $corpus: budget arm not applicable (§20.3)" ;;
  esac

  # Never leave a corpus on an experimental index.
  echo "== $corpus: restore default index"
  $SG index "$dir" 2>/dev/null
done
echo "prune campaign complete"
