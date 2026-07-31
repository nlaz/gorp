# eval/results — scored runs, kept

Every retrieval number this project has published came out of a file that was
gitignored. The number went into a doc; the run that produced it stayed on one
laptop. Asking "what was bm25 R@5 on the kernel again?" meant re-running a
two-hour job, and RESEARCH.md §13.7 shows the other cost: a published figure
stopped reproducing and there was no way to tell whether the engine, the corpus
or the harness had moved.

All of it fits in well under 1 MB. It belongs in git.

## What is here

- **`*.json`** — one file per scored run. Each row is a `(mode, kind)` cell with
  its four metrics **and `ranks`, the per-query first-correct rank**. The ranks
  are the point: paired bootstrap CIs and sign tests need them, and no
  aggregate can be un-averaged back into them. Discard those and a future
  comparison against this run becomes impossible.
- **`INDEX.md`** — every run and every metric, rolled up. **Generated.**
- **`index.json`** — the same, machine-readable.
- **`replay-ranks.jsonl`**, **`replay-exact-ranks.jsonl`** — per-query ranks from
  the agent-query replay (§13.2), which has its own row shape.

## Regenerating the index

    python3 eval/results.py            # rebuild INDEX.md + index.json
    python3 eval/results.py --check    # fail if INDEX.md has drifted

The JSON is the source of truth. If `INDEX.md` disagrees with it, the JSON is
right and the index is stale.

## Provenance

Runs from 2026-07-31 onward embed a `run` block in every row: timestamp, query
set + fingerprint, corpus + tree digest, binary path/mtime/size, git HEAD, and
the flags (`k`, `slack`, `--where`, `--stratify`, `--extra`). That is what makes
a number checkable a year later instead of merely quotable.

It is embedded per row rather than kept in a sidecar file, because a sidecar
gets separated from its data — which is the failure this directory exists to
fix.

**Earlier runs have none of that**, and it is not reconstructable. `INDEX.md`
lists them under "Runs with no provenance" rather than quietly mixing them in.
For those, `eval/results.py` recovers what the data itself proves: `queries_fp`
is a hash of the query file, so matching it against `eval/queries/MANIFEST.json`
identifies which set was scored — evidence, not a guess. A fingerprint matching
nothing is reported as `unknown (fp …)`, which usually means the set was never
checked in.

## Reading a number out of here

1. Find the run in `INDEX.md`.
2. Check its provenance row. No provenance, or a `corpus_digest` that does not
   match `bench/corpora/MANIFEST.json` today, means it was measured against a
   tree that may no longer exist — quote it as historical.
3. For a comparison, use the `ranks` arrays and `run_eval.py --baseline`, not
   the rounded metrics. `queries_fp` must match, or the two runs scored
   different questions and the harness will refuse them.
