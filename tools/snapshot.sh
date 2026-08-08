#!/usr/bin/env bash
# Behavior tripwire for the reorganization (PLAN.md Pass 5).
#
#   tools/snapshot.sh          record  tools/snapshot.txt
#   tools/snapshot.sh --check  diff against the recorded file; exit 1 on drift
#
# Ranked output over tests/corpus for a fixed query x mode x path grid. The
# refactor phases are required to leave this byte-identical; the phases that
# deliberately change behavior update it in the same commit, with the diff
# reviewed line by line.
#
# Only hit fields are recorded (path, span, line, text, score) — never timings —
# so the file is a pure function of the corpus, the query, and the engine.
set -euo pipefail
cd "$(dirname "$0")/.."

SG=target/release/semgrep
OUT=tools/snapshot.txt
CORPUS=tests/corpus

[ -x "$SG" ] || { echo "build first: cargo build --release" >&2; exit 2; }

# Ranked queries: identifier lookups, paraphrases with no lexical overlap,
# prose that must reach the control documents, and two guaranteed misses.
RANKED=(
  "how is the retry delay computed"
  "spread retries so workers do not stampede"
  "validate a session token"
  "compare digests without leaking timing"
  "drain urgent jobs before low priority ones"
  "return an unacked job to its lane"
  "parse a configuration file with comments"
  "environment overrides beat the config file"
  "close connections that have gone idle"
  "upper bound of the crossing bucket"
  "route a job to its registered handler"
  "expand a cron expression into allowed values"
  "abort a request once the deadline passes"
  "wrap the index with a mask instead of a division"
  "what happens during a rolling deploy"
  "baking bread with a fermented starter"
  "mirrors gathering light from distant stars"
  "quantum chromodynamics lattice gauge theory"
)

# Exact-mode patterns: literal, regex, anchored, and a miss.
EXACT=(
  "compute_backoff_delay"
  "fn \\w+_token"
  "^pub fn"
  "def _[a-z_]+\\("
  "memory_order_[a-z]+"
  "zzz_no_such_symbol"
)

record() {
  local cache
  cache=$(mktemp -d)
  trap 'rm -rf "$cache"' RETURN
  export SEMGREP_CACHE_DIR="$cache"
  export SEMGREP_CACHE_TTL_SECS=0

  for mode in bm25 semantic hybrid; do
    for path in cold warm; do
      # `--passage-lines 1` pins the DISPLAY so this stays a *ranking*
      # tripwire. Since §26 the default is an 18-line passage, and recording
      # that would (a) bloat every case 18x and (b) make the file move whenever
      # the fixture's text changes, even with ranking identical. The §26 display
      # shape is pinned instead by `the_default_result_is_an_eighteen_line_passage`
      # in crates/semgrep/tests/cli.rs. Keeping this flag also means the file
      # stays byte-comparable with every recording since §20.
      local flags=(--mode "$mode" -k 10 --json --passage-lines 1)
      [ "$path" = cold ] && flags+=(--no-index)
      for q in "${RANKED[@]}"; do
        echo "### mode=$mode path=$path query=$q"
        "$SG" "${flags[@]}" "$q" "$CORPUS" 2>/dev/null || echo "(no results)"
      done
    done
  done

  for pattern in "${EXACT[@]}"; do
    echo "### mode=exact pattern=$pattern"
    "$SG" -e "$pattern" "$CORPUS" --json 2>/dev/null || echo "(no results)"
  done
}

if [ "${1:-}" = --check ]; then
  [ -f "$OUT" ] || { echo "no $OUT to check against; run without --check first" >&2; exit 2; }
  actual=$(mktemp)
  trap 'rm -f "$actual"' EXIT
  record > "$actual"
  if diff -u "$OUT" "$actual"; then
    echo "snapshot unchanged ($(grep -c '^###' "$OUT") cases)"
  else
    echo >&2
    echo "BEHAVIOR DRIFT: ranked output differs from $OUT" >&2
    echo "If this was intended, re-record with tools/snapshot.sh and review the diff." >&2
    exit 1
  fi
else
  record > "$OUT"
  echo "recorded $(grep -c '^###' "$OUT") cases to $OUT"
fi
