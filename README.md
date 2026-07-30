# semgrep

**Semantic grep for coding agents.** Same shape as `grep` — one command, one
line of output per hit, `path:line:text` — but it ranks by relevance instead
of matching by regex. Ask it a question, get the k most likely places.

Named for its lineage with grep/ripgrep, the incumbent agent search tools it
benchmarks against. (No relation to r2c/Semgrep, the static-analysis tool.)

```sh
semgrep "where is the retry backoff computed" src/   # ranked — no setup, no index step
semgrep -e 'fn \w+_config' .                         # exact regex, grep semantics
semgrep --json -k 20 "auth middleware" .             # JSONL for harnesses
```

## The problem it solves

Coding agents search with ripgrep, so every natural-language intent has to be
compressed into a regex guess first. When the guess misses — wrong identifier,
wrong vocabulary — the agent gets nothing and burns a retry loop: another
guess, another tool call, more tokens, more latency.

```
        "where is the retry backoff computed?"
                    │
       ┌────────────┴────────────┐
       ▼                         ▼
 compress to a regex        pass it through verbatim
       │                         │
 rg "retry.*backoff" → 0 hits    ▼
 rg "backoff"    → 4,000 hits    net/backoff.c:41:u32 delay = …
 rg -i "delay"   → noise …       client/retry.c:88:if (retries…
       │                         drivers/usb/hub.c:212: …
       ▼
 3 round-trips, still guessing   1 call, ranked
```

Measured on 1,200 **real** search queries (CoSQA — human-written Bing queries
labelled against 20,604 Python functions), ripgrep finds the target in the top
5 **3%** of the time; semgrep finds it **21%** of the time. Both numbers are
low because scoring credits one gold function out of 20,604 — the 7× gap is
the signal, and it holds at p < 0.0001.

The bet: for an agent, the quality of the *first* result matters more than the
speed of the scan, because every miss is a full round-trip that never needed
to happen. And a miss is not cheap — an agent that falls back through phrase,
AND, then OR patterns pays ~8 full kernel scans (~25 s) to fail, against
semgrep's single ~100 ms warm query.

## Performance

Exact mode is ripgrep's own engine crates, so it gives up nothing to the
incumbent. Median wall / peak RSS, Linux kernel 6.9 (1.15 GB, 84k files):

| exact regex, kernel | wall | RSS |
|---|---|---|
| **semgrep -e** | **1.72 s** | 12 MB |
| ripgrep | 1.86 s | 11 MB |
| ugrep | 3.43 s | 12 MB |
| GNU grep | 13.1 s | 3 MB |
| BSD grep | 44.2 s | 511 MB |

Ranked mode, end-to-end including process start (measured 2026-07-28):

| ranked query | first time in a scope | cached | cache size |
|---|---|---|---|
| VS Code repo (49 MB, 4k files) | 2.5 s | **10 ms** | 63 MB |
| kernel `drivers/net/` (145 MB) | 3.9 s | **20 ms** | 150 MB |
| whole kernel (1.15 GB, 84k files) | 32 s | **115 ms** | 946 MB |

The first number is a full streaming pass over that scope — chunk, tokenize,
embed — and it is paid once. Peak RSS tracks it: 12 MB in exact mode, ~840 MB
for a warm hybrid query on the kernel, 0.78 GB during the kernel's first pass.
Full tables and methodology in [RESULTS.md](RESULTS.md).

## No index to manage: the index is a cache

There is no setup step and no `semgrep index` in normal use. The insight that
removes it: **a cold search and an index build are the same computation** — one
streaming pass — and one of them throws the work away. So semgrep writes it
down instead.

```
   query #1 in a scope              query #2, #3, … in that scope
   ─────────────────────            ─────────────────────────────
    stream every file                mmap the cached scope
    rank → answer                    diff it against the live tree
    write down what it computed      rank → answer
          │                                   ▲
          └──►  ~/.cache/semgrep/<root-hash>  ─┘
        2.5 s  (VS Code repo)                10 ms
```

The cache fills **scope by scope, as scopes are actually searched**, so the
cost tracks what you asked for rather than the size of the repo:

```
  monorepo/
  ├── services/api/   ██ searched → cached, and fast from then on
  ├── services/web/   ░░ never searched → never indexed, never paid for
  └── vendor/         ░░ never searched → never indexed, never paid for
```

Searching a wider scope later fills the rest and replaces the narrower
entries, so coverage grows along the paths an agent actually takes.

Three properties follow, and they're what make a stateful tool safe to hand an
agent:

- **Results are always true of the tree as it is right now.** Before serving
  from cache, semgrep diffs the live scope against it. Edited and deleted
  files have their chunks tombstoned out of the ranking; new and never-seen
  files are streamed in memory for that query. Repair and lazy-fill are one
  code path. (The diff is throttled to once per ~60 s per scope, so query
  bursts don't each pay for a tree walk.)
- **Warm and cold return the same answer.** Same top-k set, same top hit —
  enforced by e2e tests. A cache that changes only latency is memoization, and
  memoization doesn't need to be disclosed to the caller.
- **Nothing lands in your repo.** The cache lives in `~/.cache/semgrep`
  (override with `SEMGREP_CACHE_DIR`), keyed by canonical root — so there's no
  `.semgrep/` to gitignore and no stale directory left behind in a sibling
  checkout. Deleting it at any time costs nothing but the next first search.

`semgrep index .` still exists for deliberate prewarming (CI, or a human about
to work in a huge tree), and a hidden `--no-index` forces the pure streaming
path for harnesses. Neither is something an agent needs to know about.

The cache is bounded and inspectable. `semgrep cache` shows what it holds
and what it costs; `--prune` reclaims, `--clear` empties it. Entries are
evicted when the repo they index no longer exists, and least-recently-used
past a 2 GB budget (`SEMGREP_CACHE_MAX_BYTES`). Entries are namespaced by a
key covering the index format, the embedding dimensions, and a fingerprint of
the embedding table, so a binary that cannot read an entry never finds it —
incompatibility is a miss that refills, not an error you have to act on.

## What the agent sees

One verb, one escape hatch — the whole surface is designed so an agent never
has to make a configuration decision.

- **Grep-shaped contract.** `path:line:text` on stdout, exit 0 on hits and 1
  on none. Agents adopt it without new habits or new prompt scaffolding.
- **Ranked, not exhaustive.** The default returns the k best locations. `-e`
  switches to exact regex with grep semantics when you need every occurrence
  or proof of absence.
- **Every reply teaches the next move**, on stderr so stdout stays pipeable:

  ```
  stdout — grep-shaped, pipeable
    net/backoff.c:41:u32 delay = base << attempt;
    client/retry.c:88:if (retries < max_retries) {

  stderr — guidance, never in the way of a pipe
    semgrep: ranked top 10 of 1,514 candidates · not it? rephrase the
    query, or -e '<pattern>' for every exact match
  ```

  An exact-mode miss on a cached scope goes further and prints the top-3
  *ranked* hits for the same terms — a wrong identifier guess costs one call
  instead of two.

## How it works

Built on the Bog stack: [`ese`](../ese) (static 512-dim embeddings, compiled
into the binary, CPU-only — what makes *unindexed* semantic search feasible)
and [`anny`](../anny) (HNSW). One query fans out to two engines over one shared
chunk table, then fuses.

```
   corpus  (streamed on a cache miss, mmap'd on a hit)
                        │
                        ▼
          line-window chunks: 32 lines, 8 overlap
          ┌── 1 ─────────────── 32 ──┐
             ┌── 25 ─────────────── 56 ──┐
                ┌── 49 ─────────────── 80 ──┐
                        │
        ┌───────────────┴───────────────┐
        ▼                               ▼
  BM25 (code-aware:              embeddings (ese 512-dim,
  camelCase/snake_case            i8-quantized, mmap'd)
  subtokens + full ids)                  │
        │                                │
     top-128                          top-128
        └───────────────┬───────────────┘
                        ▼
          weighted RRF  (semantic × 0.2)
                        │
            MMR  (spread across files)
                        ▼
               path:line:text, top-k
```

- **One chunk table for everything.** BM25 and embeddings score the same
  chunks, so fusion and eval scoring are apples-to-apples and every result maps
  back to `file:line`.
- **Code-aware lexical ranking.** The BM25 tokenizer splits
  `camelCase`/`snake_case` into subtokens while keeping the whole identifier,
  and chunk text is path-augmented so file names count as evidence.
- **Weighted fusion + diversity.** RRF over the two lists (semantic
  down-weighted to 0.2 — tuned, not guessed), then MMR so results spread across
  files instead of stacking in one.
- **A format shaped by measurement.** `--stats` prints per-stage timing
  provenance, and every v2 decision closed a bottleneck it found: BM25 postings
  became a zero-deserialization mmap'd table (839 ms → 0.1 ms), embeddings
  became i8-quantized unit vectors (the brute scan was page-fault-bound, so 4×
  fewer bytes took it 2.9 s → 65 ms, quality verified neutral).

Full design in [DESIGN.md](DESIGN.md); the research log — agent economics,
CLI-surface collapse, cache design, reranker post-mortems — in
[RESEARCH.md](RESEARCH.md).

## Retrieval quality

The honest version of this section is shorter than it used to be, because an
adversarial audit of our own benchmark found that the ripgrep baseline was a
strawman (RESEARCH.md §12): it tokenized without underscores, so an identifier
in the query was shredded before ripgrep ever saw it. Fixing that improves
ripgrep 6.4× on the kernel. The numbers below use `rg-strong`, which greps the
identifier first — what a competent agent does.

**Real queries** (CoSQA, 1,200 sampled human-written queries over 20,604
Python functions — nobody who wrote them had seen the code):

| recall@5 | rg | semgrep | |
|---|---|---|---|
| real user queries | 0.03 | **0.21** | 7×, p < 0.0001 |

**Our own generated sets** (400/corpus) split by whether the query contains the
answer's own identifier. That split turns out to bracket reality rather than
represent it: real queries contain **0%** identifiers (so `direct` is easier
than reality) but still share 42% of their tokens with the answer (so
`paraphrase`, which deliberately strips vocabulary, is harder than reality).

| recall@5 | kernel | VS Code |
|---|---|---|
| semgrep, direct | **0.92** | **0.87** |
| rg-strong, direct | 0.32 | 0.36 |
| semgrep, paraphrase | 0.03 | **0.14** |
| rg-strong, paraphrase | 0.00 | 0.01 |

So the gap depends entirely on whether you already know what the thing is
called: **~2.5–3× when you do, 8–28× when you don't.** A better grep strategy
closes the first gap and does nothing to the second — which is the clearest
evidence that what remains is a capability difference rather than a
measurement artifact.

One finding that cuts against our own framing: on real queries, **BM25 alone
(0.22) matches the hybrid default (0.21)** and nearly triples semantic-only
(0.08). The advantage over grep is code-aware *lexical* ranking — subtoken
tokenization, path augmentation, ranked top-k over chunks — more than it is
embeddings.

## Does it actually help an agent?

The end-to-end claim, measured on Loc-Bench (real GitHub issues, ground truth =
the functions the real fix modified). Headless Claude agents localize each
issue with one search-tool condition; every invocation is intercepted and
instrumented. 50 instances × condition, paired on the same issues and model:

| paired agent runs | rg | semgrep | both |
|---|---|---|---|
| file-level Acc@5 | 75% | 75% | 75% |
| **function-level Acc@10** | 58% | **69%** | 62% |
| first search surfaces a gold file | 67% | 41% | **84%** |
| median cost / searches per task | $0.21 / 2 | $0.20 / 2 | $0.20 / 2 |

Read honestly:

- **File-level accuracy and cost are a tie.** On small identifier-rich repos a
  strong agent localizes in ~2 searches either way, so there's no retry loop
  left for ranked search to remove (replicating Augment's finding at
  SWE-bench scale).
- **semgrep's real edge is function-level precision** (+11pp, +17pp on bug
  reports): ranked chunk spans land inside the responsible function, grep hits
  land on call sites.
- **rg's edge is first-guess exactness** when the issue text hands the agent
  the identifier outright.
- **Both tools together wins** (84% first-search hit) — per-query routing beats
  either alone, which is why `-e` exists.

Bounding caveats: the benchmark skews small (39/50 repos under 2k files —
grep's home turf; on 2k–10k repos every condition tied), and the regimes where
ranked search should structurally separate (10k+-file repos, weaker driver
models) are queued experiments. A follow-up A/B of offline-winning rerankers
(MaxSim/SIF) *reduced* agent accuracy — engine changes here are now gated on
agent-level evals, not retrieval micro-benchmarks (RESEARCH.md §7, §9).

## Known limits

Paraphrased queries over *code* are the open problem: every engine scores
≤ 0.05 recall@5 on kernel paraphrase, and on real user queries the semantic
half contributes nothing measurable over BM25 alone (§12.3). The root cause is diagnosed, not
speculative — ese's embedding space is prose-trained, and probe similarities
like `str`~`string` = −0.002 and `mutex`~`lock` = 0.045 mean that on code it
behaves as a fuzzy lexical matcher, not a semantic model (RESEARCH.md §9.9).
The fix is a code-distilled static table, same dimensions and drop-in for the
index format, queued behind the agent-eval gate above.
