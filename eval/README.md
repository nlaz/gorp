# eval — retrieval quality & agent-task evals

Two harnesses:

- **Retrieval evals** (`generate.py`, `run_eval.py`) — LLM-generated query
  sets over the bench corpora, scored recall@k / MRR. Results in
  `RESULTS.md` §3.
- **Loc-Bench agent evals** (`locbench/`) — real GitHub issues, headless
  agents, one search-tool condition per run, full per-search provenance
  (wall time, tokens, cost, every invocation logged via PATH shims).
  Protocol sketch in `agent-eval.md`; findings in `RESEARCH.md` §7.

## The comparison principle

semgrep is benchmarked against ripgrep at two levels, and they must not be
conflated. **Keyword mode vs rg** is the mechanics-level comparison — same
engine crates, no index involved, kept honest in `bench/`. **Ranked search
vs agentic rg** is the contract-level comparison — same grep-shaped
interface, which primitive gets an agent to the answer in fewer tokens and
round-trips. The second is the product claim, and an index is not cheating
there any more than a database index cheats at a query benchmark — provided
its costs are never hidden:

> rg is stateless and always-true; semgrep is stateful, ranked, and honest
> about its state. The eval's job is to show whether that state earns its
> keep — with its costs printed next to its wins.

Concretely: every result table that credits semgrep's warm path must carry
index build time and bytes next to it (`locbench/report.py` shows
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

**Bench corpora** — full rebuild (`semgrep index`, fresh timing):

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

**The corpora are pinned and digested** — `bench/fetch-corpora.sh` pins every
clone to a SHA, and `bench/manifest.py` records a content digest of each tree:

    python3 bench/manifest.py           # record
    python3 bench/manifest.py --check   # detect a tree that has changed

vscode and wikipedia were unpinned until 2026-07-30, so the trees on disk have
`revision: unknown` — that cannot be recovered and is not invented. The digest
still makes them checkable.

**Leakage is printed above every results table.** `run_eval.py` prints, and
stores in `--out`, how much of the answer each query already contained:
identifier share, median length, gold-token overlap, and path leakage. §12.5
said no quality claim should be read without knowing which pole produced it;
this makes that structural rather than advisory. Standalone:

    python3 eval/leakage.py eval/queries/linux.jsonl bench/corpora/linux

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
calls is not. `eval/data/locbench/runs/` ($39.07 of agent spend) and
`eval/queries/` are never offered.

## Tests

The scorers are pure functions that decide every number published in
RESEARCH.md, so they have their own tests:

    python3 -m pytest eval/tests -q

`test_scoring.py` covers the Loc-Bench scorer (§11) — the cases where a scorer
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
condition built. It uses its own `SEMGREP_CACHE_DIR`; see FIXES.md #10 for why
that matters.
