# gorp

**Semantic grep for coding agents.** One command, grep's shape on input, and
it ranks by relevance instead of matching by regex. Ask it a question, get
the k most likely places — each printed as a unit view (`path:start-end`
header, numbered lines, the enclosing declaration above the match). Exact
mode (`-e`) keeps grep's `path:line:text` per match, byte for byte.

Named for its lineage with grep/ripgrep, the incumbent agent search tools it
benchmarks against. (No relation to r2c/Semgrep, the static-analysis tool.)

```sh
gorp "where is the retry backoff computed" src/   # ranked — no setup, no index step
gorp -e 'fn \w+_config' .                         # exact regex, grep semantics
gorp --json -k 20 "auth middleware" .             # JSONL for harnesses
```

## Install

gorp builds against two sibling repos —
[`ese`](https://github.com/flowercomputers/ese) (static 256-dim embeddings,
compiled into the binary, CPU-only) and
[`anny`](https://github.com/flowercomputers/anny) (HNSW) — checked out next
to it:

```sh
git clone https://github.com/flowercomputers/ese
git clone https://github.com/flowercomputers/anny
git clone https://github.com/nlaz/gorp
cd gorp
cargo build --release    # first build downloads the embedding weights (network, once)
```

The result is `target/release/gorp`: one ~39 MB binary with the embedding
table compiled in. No daemon, no GPU, no runtime downloads.

## Usage

`gorp <query> [paths…]` — the query is an identifier, a phrase, or a
question; paths default to the current directory. Exit 0 on hits, 1 on none.

| flag | does |
|---|---|
| `-k N` | ranked results to return (default 5; bare `-k` means 20) |
| `-e` | exact regex mode, grep semantics: every match, exit 1 on none |
| `-i` / `-F` / `--all` | exact mode: ignore case / literal string / print every match |
| `-C` / `-A` / `-B N` | context lines around each hit |
| `-l` | matching paths only, in rank order |
| `-g GLOB` (`--include`) | keep only paths matching the glob (repeatable) |
| `--lines A-B` | keep only results in a line range |
| `-M N` | truncate printed lines at N characters (default 200; 0 = off) |
| `--json` | JSONL: `{path, start_line, end_line, line, text, score}` |
| `--stats` | per-stage timing and provenance, on stderr |
| `gorp cache` | show what the cache holds; `--prune` reclaims, `--clear` empties |

The input is grep-shaped throughout — multiple paths, `-n`/`-r`/`-R`/`-H`
accepted by construction — because agents type grep flags at anything shaped
like grep, and rejecting them costs a round-trip (RESEARCH.md §17).

**stdout is data, stderr is commentary.** Results print alone and pipe
cleanly; guidance rides on stderr:

```
stdout — data only, pipeable
  net/backoff.c:38-41
  38:	static u32 next_delay(struct conn *c)
  39:	{
  40:		u32 attempt = c->retries;
  41:		u32 delay = base << attempt;

stderr — guidance, never in the way of a pipe
  gorp: ranked top 10 of 1,514 candidates · not it? rephrase the query
```

`GORP_NO_HINTS=1` silences the footers. Lines print dedented and capped at
200 characters (`-M`), so k hits cost about k × 200 bytes — uncapped, a
single minified line was measured carrying 73% of everything the tool ever
printed to one agent, pushing real hits out of its context.

## Set up your agent

The tool prompt is a deliverable, not decoration: one clause in it moved an
agent's ranked-search share from 7% to 98%, a larger effect than any ranking
parameter measured in this project (RESEARCH.md §6, §16.10). Paste this into
your agent's system prompt:

```
The only code search tool available is `gorp`, a ranked code search you run
with Bash. Give it anything — an identifier, a phrase, or a question: `gorp
"query"` searches the whole repository and returns the most relevant
locations as path:line:text (top 5; `-k N` for more). Start wide: add a
path argument only to narrow further after a wide search has pointed
somewhere. Example: gorp "retry_backoff backoff_delay compute_delay" →
src/net/retry.rs:142:fn backoff_delay(attempt: u32). Ranked, not
exhaustive — if the answer isn't there, rephrase.
```

(Kept verbatim — it says `path:line:text` though ranked output is now the
unit view — because this is the *measured* description and an edited one is
unmeasured.)

The example is names rather than a question on purpose: gorp embeds with a
static table, so a query reduces to its rare tokens. Across 413 real agent
queries, when a query shared no vocabulary with the answer, a description
found it 13% of the time and a name guess 50% — a wrong name still shares
subtokens with the right one (`retry_backoff` overlaps `backoff_delay`)
where "computed" shares nothing. Agents imitate the example, not the prose:
this one moved name-shaped queries +20pp.

Evidence grade, stated plainly: the description reliably changes *how*
agents search and saves round-trips (4.0 searches per task against ripgrep's
4.7, replicated), but it does not improve answer accuracy — end-to-end,
gorp against ripgrep is parity (RESEARCH.md §18, §19.7). Recommend it for
the round-trips, not for accuracy it does not deliver.

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

On 1,200 real human search queries (CoSQA), gorp finds the target in the
top 5 **~2.2× as often as the best ripgrep could conceivably do** — measured
against an oracle baseline that reads the answer first — and ~7× as often as
ripgrep as actually used. The full comparison, and how we attacked our own
numbers, is below and in [eval/REPORT.md](eval/REPORT.md).

Every miss is a full round-trip that never needed to happen, and a miss is
not cheap: an agent falling back through phrase, AND, then OR patterns pays
~8 full kernel scans (~25 s) to fail, against gorp's single ~100 ms warm
query.

## No index to manage: the index is a cache

There is no setup step and no index verb in normal use. The insight that
removes it: **a cold search and an index build are the same computation** — one
streaming pass — and one of them throws the work away. So gorp writes it
down instead.

```
   query #1 in a scope              query #2, #3, … in that scope
   ─────────────────────            ─────────────────────────────
    stream every file                mmap the cached scope
    rank → answer                    diff it against the live tree
    write down what it computed      rank → answer
          │                                   ▲
          └──►  ~/.cache/gorp/<root-hash>  ─┘
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

Three properties make a stateful tool safe to hand an agent:

- **Results are always true of the tree as it is right now.** Before serving
  from cache, gorp diffs the live scope against it: edited and deleted
  files are tombstoned out of the ranking, new files are streamed in memory
  for that query. (The diff is throttled to once per ~60 s per scope.)
- **Warm and cold return the same answer.** Same top-k set, same top hit —
  enforced by e2e tests. A cache that changes only latency is memoization,
  and memoization doesn't need to be disclosed to the caller.
- **Nothing lands in your repo.** The cache lives in `~/.cache/gorp`
  (override with `GORP_CACHE_DIR`), keyed by canonical root — no `.gorp/`
  to gitignore, nothing left behind in a sibling checkout. Deleting it at
  any time costs nothing but the next first search.

The cache is bounded and inspectable: `gorp cache` shows what it holds,
`--prune` reclaims, `--clear` empties. Entries are evicted when their repo
no longer exists, and least-recently-used past a 2 GB budget
(`GORP_CACHE_MAX_BYTES`). Entries are namespaced by index format, embedding
dimensions, and a fingerprint of the embedding table, so a binary that
cannot read an entry never finds it — incompatibility is a miss that
refills, not an error you have to act on.

## Performance

Exact mode is ripgrep's own engine crates, so it gives up nothing to the
incumbent. Median wall / peak RSS, Linux kernel 6.9 (1.15 GB, 84k files):

| exact regex, kernel | wall | RSS |
|---|---|---|
| **gorp -e** | **1.72 s** | 12 MB |
| ripgrep | 1.86 s | 11 MB |
| ugrep | 3.43 s | 12 MB |
| GNU grep | 13.1 s | 3 MB |
| BSD grep | 44.2 s | 511 MB |

Ranked mode, end-to-end including process start — the first search in a
scope streams and caches, every later one serves from the cache:

| ranked query | first time in a scope | cached | cache size |
|---|---|---|---|
| VS Code repo (49 MB, 4k files) | 2.5 s | **10 ms** | 63 MB |
| kernel `drivers/net/` (145 MB) | 3.9 s | **20 ms** | 150 MB |
| whole kernel (1.15 GB, 84k files) | 32 s | **115 ms** | 946 MB |

Full tables and methodology in [RESULTS.md](RESULTS.md).

## How it works

Built on [`ese`](https://github.com/flowercomputers/ese) and
[`anny`](https://github.com/flowercomputers/anny). One query fans out to two
engines over one shared chunk table, then fuses:

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
  BM25 (code-aware:              embeddings (ese 256-dim,
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
              unit view, top-k
```

- **One chunk table for everything.** BM25 and embeddings score the same
  chunks, so fusion is apples-to-apples and every result maps back to
  `file:line`.
- **Code-aware lexical ranking.** The BM25 tokenizer splits
  `camelCase`/`snake_case` into subtokens while keeping the whole
  identifier, and chunk text is path-augmented so file names count as
  evidence.
- **Weighted fusion + diversity.** RRF over the two lists (semantic
  down-weighted to 0.2 — tuned, not guessed), then MMR so results spread
  across files instead of stacking in one.

Full design in [DESIGN.md](DESIGN.md); the research log — agent economics,
CLI-surface collapse, cache design, reranker post-mortems — in
[RESEARCH.md](RESEARCH.md).

## What the evaluation shows

**In one line: ripgrep can only find code you can already name. gorp does
not need the name.**

| you are looking for… | example query | ripgrep | gorp |
|---|---|---|---|
| something you can **name** | `blkg_rwstat_add inline function percpu counter` | 0.34 | **0.92** |
| something you can only **describe** | `helper that increments the right per-cpu statistic by operation type` | **0.00** | 0.04 |

*(recall@5 on the Linux kernel: how often the right code is in the top 5.
Both rows are the same 199 target functions, asked for two ways.)*

The first row is a 2–3× difference — useful, not decisive. **The second row
is the product.** When the query does not contain the answer's name, ripgrep
finds it zero times out of 199 — not rarely, zero — because regex search
matches strings you supply, and no skill with grep changes that.

Benchmarks written by a tool's own authors are worth little, so we attacked
this comparison twice. The first attack found our ripgrep baseline was a
strawman (a tokenizer bug); fixing it improved ripgrep 6.4× and cut our
headline from "30×" to ~3×. The second built **`rg-oracle`** — shown the
correct answer, it tries every query word and keeps whichever ranked best,
which no real tool can do. It is a ceiling, and every number here is quoted
against it. On 1,200 real human queries against 20,604 Python functions
(CoSQA — the one query set we didn't write):

| recall@5 | ripgrep (realistic) | ripgrep (**oracle ceiling**) | gorp |
|---|---|---|---|
| CoSQA, 1,200 queries | 0.03 | **0.10** | **0.22** |

**~2.2× the best ripgrep could ever do**, ~7× ripgrep as actually used. All
three numbers look low because scoring credits exactly one function out of
20,604 — the ratios are the signal.

Read honestly:

- **4% is still 4%.** On describe-it queries gorp finds the target 4% of
  the time against ripgrep's 0% — the difference between possible and
  impossible, not between good and great.
- **The semantic half barely earns its keep on code.** BM25 alone scores
  0.22 on real queries; adding embeddings gives 0.21. The win is code-aware
  ranking — subtoken splitting, path awareness, ranked top-k — far more than
  it is "AI search."
- **End-to-end, with a real agent driving, it's parity on outcome.** Paired
  agent campaigns on Loc-Bench measured a tie on file-level accuracy and
  cost, +11pp on function-level accuracy (ranked spans land inside the
  responsible function; grep hits land on call sites), and both tools
  together beating either alone — which is why `-e` exists.

Full setup, all corpora, worked examples, and the measurement bugs we found
and fixed: **[eval/REPORT.md](eval/REPORT.md)**. The agent harnesses, the
campaigns behind the Loc-Bench numbers, and a reviewer for every search each
agent ran live in **[gorp-bench](https://github.com/nlaz/gorp-bench)**.

## Known limits

Paraphrased queries over *code* are the open problem: every engine scores
≤ 0.05 recall@5 on kernel paraphrase, and on real user queries the semantic
half contributes nothing measurable over BM25 alone. The root cause is
diagnosed, not speculative — ese's embedding space is prose-trained, and
probe similarities like `str`~`string` = −0.002 and `mutex`~`lock` = 0.045
mean that on code it behaves as a fuzzy lexical matcher, not a semantic
model (RESEARCH.md §9.9). The fix is a code-distilled static table, same
dimensions and drop-in for the index format, queued behind the agent-eval
gate.
