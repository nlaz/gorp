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
