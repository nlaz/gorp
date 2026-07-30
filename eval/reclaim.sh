#!/usr/bin/env bash
# What the eval harness is holding on disk, what it costs, and what is safe
# to delete. AUDIT.md: "eval/data/ is 1.3 GB... There is no documented way to
# reclaim it." This is that way.
#
# The rule, and the only one that matters:
#
#   Anything a checked-in script can rebuild is reclaimable.
#   Anything that cost money or nondeterministic model calls is NOT.
#
# So corpora, indices and HuggingFace caches go; agent-run transcripts and the
# query sets stay. `eval/data/locbench/runs/` is $39.07 of measured agent spend
# (RESEARCH.md §11.6) and every result in §7 and §11 is derived from it. It is
# not deletable here at any verbosity.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

DRY=1
TARGETS=()
KEEP_TOP=20
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    --force)   DRY=0 ;;
    --keep-top=*) KEEP_TOP="${a#*=}" ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      echo
      echo "usage: eval/reclaim.sh [--dry-run|--force] [group...]"
      echo "groups: caches corpora mirrors all"
      exit 0 ;;
    *) TARGETS+=("$a") ;;
  esac
done
[ ${#TARGETS[@]} -eq 0 ] && TARGETS=(all)

want() {
  for t in "${TARGETS[@]}"; do
    [ "$t" = all ] && return 0
    [ "$t" = "$1" ] && return 0
  done
  return 1
}

size() { [ -e "$1" ] && du -sh "$1" 2>/dev/null | cut -f1 || echo "-"; }

# offer <group> <path> <rebuild-command> <rebuild-cost>
offer() {
  local group=$1 path=$2 rebuild=$3 cost=$4
  want "$group" || return 0
  [ -e "$path" ] || return 0
  printf '  %-42s %6s   rebuild: %s (%s)\n' \
      "${path#"$ROOT"/}" "$(size "$path")" "$rebuild" "$cost"
  if [ "$DRY" = 0 ]; then
    rm -rf -- "$path"
    echo "      deleted"
  fi
}

echo "=== reclaimable ==="
printf '  %-42s %6s   %s\n' "path" "size" "how to get it back"

# NOT project-local: other work on this machine may depend on it. Offered
# because it is usually the largest single thing the eval fetchers leave
# behind, but named explicitly so nobody deletes it by reflex.
offer caches  "$HOME/.cache/huggingface"        "eval/fetch-*.sh"          "SHARED, not just ours — re-download"
offer caches  "$HOME/.cache/semgrep"            "any cold ranked search"   "one pass per scope"
for d in bench/corpora/*/.semgrep eval/data/*/.semgrep; do
  offer caches "$ROOT/$d" "semgrep index <corpus>" "3.5-66 s"
done

offer corpora "$ROOT/bench/corpora/linux"      "bench/fetch-corpora.sh"   "1.5 GB download"
offer corpora "$ROOT/bench/corpora/vscode"     "bench/fetch-corpora.sh"   "pinned clone"
offer corpora "$ROOT/bench/corpora/wikipedia"  "bench/fetch-corpora.sh"   "UNPINNED — a refetch is a DIFFERENT corpus"
for d in tokio okhttp etcd jekyll; do
  offer corpora "$ROOT/bench/corpora/$d" "bench/fetch-corpora.sh" "pinned clone, <40 MB"
done
offer corpora "$ROOT/eval/data/cosqa"          "eval/fetch-cosqa.sh"      "re-download"

# Mirrors are rebuildable but slow, and they are the corpus for the Loc-Bench
# work — evict the largest rather than all of them.
if want mirrors && [ -d eval/data/locbench/repos/mirrors ]; then
  echo
  echo "  locbench mirrors: keeping the $KEEP_TOP smallest, evicting the largest"
  # Smallest first, then skip the ones we keep — so what remains to evict is
  # the LARGEST, which is the whole point of a byte budget. (home-assistant
  # alone is 325 MB; the median mirror is ~14 MB.)
  # shellcheck disable=SC2012
  ls -d eval/data/locbench/repos/mirrors/*.git 2>/dev/null \
    | while read -r m; do echo "$(du -sk "$m" | cut -f1) $m"; done \
    | sort -n | tail -n +"$((KEEP_TOP + 1))" \
    | while read -r kb m; do
        printf '  %-42s %5sM   rebuild: eval/locbench/run.py (re-clone)\n' \
            "${m#"$ROOT"/}" "$((kb / 1024))"
        [ "$DRY" = 0 ] && rm -rf -- "$m" && echo "      deleted"
      done
fi

echo
echo "=== NEVER reclaimed here ==="
printf '  %-42s %6s   %s\n' "eval/data/locbench/runs" \
    "$(size eval/data/locbench/runs)" "\$39.07 of agent spend; §7/§11 derive from it"
printf '  %-42s %6s   %s\n' "eval/data/locbench/results*.jsonl" "-" \
    "the scored outputs of those runs"
printf '  %-42s %6s   %s\n' "eval/queries" "$(size eval/queries)" \
    "claude-generated; cannot be regenerated identically"
printf '  %-42s %6s   %s\n' "eval/data/*.json" "-" \
    "result files; small, and baselines compare against them"

echo
if [ "$DRY" = 1 ]; then
  echo "dry run — nothing deleted. Re-run with --force to actually delete."
else
  echo "done. Free space now:"
  df -h . | tail -1
fi
