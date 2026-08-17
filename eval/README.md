# eval — retrieval quality & agent-task evals

**Start with [REPORT.md](REPORT.md)** — setup, the six candidate conditions,
worked examples showing what each actually returns, and the results with their
caveats. This file is the operator's guide; REPORT.md is the findings.

> **Which harness answers which question.** The retrieval evals below score
> *generated* queries and **cannot referee a rendering or ranking change** —
> RESEARCH.md §21.2 measured an offline effect of 0.15–0.28 at p<0.001 that was
> worth −0.009 on real agent queries, and §22.2/§23 reproduced the pattern.
> Generated queries hand the engine the gold identifier ~70% of the time; real
> agent queries do it 0.6% of the time. Use `run_eval.py` for regression floors,
> leakage cuts and corpus-level comparisons. Gate engine changes on
> gorp-bench's `guessplay.py` — it replays real harvested agent queries against
> real gold files and functions, costs nothing, and is the instrument §14.5 and
> §22 both decided on.

## Tiered agent traces

`eval/queries/traces-*.jsonl` are real agent searches — harvested from
campaign shim logs, joined to the benchmark's gold, and sorted into three
tiers by how much of the answer the query already carried:

    golden   names a gold identifier          21% of harvested traffic
    guess    names only the gold's path        7%
    blind    shares neither                   72%

    python3 eval/validate_queries.py eval/queries/traces-v1.jsonl --traces
    python3 eval/replay_traces.py --trees ../gorp-bench/data/locbench/repos/trees

**The tier is computed, not authored** (`eval/traces.py`), and
`validate_queries --traces` recomputes every one. That is the guard on the
cross-repo seam: gorp-bench writes these files and this repo scores them, so
a tier rule that drifted would silently re-label history.

**Report per tier, never pooled.** They are different retrieval problems —
§19.2b measured a blind description finding the gold 13% of the time against
a blind name's 50% — so a pooled number moves when the *mix* moves and a
corpus refresh reads as an engine change. `replay_traces.py` prints the three
strata and never a total.

This is the cheap gate an engine change should move: real agent queries, real
gold, no API budget. The expensive one — the full arm matrix, both function
metrics — is gorp-bench's `guessplay.py`.

What is here, and what is not:

- **Retrieval evals** (`generate.py`, `run_eval.py`) — LLM-generated query
  sets over the bench corpora, scored recall@k / MRR. Results in
  `REPORT.md` §5. See the caveat above before using these to accept or reject
  an engine change.
- **Simulation testing** (`sim/`) — behavior over a *sequence* of steps
  against evolving cache state, which neither of the above can see.
- **The agent evals moved to [gorp-bench](https://github.com/nlaz/gorp-bench)**
  — SWE-Explore-Bench and Loc-Bench campaigns, the PATH shims, guess
  harvesting, and the perf benchmark. They run live agents against real
  repositories, so they cost money and hours; this repo's evals are free and
  run on every change. gorp-bench consumes this directory as a library (its
  `harness/common/gorp_repo.py` puts it on `sys.path`), so `leakage.py`'s
  identifier predicate is literally the same code on both sides rather than
  two copies that agree today.

## The comparison principle

gorp is benchmarked against ripgrep at two levels, and they must not be
conflated. **Keyword mode vs rg** is the mechanics-level comparison — same
engine crates, no index involved, kept honest in gorp-bench's `bench/`.
**Ranked search
vs agentic rg** is the contract-level comparison — same grep-shaped
interface, which primitive gets an agent to the answer in fewer tokens and
round-trips. The second is the product claim, and an index is not cheating
there any more than a database index cheats at a query benchmark — provided
its costs are never hidden:

> rg is stateless and always-true; gorp is stateful, ranked, and honest
> about its state. The eval's job is to show whether that state earns its
> keep — with its costs printed next to its wins.

Concretely: every result table that credits gorp's warm path must carry
index build time and bytes next to it (gorp-bench's `report.py` shows
efficiency with index cost both excluded and amortized), exact mode (`-e`)
never answers from the index (proof-of-absence always reads live bytes),
and staleness is surfaced, not smoothed over.

## Index overhead (measured 2026-07-27, M-series Mac)

**Real-world repos** — 49 GitHub repos indexed during Loc-Bench runs
(median 565 files; the p90 repo is ~2.6k files):

| metric | median | p90 | max |
|---|---|---|---|
| build time | **0.8 s** | 3.4 s | 5.6 s |
| index size | 5.4 MB | 39 MB | 66 MB |

Aggregate: 1.51 GB of source → 629 MB of index in 63 s (~24 MB/s;
index ≈ 0.42× source bytes).

**Language corpora** (added 2026-07-30) — four small, pinned repos covering the
languages `symbols.py` supports that nothing else exercised. They sit in the
<2k-file band where §9.7 found engine variants actually diverge; the original
three are 84k, 4k and 1k files.

| corpus | lang | files | source | symbols | has_doc |
|---|---|---|---|---|---|
| tokio | rust | 790 | 6.0 MB | 7,728 | 59% |
| commons-lang | java | 625 | 10.3 MB | 4,985 | 88% |
| etcd | go | 1,110 | 15.4 MB | 9,211 | 20% |
| jekyll | ruby | 166 | 3.3 MB | 1,068 | 45% |

The `has_doc` spread is deliberate — it is a stratum, and a stratum needs
variance. Their query sets are symbol-anchored (`--anchor symbol`), so ground
truth is a function span rather than a 30-line window: chunking-neutral by
construction, which the older three sets are not (§11.4).

**Bench corpora** — full rebuild (`gorp index`, fresh timing):

| corpus | files | source | build | index | peak RSS |
|---|---|---|---|---|---|
| VS Code | 4,041 | 49 MB | **3.5 s** | 78 MB | 362 MB |
| Wikipedia | 1,008 | 262 MB | **14.4 s** | 239 MB | 732 MB |
| Linux kernel | 84,225 | 1,147 MB | **65.6 s** | 1,333 MB | 1,113 MB |

**Reindex = rebuild.** v1 has no incremental indexing: refreshing a stale
index costs the full build again (the fold-based incremental/watch mode is
the v2 roadmap item). The saving grace is the shape of the cost: a build is
one streaming pass over the corpus — the same pass a single *cold* ranked
search already performs — plus writing the results down. So the break-even
is roughly **one search**: index the kernel in 65.6 s vs run one cold
hybrid search in ~59 s; every warm query thereafter is ~135 ms instead.
On a median real-world repo the entire question is worth less than a
second. Staleness *detection* is much cheaper than rebuild (~1 s to re-walk
84k files; `--check-stale`), so "is my index stale?" can be asked freely —
it's only the refresh that pays the pass.

## Guards

Three things run before or beside every scored number, because each of them
guards a failure that produces a *plausible* wrong answer rather than an error.

**The query sets are in git** — `eval/queries/`, with a `MANIFEST.json`
recording each set's fingerprint, corpus, anchoring and leakage profile. They
used to live in gitignored `eval/data/`, and they are `claude`-generated, so
nothing published was reproducible from the repo alone. See
`eval/queries/README.md` for what each set is and which biases it carries.

**The corpora are pinned and digested** — in gorp-bench, `fetch-corpora.sh`
pins every clone to a SHA and `manifest.py` records a content digest of each
tree:

    python3 ../gorp-bench/bench/manifest.py           # record  (run from gorp-bench)
    python3 ../gorp-bench/bench/manifest.py --check   # detect a tree that has changed

vscode and wikipedia were unpinned until 2026-07-30, so the trees on disk have
`revision: unknown` — that cannot be recovered and is not invented. The digest
still makes them checkable.

**Leakage is printed above every results table.** `run_eval.py` prints, and
stores in `--out`, how much of the answer each query already contained:
identifier share, median length, gold-token overlap, and path leakage. §12.5
said no quality claim should be read without knowing which pole produced it;
this makes that structural rather than advisory. Standalone:

    python3 eval/leakage.py eval/queries/linux.jsonl ../gorp-bench/bench/corpora/linux

`run_eval.py` also validates gold spans against the corpus first and **refuses
to score a drifted set** (`--allow-stale` overrides). A stale set does not
raise — every row scores 0 and the output looks like a measurement, the same
symptom the embedding-width mismatch produced.

`--stratify` / `--where` cut the table by any row field (`split`, `lang`,
`has_doc`) or computed leakage field (`has_identifier`, `path_seg_not_in_gold`).

## Disk

`eval/reclaim.sh --dry-run` prints everything the harness holds, its size, and
the command that rebuilds it. The rule: anything a checked-in script can
rebuild is reclaimable; anything that cost money or nondeterministic model
calls is not. gorp-bench's `data/locbench/runs/` ($39.07 of agent spend) and
`eval/queries/` are never offered.

## Tests

The scorers are pure functions that decide every number published in
RESEARCH.md, so they have their own tests:

    python3 -m pytest eval/tests -q

gorp-bench's `tests/test_scoring.py` covers the Loc-Bench scorer (§11) — the cases where a scorer
is tempted to over-credit, since that is the failure that flatters the tool
under test. `test_run_eval.py` covers the hit predicate that decides every
recall@k and MRR figure. `test_symbols.py` covers symbol extraction, which
defines the ground truth for the symbol-anchored query sets (§11.4).

## Running a lever campaign

    eval/levers.sh --list              # available conditions
    eval/levers.sh                     # all of them, all corpora
    eval/levers.sh base maxsim         # a subset
    eval/diff.py --base base --cand maxsim --metric recall@5

`levers.sh` groups conditions by index flags and rebuilds a corpus once per
distinct build rather than once per condition, and restores a default index
afterwards so a later run does not silently measure against whatever the last
condition built. It uses its own `GORP_CACHE_DIR`, so a parameter sweep cannot
contaminate the entries ordinary searches read.
