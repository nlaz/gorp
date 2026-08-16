# semgrep

**Semantic grep for coding agents.** One command, grep's shape on input, and
it ranks by relevance instead of matching by regex. Ask it a question, get
the k most likely places — each printed as a unit view (`path:start-end`
header, numbered lines, the enclosing declaration above the match). Exact
mode (`-e`) keeps grep's `path:line:text` per match, byte for byte.

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
low because scoring credits one gold function out of 20,604.

That 7× is the wrong number to quote, and we know because we tried to break it.
Give ripgrep an *oracle* — let it read the answer, try every query token, and
keep whichever ranked best, which no real agent can do — and it reaches **10%**.
So the honest headline is **~2.2× against the best ripgrep could ever do**, and
7× against the best it can actually do unaided. Details in
[eval/REPORT.md](eval/REPORT.md).

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

- **Grep-shaped contract, unit-view results.** Exit 0 on hits and 1 on none;
  exact mode prints grep's `path:line:text` per match. Ranked mode prints a
  *unit view* per hit (RESEARCH.md §34): the path once as a
  `path:start-end` header, then `line:` numbered rows dedented as a block,
  with the enclosing declaration above the matched lines and `⋮` where rows
  were elided — hits separated by a blank line. The *input* is grep-shaped
  everywhere: several paths at once, `-i`, `-A`/`-B`/`-C`, `-l`,
  `-g`/`--include`, and `-n`/`-r`/`-R`/`-H` accepted by construction. This
  was earned rather than assumed: measured against real agent transcripts,
  `-n` alone accounted for 88% of the flags typed at a grep-shaped tool, and
  semgrep used to reject every one of them (RESEARCH.md §17).
- **Ranked, not exhaustive.** The default returns the k best locations. `-e`
  switches to exact regex with grep semantics when you need every occurrence
  or proof of absence.
- **A result costs what it looks like it costs.** Lines print without their
  indentation and cut at 200 characters (`-M N`, `-M 0` for no limit), so k
  hits cost about k × 200 and no more. Unbounded, one line could be the whole
  reply: across 366 real agent searches, 23 lines over 1,000 characters
  carried 73% of every byte the tool ever printed, and a single-line 374 KB
  JSON fixture turned one `-k 5` search into 659 KB. That overruns the
  caller's tool-result limit, so the cost was also a *correctness* problem —
  the hits ranked below the long line were truncated away unseen. Capped, the
  same 366 searches cost 75% less and the worst single result is 2.5 KB.
- **Every reply teaches the next move**, on stderr so stdout stays pipeable:

  ```
  stdout — data only, pipeable
    net/backoff.c:38-41
    38:	static u32 next_delay(struct conn *c)
    39:	{
    40:		u32 attempt = c->retries;
    41:		u32 delay = base << attempt;

  stderr — guidance, never in the way of a pipe
    semgrep: ranked top 10 of 1,514 candidates · not it? rephrase the query
  ```

  The ranked footer points back at rephrasing and **does not advertise `-e`**.
  That one clause used to be there, and it was the strongest posture lever
  measured in this project: removing it moved an agent's ranked share from 7%
  to 98% at no cost in accuracy (RESEARCH.md §16.10). Exact mode stays a
  first-class escape hatch, documented in `--help`; it just isn't pitched
  after every ranked search. `SEMGREP_NO_HINTS=1` silences the footers
  entirely.

  An exact-mode miss on a cached scope goes the other way and prints the
  top-3 *ranked* hits for the same terms — a wrong identifier guess costs one
  call instead of two.

### The tool description to give your agent

The tool prompt is a deliverable, not decoration (RESEARCH.md §6): an agent's
behaviour at this tool is set more by the sentence describing it than by
anything in the engine. One clause once moved ranked usage from 7% to 98%
(§16.10), a larger effect than any ranking parameter measured here. Paste this
into your agent's system prompt:

```
The only code search tool available is `sg`, a ranked code search you run
with Bash. Give it anything — an identifier, a phrase, or a question: `sg
"query"` searches the whole repository and returns the most relevant
locations as path:line:text (top 5; `-k N` for more). Start wide: add a
path argument only to narrow further after a wide search has pointed
somewhere. Example: sg "retry_backoff backoff_delay compute_delay" →
src/net/retry.rs:142:fn backoff_delay(attempt: u32). Ranked, not
exhaustive — if the answer isn't there, rephrase.
```

This block predates §34 — it says `path:line:text` while ranked output is now
the unit view — and it stays verbatim anyway, because it is the *measured*
description (desc-v10) and an edited description is an unmeasured one
(RESEARCH.md §20.1; §26.3 set the precedent when desc-v9 said "top 10" over a
top-5 default). Re-measuring a description that names the unit view is §34.3's
standing follow-up.

**Why the example is names and not a question.** semgrep embeds with a static
table — one vector per token, rarity-pooled, word order discarded — so a
description reduces at the engine to its rare tokens. Measured across 413 real
agent queries (§19.2b): when a query shares *no* vocabulary with the answer, a
description finds it **13%** of the time and a name **50%**. Descriptions are
not bad, but they only work when they happen to contain the right rare word,
where a wrong name guess still shares subtokens with the right one —
`retry_backoff` overlaps `backoff_delay` where "computed" does not. Agents
imitate the example rather than the prose (§7.3), so the example is what
decides which of those they write: this one moved the share of name-shaped
queries **+20pp** against the same description without it.

*Evidence grade, stated plainly — and one claim here has been withdrawn.* What
is measured and replicated: the example changes how agents search (+20pp
name-shaped queries, twice, on two different frames) and it costs **fewer
searches per task** (4.0 against ripgrep's 4.7, after 3.5 against 5.0 on the
first frame) at slightly lower cost.

What is **not** true, and was claimed here for one day: that this buys better
answers. A 40-instance frame suggested +0.05 over ripgrep; a 203-pair frame
enriched for exactly the cases the mechanism predicted then returned **+0.000
on that stratum** and −0.034 pooled (§19.7). The description reliably changes
how an agent searches without changing what it finds. semgrep against ripgrep
remains what §18 measured: **parity**.

Recommend this description for the round-trips it saves, not for accuracy it
does not deliver.

The binary is `sg` (and `semgrep`, still, so nothing that resolves it by name
breaks). `--help` still describes the tool the older way, and still prints
`semgrep:` on stderr — deliberate: renaming the message prefix, the env vars,
`~/.cache/semgrep` and `.semgrep/` is the expensive half of a rename, and it
invalidates every built index to fix a cosmetic mismatch.

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
              unit view, top-k (§34)
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

## What the evaluation shows

**In one line: ripgrep can only find code you can already name. semgrep does
not need the name.**

That is the whole product thesis, and it is measurable. Searching a codebase
splits into two situations:

| you are looking for… | example query | ripgrep | semgrep |
|---|---|---|---|
| something you can **name** | `blkg_rwstat_add inline function percpu counter` | 0.34 | **0.92** |
| something you can only **describe** | `helper that increments the right per-cpu statistic by operation type` | **0.00** | 0.04 |

*(recall@5 on the Linux kernel: how often the right code is in the top 5
results. Both rows are the same 199 target functions, asked for two ways.)*

The first row is a 2–3× difference — useful, not decisive. **The second row is
the product.** When the query does not contain the answer's name, ripgrep
finds it **zero times out of 199**. Not rarely. Zero.

That is not a quirk of one benchmark. It is what regex search *is*: it matches
strings you supply. If you know the function is called `blkg_rwstat_add`, grep
is excellent and semgrep is a modest improvement. If you only know it
"increments a per-CPU counter somewhere in the block layer," grep has nothing
to match on, and no amount of skill with grep changes that.

### We tried hard to beat our own claim

Benchmarks written by the tool's authors are worth little, so the comparison
was attacked twice.

**First attack (RESEARCH.md §12):** the original ripgrep baseline turned out to
be a strawman — it tokenized without underscores, so `blkg_rwstat_add` was
shredded before ripgrep ever saw it. Fixing that improved ripgrep **6.4×** and
cut our headline claim from "30×" to ~3×. The published number was mostly our
own bad baseline.

**Second attack (§13.4–13.9):** the fixed baseline was still a *heuristic* —
"grep the identifiers, longest first." So we built **`rg-oracle`**: it is shown
the correct answer, tries every word in the query as a separate search, and
keeps whichever worked best. **No real tool can do this** — choosing the
winning word requires already knowing where the answer is. It is a ceiling: the
best ripgrep could conceivably do.

Every number below is quoted against that ceiling, not just against the
heuristic.

### Real queries, real humans

1,200 questions typed by people into a search engine, matched against 20,604
Python functions (CoSQA). Nobody who wrote these had seen the code:

| | ripgrep (realistic) | ripgrep (**perfect**) | semgrep |
|---|---|---|---|
| recall@5 | 0.03 | **0.10** | **0.22** |

**~2.2× better than the best ripgrep could ever do**, and ~7× better than
ripgrep as actually used. All three numbers look low because scoring credits
exactly one correct function out of 20,604 — the ratios are the signal.

Pre-registering that prediction before running it is how we keep ourselves
honest: we wrote down "if the ceiling reaches 0.85 on identifier queries, our
claim is wrong and we retract it" and committed it *before* the run. It reached
0.46.

### Where it does not help

Being clear about this matters more than the wins:

- **4% is still 4%.** On describe-it queries semgrep finds the target 4% of the
  time against ripgrep's 0%. That is the difference between possible and
  impossible, not between good and great.
- **The semantic half barely earns its keep on code.** Plain lexical ranking
  (BM25) scores 0.22 on real queries; adding embeddings gives 0.21. Embeddings
  alone manage 0.11 — level with a *perfect* ripgrep (0.10), and less than half
  of BM25. The win is code-aware ranking — subtoken splitting, path awareness,
  ranked top-k — far more than it is "AI search."
- **It is one number per query set**, and the sets we wrote ourselves leak: our
  "easy" queries hand over the answer's name 66% of the time, our "hard" ones
  strip vocabulary users would actually use. Only CoSQA and the replayed agent
  queries were written by someone other than us.

Full setup, all seven corpora, worked examples showing exactly what each tool
returns, and the four measurement bugs we found and fixed along the way:
**[eval/REPORT.md](eval/REPORT.md)**.

Agent campaigns are reviewable rather than described. Every finding in this
project came from reading trajectories — a silently-empty search, a verb in
`--help` that taught agents the wrong model — and each one took a person
opening JSONL by hand:

```
python3 eval/locbench/capture.py     # results + run dirs -> one JSON bundle
python3 eval/locbench/viewer.py      # bundle -> results-viewer.html
```

One self-contained page: the gate verdict, the paired scoreboard with
discordant counts beside every delta, a filterable task table, and a
side-by-side drill-down showing **every search an agent ran, what came back,
and what the engine did underneath** — with the first search to surface a gold
file marked. No external assets; it opens offline.

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
