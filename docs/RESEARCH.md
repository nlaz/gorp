# Research: collapsing modes & beating ripgrep as the agent search primitive

**Status:** research phase, opened 2026-07-27, distilled 2026-08-16. Feeds a
redesign of the CLI surface (DESIGN.md is the v1 design this revisits).
Question under study, as posed then: the tool exposed four modes
(`hybrid|keyword|bm25|semantic`); should the agent-facing surface have *no
modes at all* — and how much of the "smart driver" work that Claude Code does
in prompts can we push down into the tool? (Answered in effect: `--mode` is now
a hidden harness knob, not part of the promised interface.)

**Reading this log.** Entries are dated and supersede each other in place; a
claim carries the date of its section, not today's. Two things drifted under
the whole document after it was written: the binary was renamed `semgrep`/`sg`
→ `gorp` in 2026-08, and the agent harnesses moved out to the sibling
`../gorp-bench` repo, so `eval/locbench/…` and `eval/swexplore/…` paths below
resolve to `../gorp-bench/harness/…` today. Both are left as written.

**Reading this log after the distillation.** The 2026-08-16 pass cut a third of
the prose — procedure, restatement, and the blow-by-blow of interim looks — and
kept every measured result, its confidence interval, its n, and the verdict that
followed. **Section numbers were not touched**, because source comments cite
them by number (a bare `§9.1` in a comment means this file) and
`crates/gorp-core/tests/docs.rs` fails the build if a cited section stops
existing. Numbering here is an anchor namespace rather than an outline, which is
why nothing is renumbered and why a section kept for a single sentence is one
that something in `crates/` still points at.

## What stands

The conclusions the rest of the document is evidence for, each with the section
that earns it:

- **The engine is at parity with ripgrep for agents, and that result is
  powered.** hitRegion@5 +0.0054 [−0.0043, +0.0153] over 848 paired
  instances (§32.2). Across five arms and $1,365 of campaigns, no search tool
  we added bought accuracy (§32.3).
- **Offline retrieval eval cannot referee a ranking or rendering change.** It
  scores generated queries that contain the gold file's own identifier ~70% of
  the time against real agent queries that do it 0.6% of the time. Gains fail
  to transfer (§9.7, §10.6, §14.5) and so do *losses* (§21.2). Gate engine
  changes on replayed real agent queries; use the offline harness for
  regression floors and leakage cuts (§12, §23.2).
- **The benchmark, not the engine, was the bottleneck for a long time.** 80–87%
  of Loc-Bench instances carry no signal about the search engine, so 3pp was
  unreachable at any price (§11.5).
- **Document-side rendering is a closed direction.** No rendering improves
  retrieval on real agent queries by more than 0.023, and the long-standing
  `split`+`sif` recommendation was 0.011 *worse* than none (§23.2).
- **The embedding space is prose-shaped, not code-shaped.** `str~string`
  scores −0.002; on code the model works as a fuzzy lexical matcher (§9.9).
  The vocabulary gap it leaves is 54% of ranking misses (§32.4) and the
  §9.9 code-teacher swap is the one lever still aimed at it.
- **Score file scopes with both function metrics, never one.** `rank_func` and
  `rank_func_ovl` differ by 14.2pp, a bracket wider than every effect §20–§23
  tried to detect; a lever that moves them in opposite directions is changing
  chunk geometry rather than retrieval quality (§24.1).
- **What shipped on real-query evidence:** the declaration boost (§24.2–§24.3),
  `--bm25-pin 5` (§32.4b), the fine rerank (§29.1), the learned checklist
  (§35.6), and the unit view (§34). **What died:** MaxSim as a default
  (§13.10), the code-trained table (§10.6), function chunking (§11.4), query
  pruning (§20.6), multi-phrase OR (§31.2), the path boost (§35.5), and graph
  expansion (§35.4).
- **Display format changes the route, not the destination.** Full chunks cut
  reads-after-search 47% and moved accuracy +0.000 (§25.2).

---

## 1. The incumbent: how Claude Code actually drives ripgrep

A cost-ordered ladder: **Glob** (paths, near-zero tokens) → **Grep** →
**Read** (500–5,000 tokens per file) → **Explore subagent** (a cheaper model
does multi-hop search in an isolated context and returns only conclusions).
Grep defaults to `files_with_matches` **sorted by mtime descending** — recency
is Claude Code's only relevance ranking. Everything else is bounded rather
than ranked (`head_limit` 250, 20,000-char cap), and past 3 queries the prompt
delegates to Explore.

### 1.1 Extracted tool prompts (Claude Code v2.1.220, verbatim)

Two prompt variants ship, gated on model; the lean one is the benchmark
target. Estimated schema+description cost of Grep as rendered into the prompt:
**roughly 500–600 tokens** (description ~90 tokens lean; **13 parameters**
with multi-sentence descriptions dominate), most of it regex coaching and
output-mode plumbing. `head_limit` reads "Defaults to 250 when unspecified.
Pass 0 for unlimited (use sparingly — large result sets waste context)" —
token-economy language *inside a parameter description*. Mechanics: bundled
rg, `-l` output sorted mtime-desc; result cap **20,000 chars (~5k tokens)**,
oversized results redirected to a `<persisted-output>` block. Glob: **100
files**, mtime-sorted. Read: **2000-line default window** — "only read that
part," which is what a ranked `path:line` hit enables.

### 1.2 What the prompting compensates for

| Prompt/system mechanism | rg weakness it patches |
|---|---|
| `files_with_matches`, mtime sort, `head_limit` 250, 20k cap | unbounded output on common terms (O(hits), not O(k)); no relevance signal |
| ">3 queries → Explore subagent" | no ranking → triangulation expected, its token cost quarantined |
| "start broad and narrow down; try naming conventions" | a miss returns *nothing* — strategy must live in prompt text |
| regex-syntax coaching | query language is regex, not intent |
| parallel-tool-call guidance | probes are cheap but low-yield; throughput comes from batching guesses |

The bet: a **ranked, bounded, deduplicated top-k for an intent-shaped query**
moves that patch out of the prompt (paid every session, by the expensive
model, every loop) and into the tool (paid once, in Rust, at ~135 ms).

---

## 2. Token economics of a search round-trip

### 2.1 Cost anatomy

With prompt caching (cache reads ≈0.1× input price, ~90%+ reuse), a round-trip
costs `(result tokens) × input price` + `(reasoning + next call) × output
price` + `(prior context) × 0.1 × input price`. **Tool-result size is the
dominant controllable cost**, and **every extra round-trip costs output
tokens** (100–300 output tokens ≈ 5× the price of the same input tokens)
**plus another cache-read pass over the whole context.**

### 2.2 Measured output volumes (VS Code corpus)

| Query | Tool | Output | ≈ tokens |
|---|---|---|---|
| `dispose` | `rg -n` | 1.3 MB / 1,848 lines | ~330k (must truncate) |
| `dispose` | `rg -l` | 36 KB / 443 files | ~9–10k |
| `dispose` | `semgrep -k 10` | 1.2 KB | ~300 |
| "where is a terminal instance created" | `semgrep -k 10` (hybrid) | 1.8 KB / 10 lines | ~450 |
| same intent, keyword-ized | `rg` | 0 hits → retry loop | 0 + another round-trip |

**rg's output cost is O(corpus × term frequency); a ranked tool's is O(k).**
rg is cheap only when the term is rare — precisely when the agent has already
guessed the right identifier.

### 2.3 Where paraphrase vs keyword queries differ in cost

Input side is negligible (~10–25 tokens NL vs ~2–5 for an identifier); the
cost is the model *deriving* keywords in reasoning tokens across several
attempts. Same NL intents, keyword-ized for rg, hit top-5 3–27% of the time vs
86–99% for bm25/hybrid on direct queries — on the kernel a 30× gap.

---

## 3. External evidence (published record)

### 3.1 Why Claude Code has no index — Anthropic's own account

Boris Cherny, Jan 2026: *"Early versions of Claude Code used RAG + a local
vector db, but we found pretty quickly that agentic search generally works
better."* Latent Space (May 2025) concedes: *"at the cost of latency and
tokens, you now have really awesome search without security downsides."*
Staleness, privacy and ops all have local-index answers; the conceded cost —
**tokens and latency** — **is the attack surface**.

### 3.2 Agentic vs semantic retrieval — who found what

- **SWE-bench (ICLR 2024):** BM25 retrieval resolved 1.96% of issues (Claude
  2) vs 4.8% with oracle files.
- **Augment Code (Sep 2025):** embeddings added to their SWE-bench agent gave
  *no improvement* — "agent persistence compensates for unsophisticated
  retrieval" — but they expect embeddings to "become essential for larger
  codebases" and recommend **exposing them as a tool inside the agentic
  loop**, this project's exact shape.
- **Cursor (Nov 2025), the strongest pro-hybrid numbers:** **+12.5% QA
  accuracy** (6.5–23.5% across frontier models), production A/B +2.6% code
  retention on 1,000+-file repos; embedder trained on traces.
- **Sourcegraph Cody (2024)** dropped embeddings for ops/privacy, not quality
  — objections local static embeddings dissolve. **Amazon Science (2026):**
  "Keyword search is all you need," >90% of RAG performance without a vector
  DB. Windsurf, Cline, Devin and Amp also dropped vector search; Cursor is the
  notable dissent, with data.

**Synthesis:** nobody has published evidence against *ranked lexical search as
the loop primitive* — the debate is embeddings-vs-grep. Our own run agrees:
BM25 is the headline win (0.88–0.99 R@5 direct).

### 3.3 Tool-prompting ROI (why the prompt is part of the product)

Anthropic: rewriting one flaky tool's description → **40% decrease in task
completion time**; "agent-tool interfaces are as critical as human-computer
interfaces." Concise by default with opt-in detail (206 → 72 tokens), well
under the 25k cap.

---

## 4. The mode-collapse question

### 4.1 Current surface (v1)

Four modes plus a dozen tuning flags ≈ four tools' worth of schema tokens,
paid every session. **After the weighted-RRF tuning there is no query type
where picking a specialist mode beats hybrid** — hybrid ≈ bm25 on direct
queries and ≥ any single engine on paraphrase.

### 4.2 Is keyword mode worth keeping? What `-e` actually provides

Keyword mode ≈ rg (same crates, same speed). Ranked search cannot give **regex
semantics**, **exhaustiveness** (top-k is the wrong contract for "all call
sites"), or **exact-match certainty** (a literal hit is proof; a ranked list is
evidence). But the agent already has rg, so duplicating it helps only
harnesses where semgrep is the sole search tool.

### 4.3 Options for the collapsed surface

**A. Pure ranked, no flags** — smallest description, gives up the drop-in
claim. **B. Auto-detect router** — misrouting is a silent failure the agent
can't see or override. **C. One ranked behavior + `-e/--regex`** —
self-describing to anyone who knows grep. Recommendation: **C**, internal
engines hidden; A the fallback if `-e` is never chosen or chosen wrongly.

### 4.4 What else collapses

`--json`, `-k`, `-C` stay; tuning flags go hidden; `-i`/`-F` fold into `-e`.
`semgrep index` stays an operator command — better yet auto-built on first
ranked query, which needs design for the 59 s / 1.3 GB kernel cost.

---

## 5. The core question: push inference down into the tool?

Claude Code answers search overhead with a **delegated driver**, not a smarter
tool; delegation quarantines the cost without eliminating it (agentic
workloads run ~4× chat token use). The third pole is a **smart tool**: the
model states intent once. They compose — an Explore-style agent *equipped
with* semgrep is strictly cheaper than one with grep alone.

### 5.1 Where pushing down clearly wins

**Ranking** (BM25 + fusion + MMR) replaces triage over 443 file paths with a
top-10 — the measured 30× quality win, 135 ms of Rust. **Vocabulary
derivation** does mechanically what the model does in reasoning tokens.
**Bounded output** kills the truncation/refine dance. **Dedup/diversity**
stops the model paying tokens to notice 40 hits are one vendored file.

### 5.2 Where pushing down loses (keep the agent smart)

**Multi-hop reasoning** (hop 2 depends on hop 1's *content*),
**exhaustive/structural queries** (rg's job), **query understanding beyond
retrieval** (LLM-side expansion beats static embeddings on paraphrase — kernel
≤ 0.05 for *every* mode — but means an LLM call inside a CLI), and **judgment
about sufficiency**.

### 5.3 The efficiency model (to be validated by agent evals)

With rg the expected cost is `E[hops] × (result + reasoning + context
re-read)`, E[hops] inflated by the 73–97% top-5 miss rate on intent-shaped
queries; ranked hybrid drives E[hops] → ~1 for direct queries (R@5 0.86–0.99)
at ~450 tokens per hop. Predictions: **searches-to-success** substantially
fewer with semgrep-only vs rg-only; **tokens-to-success** dominated by avoided
round-trips; **failure mode to watch** — does the agent *trust* a top-10 and
stop early where rg's exhaustiveness would have disabused it?

### 5.4 Ceiling and levers beyond v1

Static ese embeddings are the known ceiling on paraphrase-over-code (kernel ≤
0.05); Cursor's trace-distilled embedder says that ceiling is an artifact of
the embedder, not the architecture. Levers in escalating cost: better code
embeddings; server-mode LLM query expansion; trace-trained embedder.

---

## 6. The tool prompt is a deliverable

Anthropic's 40% task-time reduction from a description rewrite says the tool
description ships with the binary. Two-sentence core: *"Search code and docs
by meaning or by keyword. Ask in plain language (or an exact identifier);
returns the top-k most relevant locations as `path:line:text`."* Plus the `-e`
escape hatch and "results are ranked — if the first page doesn't answer,
rephrase rather than paging." The number to beat: Grep spends an estimated
**500–600 tokens** on description + **13 parameters** (§1.1); a collapsed
surface (query, path, `k`, optional `-e`, optional context) should land
**under ~200**. Steal in-band result counts, truncation footers that say *how
to narrow*, and the `path:line:text` shape the model knows from rg; make
errors steer — 0 hits should say what to try, not just exit 1.

---

## 7. Real-world evals (replacing the synthetic query sets)

**#1 — Loc-Bench V1 localization ablation.** 560 instances from real GitHub
issues, gold = the 1–10 functions the real fix modified (LocAgent, ACL 2025);
**localization needs no test execution or docker.** {rg} vs {rg + semgrep},
150–200 instances, ~$20–60 per condition. **#2 — SWE-bench Verified subset
(the headline):** mini-swe-agent is bash-only, so swapping the search tool is
`semgrep` on PATH plus one prompt line; 50–100 instances,
~$100–300/condition, target **fewer tokens/steps at equal resolve rate**.
**#3 — SWE-Explore (stretch):** 848 instances, 203 repos, 10 languages.
Conventions: $ per *resolved* instance; median tokens / tool calls / searches
**conditioned on success**, pre-registered; stratify by repo file count (the
edge appears above ~1k files).

### 7.1 Pilot results (50 instances × {rg, semgrep, both} × Sonnet, 2026-07-27)

Headless `claude -p`, PATH-shim provenance, blocker shims for grep/git (haiku
demonstrably tried `git log --all --grep=<issue#>`, which would have leaked
the fix). 96% clean runs; Sonnet-initiated grep/git attempts across all 150
runs: **1**.

| finding | number |
|---|---|
| File Acc@5 | **75% in all three conditions** — dead even |
| Function Acc@10 (tolerant, paired n=48) | **semgrep 69% vs rg 58% (+11pp)**; on bug reports 92% vs 75% |
| Median cost / searches / output tokens | ~$0.20 / 2 / ~1.4–1.6k — no efficiency separation |
| First search surfaces a gold file | both **84%** · rg 67% · semgrep-only 41% |
| Tool choice in `both` | **rg 163 vs semgrep 37** (82/18) |
| semgrep invocation style | **67% used `-e` exact mode**, 33% ranked |

The §5.3 efficiency prediction did **not** materialize: Sonnet localizes small
repos in ~2 searches either way, so there is no retry loop to remove. The real
signals: **function-level precision** is where semgrep wins (+11pp) — ranked
chunk spans point inside the right function, grep points at call sites; the
84% first-search rate in `both` says per-query tool choice beats either tool
alone; **interface gravity is the product finding** — grep habits from
pretraining dominate unless the prompt actively steers; and 39/50 repos are
<2k files (max 6.4k), so the thesis is untested at ≥10k files. Runs using ≥1
ranked query hit 68% fnAcc@10t vs 50% for `-e`-only runs, and only 21/70
ranked queries were the agent's *first* search (30% came right after a
0-output search — a fallback, one round-trip late). Shipped: a decision-rule
description, and a miss-as-nudge printing the top-3 ranked hits when `-e`
returns 0 on an indexed corpus.

### 7.2 Guided-prompt ablation (same 50 instances, 2026-07-27)

1. **Instruction gravity beats interface gravity.** Agents obeyed the routing
   rule mechanically, but its first branch ("exact symbol known → rg/-e")
   matches nearly every Loc-Bench issue, so obedience meant *less* ranked
   usage: semgrep calls in guided-`both` dropped to literally 0 (from 144/14
   rg/sg); typed ranked queries in `semgrep` fell 66 → 18. Accuracy flat
   (fAcc@5 75→77, fnAcc@10t 62→65 / 69→67 — noise).
2. **The miss-nudge almost never fired, exposing a real engine gap:** agents
   scope to subdirectories in **65% of all semgrep calls (124/191)**, but
   `search()` only checked `index::exists(<path arg>)`, so those queries fell
   to the cold path and could never trigger index-gated behavior (nudge fired
   on 4/69 misses). **Fix: ancestor index discovery.**
3. Zero-search runs (Glob+Read only) rose 23 → 28 of ~98 — on small repos,
   search is optional for a strong driver.

### 7.3 Name + framing ablation (same 50 instances, 2026-07-27)

| description variant | ranked share | ranked-first | fnAcc@10t |
|---|---|---|---|
| v1 pilot — ranked-as-identity | **35%** | **38%** | **69%** |
| v2 — explicit routing rule | 10% | 2% | 65% |
| v3 — modes menu (`semgrep` name) | 9% | 4% | 65% |
| v3 — modes menu (`search` name) | 10% | 2% | 56% |

**The name-gravity hypothesis is refuted** — the identical binary under
`search` vs `semgrep` produced statistically identical usage. **Framing
hierarchy is the real lever:** the v1 description, which gives the tool a
ranked *identity* with `-e` subordinate, produced **3.5× the ranked usage** of
an explicit rule or a symmetric modes menu. **Design rule: assert identity,
don't offer a menu.**

---

## 8. Design sketch: the index is a cache (2026-07-28)

Reframe `.semgrep/` from an *artifact* the user administers to a *cache* the
tool owns: created as a side effect of the first ranked search, repaired on
access, LRU under a ~5 GB cap in `~/.cache/semgrep/<root-hash>/`, disposable,
invisible to the agent. **A build is one streaming pass — the same pass a cold
search already performs.** Cold search and index build are the same
computation; one throws the work away, the other writes it down.

Mechanisms: (1) **write-through cold path** — first-query cost ≈ today's cold
search + write I/O (kernel ~59 s → ~66 s; median real repo <1 s). (2)
**Read-repair via overlay** — diff the live tree against the cached file
table; changed/deleted files tombstone their chunk ids, changed/new files
stream through an in-memory delta, the lists fuse. No index-format change; a
delta above a threshold (say >5% of files — branch switch) is a full miss; the
staleness walk is throttled to once per corpus per ~60 s. (3) **Keyed by
canonical root** (prefer the enclosing git root — which *is* the
ancestor-discovery fix, via longest-prefix match); corrupt entries are misses,
never errors. (4) flock per entry, the loser streams; publish by write-to-tmp
+ rename.

Eval fairness becomes provable — the *cache-transparency invariant* (same
query ⇒ same results, warm or cold, up to score ties) is enforceable in e2e
tests, and "a cache that changes nothing but latency is memoization, and
nobody argues memoization invalidates a comparison." §4.4's open item is
answered: **the 59 s was being paid by the cold search anyway; write-through
makes it an investment instead of a toll.** Risks to measure: first-query
surprise (~60 s where the agent expected ~100 ms), write-through I/O (est.
~10% at kernel scale), and the staleness walk on warm queries (1 s on 84k
files vs a 135 ms query).

### 8.1 Scoped-lazy filling: index only what's been asked about

Queries carry a scope, so the cache should too. The unification that makes it
nearly free: **read-repair and lazy fill are the same mechanism.** Query-time
diff yields {stale, new, never-covered}; all three stream through the delta
path and write-through marks them covered, so coverage grows monotonically
along the agent's actual search paths. It buys **first-query cost proportional
to the scope, not the repo** (a query scoped to `drivers/net/` on the kernel
pays ~2 s, not 66 s); **the 65% subdir-scoped calls flip from worst case to
best case**; monorepos never pay for the 90% nobody searches; scoped staleness
checks in ms instead of ~1 s.

It costs format flexibility — `bm25.flat` is a sorted immutable table. The
classic answer is **segments** (Lucene-style): each fill writes a small
immutable segment, queries merge across segments plus the live delta,
compaction merges at ~8 segments and never re-embeds. Stepping stone (**scope
promotion**): keep the v2 format but key entries by queried root with
containment reuse — an ancestor entry serves any descendant scope; a wider
query builds it and evicts its children.

### 8.2 Implementation status (shipped 2026-07-28)

**Parallel pass:** parallel `tokenize_doc` + serial `add_tokenized`, rayon
workers in batches capped by count *and* bytes (256 files / 16 MB — RSS at or
below the serial baseline), serial in-order fold preserving chunk-id lockstep.
Kernel 65.6 s → **45.5 s** (1.44×, CPU util 1.9×→3.2×), wikipedia 14.4 s →
**8.7 s** (1.66×, RSS 732→661 MB), vscode 3.5 s → **2.4 s**.

**Cache phase 1** (scope-promotion form): `index::discover` (local `.semgrep`
→ ancestor walk → central-cache longest-prefix match); subtree-filtered
ranking; write-through into `$SEMGREP_CACHE_DIR` (default `~/.cache/semgrep`)
with child-entry eviction on widening; throttled scoped read-repair
(`SEMGREP_CACHE_TTL_SECS`, default 60 s) — repair and lazy fill are one code
path. The `-e` miss-nudge now gates on discovery, so it fires for subdir
scopes (previously 4/69, the §7.2 gap). 34 tests green; on VS Code a subdir
query is warm at 189 ms incl. 102 ms scoped repair walk, and write-through
runs 148 ms cold-with-cache → 5 ms warm. **Transparency invariant, precisely:**
warm and cold return the same top-k *set* and the same top hit; adjacent
near-ties can swap order because warm scores read the i8-quantized matrix
while cold scores are f32. Not yet: LRU size cap/GC, cold-cache Loc-Bench
condition, pipelined embed overlap.

---

## 9. Retrieval-quality levers: SIF, MaxSim, multi-pass (explored 2026-07-28)

The open quality problem is paraphrase-over-code (kernel R@5 ≤ 0.05 for every
engine, §3) and the fact that the semantic list had to be down-weighted to 0.2
in fusion because it *diluted* BM25. ese's source explains why:
**`encode_single` pools by uniform mean over wordpiece vectors**, so a 32-line
chunk's two discriminative identifiers are averaged against hundreds of
boilerplate tokens — the chunk vector is muddy by construction. All three
levers below attack this.

### 9.1 SIF term weighting (corpus-adaptive pooling)

Arora et al.'s Smooth Inverse Frequency: weight each token vector by
`a/(a + p(w))` (p(w) = corpus unigram probability, a ≈ 1e-3), then optionally
subtract the corpus's first principal component. Rare tokens dominate the
pool; boilerplate nearly vanishes — the embedding-side analog of idf. **The
cache already stores corpus statistics**: p(w) at the wordpiece level is a
small frequency table countable during the pass, the common component one
512-dim vector computable at build time, both living in the cache entry →
**SIF becomes corpus-adaptive**, the query side using the same table (unknown
query tokens = max weight). **Blocker: ese's API** exposes only pooled
vectors; needs `for_each_token_vector(text, impl FnMut(&str, &[f32; D]))`,
which also unlocks MaxSim. Validatable offline against the existing 1,198
ground-truth queries.

### 9.2 MaxSim reranker (late interaction, ColBERT-style)

`score(q, d) = Σ_i max_j cos(q_i, d_j)` over *token* vectors — each query
token finds its best match anywhere in the chunk, so one strong identifier
match isn't averaged away. With static embeddings this is nearly free: doc
token vectors are table lookups, so the top ~128 candidates can be reranked at
query time by re-reading chunk text — no index-format change, no storage
blowup. Rough cost: 20 query tokens × ~300 chunk tokens × 512 dims × 128
candidates ≈ a few ms with SIMD/rayon. Bonus: **line-level localization for
free** (argmax positions say which tokens matched, extending the +11pp
function-precision edge of §7.1).

### 9.3 Multi-pass / recursive search (and the cache synergy)

1. **PRF (pseudo-relevance feedback), tool-internal.** Pass 1 hybrid → top ~10
   chunks → their most discriminative terms (high tf in hits, low df in
   corpus) → appended to the query → pass 2 BM25 → fuse. "LLM query expansion
   without the LLM": the NL query only has to land *near* the target once.
   Warm cost ~2× (80 → ~160 ms). **No new APIs — implementable today.**
2. **Recursive scoped drill-down.** If pass-1 scores cluster (say ≥70% of
   top-k in one subtree), re-rank scoped to it — the agent's measured funnel
   behavior (§7.2: 65% subdir-scoped) done inside the tool, and every scoped
   pass warms the lazy cache (§8.1).
3. **Semantic→keyword handoff at the agent level** — already happens (30% of
   ranked queries followed a 0-hit exact search); PRF saves the round-trip.
4. **Two-pass cold search.** A cheap lexical scan selects candidate files;
   pass 2 embeds only those, addressing the 916 MB cold-BM25 RSS.

### 9.4 Measured results (2026-07-28, full campaign: 3 corpora × 5 conditions)

ese gained `for_each_token_vector`/`for_each_token`; semgrep gained `index
--sif`, `--maxsim` (~35 ms), `--prf N` (~32 ms), hidden and default off.

| lever | verdict | evidence (R@5 / MRR deltas vs base) |
|---|---|---|
| **PRF** | **kill** | Harmful everywhere: kernel direct bm25 −0.27 R@5 (MRR −0.39), wiki paraphrase −0.14, vscode −0.04..−0.08. Query drift amplifies whatever the seed pass found; no paraphrase gain anywhere. |
| **MaxSim** | **adopt, but re-wire** | On the *semantic list*: direct +0.05/+0.10/+0.11 R@5, MRR +0.12/+0.18/+0.16 (kernel/wiki/vscode). On *hybrid* it reranks the fused pool, overriding BM25's exact-match signal: vscode −0.05, wiki MRR +0.07. Fix: rerank the semantic list *before* fusion. |
| **SIF** | **keep as MaxSim's multiplier only** | Alone: paraphrase +0.02..0.03 but code semantic direct −0.15 on kernel (hyper-rare identifiers over-focus the chunk vector; paraphrase queries avoid those tokens). With MaxSim: wiki hybrid 0.99 direct / 0.43 paraphrase (MRR +0.08/+0.04), vscode semantic +0.12/+0.18, semantic paraphrase 7× on vscode (0.01→0.07). |
| **kernel paraphrase** | **the wall stands** | ≤0.05 in every condition. Needs a better code embedder, not more query-time machinery on static embeddings. |

### 9.5 Pre-fusion re-wire + weight sweep (2026-07-28, final)

MaxSim moved pre-fusion: the semantic head (k×3, min 24) is reranked *inside*
the semantic branch, then RRF fuses with untouched BM25.

- **The code-hybrid regression is gone** (vscode: −0.05 post-fusion → flat R@5,
  MRR +0.02) while every semantic-mode gain survives (+0.05..0.11 R@5,
  +0.12..0.18 MRR). Hybrid flat-to-positive everywhere (wiki MRR +0.04; one
  soft cell: wiki paraphrase R@5 −0.03, MRR still up). Cost ~39 ms/query.
- **SIF fails its graduation gate:** sif+maxsim2 trades direct quality (kernel
  semantic −0.07, wiki hybrid MRR −0.03 vs plain maxsim2) for paraphrase gains
  (+0.02..0.07). Not a default.
- **sem_weight 0.2 survives the sweep:** with maxsim on, w0.4/w0.6 hurt
  *everywhere* (kernel direct 0.91→0.86→0.84; wiki 0.98→0.95→0.92). BM25's
  dominance is a property of the query distribution, not a defect of the
  fusion weight.

**Shipped defaults: unchanged** (`--maxsim` and `--sif` stay opt-in hidden).

### 9.6 Knob sweep (2026-07-28: pool, blend, sif-a, centering)

`a` and the sample-estimated common component persist in `sif.bin` so query
pooling always matches build. 7 conditions × 3 corpora:

- **Pool 96 adopted as the `--maxsim` default head** (was k×3 min 24):
  semantic direct +0.03/+0.04/+0.06 R@5 (kernel/wiki/vscode), hybrid MRR
  neutral-to-positive, no real regressions. Cost 21 → 54 ms.
- **Blend: dead.** α = 0.75/0.5 flat-to-negative everywhere.
- **SIF a: more aggressive is better on code, hypothesis inverted.** a=1e-4
  beats 1e-3 on both code corpora (vscode +0.02 direct/+0.01 paraphrase;
  kernel semantic direct +0.05, recovering most of the −0.07 SIF regression)
  but trades wiki paraphrase (−0.03) — with MaxSim supplying precision, the
  single vector can afford maximal rarity focus. Milder a=1e-2 is bad
  everywhere (vscode −0.12). Default stays 1e-3; **use `--sif-a 1e-4` on code
  corpora** — doc'd, not defaulted.
- **Centering: not worth it.** Neutral on all three (one good cell — kernel
  hybrid direct 0.92, the campaign's best — but no pattern).

**Six-point pool curve** (24/32/48/64/96/128): no universal knee. Semantic
*direct* keeps creeping through 96 (kernel still rising at 128: 0.78);
semantic *paraphrase* on code **peaks at 48 and degrades past it** (vscode
0.04→0.02, MRR halves); hybrid R@5 best at 24–48 (kernel sags to 0.88–0.89 at
64/128) while hybrid MRR peaks 64–96. Warm latency 4.6 / 8.5 / 19 / 27 ms at
pools 24/48/96/128. **Narrowed Loc-Bench candidates: 48 (best all-rounder) and
96 (max semantic direct).**

### 9.7 Loc-Bench A/B: the offline gains do not transfer (2026-07-28)

50 instances × {sg-plain, sg-mx48, sg-mx96, sg-sif(a=1e-4)+maxsim}, Sonnet, v4
description held fixed.

| finding | evidence |
|---|---|
| **MaxSim hurts agent-level accuracy, monotonically with pool depth** | fnAcc@10t: plain **62%** > mx48 59% > mx96 54%; fAcc@5: 77 > 71/70. Agents searched *more* under maxsim (201 vs 142 sg calls) — worse first results beget retries. |
| **All conditions tie on 2k–10k repos** | Every cell identical (83% fAcc@5 / 75% fnAcc); variants diverge only on <2k-file repos, where plain wins. |
| **SIF small-repo hypothesis: partially supported, not adoptable** | On <2k files, sif beats its maxsim base (+4pp fAcc@5, +7pp fnAcc vs mx96) but still trails plain (70 vs 75 fAcc@5). |
| **v4 description moved behavior as designed** | ranked-first 56% (vs pilot-v1's 38%); exact-mode calls down 125→87. But fnAcc read 62% vs pilot's 69% — at n≈47 that's ~3 instances; more ranked usage demonstrably ≠ better outcomes. |
| **Deltas are small** | 2–4 instances separate conditions; directions consistent, individually within noise. |

**Decisions: the plain engine stays the default; `--maxsim` does not
graduate** — offline semantic-list gains are a misleading proxy, since agents
issue identifier-shaped queries through hybrid where BM25 carries the ranking,
and MaxSim's reorderings swap in token-similar-but-wrong chunks. SIF remains
the documented prose/paraphrase option. The lesson repeats Augment's:
retrieval micro-benchmarks and agent outcomes diverge — **gate engine changes
on agent-level evals**, which this harness makes a ~$40 question.

### 9.8 MaxSim failure forensics (2026-07-28)

Reproduced the §9.7 Deltares bait case offline with real vectors
(`tests/tokprobe.rs`, kept as a regression test: if its assertions flip,
revisit `--maxsim`). Query `scalar_None function shortcut`; gold =
`def scalar_None(obj): return obj is None`; bait = `regridder_function:
Optional[str], if min is None and max is None:`.

```
query tok   → best in GOLD        → best in BAIT
scalar        scalar   1.000        str        0.210
_             _        1.000        _          1.000
none          none     1.000        none       1.000
function      (        0.115        function   1.000
shortcut      (        0.064        regridder  0.069
TOTAL         gold 3.179            bait 3.279   ← bait wins
```

Root causes, in causal order: (1) **the tokenizer shreds identifiers** — the
prose pre-tokenizer splits on punctuation, `scalar_None` → `[scalar, _,
none]`, so the highest-signal token in code never exists as a matchable unit
and `_` scores a perfect 1.000 against *any* chunk containing an underscore
(camelCase, inconsistently, is *not* split); (2) **concept words don't appear
in code** — the gold chunk IS a function but says `def`/`(`, so "function"
scores 1.000 against bait identifiers vs 0.115 against the punctuation that
expresses the concept; (3) **no contextual awareness (confirmed)** — "none" in
`scalar_None` and in `min is None` are the SAME vector; (4) **no term
importance** without SIF stats, and the SIF condition consistently recovered
about half the gap (fnAcc 54 → 59 vs plain 62); (5) **chunk vocabulary
saturation (partially confirmed)** — ~300 tokens from a tiny high-frequency
vocabulary give nearly every chunk a perfect match; not chunk *length* per se.

Net: in the agent setting MaxSim ≈ **BM25 minus idf plus punctuation noise** —
and hybrid already *has* BM25, with idf, without the noise. **Fix path if ever
revisited** (documented, not implemented): semgrep's code-aware BM25 tokenizer
(whole identifiers + subtokens, drops <2-char tokens) for the match units, and
idf-weighted query tokens; contextual embeddings are the full fix but are a
different embedder, not a rerank tweak.

### 9.9 The layer below: ese's embedding space is prose-shaped (2026-07-28)

Architecture said the static model was trained on prose (BERT wordpiece vocab
with `##` pieces, CLS/SEP, BERT's normalization pipeline); a direct probe
confirms it (`tests/modelprobe.rs`, kept as a regression test — if its
assertion flips under a new embedder, the semantic stack's role on code
changes):

```
prose synonym pairs          code concept pairs
delete ~ remove   0.540      def    ~ function  0.037
start  ~ begin    0.756      fn     ~ function  0.173
big    ~ large    0.584      none   ~ null      0.079
error  ~ mistake  0.428      str    ~ string   -0.002
fast   ~ quick    0.355      mutex  ~ lock      0.045
                             kmalloc~ allocate  0.091
                             regex  ~ pattern   0.012
```

The space encodes prose synonymy and knows **essentially nothing about
code-concept relations** — `str`~`string` at −0.002 is the headline: to a
prose model, "str" is an arbitrary letter sequence, not an abbreviation. The
one code pair that scores well (`bool`~`boolean` 0.560) works through shared
wordpiece *surface form*. OOV is not the problem (no probed identifier fell to
UNK); the *relations* are missing.

The failure stack is three layers deep, each independent: §9.8 the tokenizer
shreds identifiers (surface form); §9.8 static vectors carry no context
(structure); §9.9 the space lacks code-concept knowledge (training
distribution). Even perfect tokenization + contextualization can't bridge
"protect with a lock" → `mutex_lock(&...)` when mutex⊥lock in the space. That
explains the measured asymmetry: semantic *direct* on code works passably
(kernel 0.68) because query identifiers overlap chunk identifiers — **on code,
ese functions as a fuzzy lexical matcher, not a semantic model** — while
paraphrase (≤0.05) needs the missing bridges. The kernel-paraphrase wall (§3,
§9.4) is reframed as a training-data problem, not a query-machinery problem,
and every §9 query-time lever was bounded by it.

**The encouraging part:** a static model is just a lookup table, so the deep
fix is the cheap kind — re-distill from a code-aware teacher (model2vec-style),
keeping ese's architecture, speed and the whole stack unchanged (same
DIMENSIONS ⇒ emb.bin drop-in). Highest-leverage next experiment, gated per
§9.7 on the agent-level eval; then **PRF** → **SIF** → **MaxSim rerank** →
**drill-down/two-pass cold**.

Open items:

- [x] Decide A vs C (§4.3) — **C shipped** (2026-07-27): tuned hybrid by
      default, `-e/--exact` the grep escape hatch, tuning flags hidden
- [ ] Eval #1: Loc-Bench localization ablation (§7), rg-only vs rg+semgrep
- [ ] Eval #2: SWE-bench Verified subset via mini-swe-agent, for the
      tokens/steps-at-equal-resolve claim
- [ ] Draft the tool description + MCP schema and count its tokens (target
      < ~200; Grep spends ~500–600)
- [ ] Decide the auto-index story (§4.4)

---

## 10. Swapping the embedding table for a code-trained one (2026-07-28)

§9.9 called for re-distilling the table from a code-aware teacher. **We should
not distill it ourselves**: `minishlab/potion-code-16M-v2` already exists —
distilled from `nomic-ai/CodeRankEmbed` (the teacher this repo independently
selected), tokenlearn-fine-tuned on 1.2M CornStack (query, doc) pairs and
contrastive-fine-tuned on 1.2M more, and it adds **43k mined code tokens to the
tokenizer** (63.5k vocab vs 30.5k). ese is **WordPiece only**, which makes
CodeRankEmbed drop-in and rules out the BPE alternatives.

### 10.1 What the swap actually required

Little, but one trap: **marker vectors must be resolved by name, not by id** —
potion-code has `[UNK]` at id 1 and **no CLS/SEP at all**, so the hardcoded
`100/101/102` lookup would have added two arbitrary accented-character vectors
to every embedding, scaled by `1/token_count`.

**Correction to an earlier assumption in this doc**: a table swap is *not*
silently cache-unsafe when dims change — `load_dir`'s `meta.dims != EMBED_DIM`
guard catches it; only a future same-dims swap would be silent.

### 10.2 The probes: layer 3 is fixed

The probes are inverted assertions — they *fail* when the space changes, and
both now fail as designed. Prose → code table: `str`~`string` −0.002 →
**0.778**, `none`~`null` ~0 → **0.675**, `fn`~`function` ~0 → **0.498**,
`regex`~`pattern` ~0 → **0.454**, `mutex`~`lock` 0.045 → **0.367**,
`kmalloc`~`allocate` ~0 → 0.214, `def`~`function` 0.037 → 0.082, prose synonym
mean ~0.5 → 0.589 (held). Code-concept mean 0.438 vs prose 0.589: **the space
now encodes code relations without having lost prose synonymy**, and §9.8's
bait/gold MaxSim inversion flips (gold 3.310 > bait 3.307), by a hair. But
`identifiers_are_shredded_by_the_tokenizer` still **passes** — layer 1 is
untouched by a table swap, layer 2 unfixable by any static model.

### 10.3 Offline results (same query sets and conditions as §9.4 base)

recall@5:

| corpus | mode / kind | base | code table | Δ |
|---|---|---|---|---|
| VS Code | semantic direct | 0.570 | **0.740** | +0.170 |
| VS Code | semantic paraphrase | 0.010 | **0.125** | +0.115 (12.5×) |
| VS Code | hybrid direct (R@1) | 0.655 | **0.725** | +0.070 |
| VS Code | hybrid paraphrase | 0.145 | 0.145 | +0.000 |
| kernel | semantic direct | 0.678 | 0.719 | +0.041 |
| kernel | semantic paraphrase | 0.005 | 0.015 | +0.010 |
| kernel | hybrid direct (R@1) | 0.633 | **0.683** | +0.050 |
| kernel | hybrid paraphrase | 0.045 | 0.040 | −0.005 |
| wikipedia | semantic direct | 0.785 | 0.605 | **−0.180** |
| wikipedia | semantic paraphrase | 0.250 | 0.120 | **−0.130** |
| wikipedia | hybrid direct | 0.975 | 0.965 | −0.010 |

BM25 is unchanged everywhere, as it must be. **The shipped default improves on
both code corpora** (MRR@10 +0.038 / +0.033); **prose regresses, as a
specialized model should**, hybrid holding at −0.010 because BM25 carries prose;
**the kernel paraphrase wall stands**.

### 10.4 Why the kernel gains so much less than VS Code

**Training-language coverage**: CornStack is Python, Java, JavaScript, Go, PHP,
Ruby. VS Code is TypeScript/JavaScript, in distribution, +0.170; the kernel is
C, out of distribution, +0.041 — consistent with the language hypothesis, not
isolating it. So the wall narrows again: not "no code in the training data" but
"no C". The model also caps the hybrid path — on CoIR its own hybrid-with-BM25
row scores 43.36 avg vs 42.31 for BM25 alone.

### 10.5 Status

`sg-code` against `sg-plain`, same 50 instances, table the only variable.
**Read that result as a best case**: all 50 instances are Python (109 gold
files, every one `.py`) and Python is CornStack's first language, so a win is an
upper bound and a failure would be strong evidence against the table.

### 10.6 Loc-Bench A/B result: the offline gains did not transfer (again)

47 pairs after dropping 2 baseline `parse_error` rows and 1 `agent_error`.

| paired | n | zero-search | med searches | file Acc@5 | fn Acc@10t | med cost |
|---|---|---|---|---|---|---|
| sg-plain (prose table) | 47 | 8 (17%) | 1 | **79%** | **64%** | $0.20 |
| sg-code (code table) | 47 | 14 (30%) | 2 | 70% | 57% | $0.19 |
| — both actually searched — | | | | | | |
| sg-plain | 33 | 0 | 3 | **76%** | **58%** | $0.21 |
| sg-code | 33 | 0 | 3 | 67% | 48% | $0.24 |

**The code table did not win, and by the §9.7 gate it does not graduate** — but
it is **not a proven regression** either: the gap is 3–4 instances, discordant
pairs 4–0 and 3–0, exact two-sided p = 0.125 and 0.250, and across both metrics
and both subsets there is exactly **one** instance where the code table won and
the prose table lost. The zero-search jump is driver noise, decided before any
result returns; conditioning does not rescue it (−9pp file, −10pp function).
§10.3's wins sit in **pure semantic**, while hybrid moved only +0.010 R@5 /
+0.070 R@1 and the default is hybrid at `sem_weight 0.2`: **we fused away most
of what we bought.**

**Decisions:** do not adopt as default (opt-in, so nothing ships differently,
and the swap mechanism made this cost an afternoon); keep the §9.8/§9.9 probes
asserting the prose-model properties; **the next question is not a better table,
it is the fusion**; **§9.7's rule holds for a second lever** — offline gains
have twice failed to reach agent-level accuracy, so the gate stays.

### 10.7 Dimensionality vs model, separated (2026-07-29)

`sg-code` confounded a code-trained table *and* 256 dims. `sg-p256` (prose table
truncated to 256, same flags, same 50 instances) separates them.

| condition | binary | file Acc@5 | fn Acc@10t |
|---|---|---|---|
| prose@512 | 72.8 MB | 79% | 64% |
| prose@256 | **39.0 MB** | 74% | 62% |
| code@256 | 73.2 MB | 70% | 57% |
| *both-searched subset (n=32)* | | | |
| prose@512 | | 75% | 56% |
| prose@256 | | 72% | **56%** |
| code@256 | | 69% | 50% |

On the both-searched subset prose@256 matches prose@512 on function accuracy
exactly — **zero discordant instances** — and trails 3pp on files, one instance.
**§10.6's attribution was half wrong**: roughly half the code table's 79→70
deficit is dimensionality, not the model. Against ripgrep on the same 47
instances (prose@512 79%/64%, prose@256 74%/62%, rg 74%/57%, code@256 70%/57%),
prose@256 ties rg on files while keeping the function-level edge and code@256 is
the one variant that surrenders it; every contrast is non-significant
(p = 0.375–1.000).

**Shipped**: `Cargo.toml` pins `dim-256` (MRL prefix truncation). Binary 72.8 →
39.0 MB (−46%), kernel index 1.3 GB → 918 MB.

---

## 11. Function chunking (2026-07-29)

Splitting §10.7's 47 instances by whether the issue text *names* a gold
identifier:

| stratum | | file Acc@5 | fn Acc@10t |
|---|---|---|---|
| issue NAMES the identifier (n=21) | rg | 81% | 62% |
| | prose@256 | 81% | **71%** |
| issue does NOT (n=26) | rg | 69% | 54% |
| | prose@256 | 69% | 54% |

The function-level edge comes entirely from grep's *best* case, and the two are
identical where ranked retrieval was supposed to separate. Conditional on
finding the right file (both: 35/47), semgrep names the right function 83% vs
rg's 77%. **The advantage is "where in the file", not "which file"** — which
explains why MaxSim and the code table produced nothing: both improve which
chunks rank highest, and file-level was already tied.

### 11.1 Measurement: dilution, not truncation

Across 7 languages / ~52k functions (regex heuristics, ±few points):

| corpus | n | median | ≤10 lines | ≤32 lines |
|---|---|---|---|---|
| python (Loc-Bench repos) | 4,038 | 10 | 52% | 88% |
| c (kernel) | 16,936 | 12 | 45% | 89% |
| typescript | 8,671 | 7 | 64% | 86% |
| rust | 9,074 | 6 | 69% | 86% |
| ruby | 4,009 | 4 | 78% | 96% |
| java | 1,819 | 3 | 86% | 98% |
| **weighted** | **51,678** | | **59%** | **89%** |

A 32-line window rarely cuts a function in half (11%); it **swallows ~3 whole
functions**. The defect is dilution — a mean over unrelated functions, which no
better embedder can undo, and it is *above* the embedding in the pipeline.

### 11.2 Rule B: attaching leading doc without a parser

| corpus | rule A (walk to blank line) | rule B (comment-aware, cap 20, 1 gap) |
|---|---|---|
| | doc / **code wrongly pulled** | doc / **code wrongly pulled** |
| python | 20% / 3% | 20% / **0%** |
| c | 9% / **36%** | 11% / **0%** |
| typescript | 5% / **55%** | 6% / **0%** |
| rust | 25% / 24% | **44%** / **0%** |
| ruby | 35% / 7% | 37% / **0%** |
| java | 54% / 0% | **58%** / **0%** |

Rule A collapses on brace languages — in TS it drags in the previous method's
body 55% of the time. **Rule B pulls 0% code everywhere and captures more doc**,
costing a ~10-entry shared prefix table rather than a grammar. Overlapping
windows already capture a comment block above a function most of the time, so
naive function-node chunking would have been a **regression**.

### 11.3 Implementation and measured tradeoffs

`funcchunk.rs`: tree-sitter only for where a function starts, Rule B for doc,
size clamps, line-window fallback everywhere else. **Binary** (8 grammars):
+6.62 MiB, 39.0 → 45.9 MB — still 25.6 MiB below the original shipped binary,
because the dim-256 win pays for it.

Window → function, cold: django 0.49s → 0.82s (1.7×), 22,341 → 39,431 chunks
(+76%), 14.6 → 19.1 MB (+31%); litellm 1.68s → 2.66s (1.6×), 76,740 → 80,070
(+4%), 53.2 → 48.1 MB (−10%); vscode 2.48s → 2.97s (1.2×), 59,921 → 68,559
(+14%), 62.6 MB (0%); linux **45.9s → 64.0s (1.39×)**, 1,509,039 → 1,465,080
(−3%), **946 → 839 MB (−11%)**. Indexing costs 1.2–1.7×; index size mostly
*improves*, because function chunks carry no overlap and BM25 postings shrink
(kernel 541 → 445 MB) more than the extra embedding rows cost. Kernel RSS fell
0.78 → 0.68 GiB.

### 11.4 Result: no benefit, and the offline eval cannot referee it

**The offline eval is structurally biased here and must not be used.**
`eval/generate.py:63` defines ground truth as a sampled `WINDOW`-line span,
often covering 2–3 functions no single function chunk contains: **the eval's
ground truth *is* one of the strategies under test.** It duly reported window
ahead (vscode hybrid R@1 −0.050; kernel semantic R@5 −0.085).

Loc-Bench, whose ground truth is the real fix's functions, is neutral:

| n=47 paired | file Acc@5 | fn Acc@10t | fn-acc GIVEN file |
|---|---|---|---|
| prose@256 window (shipped) | **74%** | **62%** | **83%** (35/47) |
| prose@256 function chunks | 70% | 57% | 82% (33/47) |
| ripgrep | 74% | 57% | 77% (35/47) |

The conditional metric — 83% → 82% — was *the* prediction, and it is flat. Sign
tests: files 0–2 (p=0.500), functions 2–4 (p=0.688). **Decision: not adopted,
and removed from the tree** (2026-07-29) — 8 tree-sitter grammars and a second
chunking path are too much standing cost for an unproven idea. Revisit only with
an instrument that can resolve 3pp.

### 11.5 The instrument is the bottleneck (the important finding)

Four consecutive engine changes have landed inside the noise: MaxSim (p≈0.25),
the code table (p=0.125), dims (p=0.500), chunking (p=0.500–0.688). Across the
47 instances scored under all five conditions:

| | file Acc@5 | fn Acc@10t |
|---|---|---|
| every condition solves it | 68% | 49% |
| every condition misses it | 19% | 30% |
| **discriminative** | **13%** | **21%** |

**80–87% of Loc-Bench instances carry no signal about the search engine.**
Pairwise discordance is ψ = 0.067 (file) / 0.088 (function). Required n at
α=.05, 80% power: 7pp → 142 instances, 5pp → 277, 3pp → 769, 2pp → 1,729.
Loc-Bench V1 holds 560, so **3pp is unreachable on this benchmark at any price**.
Screening to discriminative instances does *not* add power — McNemar depends
only on discordant pairs — but cuts ~4.5× off the cost of obtaining them.

Planned instead of more agent spend: an **offline set** of ~2,000 queries per
corpus anchored to a **symbol span**, fixing the §11.4 bias and resolving ~3pp
instead of ~7pp; an **agent launch set** of the ~120 discriminative instances,
making future A/Bs ~$25 not $116 (headline accuracy still quoted from the full
sample); and **query replay**, which removes agent stochasticity at ~5× the
sample size, for free — before any further spend.

### 11.6 Cleanup, and a bug the dim-256 rollout exposed

**The bug**: shipping 256 dims made every pre-existing cache entry unreadable,
and the dims check surfaced that as an error on *every* search in a
previously-cached scope — contradicting §8's contract ("a cache that changes
only latency is memoization, and memoization doesn't need to be disclosed to the
caller").

**The fix is structural, not a check** — `dims` is a weak proxy, since a
*different* table of the same width passes it and then silently scores
yesterday's vectors against today's queries. Entries are namespaced by a
generation key (`v2-d256-0d2d/…`) covering format version, dims, and a 16-bit
fingerprint of the embedding stack, so it moves if the table, the tokenizer,
*or* the pooling changes; an incompatible binary's entry is never discovered.
**The failure mode is "not found", so there is nothing to surface.**

**Did this contaminate earlier results? No, and it was checked rather than
assumed** — no dims-mismatch error appears in any saved agent output, all 204
unexpected semgrep exits are exit 2, and every harness isolates its cache dir.
The lesson: *any* future change to the table, dims, or index format invalidates
every cached entry, and the cache must absorb that silently.

Agent-eval spend this session: $39.07 (sg-code $13.14, sg-p256 $13.50,
sg-fnchunk $12.43).

---

## 12. Adversarial audit of our own eval (2026-07-29)

The eval-v2 statistics reported semgrep beating ripgrep at p < 0.0001 on every
metric of every corpus, with discordance as lopsided as 173-0. A result that
clean, from a benchmark we wrote ourselves, is a reason to audit the benchmark
rather than celebrate.

### 12.1 The ripgrep baseline was a strawman

`rg_agent_style` had three compounding flaws: a tokenizer with **no
underscore**, shredding `blkg_rwstat_add` before ripgrep saw it; "rarest"
approximated by **longest**, which picks `function` and `choosing` over the
identifier; and the two terms then required **on the same line**. Net: on the
queries where the answer's own name appears in the question, our baseline never
grepped for it — and that is 66% of kernel `direct` queries, 70% of VS Code's,
against 2% of either paraphrase set and 1% of wikipedia.

(An earlier version of this measurement counted words like `workaround` as
identifiers and reported 93% for paraphrase — wrong, and corrected here. The
audit needed auditing.)

### 12.2 What a fair baseline costs us

`rg-strong` tries identifier-shaped tokens first, then the phrase, then the
AND/OR fallbacks, added **beside** the legacy condition so the delta stays
auditable.

| corpus / kind | rg (legacy) | rg-strong | semgrep | fair gap |
|---|---|---|---|---|
| kernel, direct R@5 | 0.05 | **0.32** | 0.92 | **2.9×** |
| VS Code, direct R@5 | 0.155 | **0.355** | 0.870 | **2.4×** |
| kernel, paraphrase R@5 | 0.000 | 0.000 | 0.027 | — |
| VS Code, paraphrase R@5 | 0.010 | 0.005 | 0.140 | 28× |

**The published "30× gap on identifier queries" is really ~2.9×.** Fixing the
tokenizer alone improves ripgrep 6.4× on the kernel and 2.3× on VS Code. semgrep
still wins every stratum at p < 0.0001 (kernel direct 45-0), but the magnitude
of the claim was substantially our own baseline.

Paraphrase is untouched by the fix — there is no identifier to grep for, which
is the definition of the stratum. That asymmetry is "the strongest evidence
available that the remaining advantage is a real capability difference and not
an artifact: improving the opponent closes the gap exactly where theory says it
should, and nowhere else."

### 12.3 Real queries: our conditions bracket reality, they do not represent it

CoSQA: 9,020 human-written Bing queries labelled against 20,604 Python
functions, the whole corpus indexed so retrieval faces real distractors. Nobody
who wrote them had seen the code.

| query set | n | has identifier | median words | tokens present in gold |
|---|---|---|---|---|
| ours, direct | 199 | 66% | 10 | — |
| ours, paraphrase | 199 | 2% | 17 | — |
| **CoSQA (real)** | **9,020** | **0%** | **6** | **42%** |

`direct` is *easier* than reality (the name is handed over) and `paraphrase` is
*harder* (vocabulary stripped, 17 words where users type 6). **Every quality
claim this project has made was anchored to one of those two poles; neither is
where users are.**

1,200 sampled real queries:

| condition | R@1 | R@5 | R@10 | MRR@10 |
|---|---|---|---|---|
| **bm25** | 0.07 | **0.22** | 0.33 | **0.138** |
| hybrid (shipped) | 0.07 | 0.21 | 0.33 | 0.133 |
| semantic | 0.02 | 0.08 | 0.12 | 0.048 |
| rg / rg-strong | 0.01 | 0.03 | 0.04 | 0.013 |

**The fair baseline changes nothing here** — 0% of real queries contain an
identifier. **semgrep's real-query advantage is larger than on synthetic direct
queries**: 8.3× at R@5, 237-17 discordant, p < 0.0001, so the strawman
*understated* the tool in the regime that matters while overstating it on
identifier queries. **The semantic half contributes nothing**: BM25 alone (0.22)
matches hybrid (0.21) and nearly triples semantic (0.08); the win is code-aware
*lexical* ranking, not embeddings. Caveat the other way: CoSQA labels one gold
function among 20,604, so single-truth scoring makes 0.21 a floor.

### 12.4 A cost asymmetry the accuracy tables hide

`rg-strong` is expensive *because* it is fair: a paraphrase query exhausts all
five patterns, each a full 1.15 GB scan, so a kernel query ripgrep ultimately
fails to answer costs **~8 full scans, ~25 s** against semgrep's single ~100 ms
warm query. Loc-Bench could not show this because agent cost there is 91%
conversation-replay cache reads. A competent grep strategy means *more* scans on
failure, not fewer.

### 12.5 Decisions

- **Revise the README.** "3-27%" for ripgrep understates it (32-36% on direct
  with a fair baseline) and the 30× framing must go: modest advantage when the
  identifier is known, large advantage when it is not.
- **Keep both baselines**, `rg` for comparability and `rg-strong` as the honest
  opponent. Report both.
- **CoSQA becomes a standing corpus**, and the first-class one for quality
  claims, because it is the only query set not written by us.
- **Regenerate our own sets symbol-anchored** and consider retiring `direct`
  entirely — a query containing the answer's name measures tokenizer plumbing,
  not retrieval.

---

## 13. A fourth corner, and a ceiling for ripgrep (2026-07-30)

§12 left two items open — replay the queries agents actually issued, and find
out how much of the remaining gap is still the baseline. This section does both,
and measures a third leak §12 missed.

### 13.1 Path leakage: the generator was shown the answer's filename

`eval/generate.py` put the file path into the prompt and semgrep's tokenizer
does path augmentation, so the generator saw the document identifier the scorer
indexes. The clean measure is a path segment the query carries that the gold
*text* does not: 16.1% of linux `direct`, **17.1% of linux `paraphrase`**, 12.0%
of both vscode sets, 0.0% on wikipedia.

**The paraphrase row is the finding.** §12.3 treated it as the clean pole, but it
leaks *higher* than `direct`: told to avoid the chunk's identifiers, the
generator reached for the one piece of the answer it was still allowed to see.
**Neither pole is clean.** The prompt no longer passes `{path}`, and
`run_eval.py` now prints leakage above every results table.

### 13.2 Query replay: what agents actually type

497 unique ranked queries and 726 exact ones, harvested from 706 shim logs
across 42 instances. Four defects were fixed first, two of which would have
produced a *quotable wrong number* rather than an error: the bootstrap **ignored
clustering** (one instance contributed 55 of 497; it now resamples instances)
and `harvest()` **mixed regexes with queries**, 390 of 887 rows measuring how
BM25 tokenizes punctuation.

Rank of the first gold file, k=10: hybrid MRR **0.362** (hit@5 0.493), bm25
0.330 (0.473), semantic 0.306 (0.461).

| pair | MRR delta | clustered 95% CI | naive 95% CI | verdict |
|---|---|---|---|---|
| hybrid − bm25 | +0.0315 | [+0.0012, +0.0630] | [+0.0134, +0.0501] | WIN, barely |
| hybrid − semantic | +0.0563 | [+0.0079, +0.1065] | [+0.0264, +0.0887] | WIN |
| bm25 − semantic | +0.0248 | [−0.0277, +0.0804] | [−0.0105, +0.0602] | inconclusive |

**The clustering correction is not cosmetic**: a clustered lower bound of
**+0.0012** against a naive +0.0134 is the difference between a solid win and a
coin-flip from inconclusive. Every replay number here is clustered.

**This contradicts §12.3's conclusion on CoSQA, and the contradiction is the
interesting part.** There the semantic half contributed nothing; here hybrid
beats bm25, because the query distribution differs. "The lesson is that 'does
the semantic half earn its keep' has no corpus-independent answer," and §12.3 is
scoped to human prose queries.

### 13.3 The query distribution, which is a fourth corner and not a fix

| set | n | identifier% | median words |
|---|---|---|---|
| ours, direct | 199 | 66% | 10 |
| ours, paraphrase | 199 | 2% | 17 |
| CoSQA (real humans) | 9,020 | 0% | 6 |
| **agent replay, ranked** | **497** | **47%** | **4** |
| **agent replay, exact (`-e`)** | **726** | **63%** | **1** |

Replay is where this product's actual input lives, CoSQA is where human users
live, and our two generated poles are neither.

### 13.4 `rg-oracle`: a ceiling for ripgrep (prediction, pre-registered)

`rg-strong` is still a hand-tuned query planner, so §12 left open whether the
*rest* of the gap is the engine or the planning. `rg-oracle` removes the
planning: try every content token as its own pattern, keep whichever scored
best — which requires already knowing the answer, so **no agent can run it**. A
ceiling, reported as one, replacing neither `rg` nor `rg-strong`.

**Prediction, recorded before the run so it can be falsified:** CoSQA R@5 0.03 →
**0.06–0.10**, under bm25's 0.22; kernel `direct` R@5 0.32 → **0.60–0.80**
against semgrep's 0.92; kernel `paraphrase` R@5 **≈0**. **Falsification
condition:** if kernel `direct` R@5 reaches **≥0.85**, the identifier-query
claim is baseline-shaped rather than engine-shaped and §12.2's correction did
not go far enough — that would need retracting, not explaining.

### 13.5 `rg-oracle`: the result, and two ways the ceiling was wrong first

Four new corpora (rust/java/go/ruby, symbol-anchored ground truth, §13.6).

**R@5, `direct`:**

| corpus | rg | rg-strong | **rg-oracle** | semantic | bm25 | hybrid |
|---|---|---|---|---|---|---|
| jekyll | 0.034 | 0.057 | **0.205** | 0.636 | 0.864 | 0.886 |
| tokio | 0.065 | 0.085 | **0.190** | 0.420 | 0.710 | 0.700 |
| commons-lang | 0.070 | 0.106 | **0.236** | 0.492 | 0.849 | 0.864 |
| etcd | 0.090 | 0.090 | **0.165** | 0.340 | 0.705 | 0.695 |

**R@5, `paraphrase`:**

| corpus | rg-strong | **rg-oracle** | bm25 | hybrid |
|---|---|---|---|---|
| jekyll | 0.000 | **0.068** | 0.136 | 0.182 |
| tokio | 0.010 | **0.050** | 0.090 | 0.085 |
| commons-lang | 0.015 | **0.035** | 0.146 | 0.171 |
| etcd | 0.000 | **0.030** | 0.065 | 0.065 |

**The margin survives the ceiling.** rg-oracle is 1.8–3.6× rg-strong, so §12.2
did not go far enough as a bound — but semgrep is **3.7–4.3× above the oracle**
on `direct`, against a ripgrep allowed to consult the answer before choosing its
pattern. The gap is engine-shaped, not baseline-shaped, and the falsification
condition is not met anywhere here (highest oracle 0.236).

**The kernel and CoSQA oracle runs have NOT been done** — both interrupted before
producing output, so the §13.4 predictions for the two corpora with the sharpest
predictions stand untested. That is the gap in this section, not a footnote.

**bm25 ≥ hybrid on three of four corpora**, agreeing with §12.3 and §9.9's "on
code, ese functions as a fuzzy lexical matcher, not a semantic model" — and not
contradicting §13.2, a different distribution. **The paraphrase wall stands**:
0.065–0.182 R@5 for hybrid, four corpora, four languages.

#### The ceiling was not a ceiling, twice

**First: a single-token vocabulary cannot bound a conjunctive one.** `rg_strong`
also tries `A.*B`, strictly *more* selective than either token alone; across
1,374 real queries the "upper bound" lost to the thing it was bounding on **53
of them (3.9%)**. The fixture test asserting `rank(oracle) <= rank(rg_strong)`
passed throughout; a property test over real corpora broke it immediately.

**Second: ripgrep's output order was not deterministic.** Six runs of one
pattern over etcd produced **two distinct top-10 orderings**. So:

> Every `rg` and `rg-strong` number this harness has produced — including
> §12.2's fair-baseline table — carried run-to-run variance from thread
> scheduling, and no rg result was exactly reproducible.

Spread on rg-strong R@5 over 150 etcd queries: **0.0067 across three runs**
unsorted, **0.0000** with `--sort path` — small enough to overturn no published
conclusion, large enough to matter against §11.5's 3pp target. `--sort path`
makes ripgrep walk single-threaded (kernel, best of three: 1.76 s → 6.50 s,
1.92 s → 8.36 s, 1.92 s → 8.46 s, i.e. 3.7–4.4×), which puts a 150-query kernel
oracle run on the order of **hours**.

### 13.6 Four more corpora, and what they were for

tokio (rust, 790 files, 6.0 MB, 7,728 symbols, 59% has_doc, 400 queries),
commons-lang (java, 625, 10.3 MB, 4,985, 88%, 398), etcd (go, 1,110, 15.4 MB,
9,211, 20%, 400), jekyll (ruby, 166, 3.3 MB, 1,068, 45%, 176) — all in the
<2k-file band where §9.7 found engine variants diverge, with a deliberate
`has_doc` spread because a stratum needs variance. Ground truth is
symbol-anchored, so they can referee a chunking change without §11.4's
circularity.

`extract()` over 22,992 real symbols found one defect the invariant checks did
not: `def self.foo` extracted the name `self` (29 of jekyll's 1,060 ruby
symbols). Spans were correct, so ground truth was unaffected, and the invariant
checks returned **0 violations across all four corpora**. It surfaced from a
test that asserted what the name should *be*.

### 13.7 Reproducing §12.2 against a deterministic ripgrep

§13.5 made §12.2's fair-baseline table a claim nobody could check, so it was
re-measured on the same query sets with the ordering fixed.

| cell | column | §12.2 | rerun | Δ |
|---|---|---|---|---|
| kernel, direct R@5 | rg | 0.050 | 0.025 | −0.025 |
| | rg-strong | 0.320 | 0.342 | +0.022 |
| | semgrep (hybrid) | 0.920 | 0.899 | −0.021 |
| | *fair gap* | *2.9×* | ***2.6×*** | |
| VS Code, direct R@5 | rg | 0.155 | 0.155 | **0.000** |
| | rg-strong | 0.355 | 0.360 | +0.005 |
| | semgrep (hybrid) | 0.870 | 0.870 | **0.000** |
| | *fair gap* | *2.5×* | ***2.4×*** | |
| kernel, paraphrase R@5 | rg / rg-strong | 0.000 | 0.000 | 0.000 |
| | semgrep (hybrid) | 0.027 | 0.040 | +0.013 |
| VS Code, paraphrase R@5 | rg | 0.010 | 0.010 | **0.000** |
| | rg-strong | 0.005 | 0.010 | +0.005 |
| | semgrep (hybrid) | 0.140 | 0.140 | **0.000** |

**The conclusion holds.** The fair gap is 2.6× on the kernel and 2.4× on VS Code
against §12.2's 2.9× and 2.5×. "The published 30× is really ~3×" survives; the
third digit does not, and never could have.

**VS Code reproduces to ±0.005** — one query in 200 — in all four cells, with
both deterministic columns landing on 0.000: the strongest available evidence
that the harness itself is now reproducible.

**The kernel does not, and part of it is unexplained.** The rg columns move
±0.025, the right order for scheduling noise at 84k files, but the *semgrep*
column also moves — −0.021 direct, +0.013 paraphrase — and those modes are
deterministic. **Index staleness: ruled out** — rebuilding the stale kernel index
and rescoring reproduced **all 1,194 ranks exactly**. **Engine drift: open** — P6
was A/B'd as retrieval-neutral on **vscode** (400 queries × 3 modes, all 21
metrics ±0.000), precisely the corpus that reproduces here; the kernel was never
in that A/B, so kernel-only drift is consistent with everything observed without
being established. The kernel rows of §12.2 should be read as ±0.02, not to
three decimals.

### 13.8 The ceiling on real human queries (CoSQA)

Predicted **0.06–0.10 R@5, staying under bm25's 0.22**. Over all 1,200 queries:

| mode | R@1 | R@5 | R@10 | MRR@10 |
|---|---|---|---|---|
| rg (legacy) | 0.012 | 0.030 | 0.051 | 0.021 |
| rg-strong | 0.012 | 0.030 | 0.051 | 0.021 |
| **rg-oracle** (ceiling) | 0.043 | **0.101** | 0.158 | 0.069 |
| semantic | 0.022 | 0.083 | 0.122 | 0.048 |
| hybrid (shipped) | 0.068 | 0.208 | 0.330 | 0.133 |
| bm25 | 0.074 | **0.222** | 0.325 | 0.138 |

**The prediction holds, and it landed one thousandth above the band.** "Calling
that a hit would be generous; calling it a miss would be pedantic."
**§12.3's semgrep numbers reproduce exactly** — bm25 MRR 0.138, hybrid 0.133,
semantic 0.048 — while the rg columns moved (MRR 0.013 → 0.021), the §13.5
nondeterminism showing up exactly where it should and nowhere else.

**The finding: §12.3's real-query claim has the same shape §12.1 found in the
kernel claim.** The advantage reported as **8.3×** at R@5 is **2.2×** against the
ceiling (0.222 vs 0.101), itself 3.4× the `rg-strong` heuristic — most of that
8.3× was, once again, query planning rather than retrieval. **The direction of
the claim survives both corrections. The magnitude has now been wrong twice, in
the same direction, for the same reason.** Quote future gaps against the
ceiling, not against a heuristic we wrote.

**A ripgrep that reads the answer beats our semantic mode** — 0.101 vs 0.083
R@5, 0.069 vs 0.048 MRR. What survives: **bm25 at 0.222 is still 2.2× the
ceiling**, on the one query set nobody on this project wrote.

### 13.9 The kernel ceiling: the falsification test resolves

199 `direct` and 199 `paraphrase` queries over the kernel:

| condition | direct R@5 | paraphrase R@5 |
|---|---|---|
| rg (legacy) | 0.025 | 0.000 |
| rg-strong | 0.342 | 0.000 |
| **rg-oracle** (ceiling) | **0.462** | **0.000** |
| hybrid | 0.899 | 0.040 |
| bm25 | 0.920 | 0.035 |

**The claim survives.** The retraction condition was oracle `direct` R@5 ≥ 0.85;
measured **0.462**. **The prediction was also wrong, and low** — 0.60–0.80
predicted, in the direction of having *overestimated* ripgrep, because picking
the right token is not the hard part at this scale: plenty of kernel identifiers
appear in hundreds of files, and ripgrep returns those in path order with no way
to rank the gold one up. Recorded as a miss rather than reframed.

**The kernel is where `rg-strong` was already nearly optimal**: the ceiling is
only **1.4×** the heuristic here against **3.4×** on CoSQA. When the query
contains a rare identifier, "grep the longest identifier" is close to the best
available strategy; when it is ordinary English, token choice matters much more.
**Against the ceiling, the fair gap is 2.0×** (0.920 vs 0.462), against 2.7×
versus `rg-strong` and §12.2's published 2.9×. The direction never moves.

#### The paraphrase result is the strongest evidence in this document

`rg-oracle` scores **exactly 0.000** on all 199 paraphrase queries. Not 0.005.
Zero. A ripgrep allowed to inspect the answer, try every content token, and keep
whichever scores best cannot locate a single one of 199 targets once the query
stops naming them. semgrep finds 4%. §12.2's "improving the opponent closes the
gap exactly where theory says it should, and nowhere else" was argued against a
*heuristic* opponent; it now holds against a *perfect* one, the strongest form
the argument can take. The corollary: semgrep's own paraphrase number is 0.04 —
the capability difference is real, and it is a difference between 4% and 0%, not
between good and bad. §9.4's wall stands.

### 13.10 MaxSim reranking as a default: no

Re-tested because the §9 numbers that recommended MaxSim were produced under the
contaminated cache, before rg determinism, and before the NaN poisoning the
reranked head was found — a bug "reachable only via `--maxsim`, which is why no
eval run caught it."

**14 paired comparisons, 3,071 queries, 0 wins, 1 loss.**

| set | n | conditions | result |
|---|---|---|---|
| CoSQA (real humans) | 1,200 | pool 32 / pool 96 / blend 0.5 | all **inconclusive**, +0.001 to +0.003 |
| replay (real agents) | 497 | mx48 / mx96, clustered CI | all **inconclusive** |
| tokio/commons-lang/etcd/jekyll | 1,374 | `--maxsim` | 7 inconclusive, **1 LOSS** |

The loss: jekyll `paraphrase` R@5 0.182 → 0.136, delta −0.045, CI
[−0.091, −0.011], 0-4 discordant.

**"Inconclusive" here is the well-powered kind, which is the useful part.** On
CoSQA at n=1,200 the 95% CI on the R@5 delta is about ±0.007 — not "we could not
tell" but **any effect is smaller than roughly one point**, either direction.
The same question at n=88 (jekyll) genuinely cannot tell, and is reported
separately rather than averaged in.

**The direct-query trend is negative and consistent**: all four code corpora move
down (−0.005, −0.010, −0.011, −0.020), pooled −0.0116, CI [−0.0262, +0.0029],
p=0.17 — inconclusive, but 4/4 with the same sign is not the shape of a change
about to pay off. **It is not free**: warm latency, three queries averaged,
jekyll 8.2 → **12.6 ms**, etcd 8.2 → **12.2 ms**, linux 91.5 → 78.8 ms (the
kernel row is almost certainly noise at n=3).

**And §9.7 stands unrefuted**: at the *agent* level MaxSim was actively harmful —
fnAcc@10t plain 62% > mx48 59% > mx96 54%, with agents searching *more* under
maxsim (201 vs 142 calls) because worse first results beget retries. Replay
removes the agent's decisions, which is exactly the mechanism §9.7 blamed, and
inconclusive does not overturn a measured harm.

**Verdict: not a candidate for the default build** — no win on either real-query
set, one loss, a negative trend on code, a latency cost, and a standing
agent-level finding against it. §9.4's "adopt but re-wire" is superseded; it
stays available behind `--maxsim`.

#### Root cause: MaxSim works, on the channel that does not matter

**MaxSim reranks the *semantic* candidate list, before RRF fusion**, deliberately
— `maxsim.rs:28` records that post-fusion reranking "let MaxSim override BM25's
exact-match signal instead of being fused with it, which measurably hurt hybrid
on code (§9.4)." So the question is not "does the reranker work" but "does the
list it reranks decide the answer":

| corpus / mode | base R@5 | +maxsim | delta | verdict |
|---|---|---|---|---|
| etcd / **semantic** | 0.340 | **0.420** | **+0.080** | **WIN** (CI [+0.010,+0.155], p=0.040) |
| jekyll / **semantic** | 0.636 | **0.716** | **+0.080** | inconclusive (n=88) |
| etcd / hybrid | 0.695 | 0.675 | −0.020 | inconclusive |
| jekyll / hybrid | 0.886 | 0.875 | −0.011 | inconclusive |

**The reranker is not broken. It is a real +8pp on the list it touches** — 24%
relative on etcd, half the queries moving (97/200) — while on shipped hybrid 97%
of queries come back completely unchanged (1,335/1,374). Every link of the chain
is separately measured: ese's static vectors act on code as a fuzzy lexical
matcher (§9.9), so the semantic channel contributes almost nothing to the fused
result (§12.3, §13.8), so improving that list by +8pp leaves the fused output
where it was.

**Which means the honest verdict is narrower than §13.10's.** The theory is
sound; it cannot earn default status because **the bottleneck is upstream of
it.** "Reranking a weak signal more cleverly does not make it a strong one." So
**`--maxsim` should be the default for `--mode semantic`**, where it is a
measured win and is not today, and **the lever that would make MaxSim matter is
a better code embedding**, not a better rerank.

One prediction got missed: with **no length normalization** (and no IDF without
SIF stats) MaxSim should favour longer chunks, but over 600 top-5 hits mean
chunk length is 30.8 base vs 30.9 with maxsim, ratio 1.00 — fixed 32-line
windows leave no length variance to act on.

### 13.11 Post-fusion reranking, re-tested

`maxsim.rs`'s justification since §9.5 — post-fusion reranking "measurably hurt
hybrid on code" — rested on a §9.4 measurement with three known problems (the
contaminated cache, pre-determinism ripgrep, and **before the NaN fix**), and
tested one configuration: `blend_head`'s alpha at the default
1.0, a full override. `--maxsim-post` now implements a partial blend, in warm and
cold paths; swept over four corpora, 1,374 queries, paired against hybrid:

| blend | direct R@5 | Δ | paraphrase R@5 | Δ |
|---|---|---|---|---|
| base (no rerank) | 0.770 | — | 0.116 | — |
| 1.00 (pure MaxSim) | 0.514 | **−0.256** | 0.049 | **−0.067** |
| 0.50 | 0.719 | **−0.051** | 0.105 | −0.012 |
| 0.25 | 0.769 | −0.002 | 0.102 | **−0.015** |

**§9.4's verdict is confirmed, now on a measurement that can be trusted, and with
a mechanism.** The loss is monotone in alpha and there is no blend where it wins;
at 0.25 it reaches "indistinguishable from doing nothing" on direct queries by
turning itself almost off, while still losing on paraphrase.

That shape is §13.10 from the other side. MaxSim's per-token similarity over
static embeddings is a *weaker ranking signal than BM25 fused with RRF*:
pre-fusion it improves the semantic branch (+0.08 R@5) because that branch is
weaker still; post-fusion it is asked to improve on the strongest list the engine
produces, and it cannot. "The lever is not where it is applied; it is the quality
of the signal being applied."

#### The bug this experiment produced, and what caught it

The first run reported hybrid R@5 collapsing 0.770 → **0.058** — not a bad result
but a broken one, and the giveaway was the *shape*: **blend 0.3 scored worse than
blend 1.0**, impossible unless the order being preserved is upside down. It was:
`fuse` emits **higher-is-better** scores, `blend_head` **lower-is-better**
pseudo-distances, and pre-fusion this never mattered because `fuse` reads only
rank *position*. Fixed by converting at both ends, guarded by
`post_fusion_rerank_at_zero_blend_is_the_identity` and
`cold_and_warm_agree_under_post_fusion_reranking`. `--maxsim-post` is kept,
hidden and off.

---

## 14. Semantic-first (2026-08-01)

### 14.1 The decision

**Semantic search is the product. The success criterion is semantic beating
lexical, on real queries, measured against the rg-oracle ceiling. Hybrid is off
by default until semantic carries its own weight; it returns when fusing it
back in is adding a strong signal to a stronger one, not hiding a weak one
behind BM25.** (Maintainer decision, recorded 2026-08-01.)

Default mode becomes `semantic` — CLI, `SearchOptions::default()`, and the
exact-miss suggestion path. `hybrid` stays a mode flag, tuned as §9.5 left it:
benched, not unbuilt.

**What this costs today**, on CoSQA (§13.8, 1,200 real Bing queries, the only
set this project didn't write):

| mode | R@5 | MRR@10 |
|---|---|---|
| semantic (new default) | 0.083 | 0.048 |
| hybrid (old default) | 0.208 | 0.133 |
| bm25 (the bar) | 0.222 | 0.138 |

The default gets 2.5× worse on real queries, today. Taken anyway because
(1) §13.10 measured 97% of hybrid queries unchanged when the semantic branch is
reranked, so every published hybrid number was to first order a BM25 number;
(2) §13.9's rg-oracle scores exactly 0.000 on 199 paraphrase queries where
semantic scores 0.04 — "the capability is real and tiny, and it stays tiny
while the default hides it"; (3) both live failure layers have known fixes
(§14.2, §9.9) and "neither gets built while the fused default makes them look
optional."

Falsifiable exit condition: **semantic beats bm25 on CoSQA R@5** (0.083 vs
0.222), then re-decide the default with §9.5's sweep. Side effect: MaxSim
rerank is on by default in semantic mode (+0.080 R@5 on etcd, CI [+0.010,
+0.155], §13.10), so warm default latency moves from ~115 ms to ~53 ms plus the
rerank head.

### 14.2 The hypothesis: the embedder is shown the wrong text

`doc_text()` = relative path, newline, raw chunk slice, through ese's prose
pipeline. §9.8 at the token level: `scalar_None` → `[scalar, _, none]`, so the
highest-signal unit is never a matchable token; punctuation is first-class (`_`
matches `_` at cosine 1.000, pure noise mass under mean pooling); camelCase
inconsistently does *not* split (`computeBackoffDelay` stays one OOV-ish blob).

The hypothesis: identifier words, path words and comment prose carry nearly all
of a chunk's signal for a prose model; operators, values and syntax carry
little and *detract* under uniform mean pooling, since every token gets an
equal share of the average. So render the chunk into the prose the model was
trained on — `get_user_name` → `get user name` — and render the query
identically. Layer-1 fix (§9.9's taxonomy): it cannot create relations the
space lacks — no rendering makes `mutex` near `lock` — so it should move
direct/real-query scores, not the paraphrase wall. No tree-sitter needed;
`text/token.rs` already splits both cases and drops punctuation.

The lever: `--embed-preproc <variant>` at index time, persisted in `meta.json`
as `sif` is, applied identically at build, at search, and on the cold streaming
path (cold == warm must survive it). BM25 and keyword untouched.

### 14.3 Pre-registration (written before the first run)

| tag | render |
|---|---|
| `none` | today's raw `doc_text` (control) |
| `split` | code-aware tokens, subtokens only: `getUserName` → `get user name` |
| `split-whole` | subtokens + whole identifier: `… get user name getusername` |
| `split-nokw` | `split` minus language keywords and pure-number tokens |
| `split-sif` | `split` with `--sif` pooling |

CoSQA (primary), linux + vscode (direct/paraphrase strata), tokio + etcd (the
<2k-file band where §9.7 found variants diverge). Mode `semantic`; bm25 rerun
per corpus as bar and tripwire. `eval/run_eval.py`, R@5 primary, MRR@10
secondary, paired bootstrap CIs, sign tests.

1. **CoSQA semantic R@5: 0.083 → 0.11–0.16 for `split`** (below 0.10 the
   hypothesis is substantially wrong; above 0.16 I underrated surface noise).
2. **`split` does not reach bm25 (0.222) on CoSQA.**
3. **Kernel/vscode `direct` improves under `split`.**
4. **Kernel `paraphrase` stays ≤ 0.08** (currently ~0.04).
5. **`split-nokw` ≥ `split` on CoSQA; `split-sif` ≈ best overall.**
6. **bm25 identical to three decimals across conditions** (tripwire).

### 14.4 Results (2026-08-01, same day; eval/preproc.sh, 2,798 queries × 5–6 conditions)

Semantic as shipped (MaxSim on). Baseline correction: §14.1's 0.083 came from
§13.8, which predates MaxSim becoming the default — the shipped baseline is
**0.108**, and deltas are against that.

| CoSQA condition (1,200 real queries) | R@5 | Δ vs none | 95% CI | sign test |
|---|---|---|---|---|
| none | 0.108 | — | — | — |
| split | 0.116 | +0.007 | [−0.006, +0.022] | p=0.33 |
| split-whole | 0.110 | +0.002 | [−0.012, +0.016] | p=0.90 |
| split-nokw | 0.117 | +0.008 | [−0.006, +0.023] | p=0.29 |
| sif (control, added post-hoc) | 0.170 | +0.062 | [+0.043, +0.081] | 109w/35l, p≈0 |
| **split-sif** | **0.188** | **+0.080** | [+0.060, +0.099] | 133w/37l, p≈0 |
| bm25 (the bar) | 0.222 | | | |

MRR@10 0.078 → 0.125 (CI [+0.033, +0.060]); `split-sif` beats `sif` alone by
+0.018 (CI [+0.001, +0.037], p=0.045) — both components are real and they
compose. **The shipped semantic mode now recovers 85% of bm25's R@5 on real
queries, from 49% at §13.8.** Gap 1.18×, down from 2.7×.

| split-sif vs none | lang/case | direct Δ | paraphrase Δ |
|---|---|---|---|
| vscode | TS, camelCase | 0.710 → 0.825 (+0.115, p≈0) | 0.030 → 0.090 (+0.060, p=0.012) |
| etcd | Go, camelCase | 0.420 → 0.595 (+0.175, p≈0) | −0.015 (n.s.) |
| tokio | Rust, snake_case | +0.015 (n.s.) | +0.005 (n.s.) |
| linux | C, snake_case | −0.005 (n.s.) | 0.010 → 0.035 (+0.025, 5w/0l, p=0.06) |

On vscode `split` alone is +0.075 direct (p=0.006) and `split-whole` +0.115
(p<1e-4); on CoSQA and the kernel `split` alone is noise.

**The mechanism, two facets.** Rendering fixes the *units* and pays where ese's
tokenizer couldn't produce them (camelCase: TS +0.115, Go +0.175), ~nothing
where it already splits on `_`. SIF fixes the *weights* and pays where units
were fine but boilerplate drowned them (+0.062 on real Python queries). Each
lever is null exactly where the other's problem dominates, which is why no
single-lever condition showed this — and why **SIF's 2026-07-28 rejection was
an artifact of synthetic queries.**

**Scorecard (§14.3):** (1) **miss** — 0.116 is inside the band but noise
against the true baseline. (2) **hit**. (3) **half-hit** — decisive on
camelCase, null on snake_case; the prediction failed to condition on the
corpus's identifier convention. (4) **hit** (0.035 at best), and at 0.035
semantic ties bm25 on kernel paraphrase: the wall holds, but semantic no longer
trails lexical behind it. (5) **hit** (0.117 vs 0.116, both n.s.) for the wrong
reason — SIF didn't subsume the stoplist, it carried the condition. (6) **miss
as stated**: CoSQA bm25 read 0.219 under `split` (Δ −0.003, CI [−0.013,
+0.007], n.s.), because bm25 output passes through MMR, which reads the
rendered matrix. "A tripwire that fires on a coupling you forgot is doing its
job."

### 14.5 What graduates, and what gates it

`--embed-preproc split --sif` is the recommended index configuration for the
semantic-first campaign (CoSQA 0.108 → 0.188 against bm25's 0.222) — not the
default build, since offline gains failed to transfer twice (§9.7, §10.6) and
engine defaults move on agent-level evidence.

**Verdict (2026-08-03): the config does not graduate. It stays opt-in.**

*The gate was not runnable.* `replay.py` builds one index per worktree and
distinguishes conditions by query-time flags, so it can never compare two index
*builds*; and `--embed-preproc` was missing from its `INDEX_FLAGS` guard, so a
`split` condition would have passed and done nothing. Both fixed.

*The instrument the gate meant had already answered.* §16.5's `guessplay.py`
run: champion − default = **−0.008, n.s.**; rechecked on rows free of the
§16.11 bug, **+0.002, CI [−0.006, +0.009]**, semantic exactly 90 wins to 90
losses (§17.2). §15.8 corroborates (champion ≤ base in 3 of 6 corpora), and
§15.9-B gives the mechanism — gold cosine **0.325 raw → 0.111 under SIF**,
dropping a #1 hit past rank 40. Frequency weighting inverts on token-poor,
identifier-heavy queries, which §13.3 measured as the agent regime.

*And step 2's premise was false.* Neither `compat::compat_key` nor
`cache::discover` carries `sif` or `embed_preproc`, so flipping the default
would have left every existing entry serving the old space indefinitely,
internally consistent and therefore invisible — and broken cold == warm, since
`search/stream.rs` has no SIF pass.

"A gate that cannot run is indistinguishable from a gate that passed."

Next levers: the §10 code table *on top of* split-sif (the two failures were
independent, so the fixes should stack), then sif-center and `--sif-a` retuning
on CoSQA.

### 14.6 R@10, and MaxSim on top of the rendered stream (2026-08-01, follow-up)

**R@10, split-sif vs none:** CoSQA 0.173 → **0.286** (+0.112, CI [+0.089,
+0.135]) against bm25's 0.325 — 88% of the bar, same shape as k=5. vscode
direct +0.110, etcd direct +0.155 (both p≈0); snake_case flat.

| MaxSim × preproc (paired within index) | maxsim off | on | Δ (CI) |
|---|---|---|---|
| CoSQA, none, R@5 | 0.083 | 0.108 | +0.026 [+0.006, +0.046] |
| CoSQA, split-sif, R@5 | 0.148 | 0.188 | **+0.040** [+0.015, +0.063] |
| CoSQA, split-sif, R@10 | 0.229 | 0.286 | +0.057 [+0.031, +0.082] |
| vscode direct, none, R@5 | 0.560 | 0.710 | +0.150 [+0.095, +0.210] |
| vscode direct, split-sif, R@5 | 0.615 | 0.825 | **+0.210** [+0.150, +0.275] |

The three levers stack and MaxSim's contribution *grows* under the rendered
index — §9.8's diagnosis that its ceiling was the token units, not the
mechanism. The maxsim-off none cell reads 0.083, three decimals equal to
§13.8's published number: that row was the maxsim-off configuration. And no
separator character survives any `split` variant or reaches ese
(`kebab_and_snake_separators_are_removed_not_kept`, `text/prose.rs`).

**First oracle number for vscode** (`oracle-vscode.json`; rg 0.155, rg-strong
0.360, hybrid 0.870 all reproduce exactly): **rg-oracle direct R@5 = 0.540**
(R@10 0.635) — below the *old* semantic mode (0.710), let alone the rendered
index (0.825). §13.9's explanation transfers: "choosing the right token is not
the hard part; ranking the hundreds of files that contain it is."

### 14.7 SIF vs idf weighting for the pooled vector (pre-registered 2026-08-01)

SIF weights a/(a + p(w)) over *collection* frequency, hyperbolic — saturating
at 1.0 for everything rare, so `blkg` and `backoff` weigh the same; BM25's idf
is logarithmic over *document* frequency. Was §14.4's biggest lever *SIF's
shape*, or just *having any* frequency weighting? `--sif-idf` swaps the pooling
weight to ln((n − df + ½)/(df + ½) + 1) over per-file df, everything else
identical. Predictions: (1) **idf ≈ sif on CoSQA, |ΔR@5| ≤ 0.02, CI straddling
0**; (2) both beat `none` decisively; (3) weak — if they separate, sif wins on
CoSQA and idf nowhere clearly.

**Result (same day): prediction 1 holds — the curves are interchangeable.**

| split-idf vs split-sif | Δ R@5 | 95% CI | sign test |
|---|---|---|---|
| CoSQA | −0.015 | [−0.029, +0.001] | 32w/50l, p=0.060 |
| CoSQA R@10 | +0.000 | [−0.016, +0.017] | 53w/53l, p=1.0 |
| vscode direct | +0.020 | [−0.010, +0.050] | p=0.29 |
| vscode paraphrase | +0.015 | [−0.015, +0.045] | p=0.51 |

Versus `none`, idf replicates SIF's whole gain: CoSQA +0.065 R@5 / +0.112 R@10
(both p≈0), vscode direct +0.135, paraphrase +0.075. So the lever was **having
frequency-based term weighting at all**, with one borderline cell (CoSQA R@5,
p=0.060) leaning sif — prediction 3's direction. `--sif` stays canonical,
`--sif-idf` a control lever, nothing graduates: "the embedder didn't need
BM25's curve, it needed BM25's *idea*."

---

## 15. Blind search (2026-08-01): the reorientation

### 15.1 The decision

**The primary evaluation regime becomes *blind search*: queries verifiably free
of the gold's identifiers, simulating a search agent with zero prior knowledge
of the codebase's naming. Everything measured to date — every `direct` set,
CoSQA whole, the §14 scoreboard — is retained unchanged as the
*named-identifier regression board*: it may not collapse, but it no longer
defines success.** (Maintainer decision, recorded 2026-08-01.)

The sets mostly name things: 66–70% of `direct` queries contain the gold
identifier verbatim (§12), real CoSQA queries share 42% of their vocabulary
with the gold (§12.3), 47% of real agent queries carry an identifier (§13.3).
On that distribution exact matching plus idf is near the optimal decision rule.
The vocabulary-crossing cells are the interesting ones: rg-oracle 0.000 on
kernel paraphrase against semantic's 0.035, now tied with bm25 (§14.4).

Two ideas borrowed from CORE-Bench (arXiv 2409.11363): **graded information
removal** and **hard verifiable gates** — the second because `paraphrase` is
only an *instruction* to the generator, and 1–5% of paraphrase rows still name
the gold verbatim, invisible to `identifier_pct`.

### 15.2 The blindness ladder

| level | kind | the query may contain | status |
|---|---|---|---|
| L0 | `direct` | anything, incl. the gold identifier | exists |
| L1 | `paraphrase` | shared vocabulary; identifier avoidance advisory only | exists |
| **L2** | `blind` (4–8 words), `blind_long` (12–20) | zero gold-identifier tokens — incl. lowercase symbol names and rare symbol subtokens — overlap-capped, structurally gated | **new, primary** |
| L3 | `symptom` | observable behavior only | deferred until the Loc-Bench blind screen shows the stratum matters |

Real-data anchors: CoSQA's zero-gold-hit subset ≈ real L2; Loc-Bench instances
whose issue names no gold identifier ≈ real L3; the ~53% of replay-agent
queries without identifiers ≈ agent-length L2.

### 15.3 The strict-blind predicate

`identifiers()` is frozen (baked into every recorded `identifier_pct`).
Blindness is decided by a **gold-aware** predicate,
`gold_identifier_hits(query_tokens, gold_text, symbol)`: a lowercased query
token is a hit if (a) it equals a snake_case/camelCase identifier token of the
gold span; (b) it equals the gold's own `symbol` — the clause `identifiers()`
cannot express — or matches under light suffix stemming (ing/ed/es/s/er); or
(c) it equals a symbol subtoken (split on `_`/camel) with guards: length ≥ 4,
not a stopword, and not used as an ordinary word by the gold's own
comments/docstrings — `rwstat` is caught, a comment's `read` passes.

`is_blind(row)` = zero hits AND per-row `gold_token_overlap` ≤ **0.5**;
set-level gate mean overlap ≤ **0.25**. Provisional until §15.5's calibration,
then frozen.

### 15.4 The new success criterion

**Blind (primary):** semantic beats bm25 on strict-blind cells — §14.1's exit
condition re-aimed at the regime the tool exists for. **Named-identifier
(regression):** the §14 numbers are the floor. A change that wins blind by
collapsing named does not ship.

### 15.5 Pre-registration (written before the first Phase-0 re-cut run)

1. **On strict-blind generated cells, semantic (split-sif + maxsim) beats
   bm25: ΔR@5 ≥ +0.03, CI excluding 0, on ≥3 of 6 corpora** — the load-bearing
   bet; if bm25 wins even here, the §9.9 model-swap is the only move left.
2. **rg-strong ≤ 0.05 R@5 on blind cells; rg-oracle collapses toward 0.000.**
3. **CoSQA blind re-cut: bm25's advantage shrinks or flips on blind, widens on
   the named complement.**
4. **`blind_long` ≥ `blind` for semantic, ≈ for bm25.**
5. **Blind-screened Loc-Bench instances show a larger semgrep-vs-grep gap.**

### 15.6 Phase 0: the re-cut of what was already measured (same day)

`eval/blind_cut.py` re-aggregates existing result files by the §15.3 predicate,
at zero scan cost. **CoSQA splits 847 blind / 353 named**, champion semantic
(split-sif+maxsim) vs bm25, paired within stratum:

| stratum | n | semantic R@5 | bm25 R@5 | Δ | 95% CI |
|---|---|---|---|---|---|
| **blind** | 847 | 0.148 | 0.169 | −0.021 | [−0.045, **+0.004**] |
| named | 353 | 0.286 | 0.348 | −0.062 | [−0.110, −0.011] |

**On the real blind stratum, semantic search and bm25 are already statistically
indistinguishable** (MRR Δ −0.004, CI [−0.020, +0.010]); the surviving lexical
advantage is concentrated in the named 29%. Prediction 3: direction confirmed,
no sign flip yet. Under the raw pre-§14 index the blind gap was −0.081 — the
§14 levers closed three quarters of the *blind* gap while barely denting the
named one.

**The advisory paraphrase instruction leaks worse than 1–5%**: on etcd 41/200
paraphrase rows (20%) fail strict-blind once subtokens and the overlap cap
count (and 26/200 *direct* rows pass it). "A fifth of the stratum the §13
record calls vocabulary-crossing isn't." Caps frozen after calibration:
zero-hit real CoSQA queries have overlap p50 0.33 / p90 0.60, so the 0.5
per-row cap excludes 15.3% of them — strict but livable, kept; set-mean 0.25
applies to generated sets only.

### 15.7 Phase 2: the real-world blind strata (same day)

`blind_screen.py` screens by tier — *named* (gold function name or file stem
verbatim), *partial* (subtokens only, reported and never folded because common
verbs land in it), *blind*.

**Real bug reports mostly name things: 348/560 Loc-Bench issues (62%) named,
144 (26%) partial, only 68 (12%) truly blind.** The counter-fact from the same
screen: **65% of replayed agent *queries* are blind** (324/497) — the tool sees
far blinder input than the issue would suggest.

| replayed agent queries, pre-§14 ranks (MRR) | n | bm25 | hybrid | semantic | hybrid−bm25 CI |
|---|---|---|---|---|---|
| named | 108 | 0.463 | 0.505 | 0.445 | [−0.035, +0.116] |
| partial | 65 | 0.441 | 0.468 | 0.322 | [−0.012, +0.097] |
| blind | 324 | 0.264 | 0.293 | 0.256 | [−0.006, +0.068] |

Everything gets harder blind, the fused engine leads bm25 in every tier without
clearing the clustered CI at this n, old-semantic trails. Prediction 5,
anecdote-grade, re-stratifying the §7.1 pilot A/B: on the **6 blind instances**
semgrep found the gold 6/6 vs ripgrep's 4/6; on the 27 **named** instances
ripgrep is 27/27. Direction as predicted, n far too small.

### 15.8 The first blind campaign: the scorecard (2026-08-02)

Six `<corpus>-blind.jsonl` sets, 4,168 queries, every blind row verified at
generation and again by the gate (`gold_id% = 0.0` everywhere, overlap
0.03–0.11, median 7–8 words). `eval/blind.sh`. Blind R@5:

| corpus | rg-strong | rg-oracle | bm25 | hybrid | semantic | champion | Δ(champ−bm25), CI |
|---|---|---|---|---|---|---|---|
| tokio | 0.005 | 0.010 | 0.020 | 0.015 | 0.020 | 0.040 | +0.020 [−0.010, +0.050] |
| etcd | 0.000 | 0.012 | 0.012 | 0.012 | 0.012 | 0.006 | −0.006 [−0.023, +0.012] |
| commons-lang | 0.015 | 0.025 | 0.035 | 0.045 | 0.055 | 0.060 | +0.025 [+0.000, +0.055] |
| jekyll | 0.000 | 0.000 | 0.014 | 0.027 | 0.027 | 0.014 | ±0.000 |
| vscode | 0.005 | 0.015 | 0.035 | 0.030 | 0.030 | 0.025 | −0.010 [−0.035, +0.010] |
| linux | 0.000 | *(not run — stopped)* | 0.020 | 0.015 | 0.010 | 0.010 | −0.010 [−0.030, +0.010] |

**Prediction 1: MISS, 0/6.** Pooled over 1,042 blind rows: semantic 0.028 vs
bm25 0.024, Δ +0.004, CI [−0.007, +0.014]. On strictly-blind generated queries
**nobody can retrieve** — every engine sits at 1–6% R@5 — and the registered
consequence applies: *the §9.9 model swap is the only move left* in this
regime. "A prose-space embedder severed from vocabulary overlap loses its
fuzzy-lexical channel exactly as grep loses its exact one."

**Prediction 2: HIT, decisively.** rg-strong ≤ 0.015 everywhere (band ≤ 0.05);
the oracle ≤ 0.025 everywhere it ran.

**Prediction 4: MISS, inverted.** `blind_long` does nothing for semantic
(Δ ±0.000) and significantly helps **bm25** (+0.015, CI [+0.002, +0.029]).
"More words buy the exact matcher more lottery tickets for accidental overlap;
the pooled vector gains nothing."

**The synthesis, and it is the §15 finding that matters.** The blind regime
split in two under measurement:

- **Real-blind** (CoSQA's 847 zero-gold-hit human queries, overlap ≈ 0.29):
  champion semantic already at **parity** with bm25 at useful absolute levels
  (0.148 vs 0.169, §15.6). This is where users and agents live (§15.7: 65% of
  agent queries), and the §14 levers already won most of it.
- **Strict-blind** (generated, overlap ≈ 0.07 — half to a quarter of what real
  blind humans emit): a **floor for every engine**, the §13.9 paraphrase wall
  measured a third way. "These sets are the *instrument* waiting for the model
  experiment, not a battleground for the current stack."

Direct anchors confirm the sets are sound (bm25 0.77–0.98 when the query names
the gold). Operationally: quote real-blind for product claims, hold
strict-blind as the gate the §9.9 code-teacher re-distillation must move.

### 15.9 Why the blind misses miss: forensics (2026-08-02)

`examples/why_miss.rs` (the §9.8 method on real campaign rows). Three failure
mechanisms, one success mechanism.

**A — the rare words have no relations** (§9.9, confirmed on live misses). The
query's *distinctive* words, exactly the ones SIF trusts, find nothing:
`scheduled→future` 0.198, `skip→hidden` ≈0.08 (jekyll `hidden_in_the_future`,
gold cosine 0.132, rank 39); `backtrace→return` 0.312 (commons-lang
`getStackFrameList`); `offload→static` 0.229, `synchronous→async` 0.197
(tokio). The only strong gold link is surface morphology:
`publication→publisher` 0.689.

**B — SIF inverts on blind queries.** The exact matches a blind query *does*
get are its domain-common words, and SIF crushes them by design: `exception`
matches the gold at **1.000** but carries weight **0.10** in commons-lang;
`posts` w=0.23 in a blog engine; `thread` w=0.19 in tokio. On
`getStackFrameList`: gold cosine **0.325 raw → 0.111 under sif** — base
semantic ranked it #1, champion dropped it past 40; champion ≤ base on blind
cells in 3 of 6 corpora. "SIF's win on named/real queries (§14.4) is a property
of *queries that contain rare tokens*; strict-blind queries are constructed not
to."

**C — prose crowds out code.** Both jekyll misses rank markdown docs, test
prose and release notes on top: the winner for "skip posts scheduled for later
publication" is a release-notes file matching `posts` 1.000 / `later` 1.000 /
`skip→skipping` 0.542. "A prose model retrieves prose."

**D — and the hits are the same mechanism pointed the right way.** Every traced
blind hit rides a *corpus-rare prose word inside the gold's own comments*:
`spawn_blocking` wins rank 1 because its doc example says "Stand in for complex
computation" and `computation` (w≈0.96 both sides) matches 1.000. "The semantic
channel that works on blind queries is **comment prose**, not code."

Levers: (1) re-test blind cells with SIF off or query-side-asymmetric weighting
— B says the champion config is mis-tuned for the primary regime; (2) boost
comment/doc lines in the rendering (D gives §14.2's deferred tree-sitter bet a
mechanism); (3) the §9.9 code-teacher swap remains the only fix for A, the
binding constraint everywhere else.

### 15.10 Closing note: blind search is an instrument, not the product regime

Recorded 2026-08-02, maintainer decision. Strict-blind models a user *problem
statement*, but the product's user is a **coding agent** that interprets the
problem and emits vocabulary *guesses* — 47% of real agent queries carry an
identifier (§13.3), often a wrong one. Strict-blind is re-labeled the
**model-experiment instrument**: the gate the §9.9 re-distillation must move,
not a regime query-time work should chase. Nothing is deleted; the primary
regime becomes **agentic-guess search** (§16), and the boards become three:
guess (primary), blind (model-experiment instrument), named-identifier
(regression).

---

## 16. Agentic-guess search (2026-08-02): the orientation

### 16.1 The thesis and the data

**A coding agent interprets a user request and guesses vocabulary; ranked
search should make those guesses land faster than exact-matching the same
guesses.** The agent's guess is the query distribution that matters — not the
user's problem statement (§15.10), not our generated paraphrases (§13.3).

The locbench shim logs hold **2,739 real search invocations** — 609 ranked
semgrep queries + 163 `search`, 1,397 `semgrep -e` exact patterns, and 570 rg
calls (430 distinct patterns) that replay deliberately excluded (§13.2). The
exact and rg strata are the purest guesses on record: alternation ladders of
candidate spellings (`writeParquet\|save_parquet\|to_parquet`). And ripgrep's
regex engine treats `\|` as a **literal pipe**, not alternation, so the
BRE-style ladders agents habitually type were dead on arrival.

### 16.2 The success criterion

Over the checked-in guess corpora (`guesses-v0.jsonl`, `guesses-agent.jsonl`):
**one ranked query built from the agent's own guess must land a gold file in
the top 5 more often than the agent's actual exact-mode workflow did —
instance-clustered CI excluding zero — and hybrid must not trail bm25 on the
same corpus.** Named-identifier sets remain the regression floor (§14);
strict-blind remains the model-experiment gate (§15.10).

### 16.3 Method

`harvest.py` exports every invocation losslessly; `ladder.py` decomposes
ladders into guess-groups with two translations (T1 = space-joined rung
literals, casing preserved; T2 = pre-split control); `guessplay.py` replays
three arms per group against gold with clustered statistics — the agent's
actual exact pattern, the ranked translation under {bm25, semantic, hybrid} ×
{shipped default, §14 champion}, and the agents' real ranked queries re-scored.
Original scopes are primary (65% of agent calls are scoped); repo-root is the
sensitivity cut.

### 16.4 Pre-registration (written before the first harvest or replay)

1. **Hybrid-T1 beats the actual exact arm on hit@5**: Δ ≥ +0.05, CI excluding 0.
2. **The advantage is rescue, not replacement**: rescue rate ≥ 20%, and
   parity-or-worse where the exact arm already hits at rank 1.
3. **Hybrid ≥ bm25 on the guess corpus** (MRR delta positive).
4. **T1 ≥ T2** for semantic/hybrid — pre-splitting destroys casing signal.
5. **Dead ladders are real and rescuable**: ≥ 10% of `-e` ladder invocations
   used `\|`, rescued at the highest rate of any stratum.
6. **Exact hit@5 falls with ladder length; ranked-translation hit@5 is
   flat-to-rising in it.**
7. **Scope robustness**: directions of 1–3 unchanged at repo-root.

### 16.5 Results (2026-08-02, same day: 2,113 guess-groups, 33,394 arm-rows)

**P1 — significant, but smaller than registered.** Hit@5 over all 2,113
exact+rg guess-groups: **Δ +0.034, CI [+0.002, +0.071]**. Miss on magnitude,
hit on direction and significance.

**P2 — miss, and the honest headline.** Rescue rate **6.3%** (107 of 1,697
groups whose exact replay found nothing), a third of the registered ≥20%; most
wrong guesses are wrong enough that no engine rescues them. Where the exact
guess already hit rank 1 (n=232), the ranked translation degrades it 47% of the
time. "Ranked search is a better *default posture* for guessing (P1), not a
reliable safety net under any guess (P2)."

**P3 — trending, not clearing:** hybrid−bm25 on the agents' own 624 ranked
queries, MRR **+0.019, CI [−0.004, +0.044]**.

**P4 — flat** (Δ −0.010, n.s.): T1 ≈ T2; the casing-signal reasoning mattered
less than assumed.

**P5 — hit, and the campaign's most quotable mechanical fact: 19.6% of
multi-guess `-e` ladders (104/530) were dead on arrival.** The ranked
translation rescues that stratum at 12.5%, double the overall rate but below
"highest of any stratum" as worded.

**P6 — hit, cleanly.** By ladder length, exact hit@5 falls 0.172 → 0.105 →
0.084 (1 / 2–3 / 4+ rungs) while ranked-translation holds 0.202 → 0.148 →
0.137: **the gap widens monotonically with how hard the agent is guessing**
(+0.030 → +0.043 → +0.053). "A long ladder is the agent saying it doesn't know
the name; that is exactly where ranked search pays."

**P7 — hit:** root-scope replay agrees in direction (Δ +0.080, CI [−0.001,
+0.165]). **Champion config: no** — split-sif does nothing for guesses
(t1-semantic champion−default Δ −0.008, n.s.).

Scorecard: 3 hits (P5, P6, P7), 2 misses (P2, P4), 2 partials (P1, P3). The
§16.2 criterion is **not yet met** on magnitude — +3.4pp, real but modest,
concentrated where agents guess hardest. The next lever is not query-time: it
is making ranked mode the agent's default posture (§7.3's framing lever is
worth 3.5× more ranked usage) plus the §9.9 model swap.

### 16.6 The capture runs: description gravity, measured clean (2026-08-02)

70 sonnet runs, 35 instances × {cap-ranked, cap-two}, `--no-score`, the exact
tool line persisted per run dir. The haiku driver-diversity batch was stopped
before running — noted, not replaced. `guesses-agent.jsonl`: 359 invocations.

**The starkest interface-gravity number in the record: a single mechanics-only
sentence documenting `-e` collapses ranked usage from 72% to 7%** (cap-ranked:
28% of calls used the undocumented-but-working `-e` anyway; cap-two: 93%
exact). §7.3 measured framing *advice* worth 3.5×; merely *mentioning* the
exact mode is worth ~10×. Second: **median guess length is one word under both
descriptions** — agents guess *names*, not phrases, however the tool is framed,
so the guess corpus is not a style artifact of v1–v4's framing.

### 16.7 The description experiment (pre-registered 2026-08-02, before the runs)

Descriptions built *from* the findings, scored against a fresh ripgrep baseline;
the §14 flip means ranked mode IS semantic now. 30 stratified instances × 4,
sonnet, scoring on: **rg**; **desc-v4** (§7.3 winner, identity framing, `-e` as
escape hatch); **desc-v5** (ranked identity, **no `-e` mention at all**);
**desc-v6** (v5 plus "put ALL your candidates in one query" — P6 as prompt
text). Predictions: (1) ranked share v5/v6 ≥ 60%, v4 ≈ 30–40%; (2) v6's ranked
queries average more words than v5's; (3) fnAcc orders with ranked share,
v5/v6 ≥ v4 ≥ rg; (4) semgrep ≥ rg on fnAcc (§7.1's +11pp retested). Power
caveat stated before results: at n=30/condition Loc-Bench accuracy resolves
only large deltas (§11.5), so the primary read is **behavior** and accuracy is
directional.

### 16.8 Results (same day; 111/120 runs completed before an external stop)

27 instances present under all four conditions; behavior over every completed
run. **Prediction 1 HIT, decisively:**

| condition | ranked | exact | ranked share | median words |
|---|---|---|---|---|
| rg | 0 | 106 (rg) | 0% | — |
| desc-v4 (identity + `-e` hatch) | 22 | 47 | **32%** | 4 |
| desc-v5 (no `-e` mention) | 77 | 10 | **89%** | 2 |
| desc-v6 (v5 + fold-ladders) | 50 | 25 | **67%** | 2 |

Deleting one sentence moved ranked share 32% → 89% — the strongest posture
lever measured in this project.

**Prediction 2 — weak.** v6's ranked queries are barely more multi-name than
v5's (36% vs 32% with ≥3 name tokens; identical mean length). "The `-e`
deletion does the work; the coaching sentence is mostly inert."

**Predictions 3–4 — accuracy is flat, exactly as the power caveat predicted.**
Paired over 27 instances: fnAcc@10tol rg 0.59, every semgrep condition 0.63;
fileAcc@5 0.74–0.78 vs rg's 0.74. All semgrep conditions sit +4pp above rg on
functions with **one discordant pair** (w1/l0) — direction right, resolution
nil. No ordering among v4/v5/v6.

**The finding that matters for the product:** behavior is controllable at 10× by
description text *without any accuracy cost* — the 89%-ranked v5 agents
localize as well as the 32%-ranked v4 agents and the 0%-ranked rg agents, at the
same median cost ($0.19–0.22/run). Recommended description is **desc-v5**:
identity framing, no `-e` mention — the escape hatch stays available for agents
that know it, but undocumented.

### 16.9 The powered A/B (pre-registered 2026-08-02, before any run)

§16.8 at benchmark scale: **desc-v5 (semantic, `-e` undocumented) vs rg, all
560 Loc-Bench instances, one arm each, sonnet.** At ψ_fn = 0.088 (§11.5), n=560
resolves ~4pp at 80% power. Registered: primary `func_acc@10_tol`, exact
two-sided McNemar; secondaries `file_acc@5`, `file_recall@5`, first-gold-hit
search index, cost and searches per run; intention-to-treat (`-e` usage is an
outcome, not a protocol violation); **no peeking** — endpoints computed once.

Predictions: (1) **desc-v5 beats rg on the primary by ≥ +4pp, McNemar p < 0.05**
(power ≈ 70% if the true effect is exactly 4pp, so a null is informative);
(2) the delta concentrates in the partial/blind tiers of the §15.7 screen;
(3) ranked share ≥ 80%; (4) zero shim bypasses, `-e` share ≤ 15%.

### 16.9a Adversarial review, and the re-registration it forced (before any row)

Two red teams — design/statistics and harness code — voided the section as
written. The predictions above are **retracted**.

**A1 — the arm label was false, and it corrupts a published claim.** semgrep's
footer prints `not it? rephrase the query, or -e '<pattern>' for every exact
match` on stderr after *every* ranked search (`crates/semgrep/src/out.rs`,
shipped 2026-07-30): **the tool advertised `-e` adaptively, at the moment of
failure, in the arm built to withhold it.** The campaign now sets
`SEMGREP_NO_HINTS=1` in every condition. **§16.6's reading is corrected**: 12
of those 72 `-e` calls immediately follow a zero-result ranked query, so the
72%-vs-7% direction survives but the "pretraining habit" attribution is
withdrawn.

**C1 — the registered effect size was arithmetically unreachable.** For a
paired binary δ ≤ ψ. §16.9 imported ψ = 0.088 from §11.5, measured across
*engine variants*, not these arms. Measured discordance for rg vs desc-v5
(§16.8, 27 paired instances) is **ψ = 0.037**, of which the "+4pp" headline was
literally **one discordant instance**: a +4pp delta at that ψ needs b − c = 22
out of b + c = 21.

**B1 — the harness would have silently corrupted ~11% of the frame.** 28
instance pairs share a `(repo, base_commit)` and the worktree was keyed on that
pair, so concurrent workers checked out, indexed and force-removed the *same
directory*, deleting trees under live agents and leaking an index into the rg
arm. Fixed: keyed by `instance_id`.

**The re-registration.** Primary becomes **direction + significance + interval,
not a threshold**, plus a co-primary `func_recall@10_tol` (the binary endpoint
discards resolution on the 96% of instances where both arms agree). Holm across
the four secondaries. **Every stratum is exploratory**, unstarred — the blind
tier alone (n=68, ~2–6 discordant pairs) cannot be tested at any α; the
post-treatment "search usage" stratum is deleted; `--emit-screen` is relabeled a
*discordance map of this run*. Re-registered: (1) desc-v5 ≥ rg on both
primaries, func_acc McNemar CI excluding zero; (2) the headline is the CI's
upper limit — "if semantic ranking has an advantage here it is below X pp";
(3) ranked share ≥ 80%; (4) zero shim bypasses, with *un-shimmed* search
reported too, since 21–28% of agent Bash calls are `find`/`python3`/`awk`
content searches invisible to the shim and that share is arm-correlated.

A clean null licenses "**parity at n=560 with an upper bound of X pp**" plus the
behavioral result; it does **not** license "semantic ranking doesn't help
agents", because the arms differ in result exhaustiveness and both leak a fifth
of their searching into un-instrumented tools.

**Budget revision (2026-08-02).** First 88 rows cost **$0.363/row** against
$0.24 projected, so the frame projects to **~$425, not ~$270** — it covers the
whole benchmark, and the semantic arm is pricier ($0.37 vs $0.29 mean).
Approved; a cost assumption that moves 57% is a fact about the instrument.

**Attrition, monitored mid-run (2026-08-03).** Every `agent_error` is a
`--max-budget-usd 1.0` cap hit at 26–36 turns — the hardest instances, not
random noise. **Only safe if attrition is symmetric, so it is watched rather
than assumed**: at 395 rows, 8 rg vs 6 desc-v5 budget hits, 3 vs 3 checkout
errors, 3 vs 2 missing one arm. The frame is **"instances solvable within $1
and 900 s"**, not the full benchmark.

**First-chunk observation, not an endpoint**: with the footer suppressed,
desc-v5's ranked share is **100%** — zero exact-mode calls in 43 runs, against
89% under the footer-coached §16.8 conditions; and 11% of desc-v5's Bash calls
are `find`/`python3` content searches versus 4% of rg's.

### 16.10 Result: parity, bounded (2026-08-03)

1,115 agent runs, **556 of 560 instances paired**, $360.99, one analysis pass.
desc-v5 (semantic) − rg (exact):

| endpoint | semantic | rg | Δ | 95% CI | discordant |
|---|---|---|---|---|---|
| **func_acc@10_tol** (primary) | 0.674 | 0.673 | **+0.002** | [−0.018, +0.022] | 18 / 17 |
| func_recall@10_tol (co-primary) | 0.771 | 0.766 | +0.005 | [−0.014, +0.025] | 38 / 37 |
| file_acc@5 | 0.838 | 0.835 | +0.004 | [−0.014, +0.022] | 15 / 13 |
| file_acc@1 | 0.745 | 0.737 | +0.007 | [−0.009, +0.023] | 12 / 8 |
| file_recall@5 | 0.880 | 0.875 | +0.005 | [−0.011, +0.022] | 19 / 16 |
| func_acc@10_strict | 0.667 | 0.660 | +0.007 | [−0.014, +0.029] | 22 / 18 |

**This is a null, and it is the informative kind.** The registered headline is
the bound: **if semantic-default search has an agent-level localization
advantage over ripgrep on this benchmark, it is smaller than 2.2 percentage
points.** 357 instances were solved by both arms, 164 by neither, and the 35
that separated them split 18–17. Achieved ψ = **0.063** — between the pilot's
0.037 and §11.5's 0.088, so the instrument had the resolution the
re-registration claimed, and the effect is not there to find at this scale.

**Scorecard** (the §16.9a re-registration): (1) **Direction — MISS**: all six
endpoints lean positive (+0.002 to +0.007), none clears zero; the 6-for-6 sign
is worth *noticing and not believing*, since "these endpoints are
near-duplicates computed on the same runs, so their agreement is one
observation wearing six hats, not six confirmations." (2) **Bound —
delivered**: ≤ +2.2pp primary, ≤ +2.5pp recall. (3) **Ranked share ≥ 80% —
HIT, 98%**: 3,385 ranked vs 85 exact calls, `-e` down to **2.4%** with the
footer suppressed against 11% when the tool was coaching it. (4)
**Instrumentation — HIT**: zero shim bypasses in 1,115 runs, and the un-shimmed
leak that looked arm-correlated early (11% vs 4%) converged to **11% vs 12%**.

**The exploratory strata contain a trap, and it is left as one.** Bug Reports
show +0.037 with an uncorrected p=0.035 (12/3 discordant) — one line out of ten
exploratory tests, where the expected number under a global null is 0.5.
Reported unstarred, uncorrected, explicitly **not** a finding. The blind tier —
where §15.7 and §16.5 predicted the advantage would live — shows +0.015 with
3/2 discordant pairs: nothing, and underpowered besides.

**Cost is the one clean separation:** $182.80 vs $143.69 for identical work —
semantic-default agents cost **27% more** per instance for statistically
identical localization.

**What this licenses.** Warranted: *semantic-default semgrep as an agent's only
search tool is at parity with ripgrep for localization on Loc-Bench, n=556,
with an upper bound of +2.2pp, at 27% higher cost* — plus the behavioral
result, that one sentence of tool description moves ranked usage from 7% to 98%
with no accuracy consequence either way. Not warranted: "semantic search doesn't
help agents." The arms differ in exhaustiveness as well as matching semantics,
~11% of both arms' searching leaks into un-instrumented tools, the frame is
instances solvable within $1/900 s, and — the constraint that outlives this run
— **80% of Loc-Bench instances are decided before search matters**: 357 solved
by both arms, 164 by neither. §11.5 said the instrument was the bottleneck;
§16.10 is that claim confirmed at full scale.

**Attrition, as promised.** 4 instances lost: 3 with one arm abandoned after 3
budget-cap failures (all 3 missing rg), 1 failing both arms on checkout.
Budget-cap failures ran 22 rg vs 16 desc-v5 — leaning toward dropping instances
where *ripgrep* struggled, so the null is if anything conservative. Frame
556/560 = 99.3%. **By-product delivered**: `discriminative-instances.json`, the
50 instances (9%) where the arms disagreed — a *discordance map of this run*,
explicitly not a neutral screen (§16.9a C5).

### 16.11 A bug the trajectories exposed, after the result (2026-08-03)

Reading agent trajectories to illustrate §16.10 surfaced what the aggregate had
hidden: **`semgrep "query" <single-file>` returns zero results, always.** Root
cause in `corpus::walk` — when the search root *is* a file,
`entry.path().strip_prefix(root)` yields the **empty string** as that file's
relative path, and every downstream consumer (chunk read, hit materialization)
fails on it. Exact mode takes the keyword path and is unaffected, which is
exactly why the bug survived: `-e` on a file works, so nothing in the test suite
or the snapshot noticed.

Blast radius in this campaign, from the shim logs:

| | |
|---|---|
| semantic ranked searches | 3,434 |
| **scoped to a single file** | **1,610 (46.9%)** |
| of those, returned nothing | **1,610 (100%)** |
| instances that hit it ≥ once | 339 / 556 (61%) |

Scoping to a file is the natural agent move *after* locating one, so the bug
fires precisely at the follow-up step.

**Does it void §16.10? Measured, not assumed — and the answer is no.**

| stratum | n | semantic | rg | Δ |
|---|---|---|---|---|
| hit the bug | 337 | 0.677 | 0.665 | **+0.012** |
| never hit it | 219 | 0.671 | 0.685 | **−0.014** |

Both deltas are noise and point in *opposite* directions — the bug-free stratum
is if anything worse for semantic. Agents recovered by re-searching at directory
scope or falling back to Read, so the failure cost turns rather than answers. It
plausibly explains part of the **27% cost premium**.

**Status of the claim.** §16.10 stands as measured — it is what the shipped
binary does, and parity is robust to the bug by the stratification above. What
is *not* established is how a fixed binary would perform; that is a new
experiment, justified only if something else changes too (the §9.9 model swap is
the candidate). The fix ships regardless: "your search silently returns nothing"
is the worst failure mode a search tool can have.

**The process lesson, which is the reason this section exists.** Two adversarial
reviews, a smoke test, and 1,115 runs did not surface this; *reading four
trajectories* did. The reviews checked the experiment and the harness. Nobody
checked whether the tool worked on the input agents actually give it. Add to the
pre-run checklist: **replay a handful of real agent invocations, verbatim, and
look at what came back.**

---

## 17. Where retrieval actually fails at agent scale (2026-08-03)

Re-classifying all 3,519 desc-v5 searches in the §16.10 campaign by cause of
emptiness: 1,993 (95.9% of empties) ranked search at a **file** scope — the
§16.11 bug; 69 (3.3%) usage error (exit 2); 16 (0.8%) exact mode, a genuine
zero-match. 2,078 of 3,519 searches (59%) returned nothing, and 82 of 445
instances (18%) never received a single non-empty result. A failure taxonomy
built on that is a taxonomy of the bug.

### 17.1 The instrument: guessplay's pre-fix run, with the bug separated out

`guessplay.jsonl` (33,394 rows, 2026-08-02) predates the fix, but the bug is
cleanly *separable*. Ranked rows by scope shape: file, n = 5,117, found gold
@5 **0 (0.0%)**; directory, n = 4,537, 2,532 (55.8%). A rate of exactly zero
is not a quality result, it is a structural one. Excluding file-scoped rows
leaves a **4,537-row bug-free frame**, which every number below uses — and it
means **§16.5's numbers were computed on a sample where 53% of ranked rows
were forced to zero**, which dilutes a paired difference without biasing it.

### 17.2 §16.5's champion verdict survives the correction

Rechecked, paired on identical (gid, arm, mode), hit@5:

| frame | n | default | champion | Δ | CI | w/l |
|---|---|---|---|---|---|---|
| all rows (as §16.5 reported) | 9,654 | 0.206 | 0.207 | +0.001 | [−0.003, +0.004] | 151/143 |
| **bug-free rows only** | 4,537 | 0.438 | 0.440 | **+0.002** | [−0.006, +0.009] | 151/143 |
| — semantic only | 1,300 | 0.416 | 0.416 | +0.000 | [−0.021, +0.020] | **90/90** |
| — hybrid only | 1,937 | 0.451 | 0.461 | +0.010 | [+0.001, +0.021] | 59/39 |
| — bm25 only | 1,300 | 0.441 | 0.432 | −0.009 | [−0.015, −0.004] | 2/14 |

The null is not an artifact of dilution: semantic under split-sif is **90 wins
and 90 losses**, a tie to the row. (bm25 moves through §14.4's prediction-6
coupling — bm25-mode output passes through MMR.) §14.5's verdict is safe as a
decision rather than a guess.

### 17.3 Semantic has no distinctive weakness against bm25

Paired on the 1,300 bug-free rows where both modes ran the same query: both
found 469 (36.1%), **bm25 only** 104 (8.0%), **semantic only** 72 (5.5%),
neither 655 (50.4%). semantic 0.416 vs bm25 0.441. The discordant sets are
near-symmetric, with no distinguishing feature: both median length 1 word,
~50% single-word queries, ~45% containing a code identifier. **The question
"where does semantic lose to lexical" has no answer on real agent queries,
because it does not systematically lose.** The 50.4% they *both* miss is the
real target.

### 17.4 The taxonomy of misses

All 1,300 rows by whether the gold was reachable from the searched path: 645
(49.6%) found, gold inside scope; 476 (36.6%) **true ranking failure** — gold
inside scope, not in top-5; 179 (13.8%) **unanswerable** — gold outside the
searched path. So 27% of all misses were structurally impossible, and no
engine or ranking change recovers those.

Of the 476 true ranking failures, **69% share no vocabulary with the gold at
all**. Overlap predicts the outcome monotonically: 49.7% of found rows, ~40%
of discordant rows, 30.8% of missed rows. That is §15's blind wall on real
agent queries, and it is a *model* problem: the embedder cannot relate words
it was never shown to relate.

### 17.5 The fix that looked obvious and is wrong

Searching the repo root instead of whatever the agent picked. hit@5, paired:

| frame | n | agent scope | root | Δ | CI | w/l |
|---|---|---|---|---|---|---|
| all bug-free rows | 4,537 | 0.438 | 0.425 | **−0.013** | [−0.022, −0.003] | 206/263 |
| — semantic | 1,300 | 0.416 | 0.405 | −0.012 | [−0.030, +0.005] | 64/79 |
| — bm25 | 1,300 | 0.441 | 0.423 | −0.018 | [−0.035, −0.002] | 49/72 |
| file-scoped rows (pre-fix) | 5,117 | 0.000 | 0.463 | +0.463 | [+0.450, +0.478] | 2371/0 |

**Blanket widening is a net loss.** It rescues 206 rows and costs 263: the
agent's scope choice carries real information. Any scope fix has to be
*selective* — conditioned on a signal that the current scope is wrong — not a
default. (The last row is the bug again, not a scope result.)

### 17.6 What this says to do next, in order

1. **The vocabulary wall is the dominant addressable failure** (69% of true
   ranking failures), not a ranking-parameter problem: the §9.9 code-teacher
   swap, gated on §15's strict-blind instrument.
2. **Scope needs a confidence signal, not a wider default** — 13.8% of rows
   are reachable that way and 0% by widening unconditionally.
3. **Not ranking parameters.** split-sif is null on the clean frame,
   semantic-vs-bm25 is a tie.
4. **Re-run the campaign only if something else changes.**

**The methodological note.** Two findings here inverted an answer that looked
settled: §16.5's null was computed on a half-zeroed sample, and "widen the
scope," which follows directly from the 13.8%, is a measured regression. Both
were one paired comparison away from being written down wrong.

---

## 18. The two-tiered rerun (2026-08-03)

The §16.10 campaign measured a broken tool: 47% of the treatment arm's
searches returned nothing, and the harness never noticed because the only
per-search record was `(argv, exit, stdout_bytes)`. So: a small instrumented
tier, a gate, then the full run. Tier 1 is underpowered on purpose — its job
is to find the next §16.11 *before* 1,100 runs are paid for.

### 18.1 The instrument that was missing

`SEMGREP_TRACE_FILE` already existed and works underneath `shim.py` without
perturbing the argv an agent sees; **`run.py` never set it.** It does now,
plus `files_walked` — the field that separates "empty scope" from "unreadable
scope". `triage.py` reads those envelopes and exits nonzero, so it stops a
campaign rather than describing one; run against the **old** campaign it
correctly fails on 69 usage errors, **455 distress signals**, and 82 instances
where every search was empty. Two of its own defects surfaced there: a disk
figure printed as `580.0%`, and — the one that matters — **the empty-result
gate passing vacuously at 0/0 when no traces exist**, the same silence the
tool exists to end. A missing trace now fails the gate.

### 18.2 Tier 0 (free, offline)

All 3,519 logged agent invocations replayed against the fixed binary on the
frozen fixture: usage errors (exit 2) 69 → **7**; returned nothing (exit 1)
2,008 → **90**; returned hits (exit 0) 1,442 → **3,422**; regressions **0**.
(Fixture corpus, so the exit-code *shape* is the signal.)

### 18.3 What tier 1 found, on its first four rows

The two-instance smoke run reported `path_taken=built_but_missed` twice.
Reproduced deterministically: `cache::discover` refuses a non-directory root,
so a file-scoped search misses; `build_through` builds a **complete index for
that file** and writes it; re-discovery misses again on the same check; and
the budget sweep deletes the fresh entry, judging the root dead by
`root_exists: root.is_dir()` — right for "the checkout was deleted", wrong
once §16.11 made file scopes legitimate. Every file-scoped search built an
index and threw it away, on roughly half of all agent searches, at ~20 ms
each. A second defect fell out: `enforce_budget_protecting` passed `keep` only
to the LRU pass, not to the dead sweep that runs first.

**This is the entire case for the instrumentation.** Four rows, and it
surfaced a defect that eight weeks of tests, two adversarial reviews, and a
1,115-run campaign had not.

### 18.4 Tier 1 results (40 instances × 2 arms, $18.70)

Every gate cleared, against §16.10 in parentheses: ranked searches returning
nothing **0 of 138** (59%); instances where every search was empty **0** (18%,
82); distress signals attributable to the tool **0** (455); usage errors the
tool is answerable for **0** (69); leaked worktrees / non-ok rows **0 / 0**
(4 / 1). The five remaining exit-2s are the tool being correct. **Gating on
the raw count would have failed the run for rejecting a bad path, which is the
single most useful error the tool emits** — so the gate is on unrecognised
*flags*.

Accuracy, **not** a result at n=40: `func_acc@10_tol` rg 0.625 vs desc-v5
0.675 (w2/l0), `file_acc@5` 0.775 vs 0.800 (w1/l0), $0.221 vs $0.246 per run,
3.8 vs 3.6 searches.

### 18.5 Tier 2 pre-registration (before the first row)

Endpoints carry forward from §16.9 unchanged: primary `func_acc@10_tol`, exact
two-sided McNemar over discordant pairs; secondary `file_acc@5`, cost,
searches per run. The binary endpoint "discards resolution on the ~96% of
instances where both arms agree". **Registered expectation: parity** — §16.11
measured the file-scope bug as costing nothing (bug-hit +0.012, bug-free
−0.014, both noise) and §17 put the ceiling on the vocabulary wall. **A null
is the predicted result, not a disappointment.**

### 18.6 Tier 1b: an independent 40, and what it cost to get one

**`--seed` barely moved the sample.** `stratified_sample` shuffled each
category then `sort(key=repo)`; Python's sort is stable, so repo order came
out alphabetical for every seed and **seed 1 and seed 2 shared 37 of 40
instances**, with no error and no warning — any claim of the form "validated
on an independent sample" would have been false. Fixed by shuffling the repo
*order*; seeds 1–3 now cover 99 distinct instances instead of 43.

**The run** (seed 2, 33 of 40 instances new, $19.84): gate passed, 165 engine
traces, **0 ranked searches returning nothing**, 0 distress signals, 0 usage
errors the tool is answerable for. Three non-ok rows failed the gate first,
both causes environmental rather than tool defects: a missing `git-lfs` on
`UCL__TLOmodel-1524`, symmetric across arms (one instance in 40 is ~14 of 560
at full scale), and the `--budget-usd` guard firing at $1.02 on
`Netflix__metaflow-2141`, which completed at 1.5 in 24 searches.

Accuracy across both tiers, paired, ok rows only, `func_acc@10_tol`: tier 1a
rg 0.625 vs desc-v5 0.675, **+0.050** (w2/l0); tier 1b 0.575 vs 0.550,
**−0.025** (w1/l2); **pooled distinct, n = 73, 0.616 vs 0.616, exactly
0.000** (w2/l2). The sign reversed on an independent sample, on four
discordant pairs total. **This is §18.5's registered prediction landing before
the money was spent** — had tier 1a run alone, +0.050 would have looked like a
result. `file_acc@5` pooled 0.795 vs 0.781; cost at parity ($0.249 vs
$0.247), the 27% premium §16.10 measured having gone with the file-scope bug.

---

## 19. The description A/B: restoring the micro-example (2026-08-04)

The largest lever this project has measured is not in the engine: **§16.6
moved an agent's ranked share from 72% to 7% by mentioning `-e` in one
clause.** Description effects are an order of magnitude larger than any §9
ranking parameter.

### 19.1 What the post-fix trajectories show

First campaign run on a working tool (366 searches over 5 runs). Empty results
fell from 55–68% to ~2%, repeated-identical-queries from 1,040 to 0, `--help`
probes from 27 to 0. What is left is query shape: 1 word 124 (34%), 2 words
125 (34%), 3 words 40 (11%), 4+ words 77 (21%). **68% of queries are one or
two words** — identifier guesses at a tool built to take descriptions, with
almost no surface to overlap on.

### 19.2 The candidate, and why it is a defect rather than an idea

`desc-v5` — the description in every campaign since §16.7, 695 runs — **has no
micro-example**, because v5 was produced by cutting `-e` out of `desc-v4` and
the example was the clause that named a mode. §7.3's winner was
ranked-as-identity framing **plus** a micro-example, and §7.3 separately found
that *agents imitate examples more reliably than they follow rules*. `desc-v7`
is `desc-v5` with the v4 example restored and nothing else: one inserted
sentence, 237 characters identical before it and 95 after, verified by diff.

### 19.2a The prior already in the logs, and its confound

Query length by condition from the shim logs (`queryshape.py`, ranked searches
only): desc-v4, with an example, n = 23, **3.74 words**, 30% ≤2 words;
desc-v5, neither, n = 4,129, 2.40, 69%; desc-v6, a rule and no example,
n = 50, 2.38, 64%.

**The clean one is desc-v6** — desc-v5 plus an explicit instruction — which
moved query length by **−0.02 words**. A rule telling agents to write longer
queries did not produce longer queries, which is why the lever under test is
an example. **The confounded one is desc-v4**: it also mentions `-e` and calls
the tool "a ranked hybrid code search" where v5 says "a ranked code search".
Three differences, one outcome, n = 23 — a prior, not a result.

### 19.2b What a static model does with a paraphrase (and why v7 was wrong)

`ese` is a *static* embedding table — one vector per token, pooled by SIF
rarity weight, word order discarded — and `sif.rs`'s weight `a/(a + p(w))`
with `a = 1e-3` puts a word appearing in 1% of the corpus at 0.09 while a rare
one sits near 0.99. **A paraphrase is therefore reduced, at the engine, to its
rare tokens** — "where is the retry backoff computed" is close to "retry
backoff computed" — and if those tokens miss, nothing is left.

Measured on `guessplay.jsonl` by `stylecut.py`, restricted to the arm where
the agent wrote the ranked query itself (`ranked-own`), default config,
original scope, non-file scopes only — n = 413, hit@5 by style
(semantic / bm25 / hybrid): identifiers n=194, 3.2 words, 0.526 / 0.500 /
**0.526**; plain words n=155, 3.8, 0.503 / 0.503 / **0.548**; mixed n=22, 6.6,
0.409 / 0.500 / 0.455; paraphrase n=42, 7.5, 0.357 / 0.357 / **0.357**.

Stratified by §17.4's predictor — does the query share any subtoken with the
gold function: identifiers 0.581 (n=105) vs **0.461** (n=89); plain words
0.567 (n=60) vs 0.537 (n=95); paraphrase 0.824 (n=17) vs **0.040** (n=25).

**A paraphrase that misses the gold's vocabulary finds it 4% of the time. An
identifier guess that also misses finds it 46%.** A paraphrase is not a way
around not knowing the name — it is bimodal, superb when it happens to contain
the right rare word and near-total failure when it does not. An identifier
guess degrades gracefully, because a wrong guess still shares subtokens with
the right one: `retry_backoff` and `backoff_delay` overlap where "computed"
and `backoff_delay` do not. Two things follow: **semantic − bm25 is ≈ 0 in
every stratum**, and **query length is the wrong endpoint** — desc-v4's +1.34
words is **−7pp identifiers and +5pp paraphrase**, so a description that
raised mean length would be a regression reported as a win.

**The number to quote.** The four-way classifier is fuzzy at one boundary
(`cpp_appendColumnToParquet` lands in "plain words"); the clean split is
**does the query contain English function words**. Collapsed that way, hybrid
hit@5:

| | name-like | description |
|---|---|---|
| all | 0.536 (n=349) [0.484, 0.590] | 0.391 (n=64) [0.281, 0.516] |
| shares gold vocab | 0.576 (n=165) | 0.636 (n=33) |
| **shares no gold vocab** | **0.500** (n=184) [0.429, 0.571] | **0.129** (n=31) [0.032, 0.258] |

The overall difference is not conclusive — those CIs overlap. **The blind
stratum is**: disjoint CIs, and where the query already carries the gold's
vocabulary a description does marginally *better*. Descriptions are not bad;
they are **entirely dependent on lucky rare-token overlap**. Observational,
and n is small in the cut that carries it (31 blind descriptions). Quote
**0.129 vs 0.500**; the direction is far better established than the
magnitude.

### 19.3 Pre-registration (amended 2026-08-04, before the first row)

Endpoints carry forward from §16.9/§18.5. The amendment replaced prediction 1
(*query length*) and "parity or a small gain" for desc-v7 after §19.2b, with
**no desc-v7 or desc-v8 row yet run**. **Five arms, a factorial**: v5 vs
`desc-v6` isolates the *instruction*, v5 vs v7/v8 isolates *having* an
example, **v7 vs v8 isolates the style the example demonstrates** (35
characters inside the example's quotes), `rg` as control. Registered:

1. **Style moves; length is not the endpoint** — floor, desc-v8 raises the
   identifier share ≥5pp over desc-v5, v7 raises the paraphrase share. **If
   style does not move, predictions 2–4 are void rather than negative.**
2. **desc-v8 ≥ desc-v7 on accuracy.**
3. **desc-v7 ≤ desc-v5** — the uncomfortable one, registered because it is
   what §19.2b implies about our own proposal from yesterday.
4. **Cost does not rise.**

Pre-specified subgroup: the effect should sit in the **`blind` tier** and be
absent in `named`, so a pooled null with a blind effect is a pass;
`func_recall@10_tol` co-primary. Disclosed peek at 81 of 200 rows: v8 and v7
identical on every endpoint, **zero discordant pairs** at n=16. Confounds
named in advance: the example's content (`retry backoff` is networking
vocabulary, Loc-Bench is not mostly networking bugs), and desc-v8 conflating
identifier shape with three candidate names in one query.

### 19.4 How to run it

`campaign.sh` takes `CONDITIONS` / `LIMIT` / `OUT`, plus
`INSTANCES=$(tierframe.py)` and `BUDGET=1.5` for §19.6's frame; analysis is
`queryshape.py --since <run-id>` (without `--since` it compares arms across
different instances), then `ab_analyze.py` and `reweight.py`. A style delta
against `rg` is impossible — rg has no ranked mode — so the style check is a
*within-arm replication*. **Prediction 1 is answerable from the shim logs
alone, so run `queryshape.py` after the first chunk**: it is free and gates
the ones that are not.

### 19.5 What the five-arm campaign found (40 instances × 5 arms, $71.18)

200 cells, 213 attempts, 10 of them the `--budget-usd` guard firing. Primary
`func_acc@10_tol`, paired within instance:

| arm | accuracy | $/run | searches/run |
|---|---|---|---|
| **desc-v8** (identifier example) | **0.600** | **$0.268** | **3.5** |
| desc-v5 (no example, ships today) | 0.550 | $0.303 | 4.4 |
| desc-v7 (paraphrase example) | 0.550 | $0.312 | 4.7 |
| rg | 0.550 | $0.277 | 5.0 |
| desc-v6 (a rule, no example) | 0.525 | $0.280 | 4.5 |

**Prediction 1: passed for v8, failed for v7.** desc-v8 raised the identifier
share **+20pp** over desc-v5 (65% vs 45%, n=161 vs 218) against a floor of
+5pp. v7 was registered to raise the *paraphrase* share and did the opposite:
paraphrase fell 1pp while identifiers rose 8pp. **Showing an agent a question
did not make it ask questions.**

**Prediction 2 (v8 ≥ v7): directionally yes, unresolved** — Δ = +0.050
CI[−0.075, +0.175], 4 discordant to 2, p = 0.69. **Prediction 3 (v7 ≤ v5):
failed** — Δ = **+0.000** exactly, 1 discordant to 1, recorded as a miss
rather than reinterpreted. **Prediction 4 (cost does not rise): passed, and
then some** — desc-v8 is the *cheapest* arm and uses the fewest searches, 3.5
against rg's 5.0.

**The pooled accuracy result is a null.** +0.050 for desc-v8 over v5, over v7
and over rg — the same figure three times, on 4-to-2 and 6-to-4 discordant
splits with p between 0.69 and 0.75. Reweighted, +0.044 CI[−0.099, +0.188].
The pre-specified blind subgroup shows +0.167 in all three comparisons — the
*same* +0.167, because v5, v7 and rg all score 0.333 on the 6 blind instances
while v8 scores 0.500. **That is one instance**, reported only because
pre-registering a subgroup obliges reporting it whatever it says. **desc-v6 is
inert for the third time**: −0.025 pooled, −0.167 blind, +0.40 words with −5pp
identifiers on n=245 queries.

**Limitations.** 5 of 200 cells come from an arm re-run until it succeeded,
conditioning them on termination; budget censoring fired on three instances
and is symmetric only by luck; and those re-runs were wrongly called necessary
for equal budget across arms, since runs are stochastic (desc-v6 succeeded at
$0.98 under a $1.00 cap and then failed at $1.51 under a $1.50 one) and a cap
only binds when hit.

**What shipped.** desc-v8 is README's recommended tool description, with the
evidence grade stated: behaviour change measured, accuracy gain directional
and unconfirmed. **`cli.rs` keeps its desc-v5-derived `--help` text, so
`--help` and README now differ, and `--help` still advertises "a question"** —
the style §19.2b found worst when blind. Deliberate, and what §19.6 decides.

### 19.6 Pre-registration: the blind-enriched frame (before the first row)

Supersedes §18.5's 560 × 2: rg against **desc-v8** on **204 instances in equal
strata** — all 68 `blind`, plus 68 `partial` and 68 `named` (`tierframe.py`,
seed 1). The dataset is 62/26/12, so a random 560-instance frame spends 62% of
its budget where §19.2b predicts *no* effect and still yields only 68 blind
pairs; equal strata buy the same 68 for ~$134 instead of ~$368. A pooled mean
here must not be quoted beside §16.9/§18; `reweight.py` reweights to the true
348/144/68 shares with a wider stratified bootstrap CI. Primary: the **blind
stratum**, n = 68 pairs, `func_acc@10_tol`, exact McNemar. `--budget-usd 1.5`
for every cell from the start. Registered:

1. **An effect in `blind`, ≈0 in `named`.** A pooled null with a blind effect
   is a **pass**. **A blind null falsifies the mechanism on real agents, and
   §19.2b's observational finding should then be treated as a property of that
   offline replay rather than of agent behaviour.**
2. **Searches per run stays below rg** — §19.5's 3.5 vs 5.0 is the most
   promising unregistered number in this project and therefore the one most
   likely to be noise.
3. **Cost does not rise.**

Blind is the primary *because* §19.5's blind signal was one instance, not
because it was positive; a blind effect with `named` also moving is general
competence, reported as failing prediction 1.

### 19.7 The blind-enriched result: the registered primary is zero

204 instances in equal strata, rg against desc-v8, **408 of 408 cells**,
$120.86, 204 paired instances at the designed 68 / 68 / 68.

**The registered primary — the blind stratum — is exactly zero.**

| stratum | n | desc-v8 | rg | Δ | discordant |
|---|---|---|---|---|---|
| **blind (primary)** | 68 | 0.471 | 0.471 | **+0.000** | 3/3 |
| partial | 68 | 0.515 | 0.588 | −0.074 | 3/8 |
| named | 68 | 0.574 | 0.603 | −0.029 | 1/3 |
| pooled | 204 | 0.520 | 0.554 | −0.034 CI[−0.078, +0.010] | 7/14, p=0.19 |
| reweighted to population | 204 | | | −0.037 CI[−0.082, +0.005] | |

(First read at 407 cells gave blind +0.000 on n=67 and pooled −0.034.)

§19.6 registered this in advance: *"A blind null falsifies the mechanism on
real agents, and §19.2b's observational finding should then be treated as a
property of that offline replay rather than of agent behaviour."* **It is a
blind null. The mechanism is falsified on real agents, and that sentence is
now binding.**

**The manipulation worked; the outcome did not follow.** The style shift
replicated cleanly on a harder frame — 62% identifier-shaped queries over 795
ranked searches, against desc-v5's 45% baseline in §19.5. Agents read the
example, imitated it, and wrote the queries §19.2b said would find more. They
did not find more. **A description can reliably change how an agent searches
without changing what it finds.** And tier-1's +0.050 reversed: at 204 pairs
it is −0.034 on 7-to-14, the point estimate changing sign exactly as in §18.6,
two days after we wrote §18.6 down.

**The two efficiency predictions passed, and replicated.** Searches below rg:
**3.97 against 4.68** per run, median 2 against 3, paired Δ **−0.72** (§19.5
saw 3.5 against 5.0). Cost: **$0.281 against $0.290**, paired Δ −$0.008. So
desc-v8 buys **fewer round-trips at no accuracy gain**, and the honest summary
of semgrep against ripgrep is unchanged from §18: **parity**, with a negative
point estimate whose CI includes zero. Nothing here tests **desc-v8 against
desc-v5**, the actual ship decision, which still rests only on §19.5's +0.050
over 4-to-2 discordant pairs; README has been corrected accordingly.

**Three ways this could still be wrong, in the direction of the hypothesis.**
The frame is deliberately hard (33% blind against a 12% population), though
the reweighted figure is also negative; attrition is rg-favourable, all 3
failures being rg cells the budget guard truncated, 3 of 411 attempts; and
`func_acc@10_tol` is blunt, though the co-primary recall is −0.025 with
16-to-20 discordant.

**What §19.2b now means.** Its measurement stands as a description of the
*offline replay*; the inference to agent behaviour does not survive. The
likeliest reconciliation is selection: an agent instructed to write names
writes names for targets it cannot name, and those names are guesses, where
the agents in the replay who wrote names were often agents who had a name. A
hypothesis, not a finding, and the thing to test next.

### 19.8 Three ways a search disappeared without being counted

None moves a published endpoint.

**1. Paths the scorer could not read (fixed).** `first_gold_hit_seq` matched a
two-component `dir/base` tail anywhere in the output, but **semgrep prints
paths relative to the scope it was given; rg prints them as passed** — so
`semgrep q msal/` yields `application.py:162:` where rg yields
`msal/application.py:162:`. A one-armed undercount, 13 of 204 desc-v8 rows
against 5 of 204 rg rows, including one where all four searches returned the
gold file and the metric read `None`. Now resolved against each invocation's
own scope; re-scoring moved 68 desc-v8 and 31 rg rows and **changed no
endpoint**.

**2. Calls the permission layer refused.** Claude Code evaluates a compound
command as a whole, so `rg …; rg …; git log …` under `Bash(rg *)` has *both
searches* refused and the shim never runs. **288 refused calls across 88
tasks**, roughly symmetric (rg 19% of tasks, desc-v8 24%): restricting to the
164 tasks where both arms genuinely searched leaves the primary at **−0.030**
against −0.034.

**3. The tool called as a tool that does not exist.** Four desc-v8 agents
emitted a structured `tool_use` block (`{"name": "semgrep", "input":
{"query": …, "path": …}}` → `Error: No such tool available: semgrep`) rather
than a Bash command: the input schema is the description's own signature, and
`semgrep "query" [path]` reads as a spec with named slots. **This happened 8
times across 4 desc-v8 tasks and 0 times to rg in 204** — mostly
self-correcting, but a *self-inflicted, one-armed* loss created by how we
worded the treatment.

None is worth re-running §19.7 for; the largest, (2), moves the primary by
0.004. **All three were invisible in every table and obvious the moment
someone opened a single task and read it against its own numbers** — the third
time in this project (§16.11 and §17 the others) that trajectories caught what
aggregates could not.

### 19.9 What agents do with a pipe, and `sg`

**The denial trigger, diagnosed.** Recovering the command behind each refusal
from the transcripts — 144 of 288 are reconstructible — the trigger is **not**
compound commands, which §19.8 guessed. It is *any binary in the command
outside the allowlist*, wherever it sits. First binaries: `python3` 62, `git`
23, `rg` 13, `find` 10, `grep` 9, `semgrep` 5, `cat`/`awk` 3 each. Of the 18
refusals whose command *begins with the arm's own permitted tool*, nearly all
die on what they pipe or chain into, not on the tool; only 2 were the
quoted-`|`-read-as-a-pipe false positive that looked likely. **The allowlist is
behaving as designed and stays as it is.** §19.8's proposed widening is
withdrawn: it would have loosened a gate that is not the problem.

**Piping, measured.** Of commands beginning with the search tool, rg is piped
in **252 of 863 (29%)** and semgrep in **32 of 778 (4%)**. Targets: `head`
237, rg 27, grep 15, sed 9, tail 4, xargs 3, wc 3, sort 2, awk 1. **79% of all
piping is `head`, which `-k` already does** — the most plausible reading of
the 7× gap: rg has no bounded mode, so an agent bounds it by hand. Not
entirely, though: agents still write `-k 5 | head -30`, belt and braces, a
small argument that `-k` is not as legible as we think. Of the 32 semgrep
pipes, **2** wanted something `-k` cannot give, both narrowing to a line range
(`awk -F: '$2 < 2297'`, `grep -E "8[0-9][0-9]|9[0-3][0-9]"`).

**The defect that made piping unsafe.** `sg -e "def " big/ --all | head -1`
printed a Rust panic — `failed printing to stdout: Broken pipe (os error 32)` —
where rg exits quietly. Rust sets `SIGPIPE` to `SIG_IGN` before `main`, so the
write returns `EPIPE` and `println!` panics. It only fires past the ~64 KB pipe
buffer, so the `-M 200` cap hid it in ranked mode while `--all` still
reached it. Restoring the default disposition (ripgrep's own fix, one call)
makes the process die of SIGPIPE like every other filter. **`| head` is the
most common thing anyone does to this tool and it could crash it**, unnoticed
for as long as the tool has existed, because nothing in the eval harness pipes.

**What shipped.** **`--lines A-B`**, absorbing the one pipe `-k` could not
serve with no second binary — which matters where the caller's shell may
refuse one. **`-` reads paths from stdin**, so `find … | sg "query" -` works
without `xargs`; speculative, since 3 xargs uses in 1,641 invocations is not
demand. And **`sg`** alongside `semgrep`: two `[[bin]]` targets over one
source, because nine scripts plus the test harness resolve `semgrep` by name.
Env vars, `~/.cache/semgrep`, `.semgrep/` and the `semgrep: ` stderr prefix
all stay, leaving `sg` printing `semgrep: …` — deliberate, and the cheap half
of a rename whose expensive half invalidates every built index.

**desc-v9, shipped unmeasured.** desc-v8 with the name changed to `sg` and one
clause folded into the identity sentence — *a ranked code search you run with
Bash* — aimed at §19.8's third channel. **It changes two things at once and
therefore attributes neither.** §16.6 and the `search` name-gravity arm both
say a name alone can move behaviour, so if a later campaign moves, the honest
reading is "v9 moved", not "the Bash clause worked".

### 19.10 Pre-registration: three arms, and what power is actually for sale

rg, desc-v5 and desc-v9 on §19.7's own 204 instances: **desc-v8-or-v9 against
desc-v5 has never been measured at power**, and **desc-v9 has never been
measured at all**.

The observed discordant rate on `func_acc@10_tol` is 10.3%, fixing the
smallest detectable accuracy effect at 80% power: **204 → ±0.060; 300 →
±0.050; 560, every instance in the dataset → ±0.038**. Every effect this
project has measured is ≤0.05, and §19.7's own −0.034 would need **682
instances**. **Accuracy cannot be powered at any price on this dataset** — a
reason to stop calling it the primary endpoint and to publish the bound beside
every accuracy null. One endpoint can be powered: **searches per run**,
observed Δ **−0.72**, **226** instances for 80% (against 682 for
`func_acc@10_tol` at −0.034, 879 for `func_recall@10_tol` at −0.025, 1,801 for
cost per run at −0.008).

**Primary: searches per run, desc-v9 vs rg**, paired within instance,
bootstrap CI. **Registered power: 76%, not 80%** — at n=204 with Δ=−0.72 and
sd=3.84, the frame chosen for exact comparability with §19.7 over the 226
instances 80% would want. **A null here therefore carries a real chance of
being a miss rather than an absence, and saying so is part of the registration
rather than an excuse available afterwards.** Prediction: desc-v9 uses *fewer*
searches than rg, by roughly −0.72; a positive delta falsifies the efficiency
claim outright. Secondaries, none powered: `func_acc@10_tol` over all three
pairs (bounded to ±0.060, Holm-corrected), `func_recall@10_tol`, cost, the
strata.

`desc-v9 vs desc-v5` is the open ship question and is **confounded by
construction**, since v5→v9 bundles the naming example, the `sg` rename and
the Bash clause; `desc-v9 vs desc-v8` is **exploratory only**. **Registered
now because it would be tempting later:** if searches fall and accuracy stays
flat, that is §19.7's dissociation replicated, not a disappointment.

### 19.11 The three-arm result: a null at 44% power, and a number that reproduced

rg, desc-v5 and desc-v9 on §19.7's own 204 instances. 612 of 612 cells, 613
attempts, one `parse_error` recovered on retry, $169.62.

**The registered primary is a null, and an underpowered one.** Searches per
run: **desc-v9 vs rg 4.15 vs 4.59, Δ −0.441 CI[−0.912, +0.039]**; desc-v5 vs
rg 4.27 vs 4.59, −0.314 [−0.814, +0.172]; desc-v9 vs desc-v5 4.15 vs 4.27,
−0.127 [−0.490, +0.225]. The interval crosses zero by 0.039. §19.10 registered
76% power, sized on §19.7's −0.72; the effect came in at −0.44, and **at that
effect the realised power is 44%** — 488 instances would have been needed for
80%. This null is closer to a coin flip than to evidence of absence, which is
what §19.10 committed to saying rather than discovering afterwards. The
efficiency claim is now *weaker* than when it had two consistent point
estimates behind it: −1.5, then −0.72, now −0.44, each smaller than the last,
which is the shape of a regression to no effect at all.

Accuracy, bounded to ±0.060 as registered: desc-v9 vs rg **−0.044** (7/16,
p = 0.093); desc-v9 vs desc-v5 **−0.010** (6/8, p = 0.791); desc-v5 vs rg
**−0.034** (7/14, p = 0.189). **Two results survive being nulls.** `desc-v5 vs
rg`'s −0.034 is the same figure to three decimals as §19.7's `desc-v8 vs rg`,
from an independent campaign with a different treatment arm — a number that
reproduces exactly across frames is worth more than most of the deltas in this
document. And the **blind stratum is +0.000 in all three pairs**, the third
independent time it has landed on exactly zero. §19.2b's mechanism predicted
the effect would live there; three campaigns now say it does not live
anywhere.

**The ship question, answered as well as this dataset can.** desc-v9 ≈ desc-v5
on every endpoint: −0.010 accuracy, −0.127 searches, −$0.023 cost. The style
shift replicated (64% identifier-shaped queries against desc-v5's 50%, n=851
and 880). The description reliably changed *how* agents search and moved
*nothing* about what they found. Since v5→v9 bundles the example, the rename
and the Bash clause, the null is at least unambiguous: no component of it
mattered enough to show.

**What the whole §19 arc adds up to.** Six description arms, three campaigns,
~$360. Descriptions move agent behaviour reliably and measurably — the
identifier share moves 15–20pp on demand, replicated four times. **None of it
moves the answer.** The honest summary of semgrep against ripgrep is unchanged
from §18: parity, with negative point estimates whose intervals include zero.
The remaining ceiling is where §17.6 put it — the embedding model, not the
description, not the ranking parameters, and not, on this evidence, how the
agent is told to phrase a query.

---

## 20 Pruning the chunk before it is embedded, and budgeting by content

§14 asked what *rendering* to hand the embedder and found `split` + `sif`
(§14.4). It never asked the prior question: of the tokens in a chunk, which ones
should be there at all. Under uniform mean pooling every surviving token takes an
equal share of the vector, so dropping one hands its mass to the rest. Pruning is
reweighting.

### 20.1 What is actually in the token stream (2026-08-05)

**`function` and `export` are not in the `split-nokw` keyword table.** Nor are
`type`, `readonly`, `declare`, `null`, `undefined`, `true`, `false`, `as`,
`from`, `of` — 43 tokens missing in all, checked against the seven corpus
languages. On the corpus where §14.4 measured `split`'s largest win the two most
common boilerplate tokens in the language were being embedded as content: on the
example vscode chunk `split-nokw` dropped 2 tokens from 32; it should have
dropped 4. The table is left **frozen** and the repair added beside it as
`KEYWORDS_EXTRA`, so `prune-kw` is an attributable arm rather than a silent edit
to a published condition
(`the_frozen_table_really_was_missing_function_and_export`).

The ladder, on that chunk. Each rung is a strict subset of the one above
(`ladder_is_cumulative`), so a delta is attributable to one step:

| tier | tok | mass/tok | what it adds |
|---|---|---|---|
| `split` | 34 | 2.9% | — |
| `split-nokw` (shipped) | 32 | 3.1% | the frozen table |
| `prune-kw` | 30 | 3.3% | the repaired table |
| `prune-lex` | 24 | 4.2% | builtin namespaces, primitive/annotation types, unit suffixes (`math`, `number`, `ms`) |
| `prune-decl` | 16 | 6.2% | declaration positions only; every reference dropped |
| `prune-uniq` | 18 | 5.6% | `prune-lex`, each distinct token once |
| `prune-soft` | 29 | 3.4% | `prune-lex`, declarations emitted twice |

At `prune-decl` the body reduces to `compute backoff delay attempt jitter`, which
exposes the second finding: **11 of the 16 surviving tokens are the file path.**
69% of the pooled mass says where the file lives, so every window in a long file
converges toward one vector and within-file discrimination fails exactly where a
file has the most chunks. Hence `PathRender`, orthogonal to the tier: `full`,
`dedupe`, `tail` (last two segments), `scaled` (deduped, capped at 25% of the
body's count).

Pruning is **document-side only**. A natural-language query has no declaration
sites and the low-signal table would eat real query words, so `render_query`
stops at keyword pruning. This does not break the one-space invariant: that
constrains the token→vector mapping, not what content each side contributes.

### 20.2 A line is not a unit of content

`ChunkParams.window` is 32 lines. Non-whitespace characters per 32-line window:

| corpus | p10 | median | p90 | p99 | max |
|---|---|---|---|---|---|
| vscode (ts) | 563 | **931** | 1,418 | 2,253 | 6,767 |
| linux (c/h) | 504 | **693** | 1,012 | 1,524 | 2,885 |
| tokio (rs) | 429 | **677** | 975 | 1,409 | 3,283 |
| jekyll (rb) | 386 | **675** | 874 | 1,106 | 1,419 |

A vscode chunk carries 35% more content than a linux chunk at the same line
count, the p10→p90 spread inside one corpus is 2.5×, and the worst vscode window
holds 6,767 non-whitespace characters — seven times the median, pooled into one
vector by a uniform mean. `ChunkParams.budget` cuts line-aligned windows to a
content budget instead, carrying the overlap across as a fraction (25% at the
defaults) so it is a reparameterization rather than a second overlap policy. The
unit is cAST's (arXiv 2506.15655).

Two external results disagree with our defaults in the same direction. The
controlled study of 864 RAG code-completion settings (arXiv 2605.04763) found
~2,000 non-whitespace characters optimal and **function-level chunking never
Pareto-optimal**, trailing by 3.57–5.64pp; cAST budgeted 4,000. Our median chunk
is 700–930. That study also found retriever choice worth ≤1.11pp against a
3.43–6.51pp spread between chunking strategies — if that transfers, chunking is a
larger lever than the bm25-vs-semantic axis. It may not: every retriever there
was contextual or lexical, none a static bag-of-words model.

### 20.3 Pre-registration (written before the first row)

Scoring as §14: `run_eval.py`, semantic mode, paired per query, 2,000-resample
bootstrap CIs, exact sign tests, leakage above every table. Tiers run on all five
sets; the budget arm skips cosqa (one short Python function per file, 20,604
docs, so a 32-line window and an 800-character budget produce the same single
chunk and the comparison is structurally empty).

Registered, in falsifiable order: **(1)** `prune-kw` gains on TS (vscode ≥
`split-nokw` + 0.01) and does nothing on C (|Δ| < 0.01 on linux). **(2)**
`prune-lex` ≥ `prune-kw` on vscode and etcd `direct`; **if it loses on 3 of 5
corpora the tier is dead**. **(3)** `prune-decl` < `prune-lex` on `direct`, ≥ on
`blind` (71.5% of tokio `direct` queries contain the gold identifier, §13.1).
**(4)** `prune-soft` ≥ `prune-decl` everywhere — weighting should dominate
deletion when the deleted tokens are sometimes the answer. **(5)** Path handling
matters only at the aggressive end: three path arms within 0.02 at `prune-lex`,
`scaled` ≥ `full` + 0.02 at `prune-decl`. **(6)** SIF partially subsumes
`prune-lex`: Δ(`prune-lex` − `prune-kw`) smaller with `--sif` on every corpus.
**(7)** The budget at parity is a no-op: `chars-800` vs `lines-32`, |Δ R@5| <
0.02 on all four corpora.

**Tripwire.** bm25 cells must be identical across tier arms up to MMR, which
reads the embedding matrix (§14.4 point 6). Any other bm25 movement is a bug, not
a result.

### 20.4 How to run it

`eval/prune.sh [corpus...]` runs every arm; `python3 eval/diff.py --base prune-kw
--cand prune-lex prune-decl prune-soft` compares. Results land in
`eval/results/lever-<corpus>-prune-<tag>.json` under the lever campaign's naming;
the script skips any condition whose output already exists.

### 20.5 Run 1, and the defect it was measuring instead

Four corpora completed (tokio, etcd, vscode, cosqa; linux stopped mid-run).
Retained as `lever-<corpus>-prune-qsym-<tag>.json` — `qsym` for query-symmetric,
which is what this run turned out to be about.

**Against the incumbent `split-nokw`, R@5 on the primary cell:**

| arm | tokio | etcd | vscode | cosqa |
|---|---|---|---|---|
| `split-nokw` | 0.515 | 0.620 | 0.750 | 0.117 |
| `prune-kw` | **0.585** | **0.675** | **0.780** | 0.122 |
| `prune-lex` | 0.590 | 0.645 | 0.770 | 0.111 |
| `prune-soft` | 0.580 | 0.645 | 0.745 | 0.118 |
| `prune-uniq` | 0.580 | 0.590 | 0.755 | 0.099 |
| `prune-decl` | 0.395 | 0.420 | 0.545 | 0.072 |

`prune-kw` is +0.070 on tokio (CI [+0.030, +0.115], p=0.003) and +0.055 on etcd
(CI [+0.010, +0.100], p=0.027).

**Against the §14.4 champion `split`+`sif`, which is the bar that matters:**

| arm | tokio | etcd | vscode | cosqa |
|---|---|---|---|---|
| champion | 0.545 | 0.595 | 0.825 | 0.188 |
| `prune-kw` Δ | +0.040 n.s. | **+0.080** p=0.002 | −0.045 n.s. | **−0.066** p<0.001 |

The headline against `split-nokw` was flattered by a weak baseline. The repaired
table beats the champion on one corpus of four and **loses on CoSQA**, the only
set with real human queries and the one §12 says to prefer for quality claims.

**Predictions, scored:**

1. **Partial.** The repair gains, but the largest gain is tokio (Rust), which has
   no `function` or `export` — it has `as`, `where`, `type`, `in`, `true`,
   `false`. The 43 missing words were a general oversight, not a TS one.
2. **Failed, by its own kill condition.** `prune-lex` − `prune-kw` is +0.005,
   −0.030, −0.010, −0.012: worse on three, all n.s. A hand-written stoplist adds
   nothing over fixing the keyword table.
3. **First half confirmed, hard** (−0.195 to −0.225 on `direct`, p<0.001 on every
   corpus). **Second half unsupported**: on `blind`, −0.015 / +0.017 / +0.000,
   none significant. Registered reading: declaration-position is the wrong axis.
4. **Confirmed.** `prune-soft` beats `prune-decl` by +0.185 to +0.225 everywhere
   and is indistinguishable from `prune-lex`.
5. **First half holds** (path arms within 0.025 at `prune-lex`). **Second half is
   backwards**: at `prune-decl`, `scaled` is the *worst* arm on all four corpora
   (tokio 0.375 vs full 0.395, vscode 0.490 vs 0.545). At 69% of the tokens the
   path is still carrying signal, not crowding it out.
6. **Mixed.** SIF helps CoSQA enormously (`prune-kw`+sif +0.048, p<0.001) and
   hurts etcd (−0.085, p=0.001). No clean statement about subsumption.
7. **Holds.** `chars-800` vs `lines-32` at `prune-kw`: −0.010, −0.030, +0.015,
   all n.s. The reparameterization is free.

**The defect.** `render_query` kept `Keywords::Extended`, so the query side was
pruned too. Measured on CoSQA's 1,200 real queries, the extended table removes
**1,194 of 7,564 query tokens (15.8%), affecting 771 queries** — against 217
tokens (2.9%) for the frozen legacy table. It looked like query-side damage
charged to a document-side lever. **That reading was wrong, and §20.6 is the
correction** — removing the query-side pruning was tried and lost everywhere,
including on CoSQA. The measurement is real and only the inference from it was
mistaken. Run 2 is `prune-qasym-`; `qsym` is the shipped behavior.

### 20.6 The correction that lost: prune both sides or neither

**Paired Δ R@5, asymmetric (query not pruned) minus symmetric:**

| corpus | `prune-kw` | `prune-lex` | `prune-kw`+sif |
|---|---|---|---|
| tokio | **−0.040** p=0.039 | **−0.055** p=0.013 | **−0.075** p=0.001 |
| etcd | −0.020 n.s. | −0.025 n.s. | +0.000 n.s. |
| vscode | −0.010 n.s. | −0.025 p=0.062 | −0.015 n.s. |
| cosqa | −0.003 n.s. | −0.003 n.s. | **−0.014** p=0.021 |

**Every delta is negative or zero — 11 of 12, across 4 corpora — and the CoSQA
arms the change was written to rescue lost too** (`prune-soft` −0.010 p=0.012,
`prune-uniq` −0.014 p<0.001, `lex-sif` −0.014 p=0.012). The hypothesis is refuted
on its own chosen corpus.

The mechanism. Ranking is cosine against a fixed query, so `|q|` cancels and the
score decomposes additively over the query's tokens:

    score(d)  ∝  <C_q, d> + <K_q, d>        C = content tokens, K = keywords

Prune neither side and `<K_q, d>` is a real matching term. Prune both and it
vanishes. Prune documents only and `K_q` survives in the query while every
document has had its counterpart deleted: word vectors are not orthogonal, so the
term is non-zero and *varies by document*. It is an additive term with nothing to
align with, and it reshuffles the ranking on noise. Query-side pruning is
therefore not a feature; it is the removal of a noise term that chunk-side
pruning manufactures.

The operative rule: **prune the two sides identically, or do not prune at all.**
Transforms a query structurally cannot mirror — declaration position; the
low-signal table, which eats "parse a number from a string" — stay document-side.
Pinned by `keyword_pruning_is_symmetric_and_the_rest_is_not`.

**Against the bar.** Symmetric `prune-kw` versus the §14.4 champion: tokio +0.040
n.s., etcd **+0.080 p=0.002**, vscode −0.045 n.s., cosqa **−0.066 p<0.001**. One
win, two nulls, one loss — on the corpus §12 says to weight most. The repair is a
real defect fixed and not, on this evidence, a shipping win; `split`+`sif`
survives §20 as the champion. What §20 produced instead is three negative results
(the stoplist adds nothing over the repair, declaration-position deletion costs a
fifth of recall, path capping hurts), one general rule, and one lever free at
parity and untested at size.

### 20.7 The symmetry confound in §20.5, and the arm that would settle it

| tier | mirrorable query-side? | how it did |
|---|---|---|
| `prune-kw` | yes — one table, both sides | best prune arm |
| `prune-lex` | yes in principle, **never run that way** | −0.005 to −0.030 vs `prune-kw` |
| `prune-decl` | **no** — prose has no declaration sites | −0.195 to −0.225 |

**The ranking tracks symmetry exactly.** `prune-lex` was withheld from the query
side on the same intuition §20.6 has since shown to be backwards, so §20.5's
conclusions for predictions 2 and 3 are confounded with asymmetry. Both stand as
*measurements of the arms as run*; neither is safe as a statement about pruning.

One arm discriminates for the stoplist: **`prune-lex` with the low-signal table
applied to queries as well.** Nothing discriminates for `prune-decl`, and that is
the more interesting half: if §20.6's rule is right, declaration-position pruning
is not a weak lever but a **structurally inapplicable** one for a bag-of-words
retriever, which predicts the observed ordering (`kw` > `lex` > `decl`) from
symmetry alone.

### 20.8 The symmetry arm: a null, and a dose-response that holds

**Mirroring the low-signal table onto the query moves nothing:**

| corpus | `lex` | `lex-sym` | paired Δ |
|---|---|---|---|
| tokio | 0.590 | 0.590 | +0.000 [−0.020, +0.020] |
| etcd | 0.645 | 0.635 | −0.010 [−0.030, +0.010] |
| vscode | 0.770 | 0.760 | −0.010 [−0.025, +0.000] |
| cosqa | 0.111 | 0.109 | −0.002 [−0.008, +0.004] |

All n.s., and `lex-sym` still fails to beat `prune-kw` (etcd −0.040 p=0.077,
cosqa −0.013 p=0.052). **§20.5's prediction 2 stands as originally read.**
Mirrored dedupe is likewise a null (−0.025 to +0.003, all n.s.).

Not a contradiction of §20.6: the mechanism predicts `<K_q, d>` scales with how
much of the query belongs to the pruned class.

| pruned class | share of query tokens | cost of leaving it unmirrored |
|---|---|---|
| low-signal table | 2.8–7.4% | **0.000 to −0.010** (n.s., §20.8) |
| keyword table | 14.9–15.8% | **−0.040** tokio, negative on 4/4 (§20.6) |
| non-declaration tokens | ~100% (a query is all references) | **−0.195 to −0.225** (§20.5) |

Three magnitudes, three effect sizes, monotone; inferred from the middle row and
it postdicts the other two. It sharpens §20.7: `prune-decl`'s mismatch is
*maximal*. The prediction that would break this: a document-side transform
removing ~15% of query-mirrorable vocabulary should cost ~0.04 when unmirrored,
whatever the transform is about.

### 20.9 Linux, and the size sweep that went the wrong way

**A correction first.** Three linux arms — `nokw`, `kw`, `lex` — did land, from
the interrupted run of 2026-08-04 23:39–23:42, under the `qsym` configuration.
They are valid and they change the headline.

**linux (C, 84k files, 199 `direct` queries), semantic R@5:**

| arm | R@5 | vs incumbent | vs champion (0.734) |
|---|---|---|---|
| `split-nokw` | 0.764 | — | +0.030 n.s. |
| `prune-kw` | **0.814** | +0.050 p=0.006 | **+0.080 [+0.025, +0.141] p=0.011** |
| `prune-lex` | **0.824** | +0.060 p=0.008 | **+0.090 [+0.035, +0.146] p=0.002** |

The repair's record against the champion across five corpora is **two significant
wins (etcd +0.080, linux +0.080), two nulls (tokio, vscode), and one significant
loss (CoSQA −0.066)** — and the wins are the two largest trees. It does not
settle the question: the loss is on the only corpus whose queries nobody here
wrote. Linux is the one corpus where `prune-lex` is best; it lost on 3 of 5 and
is dead as a general lever, but the exception is the largest corpus and is not
noise.

**The sweep.** Rendering held at `prune-kw`, chunk budget swept:

| corpus | lines-32 | chars-800 | chars-1600 | chars-2400 |
|---|---|---|---|---|
| tokio | 0.585 | 0.575 | 0.550 | 0.525 |
| etcd | 0.675 | 0.645 | 0.655 | 0.655 |
| vscode | 0.780 | 0.795 | 0.765 | 0.760 |
| linux | 0.814 | 0.804 | 0.774 | 0.759 |

**Every corpus is flat or declining as the budget grows, monotone on three of
four.** No single comparison reaches significance, but 11 of 12 point down, and
the two largest corpora lose the most at 2,400 (tokio −0.060, linux −0.055, both
p≈0.07–0.08). The external result does not transfer: those studies used BM25 and
transformer retrievers with attention to spend across a long chunk; this engine
pools by a **uniform mean**, so a bigger chunk is a strictly more diluted vector.
Chunk-size guidance from the RAG literature should be assumed not to transfer to
a static bag-of-words retriever until measured, in either direction.

The budget is still worth keeping: free at parity, it equalises the 35%
language-density gap and caps the 6,767-character tail. It is a fix for the worst
chunks, not a knob to turn up.

**Final ledger for §20.** One shipped defect found and fixed (43 missing words).
One arm that beats the champion on the two largest corpora and loses on the most
trustworthy one. Five negative results that close off directions (stoplist,
declaration pruning, path capping, dedupe, larger chunks). One rule with a
quantitative form: mirror what the query can mirror, and the cost of not doing so
scales with the unmirrored share. `split`+`sif` remains the default until the
CoSQA loss is understood.

---

## 21 Renderings at agent scale: the free gate

§20 produced a split verdict; §9.7's standing rule is that engine changes are
gated on agent-level evidence. This section runs the four renderings against the
queries agents actually typed.

### 21.1 Pre-registration (written before the first row)

**Instrument.** `guessplay.py` over `eval/queries/guesses-v1-descv9.jsonl`,
desc-v9 only: **854 ranked queries over 186 instances**. Five configs: `default`
(shipped `none`), `split`, `prune-kw`, `prune-decl`, `champion` (`split`+`sif`).
Semantic (shipped) and bm25 (tripwire). No API spend. `guesses-v0` is not the
corpus: its 624 ranked rows are V4-era with zero `desc-*` rows, and 208 of 624
(33%) are pre-§16.11 file-scoped rows scoring 0.000 in every config.

**The dose.** `cache::discover` returns `None` for a non-directory root, so a
**file-scoped search finds no index at all** and the cold path renders from the
search flag. **394 of 854 (46%) of desc-v9 ranked searches are file-scoped**, 334
root, 126 directory; every arm therefore carries both levers. `--sif` exists only
under `Cmd::Index`, so **`champion` is partially treatable by construction** and
is not the headline.

**P1 — the gate, about power not recall.** **ψ_offline** = share of *instances*
where an arm and the control disagree on "did any of this instance's ranked
queries surface a gold file at rank ≤5". Floor: **ψ_offline ≥ 0.06 with |b−c|/n ≥
0.03**. Prior, measured on the old corpus: champion vs default is **0 of 40
discordant, ψ_offline = 0.000**, while query-level hit@5 moves +0.038. The
registered expectation is that **no arm clears P1** and the output is a bound.
**P2 —** `prune-kw` ≥ control, Δ hit@5 ≥ 0.00 under a cluster bootstrap over
instances (4,000, seed 1); the measured design effect is **1.64×**, so per-query
intervals may not be quoted. *Kill:* Δ ≤ −0.02 excluding zero. **P3 —**
`prune-decl` loses pooled by ≤ −0.05 **and** the loss is a function of query
length (Δ in the 1-word stratum ≥ pooled + 0.05, monotone across {1, 2, 3–4, 5+}
words); confounded with the 69%-path-token domination it cannot separate. **P4 —**
`split` bounds the ladder, |Δ hit@5| < 0.02.

**Tripwires (each voids the run, not the arm).** bm25 invariance |Δ| ≤ 0.005; one
`bin_sha256`; index readback of `{embed_preproc, sif}` from `meta.json`.

A null **will** license withdrawing `prune-kw` as a shipping candidate and
closing the direction with a number. It **will not** license any claim that
rendering does not matter to retrieval, nor any statement about agent *accuracy*
(±0.060 at n=204, ±0.038 at all 560, ±0.15 at a 40-instance tier).

### 21.2 The gate: one arm clears it, in the losing direction

Run 2026-08-05, `guessplay-v1.jsonl`, one binary (`d89fa15f10c6abd8`), 854
desc-v9 ranked queries over 186 instances, five configs, semantic + bm25.

**A harness bug found first.** Every file-scoped row scored 0.000 in every config
because `guessplay.score()` prefixed the scope path as though it were a
directory — `pkg/trainer.py` plus a hit of `trainer.py` composed to
`pkg/trainer.py/trainer.py`. This had been read as the §16.11 file-scope engine
bug; it is a separate scoring defect and it was still live. Fixed; base hit@5
0.000 → 0.356.

**And then the fix showed why that half cannot answer this question anyway.**
With correct scoring, all four arms return **Δ = +0.000, ψ_offline = 0.000** on
file-scoped rows, both modes, n=295 — structural, because a file scope yields
hits that all carry the scoped file's own path. **The rendering cannot affect 46%
of real agent searches.** Not a null — an identity. The gate rests on the 460
directory- and root-scoped queries over 148 instances.

| arm | Δ hit@5 | cluster 95% CI | ψ_offline | b/c | \|b−c\|/n | P1 |
|---|---|---|---|---|---|---|
| `split` | −0.007 | [−0.027, +0.014] | 0.061 | 5/4 | 0.007 | no |
| `prune-kw` | −0.022 | [−0.055, +0.012] | 0.088 | 6/7 | 0.007 | no |
| `prune-decl` | −0.009 | [−0.065, +0.039] | **0.149** | 8/14 | **0.041** | **clears** |
| `champion` | −0.015 | [−0.049, +0.017] | 0.108 | 7/9 | 0.014 | no |

**P1 — one arm clears, pointing down.** Only `prune-decl` meets both halves, at
14 instances worse against 8 better (p=0.286). Every other arm moves instances
symmetrically — discordance without signal, which inflates b+c without inflating
|b−c| and therefore *reduces* McNemar power.

**P3 — falsified, and this is the result.** Registered: `prune-decl` loses by ≤
−0.05. Measured: **−0.009, CI [−0.065, +0.039]** — indistinguishable from the
shipped default. Offline it lost by **0.15 to 0.28 with p<0.001 on every one of
five corpora** (§20.5). The length strata do not rescue the dose law either —
−0.016 / +0.113 / −0.068 / −0.005 across {1, 2, 3–4, 5+} words is not monotone.

**P2 — missed.** `prune-kw` is the *worst* of the four at −0.022 (CI includes
zero, so the kill does not fire, but the ≥0.00 floor is not met). **P4 — holds.**
`split` at −0.007, |Δ| < 0.02, which bounds every rendering above it.

**Tripwire 1 tripped, on a mis-set threshold.** `prune-decl` bm25 Δ = +0.011
against a registered ≤0.005 (CI [−0.002, +0.025], includes zero). bm25 output
passes through MMR, which reads the embedding matrix (§14.4 point 6), and
`prune-decl` perturbs it most; §14.4 recorded the identical tripwire for the
identical reason. Registering it a second time at an unreachable threshold is the
error, not the engine. Tripwires 2 and 3 passed.

**Decision: phase 2 is not bought.** At ψ=0.149 a 40-instance tier yields ~6
expected discordant pairs against the 6 all-one-way needed for p<0.05, which the
8/14 split already contradicts. The other three arms are below the floor outright.

**What this licenses.** Third confirmation of §9.7's rule (after §9.7 and §10.6),
and the first with the size of the miss measured: an offline deficit of 0.15–0.28
at p<0.001 corresponds to −0.009 [−0.065, +0.039] on real agent queries.
**Offline retrieval eval on generated queries does not predict agent-regime
behaviour for a rendering change** — not merely "gains fail to transfer", but
losses fail to transfer too, which is the stronger and more useful form.
`prune-kw` is withdrawn as a shipping candidate. `split`+`sif` remains the default.

**What it does not license.** Nothing about agent *accuracy*. Nothing about
renderings on descriptive queries — §20.9's linux +0.090 [+0.035, +0.146] p=0.002
stands. And nothing about the 46% of searches that are file-scoped, where no
rendering can matter by construction; if that share is worth attacking, the lever
is scope handling, not rendering.

---

## 22 Rescuing the keyword lever, and making the file-scoped half measurable

§21.2's two negatives — `prune-kw` worst at −0.022 despite two significant
offline wins, and 46% of agent searches returning Δ = exactly +0.000 — are
defects rather than findings.

### 22.1 Pre-registration (written before the first row)

**Root cause 1 — the table fires in the wrong position.** `prune-kw` deletes
tokens that are *identifier components*. Measured against the 421 gold function
names agents were hunting in §21:

| rule | gold function names damaged |
|---|---|
| naive (drop the subtoken anywhere) | **20.9%** (88 of 421) |
| positional (drop only a whole-run keyword) | **0.7%** (3 of 421) |

`__init__` alone is 30 of the 88; the rest are `from_*`, `as_*`, `for_*`, `in_*`.
When an agent searches `__init__`, `prune-kw` deletes `init` from the query *and*
from every chunk, so the function is unfindable by the name it has. `prune-kw`
stays frozen; `prune-kw-pos` is the repair, and it cuts queries less — 9.1% of
agent query tokens against 13.7%.

**Root cause 2 — the file-scope zero is a metric artifact.** `guessplay` scored
`rank_of_gold(hits, gold_files)`; under a file scope the rank histogram over
2,928 file-scoped rows is exactly `{1: 1050, None: 1878}`, no other value occurs.
But the endpoint is `func_acc@10_tol`, and *within-file chunk order decides which
functions the agent sees*. §22 scores those rows at function level: `SearchHit.line`
containment → innermost `symbols.extract` span → `scoring.func_match(...,
tolerant=True)`. Tolerant only: 704 of 1,149 gold quals are dotted while
`symbols.extract` yields bare leaves, so `func_acc@*_strict` is not computable.

**The design is a 2×2** over {naive, positional} × {symmetric, query-untouched},
completed by `default`, `prune-kw`, `prune-kw-pos`, `prune-kw-pos-q0`; `split`
and `prune-decl` ride along for continuity.

**Registered predictions.** **1.** `prune-kw-pos` − `prune-kw` ≥ **+0.02** hit@5,
cluster bootstrap over instances (4,000, seed 1); *kill:* below that, the
identifier-component story is wrong and the keyword lever is closed rather than
re-tuned a third time. **2.** The gain concentrates: Δ on the 21% of instances
whose gold function name contains a table word ≥ 2× Δ on the remainder — "a
uniform gain means prediction 1 passed for the wrong reason." **3.** The
query-side axis, registered **without a preferred direction**: §20.6's dose law
says the 9.1% unmirrored share costs ≈ −0.02, against which chunk boilerplate is
*obligatory* while a query token is *elective*. Two-sided, |Δ| ≥ 0.02 to call it;
**a null is the most likely and most useful outcome**, putting 9.1% below the
dose law's detection floor and letting the simpler rule win on parsimony. **4.**
The `def`/`class` sub-test: those two are 84% of the disputed share (169 + 84 of
~300), so if `prune-kw-pos-q0` wins the gain must **not** come predominantly from
the 253 queries containing them. **5.** Function-level scoring makes file scopes
discriminative: ψ_offline > 0 on file-scoped rows for at least one arm, against
the current exact 0.000. **6.** Tripwire — bm25 unchanged beyond the
MMR-mediated drift. **7.** Tripwire — one `bin_sha256`.

A null on prediction 1 licenses that the keyword lever is closed: two repairs,
both measured, neither transferring. It does **not** license any claim about
renderings on descriptive queries (§20.9's linux +0.090 [+0.035, +0.146] p=0.002
stands) nor about agent *accuracy* (§11.5).

### 22.2 The repair works, and it buys nothing

Run 2026-08-05, `guessplay-v2.jsonl`, one binary (`e09664634db0c898`), 854
desc-v9 ranked queries over 186 instances, six configs, semantic + bm25, both
metrics. 10,248 rows, perfectly balanced (1,708 per config).

**P1 — passes, exactly at its floor.** `prune-kw-pos` − `prune-kw` = **+0.022, CI
[+0.010, +0.035]**, excluding zero. The first positive result in the §20–§22 arc.

**P2 — fails, and it invalidates P1's stated mechanism.**

| stratum | n | `prune-kw` | `prune-kw-pos` | Δ | 95% CI |
|---|---|---|---|---|---|
| gold name damaged | 123 | 0.423 | 0.447 | **+0.024** | [+0.000, +0.054] |
| gold name intact | 338 | 0.426 | 0.447 | **+0.021** | [+0.007, +0.036] |

1.14×, not 2×. Recovering `__init__` is *not* what happened.

**What actually happened, and it is the finding.** Positional pruning deletes
less, so it converges on not pruning at all:

| arm | vs `default` (no rendering) | vs `split` (no keyword pruning) |
|---|---|---|
| `prune-kw` | −0.022 [−0.055, +0.012] | −0.015 [−0.043, +0.010] |
| `prune-kw-pos` | **+0.000** [−0.030, +0.033] | **+0.007** [−0.018, +0.031] |

`prune-kw-pos` is indistinguishable from doing nothing on both baselines and both
metrics (function-level: −0.002 [−0.023, +0.017] vs default). **The +0.022 is not
a gain over the baseline; it is the removal of a self-inflicted loss.** The
keyword table's whole measurable contribution on real agent queries is the damage
it does. §20.9's offline wins stand as offline facts and remain the third
instance of §9.7's rule.

**P3 — the query axis is a null, and parsimony decides it.** `prune-kw-pos-q0`
against `prune-kw-pos`: −0.007 [−0.025, +0.010] file-level, +0.004 [−0.006,
+0.016] function-level, both far inside ±0.02. **9.1% of query tokens is below
the dose law's detection floor in this regime**, so §20.6's rule does not extend
here — not because it is wrong, but because the effect is too small to see at
this share. **Do not touch the agent's query.** An agent's tokens are elective
and the engine gains nothing measurable by second-guessing them.

**P4 — moot.** Conditional on `prune-kw-pos-q0` winning; it did not.

**P5 — passes, and it recovers half the corpus.** ψ_offline **0.050–0.058**
against the exact 0.000, with real discordance (3/4, 3/4, 1/5). Function-level
hit@5 on file scopes is **0.193**, *higher* than directory-scoped 0.157 — the
half §21.2 wrote off is both measurable and more productive than the half we were
scoring. Future work has 100% of the corpus available rather than 54%.

**P6 — tripwire holds.** `prune-kw-pos` vs `prune-kw` in bm25 is **+0.000
exactly**, both metrics, ψ=0; all arms sit at +0.002 [+0.000, +0.007] against
`default`, the identical MMR-mediated drift §14.4 documented. **P7 — one binary**
across all 10,248 rows.

**Ledger for §22.** Two of seven predictions passed as stated (P1, P5), one
failed and took P1's mechanism with it (P2), one is an informative null (P3), two
are tripwires that held (P6, P7). The keyword lever is closed: repaired, it
reaches parity with an unrendered index and no further. What §22 leaves behind is
a scoring instrument that can see every agent search rather than half of them,
and one design rule with evidence behind it — leave the agent's query alone.

---

## 23 The powered agent-regime bound

§21 and §22 each ran 854 queries over 186 instances and returned nulls, bounding
a rendering effect at roughly ±0.03 — wider than any effect this project has ever
shipped on. §23 buys the tighter bound with the corpus already on disk.

### 23.1 Pre-registration (written before the first row)

**Frame.** All `desc-*` conditions: **7,657 ranked queries over 467 instances**
(`guesses-v1-desc-all.jsonl`), **2.51× the instances**, so the cluster bootstrap
narrows by ≈1.58× and §22's [−0.030, +0.033] becomes ≈[−0.019, +0.021]. Arms:
`default` (shipped, no rendering), `split` (base of the ladder), `champion`
(`split`+`sif`, §14.4's offline winner and the standing recommendation),
`prune-kw-pos`. `prune-decl` is dropped after two nulls. Both scopes, both
metrics (`rank`, `rank_func`), semantic + bm25.

Pooling six description regimes is legitimate: they differ in identifier share
(desc-v5 ≈ 45–50%, desc-v8/v9 ≈ 62–65%, §19.11), which widens the *population*
the bound covers. Registered check: report the per-condition cut, and withdraw
the pooled bound if arms disagree in *sign* across regimes.

**Registered predictions.** **1.** No rendering beats `default`: every interval
contains zero on both metrics; *kill:* an interval excluding zero on the primary
metric reopens the direction. **2.** The |CI| half-width on `champion` − `default`
shrinks by 1.4–1.8× against §22's; if not, every interval published on this
corpus is optimistic. **3.** `champion` is not distinguishable from `default`,
|Δ| < 0.02 — **if `champion` loses at this width, the shipped default should
change**. **4.** File scopes stay discriminative, ψ_offline > 0 at function
level. **5.** Tripwire — bm25 |Δ| ≤ 0.005. **6.** Tripwire — one binary.

A clean null licenses: "No document-side rendering moves retrieval on real agent
queries by more than ±0.02, across 7,657 queries and 467 instances spanning six
description regimes." It does **not** license claims about agent *accuracy*,
ranking or chunking levers, or descriptive-query retrieval.

### 23.2 The bound, and the direction closes

Run 2026-08-05, `guessplay-v3.jsonl`. **62,808 rows, 7,657 ranked queries over
467 instances**, six description regimes, one binary (`eb9aec404d324b56`).

**Semantic mode, directory- and root-scoped, against the shipped `default`:**

| arm | Δ recall@5 | 95% cluster CI | ψ | b/c |
|---|---|---|---|---|
| `split` | **−0.011** | **[−0.022, −0.002]** | 0.037 | 6/8 |
| `champion` (`split`+`sif`) | +0.005 | [−0.013, +0.023] | 0.098 | 16/21 |
| `prune-kw-pos` | −0.007 | [−0.021, +0.007] | 0.063 | 8/16 |

**P1 — holds.** No arm beats `default`; the kill condition did not fire. `split`
excludes zero **downward** at the pooled n: −0.011 [−0.022, −0.002].

> **Amended by §23.3.** That significance is carried by the pooled sample, not
> replicated in the clean half. The point estimate is stable — −0.011 pooled,
> −0.012 post-fix, −0.011 pre-fix — but on post-fix data alone the interval is
> [−0.024, +0.000] and touches zero. The honest claim is **"`split` is
> consistently ≈−0.011 and reaches significance only at the pooled n"**, not
> "`split` is a significant loss".

**P2 — the bound tightened as registered.** §21.2's `champion` half-width 0.033 →
0.018 here, a **1.83×** narrowing against a registered 1.4–1.8×, so the design
effect assumption was mildly conservative rather than optimistic.

**P3 — passes, and it is the actionable one.** `champion` sits at **+0.005, |Δ| <
0.02**. The §14.4 recommendation is **indistinguishable from doing nothing** on
real agent queries at a ±0.023 bound. §14.5 refused it once, §21.2 measured
−0.015, and this settles it at 2.5× the frame: **`split`+`sif` should not be
adopted as the default.** The shipped `EmbedPreproc::None` stands, and the reason
is now a number rather than an absence of evidence.

**P4 — file scopes stay discriminative.** ψ_offline > 0 at function level on
file-scoped rows (0.020). The gap widened: function-level hit@5 is **0.272 on
file scopes against 0.152 on directory scopes** — **1.8× more productive** than
the half we had been scoring. **P5, P6 — tripwires hold.** bm25 deltas +0.002 /
−0.001 / +0.001, all within 0.005; one binary across all 62,808 rows.

**The registered heterogeneity check fired, and it was mis-specified.** `split`
is negative in all five (consistent); `champion` is 3+/2− and `prune-kw-pos`
1+/3−. But **a null arm scatters around zero by construction**, so sign
disagreement among nulls cannot distinguish heterogeneity from noise. Testing
spread against sampling error instead: `prune-kw-pos` 0.019 and `split` 0.012
against an expected 0.058 (noise); `champion` 0.069, marginally above, driven
entirely by desc-v7 — n=92, +0.054, about 1.3 SE. The pooled bound stands. The
check should have been on between-regime variance against sampling variance.

**What §23 licenses.** *No document-side rendering improves retrieval on real
agent queries by more than 0.023, across 7,657 queries and 467 instances spanning
six description regimes — and the ladder's base is 0.011 worse than no rendering
at all.* That closes the rendering direction on a measurement rather than on
exhaustion, and it retires the standing `split`+`sif` recommendation.

**What it does not license.** Nothing about agent *accuracy*. Nothing about
ranking, chunking, or scope handling. And nothing about descriptive-query
retrieval, where §20.9's linux +0.090 [+0.035, +0.146] p=0.002 stands — that
result is real, it simply describes a different task, which is the whole finding
of §21 through §23.

### 23.3 Audit of §21–§23, and one correction

Twelve checks against the raw artefacts.

**What held.** All 69 errored gids error in **all four arms**, so an errored row
penalizes every arm identically; zero pairing drops. Independent recomputation
with fresh code and seed 7: `split` −0.0113 [−0.0214, −0.0019], `champion`
+0.0046 [−0.0133, +0.0214], `prune-kw-pos` −0.0070 [−0.0211, +0.0071] — seed 7
gives an upper bound of 0.021, so **the published 0.023 is the conservative one**.
Corpus provenance is exact: 7,692 ranked `desc-*` invocations in the raw shim
logs against 7,657 replayed, delta **35 — exactly the empty-pattern residuals**.
Replay fidelity **98.0%** on 500 post-fix invocations against the agent's own
stored stdout; the 10 disagreements are k-truncation tail ranks. **The instrument
reproduces what agents actually saw.**

**What did not.** `b49e818` (2026-08-03 16:03) fixed ranked search over a
single-file scope returning nothing, always. **50.1% of the §23 corpus predates
it**, and 58.5% of those queries are file-scoped. Replay fidelity on the pre-fix
half is **62.6%**. Re-run on the clean half only (3,821 queries, 232 instances):

| arm | pooled | post-fix only | pre-fix only |
|---|---|---|---|
| `split` | −0.0113 **[−0.0215, −0.0022]** | −0.0119 [−0.0244, **+0.0000**] | −0.0107 [−0.0236, +0.0019] |
| `champion` | +0.0046 [−0.0134, +0.0225] | −0.0060 [−0.0295, +0.0157] | +0.0169 [−0.0078, +0.0418] |
| `prune-kw-pos` | −0.0070 [−0.0212, +0.0067] | −0.0114 [−0.0302, +0.0055] | −0.0019 [−0.0205, +0.0166] |

The **point estimates replicate** across the confound, so the effect is not an
artefact of the broken half; what does not replicate is the *significance*, a
function of n (378 instances pooled against 194). **The ±0.023 bound is
unaffected and in fact tightens post-fix** (champion's upper bound 0.0157).

**Harness gaps found.** (1) Two definitions of "is this scope a file" in one file
disagree on exactly one scope — `.github`, a dotfile *directory* — affecting 4
rows of 30,628 (0.013%); immaterial, and a latent trap. (2) `bin_sha256`
fingerprints the binary, not the source: it changed between the §22 and §23 runs
with `crates/` byte-identical (a relink), so the tripwire can false-alarm but
never false-pass. (3) The pre/post-fix split is not recorded in the corpus, so
any future campaign inherits the same 50% contamination silently.

**What this audit does not cover.** It validates the *replay* against agent
stdout and the *arithmetic* against the raw rows. It does not validate
`symbols.extract`'s function spans against ground truth — the function-level
metric rests on a regex extractor that under-counts by design, so `rank_func`
figures should be read as a lower bound on within-file discriminability rather
than as a calibrated rate.

---

## 24 The within-file gap, and the metric that was hiding it

§23 closed the document-rendering direction at 0.023 and left behind an
instrument that can see file-scoped searches, and a gap nothing has ever been
aimed at. This section measures how much of a lower bound `rank_func` is, and
tests three candidates against the corrected instrument.

### 24.0 What the reproduction established

All 2,188 file-scoped agent searches from `guessplay-v3.jsonl` were re-executed
live against files restored from the pinned git mirrors at each instance's
`base_commit`. 2,149 completed, and **all 2,149 reproduce their recorded
`rank_func` exactly** — the harness is faithful even though `bin_sha256` has
moved since.

The funnel, on the shipped default (`EmbedPreproc::None`, semantic):

| | n | share |
|---|---|---|
| ranked agent searches | 7,657 | |
| — scoped to one file | 4,216 | **55.1%** |
| — — aimed at a file holding no gold function | 2,028 | 48.1% of file-scoped |
| — — aimed right | 2,188 | 51.9% of file-scoped |
| — scoped to a directory or the repo root | 3,441 | 44.9% |

Of the 2,188 that aim right, the gold function is in the top 5 **52.9%** of the
time. Of the 803 that never surface it, **801 had file-level rank 1**: the engine
returned chunks, from the right file, and none were credited to the right
function.

**The cut that matters.** Whether the query contains the gold function's own name
is the only cut tested that separates them, and it survives both metrics:

| | n | share | strict@5 | chance@5 | lift | overlap@5 |
|---|---|---|---|---|---|---|
| names the function | 670 | 31% | 76.1% | 23.5% | **3.2×** | 87.3% |
| describes it instead | 1,479 | 69% | 42.4% | 26.5% | **1.6×** | 58.0% |

Chance is computed exactly per file, `1 − (1 − p)⁵` over the union of gold spans.
The median file-scoped query is *two words*. **69% of the traffic describes, and
there the engine is barely above chance.**

Three mechanisms ruled out. **Not name ambiguity** — the gold name appears a
median 3 times when found and 4 when missed, and the top-5 rate does not fall
monotonically with occurrence count. **Not the extractor going blind** — all 648
distinct (instance, scoped file, gold function) triples resolve in **100%** of
cases. **Not big files** — measured as lift the engine *improves* with size,
reaching 8.7× chance above 2,000 lines.

### 24.1 Pre-registration (written before the first campaign row)

**The metric is the first finding, and it changes what the rest can measure.**
`rank_func` credits a hit only when the chunk's best-matching *line* falls inside
the gold function. Chunks are 32 lines; the median gold function is 12. Scoring
the same 2,149 searches by whether a returned chunk *overlaps* the gold function:

| | @5 |
|---|---|
| strict (`rank_func`, what §22 and §23 publish) | 52.9% |
| overlap (`rank_func_ovl`) | 67.1% |
| **bracket** | **14.2pp** |

with the spread +19.8pp on gold functions under 10 lines and +6.8pp on 30–99 line
ones — the signature of chunk granularity, not of ranking. Of 160 named top-5
misses, **75 are recovered by overlap (the measurement) and 85 are genuine**.
§22.1 chose strict deliberately, because overlap credit "would blunt the very
ordering this is built to measure", and that reasoning still holds. What it did
not anticipate is the *size* of the understatement: 14.2pp is larger than every
effect §20–§23 tried to detect. **Neither number is the truth** — strict
under-credits short functions, overlap over-credits a window that merely brushes
one — so both are emitted always, and a result that moves only one of them is a
result about the metric.

**Three candidates, each an independent flag, measured factorially.** **#1 the
same-file dedupe** — `hit.rs` drops any candidate whose span overlaps an
already-kept candidate in the same file; on the `update_sources` case the chunk
holding the declaration overlaps two higher-scoring neighbours that each contain
a *call site*, and under `--overlap 0` it goes from absent to **rank 2**. That
crude proxy is worth **+2.0pp overlap@5, CI [−0.001, +0.043]** across all 1,542
distinct file-scoped queries. MMR is *not* the cause: `--no-diversify` swaps two
ranks and rescues nothing. **#3 a finer, wider pass at file scope** — a file
scope never resolves an index, so it is always the streaming path (44.7 ms over
37 chunks, `candidate_width(k) = k*3` capping the pool at 30); both window and
cap are affordable to change there and nowhere else. **#2 declaration-aware
scoring** — `prose::declaration_sites()` exists, built for `PruneDecl`; §22
showed using it to *delete* tokens buys nothing, using it as a ranking feature is
untested.

**Registered predictions.** **1.** The bracket is real and sized: overlap@5 −
strict@5 ≥ **+0.10**, and ≥2× larger under 10 lines than over 30 — *a check on
the metric fix itself*. **2.** `--dedupe-overlap 0.5` − `0.0` ≥ **+0.02**
overlap@5 (cluster bootstrap, 4,000, seed 1); *kill:* below +0.01. **3.** #1's
gain concentrates ≥2× on rows where a higher-scoring neighbour overlaps the gold
span — a uniform gain means prediction 2 passed for the wrong reason. **4.** #3 ≥
**+0.02** strict@5 with the gain larger on functions under 10 lines than over 30;
a flat profile falsifies the dilution mechanism. **5.** #2 recovers ≥ **20** of
the 85 genuine named misses, **two-sided on the describe half** because a
declaration boost could plausibly hurt descriptive queries. **6.** Tripwire — no
arm loses more than 0.01 file-level `rank@5` on directory scopes in the
confirmation run. **7.** Tripwire — one binary and one `arm_flags` per arm.
**8.** Tripwire — cold == warm; #2 must be mirrored on both paths.

**What a null on all three licenses.** That within-file ranking is not reachable
by candidate-set or scoring changes of this kind, and the remaining lever is the
*query* side. It does **not** license a claim about the 48% of file-scoped
searches aimed at the wrong file.

**What this cannot settle.** The recoverable pool — right file, gold function
outside the top 5 — is **9–13% of all agent searches** depending on the metric,
and that is a *ceiling, not a backlog*. Some unknown share is the agent having
asked a different question than the benchmark grades: when a query reads
`periodic task maintenance loop` and the engine returns `_loop_coroutine` while
gold is `_send_message`, the engine was right and the query pointed elsewhere.
Separating the two needs query-intent labelling, which nothing in the harness does.

### 24.2 One of three, and the two that died on their own floors

Run 2026-08-06, `guessplay-v4.jsonl`, one binary (`ef37824e9d3b71e8`), 33,728
rows: a full 2×2×2 on the file-scoped half of the desc-all corpus, 402 instances,
semantic mode. 17,504 rows land on a file that holds a gold function; 2,188
queries (331 instances) are paired across all eight arms.

**Main effects, each lever averaged over the other two** (four paired contrasts
each, cluster bootstrap over instances, 4,000, seed 1):

| lever | strict@5 | overlap@5 |
|---|---|---|
| #1 dedupe 0.5 | −0.003 [−0.011, +0.005] | **−0.009 [−0.017, −0.000]** |
| #3 file-window 12 | +0.008 [−0.013, +0.028] | **−0.052 [−0.075, −0.030]** |
| #2 decl-boost 1.0 | **+0.027 [+0.006, +0.049]** | **+0.033 [+0.013, +0.052]** |

**P1 — passes, both clauses.** The bracket on the control arm is **+14.4pp**
(52.4% strict, 66.8% overlap) against a floor of +10.0pp, and it is +19.8pp under
10 lines against +7.2pp over 30 — a 2.75× ratio against a registered 2×. Every
number in §22 and §23 about file scopes is a lower bound by roughly this much.

**P2 — killed, and it takes the default with it.** Registered at ≥ +0.02
overlap@5 with a kill below +0.01. Measured: **−0.009 [−0.017, −0.000]**, a small
*significant loss*. The mechanism is real but the case is not the population:
keeping neighbours crowds the top-k with one file's chunks more often than it
rescues the right one, which the snapshot showed plainly when 85 of 114 cases
moved and three `native/ring.c` chunks took slots other files had. **The default
is reverted to 0.0 and the snapshot is byte-identical to its pre-§24 state.**
*What misled the plan:* §24.1 sized the lever from `--overlap 0` (+2.0pp
[−0.001, +0.043]), a proxy that changes *chunking* and **inverted the sign of the
thing it stood in for**. A one-case rescue plus a proxy is not evidence.

**P3 — moot.** Conditional on P2, which failed.

**P4 — the mechanism confirms while the lever fails.** `--file-scope-window 12`
measured +0.008 [−0.013, +0.028] against a registered ≥ +0.02: null. But the
discriminating clause passes cleanly — **+4.8pp strict on gold functions under 10
lines against −1.1pp on those over 30**. Dilution is real and finer chunks do
address it; they also cost more than they pay, overlap@5 falling **−0.052
[−0.075, −0.030]**, because a 12-line chunk brushes a gold function far less
often than a 32-line one. **A lever that moves the two metrics in opposite
directions is changing chunk geometry, not retrieval quality** — and §22/§23 had
no way to see that distinction.

**P5 — passes, and on both halves.** `--decl-boost 1.0` recovers **58 of the 92
named rows** the control missed at overlap@5, against a floor of 20. The
registration was two-sided on the describe half; it does not hurt:

| | n | Δ overlap@5 |
|---|---|---|
| query names the gold function | 685 | **+0.069 [+0.025, +0.117]** |
| query describes it instead | 1,503 | **+0.039 [+0.009, +0.066]** |

The best arm in the factorial is decl-boost alone — **56.3% strict / 71.6%
overlap** against the control's 52.4% / 66.8%.

**This is the first engine change in §20–§24 to beat an unrendered index on real
agent queries.** §20–§23 spent four sections on what a chunk is *made of* and
found a bound of 0.023; this changes what a chunk is *worth* and clears it. A
chunk that declares an identifier and a chunk that calls it were scored alike,
and for a query that names a function those are not the same answer.

**P6 — pending the confirmation run.** The factorial ran `--file-scopes-only` and
is blind to directory scopes by construction. **P7 — one binary**
(`ef37824e9d3b71e8`) across all 33,728 rows, one `arm_flags` per arm, verified
live: on the first 750 paired queries each lever changed 15–28% of rows, so no
arm was a silently-unwired null. **P8 — cold == warm** holds with the boost on,
asserted by `cold_and_warm_agree_with_the_declaration_boost`, which also asserts
the boost reorders something on the fixture — an inert boost would satisfy the
equality trivially.

**Ledger for §24 so far.** Three of eight predictions pass as stated (P1, P5,
P8), one is killed on its own floor and reverts a default (P2), one is moot (P3),
one fails as a lever while confirming its mechanism (P4), one tripwire held (P7),
one is outstanding (P6).

### 24.3 The weight sweep (registered before the run)

§24.2 measured `--decl-boost` at **w = 1.0**, a first guess. P6 is now discharged
and the lever is a shipping candidate, so the weight gets chosen deliberately.
Arms 0.0 (control), 0.5, 1.0, 2.0, 4.0 on the 2,188 paired file-scoped queries.

**A sweep over the corpus that established the effect cannot also establish its
size.** Two commitments, registered: **(1)** the effect is not re-estimated by
this run — its size is the independent full-corpus confirmation of §24.2 at
w = 1.0: **+0.039 [+0.015, +0.062] strict and +0.048 [+0.024, +0.072] overlap on
file scopes, +0.017 [+0.007, +0.029] bm25 on directory scopes**, and a larger
figure produced by the selected arm is a selection artifact. **(2)** The rule is
parsimony, not argmax: take the *smallest* weight whose overlap@5 gain has a CI
excluding zero and whose point estimate is within 0.01 of the best arm. *Kill:*
if no weight clears zero, the §24.2 result does not replicate and the lever is
withdrawn rather than tuned.

**Result: flat, and 0.5 wins on parsimony.** Run 2026-08-06,
`guessplay-v6.jsonl`, one binary (`8bc13ebc1071f3e4`), 21,080 rows.

| w | strict@5 | overlap@5 | Δ overlap vs w=0 |
|---|---|---|---|
| 0.0 | 52.4% | 66.8% | (control) |
| **0.5** | **56.6%** | 71.3% | **+0.046 [+0.025, +0.067]** |
| 1.0 | 56.3% | 71.6% | +0.048 [+0.023, +0.072] |
| 2.0 | 55.8% | 71.4% | +0.047 [+0.021, +0.072] |
| 4.0 | 56.4% | 71.3% | +0.045 [+0.018, +0.071] |

Every arm clears zero, so the kill does not fire and §24.2 replicates on a second
binary. The spread across an **8× range of w is 0.003** — inside the noise of
every individual interval.

The flatness is the finding, not an inconvenience. A multiplicative boost whose
effect is invariant to its own magnitude is acting as a **reordering signal**,
not a score adjustment: what matters is that declaring chunks sort above calling
chunks, not by how much. It also makes the default safe — the failure mode of a
large `w` (one declared token dominating a fused score) never fires here, and
choosing the smallest effective weight means it cannot start firing on a corpus
this one does not resemble.

Per the first commitment, **the published effect for the lever remains the
independent full-corpus confirmation at w = 1.0** — +0.039 [+0.015, +0.062]
strict, +0.048 [+0.024, +0.072] overlap, +0.017 [+0.007, +0.029] bm25 on
directory scopes. The 56.6% strict above is the argmax of five arms on the corpus
that selected it and is not quoted as the effect size.

**Shipped**: `decl_boost` defaults to 0.5. Cost 1.1–1.5 ms, flat in corpus size
(the `k*3` candidate chunks it re-reads), ~3% of a warm kernel query. Snapshot
re-recorded — 78 of 114 cases move.

### 24.4 Ledger

| # | prediction | outcome |
|---|---|---|
| 1 | the bracket is real and sized | **pass** — +14.4pp, 2.75× on short functions |
| 2 | #1 dedupe ≥ +0.02 | **killed** — −0.009 [−0.017, −0.000]; default reverted |
| 3 | #1's gain concentrates | moot (conditional on P2) |
| 4 | #3 finer window ≥ +0.02 | **fails as a lever, mechanism confirmed** |
| 5 | #2 recovers ≥20 named misses | **pass** — 58 of 92, and both query halves gain |
| 6 | directory half loses ≤ 0.01 | **pass** — it *gains*, +0.017 bm25 |
| 7 | one binary, one arm_flags | **pass**, verified live |
| 8 | cold == warm | **pass** |

Two candidates died on floors written before the data existed, and both were
argued for from a single vivid case. **#1 was sized by a proxy that measured
something else** — `--overlap 0` was worth +2.0pp and looked like evidence for
the dedupe rule; it changes chunking, and the real rule is −0.009. **#3 was right
about its mechanism and wrong about its value** — finer chunks demonstrably fix
dilution (+4.8pp strict under 10 lines) and cost more than that elsewhere
(−0.052 overlap). Only the two-metric bracket §24.1 built could tell those apart.

**What §24 does not claim.** Every number here is retrieval quality on replayed
queries. §11.5 stands: whether this changes what an agent *does* is not
purchasable on this benchmark at any n it can hold, and the 9–13% recoverable
pool remains a ceiling containing an unknown share of queries that point
somewhere other than gold. What changed is that the direction §23 closed is not
the only one, and the instrument can now see the half of agent behaviour that §21
wrote off.

### 24.5 Reproducing §24

`eval/data/` is gitignored, so the three campaign files are not in the tree. All
three are `guessplay.py --corpus eval/queries/guesses-v1-desc-all.jsonl --configs
default --scopes orig` with arms passed as `--extra-search-flags`:

- **§24.2 — the 2×2×2** (`guessplay-v4.jsonl`, 33,728 rows, ~35 min, no index
  builds): `--file-scopes-only --modes semantic`, arms the full cross of
  `--dedupe-overlap {0.0,0.5}` × `--file-scope-window {0,12}` ×
  `--decl-boost {0.0,1.0}`.
- **§24.2 P6 — the full-corpus confirmation** (`guessplay-v5.jsonl`, 31,668 rows,
  both scopes, ~1 h): `--modes semantic,bm25`, arm `--decl-boost 1.0`.
- **§24.3 — the weight sweep** (`guessplay-v6.jsonl`, 21,080 rows, ~25 min):
  `--file-scopes-only`, arms `--decl-boost {0.5,1.0,2.0,4.0}`.

Read any of them back with `--compare-by arm_flags --compare-metrics
rank,rank_func,rank_func_ovl`, through the shipped harness rather than an ad-hoc
script. Two things the comparator does *not* do. It reports the whole scoped
population, so its file-scope rates (0.347 → 0.371 overlap@5 on v5) are diluted
by the 48% of file scopes that name a file holding no gold function and are
`None` for every arm; §24.2's rates are the right-file subset. And it contrasts
one arm against one base, so the *main effects* come from averaging the four
paired contrasts per lever.

---

## 25 What the agent is shown, not what the engine scored

The engine scores 32-line windows and prints **one line** per hit, so "the
answer was in the returned window" and "the agent saw the answer" differ by 14
points (§24.1's bracket).

Measured over 400 real file-scoped agent searches: of the 294 where the returned
window contained the answer, **77 (26%) showed the agent a line belonging to
something else** — median 7 lines away, and 64 of those inside a different
function entirely. §25 tests two ways to close that on the only instrument that
can see the difference: real agents.

### 25.1 Pre-registration (written before the first paid run)

**Neither candidate changes ranking, so `guessplay` cannot referee either.**
Offline replay measures which chunks come back; this question is about what the
agent does with them — which is what made this the first campaign worth buying.

**The two formats, costs re-measured at k=10 over 150 real agent searches:**

| | median bytes | vs today |
|---|---|---|
| today — one line per hit | 552 | 1.0× |
| `--headers` — span + declared names before each hit | 1,113 | **2.0×** |
| `--full` — every line of all 10 chunks | 11,315 | **20.5×** |

*(An earlier estimate put `--headers` at 314 bytes; that was derived from a k=3
example and is corrected here.)*

Full chunks never repeat a line: across 10,935 pairs of returned hits **zero
overlapped**, because §24.2's dedupe rule drops any chunk sharing a line with a
better one.

**What is purchasable, computed before proposing the spend.** §11.5 and §19.10
concluded agent *accuracy* is unpurchasable — ±0.038 at all 560 instances
against effects always ≤0.05. The endpoint these formats target is behavioural.
From the 3,502 transcripts already on disk:

| endpoint | paired sd | instances for 80% power |
|---|---|---|
| **reads-after-search per run** | 1.48 | 69 at Δ=0.50, **275 at Δ=0.25** |
| cost per run | $0.148 | 35 for a 25% change |
| input+cache tokens | 270k | 96 for a 25% change |
| `func_acc@10_tol` | — | 682 (§19.10) — never |

Baseline is **1.85 reads-after-search per run**, so Δ=0.25 is a 14% reduction.
*(Corrected before the run: an earlier pass counted a search by any of the four
shimmed tool names and got 1.98. `displaycmp.py` counts only searches by the
arm's **own** tool — an arm told to type `sg` that emits `semgrep` is escaping
its treatment. The paired sd is 1.48 either way.)*

**Design: four arms × 280 instances** — `rg`; `disp-line` (desc-v9, shipped,
internal control); `disp-full` (desc-v9 + `--full`); `disp-head` (desc-v9 +
`--headers`). The three `sg` arms are byte-identical except for a flag `shim.py`
injects invisibly, "appended to the real invocation but never shown to the agent
— its commands and the logged argv stay clean", so the contrast is display and
nothing else. `disp-line` cannot be replaced by reusing existing desc-v9 rows:
those came from a pre-§24 binary, which is the §23.3 trap exactly.

**Registered limitation.** desc-v9 says output is `path:line:text`, which
under-describes `--full`. Varying the description per arm would confound display
with the strongest lever this project has measured (§19: 7%→98% ranked share),
so it is held identical and `--full` runs *handicapped by a description that
undersells it*. A win is therefore strong; a null is ambiguous.

**Frame: a plain random 280 of 560, seed 25,
`eval/data/locbench/display-frame-280.json` (sha256 `80bda274604a0062`)** —
deliberately not `tierframe.py`'s equal strata, since no §19.2b-style stratum
prediction applies and the primary endpoint is continuous.

**Registered predictions:** (1) **primary** — `--full` reduces
reads-after-search vs `disp-line`, paired, bootstrap CI (4,000, seed 1), powered
to **Δ=0.25**; a positive delta falsifies the mechanism. (2) **co-primary** —
cost and tokens, registered as an *expected loss*, powered to a 25% change at
n=35/96. (3) `--headers` achieves ≥ half of `--full`'s reduction at ~2× rather
than ~20× the bytes. (4) accuracy (`func_acc@10_tol`) is a bounded, unpowered
secondary, Holm-corrected, bound printed beside it. (5) `disp-full` vs `rg`, the
product claim, same bound. (6) **tripwire** — truncation, since the agent's
tool-result limit silently deletes hits ranked below a long one. (7) tripwire —
the three `sg` arms' `tool_line_text` byte-identical. (8) tripwire — one binary,
`triage.py` clean per tier. (9) **gate** — `queryshape.py`: if display changed
how agents *write* queries, every downstream reading is conditional on that.

**Budget, re-priced on a 6-instance × 3-arm smoke:** `disp-line` $0.280/run,
`disp-head` $0.266 (0.95×), `disp-full` $0.359 (**1.28×**, not the 4.3× a single
instance had suggested). With `rg` at its historical $0.283 that is **~$332 for
280 × 4**. The same smoke showed `disp-full` using fewer searches than the
control (3.8 vs 6.7 over 6 instances) — noted here only because it was visible
before the frame ran.

### 25.2 Full chunks change what agents do; headers change nothing

Run 2026-08-06/08, `results-display.jsonl`, 1,156 rows, **278 instances complete
in all four arms** — the frame delivered its registered power (detectable
Δ=0.249 against a registered 0.25). Spend **$295.76**, under the $317 estimated.

**P1 — passes, at three times the registered effect.** Paired over 280:

| endpoint | control | `--full` | Δ | 95% CI |
|---|---|---|---|---|
| **reads after a search** | 1.729 | 0.921 | **−0.807** | [−1.007, −0.611] |
| reads (all) | 3.293 | 1.821 | −1.471 | [−1.764, −1.189] |
| searches | 3.418 | 2.789 | −0.629 | [−0.932, −0.354] |
| turns | 9.13 | 6.99 | **−2.14** | [−2.72, −1.58] |
| median search bytes | 609 | 12,630 | +12,021 | [+11,150, +12,927] |

The agent opens the file after a search **47% less often**, and searches less as
well as reads less.

**P2 — the cost is real, and not where it was expected.** `--full` costs
**+$0.042/run [+0.024, +0.059]**, about 18%:

| usage | control | `--full` | Δ |
|---|---|---|---|
| output tokens | 2,692 | 2,325 | **−367** [−645, −78] |
| cache **read** tokens | 272,524 | 261,316 | −11,207 (null) |
| cache **creation** tokens | 17,684 | 26,106 | **+8,421** [+6,464, +10,417] |

Twenty times the bytes per search produces *no* significant change in total
tokens read and *fewer* output tokens, because the shorter trajectory cancels
the bigger results. The entire premium is **cache creation**: each large tool
result is a new block that must be written, and writes are the expensive
direction. §2.1's framing survives — "fewer, better round-trips beat cheaper
individual round-trips" — and this is that trade priced: **2.14 fewer
round-trips for 18% more dollars.**

**P3 — fails, and it is the most interesting failure here.** `--headers` was
registered to deliver ≥ half of `--full`'s reduction (≤ −0.40). Measured:
**−0.007 [−0.175, +0.168]** on reads-after-search, +0.179 on reads, 0.000 on
searches — a flat null on every behavioural endpoint, at 1.9× the bytes.

§25.1 sized headers from the finding that naming a chunk's declarations would
surface the gold function in **88%** of the cases where the shown line missed
it. That number was about *availability*, and it was correct. It predicted
nothing, because **an agent that is told the answer is nearby still opens the
file.** Only being handed the code removes the reason to. "Availability is not
use, and 88% of a gap closed on paper bought exactly zero behaviour."

**P4 — accuracy unmoved, and bounded rather than implied.** `func_acc@10_tol`:
`--full` **+0.000 [−0.032, +0.032]**, `--headers` −0.011, `rg` −0.011; McNemar
p = 1.000/0.648/0.629. `file_acc@5` likewise null. This frame resolves ±0.032,
so a smaller effect is not excluded and not claimed. **"The display format
changes the route, not the destination."**

**P5 — `--full` beats `rg` on every efficiency endpoint**, paired over 280:
reads-after-search **−0.504 [−0.682, −0.339]**, reads −0.721 [−0.964, −0.496],
searches −0.696 [−1.007, −0.386]. `rg` also beats the *control* on
reads-after-search (−0.304): one line of grep output is a weaker invitation to
open a file than one line of ranked output, presumably because grep's line is
the literal match. Full chunks beat both.

**P6 — truncation tripwire holds.** Zero truncated search results in any arm,
including 12.6 KB medians — the failure mode `out.rs` documents from a 659 KB
incident did not occur once in 280 runs.

**P7 — descriptions byte-identical** across the three `sg` arms
(`tool_line_sha256`, one distinct value).

**P8 — fired, and benign on inspection.** Eight distinct `semgrep_sha256`
collapse to **two** once historical runs on the same instances are excluded; no
commit touched `crates/` during the campaign and the two hashes are distributed
near-identically across arms (248/41, 249/41, 253/44, 248/40) — §23.3's finding
2, that the fingerprint tracks the link and not the code, and can false-alarm
but never false-pass. `triage.py` failed three checks at 63 rows (one unknown
flag `--iC`, one instance whose every search used a nonexistent path, four
non-ok rows); errors were **arm-symmetric** (rg 2, line 1, full 2, head 2) with
five of seven from a single instance failing in every arm. The check that would
have implicated the treatment — ranked searches returning nothing, the §16.11
signature — was 0 of 213.

**P9 — the gate passes: agents did not change how they write.** `disp-full` vs
`disp-line` identifier share 67% vs 67%, plain-word 25% vs 27%, paraphrase 3% vs
3%, mean words/query +0.42.

**Ledger.** Seven of nine as registered (P1, P2, P4, P5, P6, P7, P9), one
decisive failure (P3), one tripwire fired and diagnosed (P8).

**What ships.** `--full` is the first change in this project measured to alter
agent behaviour: half the file-reopening, two fewer turns, same accuracy, 18%
more cost — a trade rather than a free win, and a *default* question rather than
a *feature* question. `--headers` is measured and not adopted: 1.9× the bytes,
nothing an agent does differently.

**What this does not settle.** Accuracy is bounded at ±0.032 and untouched, so
none of this is evidence that agents *solve more*; what it buys is a shorter
route to the same answer. The 18% is charged on a cache-write behaviour that is
a property of this harness's caching, not of the format.

### 25.3 Three analysis bugs, caught before the data existed

All three were found by running the analyser on partial data rather than waiting
for the frame, and each would have produced a confident wrong answer on a $296
campaign: **the sign was inverted against its own label** (`boot_ci` returns
`mean(first) − mean(second)` and pairs were passed `(base, cand)` under headings
reading `cand − base` — the primary would have published as **+0.81
reads-after-search, an increase, when it is −0.81**); **it swept in the smoke
runs**, growing n with instances chosen after the fact; and **it paired on
`(run, instance)`**, silently discarding **53 of 278 instances** because a
resumed campaign writes a new run directory (fix: key by instance, latest run
wins). A fourth was a crash — an interrupted run writes a `message` that is a
bare string, and the walker died on exactly the campaigns worth analysing.

**The analysis path deserves the same pre-run verification as the treatment.** A
registered prediction protects against choosing the hypothesis after the fact.
It does nothing about an arithmetic error, and three of the four above would
have survived any amount of pre-registration.

---

## 26 Passages by default, at eighteen lines

§25 established that showing the whole 32-line passage instead of one line
altered agent behaviour: file-reopening fell **1.729 → 0.921** over 1,120
sessions, sessions ran **2.14 turns** shorter, accuracy did not move (±0.032),
and it cost **+18%** — entirely in cache *writes*.

It also killed the cheap alternative. Region headers, sized from a finding that
naming a passage's declarations would surface the answer in **88%** of the cases
a single line missed, moved **nothing** (−0.007 [−0.175, +0.168]).
**"Availability is not use"**: being told the answer is nearby does not stop an
agent opening the file, and only being handed the code does.

That leaves 20× output as the price of a real win. §26 ships the cheaper point
on that curve and buys the campaign that says whether it holds.

### 26.1 Pre-registration (written before the first paid run)

**The shipped default is an 18-line passage, 8 before the match and 9 after.**
Chosen from a coverage/bytes curve over 232 real agent searches: 18 lines holds
**94% of the whole passage's coverage for 46% of its bytes**, and the two steps
beyond it cost 1,520 and 1,921 bytes per point gained against ~460 below.

The extra line goes *after* on measurement, not intuition: 8/9 scores 57.3%,
6/11 and 4/13 both 53.9%, and 0/17 falls to 50.0%. One line of forward bias is
the whole of it.

**Output costs, measured on 150 real searches with the shipped binary** (the
plan's estimates were derived in Python without the line-number prefix):

| arm | flags | median bytes | vs control |
|---|---|---|---|
| `pl-1` | `--passage-lines 1` | 556 | 1.0× — the pre-§26 default |
| `pl-18` | `--passage-lines 18` | 5,874 | **10.6×** — the new default |
| `pl-18k5` | `--passage-lines 18 -k 5` | 2,917 | 5.2× |
| `pl-full` | `--full` | 10,796 | 19.4× — §25's measured winner |

**Shipping before measuring, and why that is defensible here.** The default
rests on a coverage curve, the evidence class that failed for labels. The
difference is that this is a *reduction from a measured winner*: the mechanism
§25 proved — the agent can read the code and stop — is preserved at 18 lines,
and only its sufficiency is unknown. **"If the campaign shows 18 lines loses the
effect, the registered response is to move the default to the whole passage, not
to explain the result."**

**What changed for every caller:** output is ~10× larger; results are separated
by a blank line; a consumer *counting* output lines now sees ~180 rather than
10, though every non-blank line is still `path:line:text`. Three CLI tests
counted lines to count results and all three failed — the canary for exactly
that breakage. `MAX_COLUMNS` still clips every line, so the worst case is ~36 KB
against the ~64 KB of the 32-line arm where §25's truncation tripwire measured
**zero truncated results in 1,120 sessions**.

`tools/snapshot.sh` pins `--passage-lines 1` rather than re-recording: it is a
*ranking* tripwire, and the file stays byte-comparable with every recording
since §20 — which is also the proof that `pl-1` reproduces the pre-§26 output
exactly, on which the control arm depends. The new display shape is pinned by
`the_default_result_is_an_eighteen_line_passage`.

**Design: four arms × 140 instances, ~$157** at the $0.28/run §25 measured.
Every arm passes an explicit `--passage-lines`, so no arm inherits the new
default. Frame: **140 drawn at seed 26 from the 280 instances §25 never ran** —
`passage-frame-140.json`, sha256 `3a8962d12634dbce`, recorded before the first
run, zero overlap with §25's frame by construction. *(Overlap would not threaten
the primary test, a within-campaign paired contrast that re-measures both arms;
the complement is used because the claim was made.)*

**The primary test is non-inferiority, and the margin is what n buys.** At n=140
and the measured paired sd of 1.48, the margin is **0.35**.

1. **Primary — `pl-18` non-inferior to `pl-full`.** The 95% CI on
   (`pl-18` − `pl-full`) reads-after-search must exclude **+0.35**; against
   full's −0.807 that is "18 lines retains at least 57% of it". *Kill:* if the
   CI includes +0.35 the default moves to the whole passage.
2. **Sanity — `pl-18` beats the control**, CI excluding zero.
3. **`pl-18k5`** — the same test at five results, registered as expected weaker
   (§25 measured ranks 6–10 carrying 71.3% → 81.4% coverage).
4. **Co-primary — cost as a prediction:** `pl-18` ~**10%** over control,
   `pl-18k5` ~5%, because §25.2 established the premium is proportional to
   output bytes through cache creation. A cost that does not scale with bytes
   falsifies that mechanism and is worth more than the arm it came from.
5. **Accuracy, bounded and not powered** — `func_acc@10_tol`, bound ~±0.045.
6. **Tripwire — truncation**, zero expected.
7. **Tripwire — one binary, identical descriptions**, `triage.py` recorded
   rather than assumed clean.
8. **Gate — `queryshape.py`**: query style must not shift between arms.

### 26.2 Eighteen lines is worse, and the economy it was for does not exist

Run 2026-08-08, `results-passage.jsonl`, 579 rows, **138 of 140 instances
complete in all four arms** — registered power delivered exactly (margin 0.353
against a registered 0.35). Spend **$140.02**, under the $157 estimated.

**P1 — fails, by 0.014.** The CI on (`pl-18` − `pl-full`) reads-after-search
measured **+0.243 [+0.121, +0.364]** — the upper bound clears the 0.35 margin by
fourteen thousandths. It is *not* merely a power shortfall: the interval
**excludes zero**, so **18 lines is measurably worse than the whole passage**
rather than unproven against it. The point estimate says 18 lines retains 70% of
the effect; the interval cannot rule out 55%.

| arm | reads after a search | vs control | vs `pl-full` | bytes | cost |
|---|---|---|---|---|---|
| `pl-1` | 1.564 | — | +0.800 | 605 | — |
| `pl-18k5` | 1.107 | −0.457 [−0.664, −0.250] | **+0.343 [+0.171, +0.529]** | 3,435 | **−12%** |
| `pl-18` | 1.007 | −0.557 [−0.779, −0.343] | **+0.243 [+0.121, +0.364]** | 7,167 | −3% |
| `pl-full` | 0.764 | −0.800 [−1.050, −0.564] | — | 11,636 | +5% |

**A clean dose-response.** More lines, more effect, monotonically, every
contrast against control excluding zero. That is the opposite of what the
coverage curve implied: 18 lines holds 94% of the whole passage's *coverage* and
only **70% of its behaviour**. §25's lesson lands a second time — availability
predicted behaviour and was wrong again, this time about a quantity rather than
a kind.

**P2 — passes.** `pl-18` beats the control by −0.557 [−0.779, −0.343], so the
primary is a comparison between two live treatments.

**P3 — fails clearly.** `pl-18k5` gives back +0.343 [+0.171, +0.529].

**P4 — the prediction fails and the mechanism survives**, which is the most
useful result here:

| | bytes | cache creation | output tokens | cost |
|---|---|---|---|---|
| `pl-18k5` | 5.7× | **−3,207** [−4,679, −1,661] | −240 | **−12%** [−0.046, −0.007] |
| `pl-18` | 11.8× | +915 [−981, +2,940] | −467 | −3% (null) |
| `pl-full` | 19.2× | **+5,635** [+3,065, +8,256] | −687 | +5% (null) |

Cache creation scales with bytes exactly as §25.2 said. **Cost does not**,
because the shorter trajectory's output-token saving cancels it. Passages are
not expensive: the whole passage is **+5% [−4%, +13%]**, a null, and both
shortened arms are *cheaper than showing one line*. That is a failure to
replicate §25.2's headline +18% [+2.4%, +5.9%]; the intervals barely overlap.
§25 ran 278 instances and §26 ran 138 disjoint ones with the same binary and
measurement, so the cost premium is smaller and noisier than one campaign
suggested — and **the entire economic case for shortening was built on a number
that did not hold.** The lever was chosen to buy something that was not for
sale.

**P5 — accuracy unmoved and bounded.** `func_acc@10_tol`: `pl-18` +0.014
[−0.022, +0.051], `pl-full` −0.007 [−0.036, +0.022], `pl-18k5` +0.000 [−0.036,
+0.036]. All null at a resolution of ±0.045.

**P6 — truncation zero** in all four arms. **P7 — tool lines byte-identical**,
one binary. **P8 — query style unchanged.**

**The registered response, applied.** The campaign showed 18 lines losing the
effect, so **the default is the whole passage.** `--passage-lines` stays as the
knob and 18 remains reachable. Writing that rule in advance was worth the whole
exercise: the temptation with +0.364 against a 0.35 margin is to observe that it
misses by 0.014 and that 18 lines keeps 70% of the effect at 62% of the bytes.
Both statements are true; neither is the test that was agreed to, and the cost
argument that would have justified bending it turned out to be measuring noise.

**Ledger.** Six of eight as registered (P2, P5, P6, P7, P8, and P4's mechanism),
two decisive failures (P1, P3), one prediction inside P4 falsified in a way
worth more than the arm that produced it.

**What §26 leaves:** a default that is measured rather than inferred, a knob that
makes the trade available, and a sharper instance of the availability trap — it
is not only that naming a thing fails to change behaviour (§25's labels),
*showing 94% of it changes only 70% as much.*

### 26.3 The endpoint changes, and so does the answer

§26.2 scored the campaign on file-reopening because that is what §26.1
registered. **That was the wrong objective.** The tool exists to make an agent
cheaper and faster at constant accuracy; reads-after-search was chosen because
it was the *powerable* endpoint, and powerable is not the same as important.

Re-scored on cost, over the same 138 paired tasks:

| arm | cost/run | vs default | turns | wall | accuracy |
|---|---|---|---|---|---|
| whole passage, k=10 | $0.236 | — | 6.56 | 29.3 s | 0.609 |
| 18 lines, k=10 | $0.218 | −8% [−0.040, +0.002] | 7.23 | 32.8 s | +0.022 |
| **18 lines, k=5** | **$0.199** | **−16% [−0.060, −0.015]** | 8.01 | 36.3 s | +0.007 |
| one line, k=10 | $0.225 | −5% [−0.030, +0.008] | 9.17 | 39.1 s | +0.007 |

Accuracy is tied everywhere, on 3–6 discordant pairs.

**18 lines at k=5 dominates the pre-§25 default outright** — 12% cheaper, 8.01
turns against 9.17, 36.3 s against 39.1, accuracy tied. Against the whole passage
it *is* a trade: **16% cheaper for 1.5 turns and 7 seconds.**

The mechanism is §25.2's read the other way. Richer results monotonically cut
what the model reads (269,729 → 226,623 tokens) and writes (2,624 → 1,937)
because the session shortens; what rises is **cache writes** (17,248 → 22,883).
`k=5` is the only arm with *fewer* cache writes than the control (14,041): it
shortens the session without inflating each result.

**Shipped: `k=5`, `passage_lines=18`.** This is an endpoint switch made after
seeing the data, which is exactly what pre-registration prevents. Cost was
registered as a co-primary so it is not fished, but the *decision rule* was
written on reads-after-search and is being overridden — recorded rather than
presented as the plan. Two things temper it: the −16% interval excludes zero
comfortably, and cost is the endpoint that already failed to replicate once
(§25's +18% became §26's +5%), so a confirmation on an independent frame is owed
before the number is quoted as settled.

`desc-v9` still says "top 10" and now returns 5. The mismatch is *as measured* —
the `pl-18k5` arm ran with that description — so the description stays frozen
under its own name (§20.1's rule). A `desc-v10` saying "top 5" is a separate arm,
not an edit.

### 26.4 A line is not a unit of cost

A line budget prices prose and code differently for output that is nominally
identical. At 18 lines, k=5, with the per-line cap active:

| corpus | median line | bytes/search at 18 lines |
|---|---|---|
| linux (C) | 30 chars | 4,668 |
| vscode (TS) | 33 | 10,875 |
| wikipedia (prose) | **180** | **13,470** |

Nearly 3× for the same nominal window, and the worst single passage was 14,358
characters before clipping. `--passage-chars` budgets content instead, growing
line by line around the match until the next line would exceed it — the same
unit `ChunkParams::budget` already uses for chunking (§20.2).

**800 characters, because it is the equivalence point.** Over 109 real agent
searches at k=5 it scores **51.4% at 2,880 bytes** against 18 lines' **51.4% at
2,853** — the same behaviour, to the search. 600 costs 2,140 and scores 48.6%:
three searches fewer out of 109, which is noise and may well be free. It is not
taken, because changing the unit *and* the effective size together would leave
the next campaign unable to say which one moved.

What it buys across the three corpora: **5,492 / 8,413 / 2,321** — prose falls
**83%** and the worst corpus **38%**.

**What it does not buy.** It does not equalise cost across languages; the spread
stays ~3.5×. Roughly half of printed output is the per-line `path:line:` prefix,
which scales with *line count* rather than content, so a content budget hands
short-line C more lines and more overhead — the first measurement at 600
characters actually *inverted* the problem, making the kernel dearer than
Wikipedia. Charging a `LINE_OVERHEAD` recovers part of it; the path part is not
knowable in the engine, which cannot tell whether the CLI will print one. **The
property delivered is a bounded worst case, not a flat cost.**

Unmeasured: every number in §26.4 is coverage and bytes. Whether 4 lines of
prose is *enough to act on* where 20 lines of C is has not been tested on an
agent, and §25's labels are the standing warning about exactly that inference.

---

## 27 Claude Code with semgrep enabled, on SWE-Explore (2026-08-08)

Every agent-scale result §16–§26 was measured on Loc-Bench, and measuring the
instrument found three limits that bound all of them.

**It is 100% Python.** All 1,149 gold files across 560 instances are `.py`.

**A sixth of every campaign was inert.** Across 5,394 ok rows, **922 (17.1%)
never invoked the search tool at all** — and they *out*-score the ones that did,
0.868 against 0.771 on `file_acc@5`. Inertness tracks the issue-text tier: 23.0%
in `named`, 12.8% in `partial`, 6.3% in `blind`. A session that never searches
cannot respond to a search change, so this is very likely the mechanism behind
§19.10's "agent accuracy is unpurchasable at ±0.038" — read as an instrument
limit, a good part of it is frame composition.

**The benchmark forces a choice nobody faces.** `run.py` removes `Grep`
entirely, so §16–§26 measured semgrep *instead of* ripgrep. The product question
— does semgrep help an agent that **still has grep** — has never been asked.

SWE-Explore (arXiv 2606.07297, June 2026) answers all three: 848 issues over 203
repositories in **10 languages**; the task is a ranked list of
`(path, start, end)` at K=5; gold is line-level regions derived from what
successful repair trajectories actually read, intersected across ≥2 trajectories
and manually audited. Its published `claude_code` explorer is stock Claude Code
with ripgrep-backed `Grep` — the control this project has never run.

### 27.0 What the setup cost, and the four defects it found

**Checkouts are per-instance** at each `base_commit`, so gold line numbers are
valid — the gate that would have invalidated everything, and it passes.

**The issue text is not in the dataset.** Upstream resolves it from an
unpublished `unify_trajs/`. Rebuilt from the three source sets — SWE-bench
Verified (451), Multilingual (182), Pro (215, after stripping its `instance_`
prefix) — **848/848**. It surfaced that Pro's statements are rewritten
("# Description:", curly quotes) rather than raw issues, a query-distribution
difference worth stratifying on.

**No prefetch is possible.** 19 checkouts across all ten languages average
**32.1 MB** (1.2 MB axum → 113 MB teleport), extrapolating to **26.6 GB** for
848 against 21 GiB free. So the runner fetches, indexes, runs all three arms and
evicts under a byte-capped LRU — an LRU and not a refcount because
`eval_runner.py`'s loop is explorer-major.

Gold shape, because it decides which metrics mean anything: core regions per
instance median 4, mean 4.7; core region *size* **median 5 lines, p90 1,037, max
9,705** — 59.4% are ≤32 lines and **29.8% are over 200**, because the
trajectories include whole-file reads. So `Rec_ℓ` is dominated by the giant
regions and `HitRegion` by the small ones.

Four defects surfaced across four smoke runs costing about four dollars, and
**every one would have produced a clean, publishable, wrong number**:

1. **32 leaked MCP tools** from the operator's own config inflated the system
   prompt — and prompt size *is* cache-creation tokens, the co-primary endpoint.
   Fixed with `--strict-mcp-config` and `--setting-sources ""`; cost per run fell
   from $0.09–0.51 to $0.03–0.16.
2. **`--permission-mode dontAsk` does not enforce `--allowedTools`.** It means
   "do not prompt", not "restrict"; with Bash enabled the agent ran `grep -n`
   directly. locbench never relied on the allowlist — it blocks `grep`/`git`
   with PATH shims — and dropping those shims was an error made here. Left in,
   all three arms converge on shell grep and the null means nothing.
3. **Upstream's prompt steers away from the treatment.** `EXPLORE_PROMPT` says
   "Use Glob, Grep, and Read tools"; measured consequence, `bash_calls` was
   **0** in every arm on every instance and all three arms returned identical
   answers. An appended system prompt saying a tool exists does not survive a
   user prompt naming three others — §25's *availability is not use*, one level
   up, at the tool surface. The clause is now amended per arm, one tool name in
   the same position, `cc` keeping upstream's prompt byte-for-byte with an
   assertion that fires if upstream rewords it.
4. **Silent skips.** Transient archive-API failures under four workers dropped
   **29 of 31** instances from a pass. Silent skips cost money *and* select which
   instances get measured. Now retried with backoff.

**A correction to the paper's own numbers.** Every quoted number holds — Claude
Code at HitReg 0.531, HitFile 0.667, CtxEff 0.829, 48.0% downstream resolve —
but the models do not: the real set is **GPT-5.4**, GPT-5.4-mini, Kimi-K2.6,
Sonnet-4.5, GLM-4.7, Gemini-3-Pro, and "all agentic explorers are driven by
GPT-5.4". **So their "Claude Code" row is the Claude Code *scaffold* routed to
GPT-5.4, not a Claude model**, and a Sonnet arm cannot reproduce it. Their Table
5 prices the swap under a fixed scaffold: GPT-5.4 → Sonnet-4.5 moves HitReg
0.516 → 0.428 and CtxEff 0.771 → 0.715. The calibration gate is retargeted at
the Sonnet row and read as a band, not an equality.

### 27.1 The pilot (n=31, exploratory)

Three arms — `cc` (Read, Glob, Grep: upstream's baseline), `cc-rg`
(+ `Bash(rg *)`), `cc-sg` (+ `Bash(sg *)`) — over a language-stratified 31 that
deliberately oversamples non-Python (6 Python of 31). Paired, `boot_ci`, 4,000
resamples, seed 1.

| endpoint | cc | cc-rg | cc-sg | sg − cc | rg − cc |
|---|---|---|---|---|---|
| hitRegion@5 | 0.432 | 0.436 | 0.494 | **+0.062 [+0.020,+0.105]** | +0.004 [−0.032,+0.044] |
| hitFile@5 | 0.505 | 0.496 | 0.556 | **+0.051 [+0.005,+0.100]** | −0.009 [−0.048,+0.032] |
| ctxEff | 0.883 | 0.937 | 0.933 | +0.051 [−0.006,+0.112] | **+0.055 [+0.005,+0.111]** |
| nDCG@500 | 0.950 | 0.955 | 0.975 | +0.025 [+0.002,+0.062] | +0.005 [−0.011,+0.021] |
| recall@100 | 0.127 | 0.114 | 0.144 | +0.017 [−0.004,+0.042] | −0.013 [−0.031,+0.001] |
| precision | 0.715 | 0.736 | 0.688 | −0.026 [−0.134,+0.067] | +0.021 [−0.055,+0.091] |
| cost $ | 0.182 | 0.193 | 0.195 | +0.013 [−0.025,+0.045] | +0.011 [−0.023,+0.038] |
| turns | 8.32 | 8.77 | 9.36 | +1.03 [−0.55,+2.61] | +0.45 [−0.84,+1.55] |

**The third arm has already paid for itself twice.** Bash alone is +0.004 and
−0.009 on coverage, so semgrep's +0.062 is not a shell effect. And it *takes one
away*: **ctxEff is a Bash effect, not a semgrep effect** (+0.055 `cc-rg`, +0.051
`cc-sg`). Run as two arms, semgrep would have been credited with the gain — and
CtxEff is the metric the paper's Table 4 ranks highest (Pearson +0.950 against
downstream resolve).

Only **1 of 31** instances produced identical regions across `cc` and `cc-sg`.
**Invocation rate:** `cc-rg` 16/31 (52%), `cc-sg` 14/31 (45%), at 2.4 and 1.2
calls per session. Per language, `sg` usage: Go 3/3, Rust 2/3, C 2/3, JS 2/3,
TS 2/3, Python 2/6, Java 1/3, Ruby 0/3, PHP 0/3.

**None of the above is a result, and §18.6 is the reason to say so.** The starred
endpoints rest on 8–11 discordant pairs; nine endpoints carry no multiplicity
correction; the frame is not population-weighted; and `precision` moves opposite
to recall, §24.1's signature of a geometry change. Split by whether the tool was
invoked, `hitRegion` gains **+0.072 [+0.010,+0.142]** where `sg` ran (n=14) and
**+0.054 [−0.000,+0.110]** where it did not (n=17) — a tool never called cannot
cause the second figure, so either it is noise at n=17 or the amended prompt
clause is doing work on its own. Post-treatment conditioning either way.

**Power** from the pilot's paired sds (hitRegion 0.126, cost 0.097), at 80%:
n=150 gives MDE 0.029 / 0.022 / 1.00 turns; n=400 gives 0.018 / 0.014 / 0.61;
**n=848 gives 0.012 / 0.009 / 0.42**.

### 27.2 Pre-registration for the powered run (n=848)

Endpoints, thresholds and analysis were fixed in the approved plan before R1 ran.
What is *not* clean: R1's interim (n=150) has been seen, because the plan
registered the independent-subset check as descriptive and non-stopping. It has
not moved a threshold.

**The ladder.** One run id (`s27`), each rung a longer prefix of
`bench-ladder.jsonl` (seed 27, sha `fe88b90f`): R0 n=31 → R1 n=150 → R2 n=848.
Gates are **harness health only**; no stopping rule reads an endpoint.

**Primary:** `hitRegion@5`, `cc-sg − cc`, paired bootstrap (4,000, seed 1). MDE
0.012 at n=848 from a paired sd of 0.126, independently confirmed at 0.128 by
the full-vs-full retest control.

**Co-primary — cost.** §25.2's registered mechanism predicted +5–10% cost with
turns *flat or down*. R1 measured **+11% cost and +0.83 turns**, both p<0.001 on
the sign test, so the turns half is already contradicted and is recorded as a
**failed prediction**, not adjusted to fit.

**Confound:** `cc-rg − cc` printed beside every endpoint. R1 has it flat on
coverage (+0.001 hitRegion) and not flat on cost (+$0.0136, +0.43 turns), so the
semgrep-specific increment is about +$0.005.

**Secondary**, Holm-corrected: `hitFile@5`, `ctxEff`, `nDCG@500`, `recall@100`,
`precision`; `nDCG@500` (0.971) and `FUH` (0.974) near ceiling — bound printed
rather than null asserted. **Per-language** exploratory and reweighted; at n=848
only Python (547) and Go (84) are powered, C++ (1) never will be, strata under
n=8 unreported.

**Tripwires.** Invocation rate is a **dilution factor, not a floor** — the
registered 70% was wrong and is withdrawn (R0 45%, R1 35%). Truncation = 0;
`cc`'s prompt sha256 equal to upstream's; malformed-output symmetric; one binary.
**Calibration** retargeted at the paper's Sonnet-4.5 row (HitReg 0.428, CtxEff
0.715), read as a band.

**Registered expectation.** R1's independent 119 gave **+0.010 [−0.0095,
+0.0305], w/l 15/13** — the pilot's +0.062 did not replicate, as its at-MDE flag
predicted. The honest expectation at n=848 is **a small positive or a null**, and
the likely deliverable is a *bound*. **Registered response to a null:** report it
as a bound with the conservative bias attached — the gold is what grep-driven
agents read, so a region semgrep surfaces that those trajectories never needed
scores as noise, and the detectable effect is a lower bound. Do **not** re-cut
for a stratum that moved.

### 27.3 The result: a powered null on quality, at 18% more cost

848 instances, three arms, 2,544 sessions, **$444.26**. Every arm complete, zero
non-ok rows, paired on all 848.

**Primary — `hitRegion@5`, `cc-sg − cc`: +0.0018 [−0.0079, +0.0113]**, 118 wins
against 121 losses, p=0.897, MDE 0.0137. **Enabling semgrep alongside `Grep`
does not improve region coverage**; the true effect is no larger than about
**±0.011**. Every other quality endpoint agrees: `hitFile@5` +0.007 [−0.003,
+0.017], `ctxEff` +0.001, `nDCG@500` −0.005, `precision` +0.010. `recall@100` is
+0.0047 (p=0.032 raw) and dies at Holm 0.158, flagged at its own MDE.

| | sg − cc | rg − cc | **sg − rg** |
|---|---|---|---|
| cost | +$0.0286 (**+18.1%**) | +$0.0214 (+13.5%) | +$0.0072 (**+4.5%**) |
| turns | +1.225 [+0.947,+1.535] | +0.747 | +0.479 |

Both overwhelming on the sign test — cost 626/222, turns 460/197, p<0.001.
**Most of the price is having a Bash tool at all, not semgrep**: of the 18.1%,
13.5 points are `rg`'s too. This also finishes off §25.2's registered
prediction of +5–10% cost with turns flat or down: cost came in at 18% and turns
went **up** by 1.2. The mechanism — output bytes drive cache creation — survives
in direction; the turns prediction was simply wrong.

**The ladder, which is the methodological result:**

| rung | n | `hitRegion@5`, sg − cc |
|---|---|---|
| R0 pilot | 31 | **+0.0624** [+0.0196, +0.1051] |
| R1 independent | 119 | +0.0100 [−0.0098, +0.0306] |
| R2 new only | 698 | −0.0023 [−0.0131, +0.0092] |
| **pooled** | **848** | **+0.0018** [−0.0079, +0.0113] |

A monotone decay from a starred, CI-excludes-zero, p=0.022 "finding" to nothing.
Every rung was consistent with the next; only the first was worth publishing, and
it was the only one that was wrong. The pilot's estimate sat exactly at its own
detection limit and `analyze.py` printed *"~at MDE, expect regression to the
mean"* beside it before R1 ran. **That flag was worth more than the number it
annotated**, and it is now the standing reason this project does not report an
effect whose magnitude equals its MDE. Two pilot sub-findings also evaporated:
ctxEff's +0.055 *Bash* effect is +0.0056 at n=848, and the worrying +0.054 among
sessions that never invoked the tool is +0.0058.

**The dilution argument does not survive either.** 41% of `cc-sg` sessions
invoked `sg` (350/848):

    sg invoked      n=350   -0.0039 [-0.0184, +0.0108]
    sg not invoked  n=498   +0.0058 [-0.0069, +0.0188]

Among the sessions that actually used the tool the point estimate is *negative*.
(Post-treatment conditioning, so descriptive only — but it can only weaken the
dilution case.)

**Cross-language: the reason this benchmark was chosen, and it is null too.**
Every stratum spans zero: Python +0.001 (n=547), Go +0.000 (84), JavaScript
+0.005 (40), TypeScript −0.006 (38), Rust −0.015 (31), Java +0.018 (30), PHP
+0.020 (28), C −0.017 (27), Ruby +0.027 (22). The hypothesis that semgrep would
earn its place outside Python is not supported.

**Calibration.** Our `cc` scores HitReg 0.457 against the paper's Sonnet-4.5 row
at 0.428 — inside the band. CtxEff 0.931 against 0.715 is far higher and
unexplained; a reason to treat our CtxEff as non-comparable rather than as an
improvement.

**What this is:** a powered answer to the product question §16–§26 never asked —
**adding semgrep to an agent that already has ripgrep-backed `Grep` buys no
measurable retrieval quality and costs 18% more.** For a tool whose case has
always been "a better primitive inside the loop" (§3.2), that is the strongest
disconfirming evidence this project has produced. It is *not* a verdict on
semgrep as a replacement for grep: §16–§26 measured semgrep *instead of*
ripgrep, a different question. Two structural limits, registered before the run:
the gold is what grep-driven agents read, so the measurable effect is a **lower
bound**; and `FUH` (0.974) and `nDCG@500` (0.965) sit near ceiling.

**Harness ledger.** Four defects (§27.0), three of whose defining property was
**silence**. Two more surfaced during the run: **the LRU never evicted for an
entire rung** — it tracked only what its own process fetched and a `--resume`d
instance never requests a checkout, so R1's 150 trees were an invisible floor;
215 checkouts and 9.0 GB while reporting under a 5 GB cap, silently, through $81
of spending. And **the evictor deleted the working directory of live agents**,
protecting only the instance it was ensuring: **432 of 848 `cc-sg` rows died at
2.7 s with 1 turn** — 51% of the treatment arm, non-randomly, since it struck
hardest where eviction pressure was highest. That one was not silent;
`triage_swex.py` failed the run and refused the analysis. Compounding it,
`--resume` keys on instance id regardless of status, so those 432 dead cells
would have been treated as complete had they not been stripped by hand.

The gate also fired once on the tool itself: one `sg` invocation in 484 used a
flag that does not exist (`sg "query" --path lucene-core`); the agent recovered
and scored 0.6 on that instance. **The gate was overridden deliberately and it is
recorded here rather than quietly passed** — 0.2% of invocations, no effect on
any endpoint. It is still a real finding about the compat surface: agents reach
for `--path`.

---

## 28 Grep removed: semgrep against ripgrep, head to head (2026-08-09)

§27 answered the *additive* question and got a powered null, but the mechanism
behind that null is **a choice the agent makes**, not a property of the tool:

| regime | semgrep usage |
|---|---|
| Loc-Bench `both` — rg + semgrep, routing advice in the description | **0.00 calls/session** (rg 3.51) |
| §27 `cc-sg` — semgrep + native `Grep`, tool named in the prompt | 41% of sessions, 1.4 calls |
| §27 pre-fix — semgrep available, prompt named `Grep` instead | **0%** |

With any lexical tool present, agents reach for it. §27 also showed they *add*
semgrep rather than substitute it — `Grep` usage fell only 0.48 of 3.45 while
total searching rose — so the treatment was diluted by construction and the null
was measured at ~41% delivery. §28 removes the choice: two arms, `Grep` gone from
both, exactly one Bash search tool each.

### 28.0 Design, and what is already known

| arm | `--tools` | allowlist | status |
|---|---|---|---|
| `cc` | `Read,Glob,Grep` | — | **already run** (§27, 848 rows, $133.97) |
| `sub-rg` | `Read,Glob,Bash` | `Bash(rg *)` | new |
| `sub-sg` | `Read,Glob,Bash` | `Bash(sg *)` | new |

Three contrasts: **`sub-sg − sub-rg`** (primary — head-to-head with no native
fallback), **`sub-sg − cc`** (the product question), **`sub-rg − cc`** (does
removing native `Grep` cost anything by itself — the control that keeps the other
two interpretable). `RG_LINE`/`SG_LINE` are reused verbatim so descriptions do
not become a second variable; the prompt clause drops `Grep` for the new arms.
Removal is enforced in **two** places and needs both: `Grep` out of `--tools`,
and PATH shims blocking shell `grep`/`egrep`/`fgrep` — `--allowedTools` enforces
nothing under `--permission-mode dontAsk`, which §27.0 learned the hard way.

**The substitutive regime is not new; only this benchmark is.** Loc-Bench ran it
at scale and it was parity:

| contrast | n | delivery | file_acc@5 | func_acc@10_tol |
|---|---|---|---|---|
| desc-v5 − rg | 560 | 80% | +0.0018 [−0.0179, +0.0214] | +0.0018 [−0.0196, +0.0232] |
| desc-v9 − rg | 204 | 91% | −0.0196 [−0.0539, +0.0147] | −0.0392 [−0.0833, +0.0049] |

MDEs 0.027 and 0.030 on the first row, and §27's held-Bash contrast
(`cc-sg − cc-rg`) was −0.003 [−0.013, +0.006]. **The registered expectation is
therefore parity**, |Δ| < 0.012, recorded before the run so a null is a
prediction rather than a rationalisation. What §28 adds: multi-language
line-level gold instead of Python function names, delivery near 100% instead of
~45%, and the `sub-sg − cc` contrast nobody has measured on any benchmark.

**Harness changes, and the two that would have failed silently.**
`campaign.sh`'s `count_ok()` globbed every arm file under the run id, so a
two-arm rung under `s27` would have started at 2,544 ok rows against a target of
1,696, printed "rung complete" and exited **having run nothing**. And
`triage_swex.py` gates against the *registered* arm set, so five arms in one
results directory would have failed both its checks. Both are now scoped by an
explicit `--arms`, and `analyze.py` gained `--arms`/`--contrasts` — its arm
intersection previously ignored unknown arms in silence and would have
cheerfully re-reported §27 while §28's rows sat unread. Verified by
byte-comparing every number the parameterised analyser produces against the §27
defaults.

### 28.1 Pre-registration for the powered run, written after the R1 gate

**R1 (n=120, both arms, $53.08) passed its gate**, and its one registered
diagnostic is the premise of the section:

| arm | sessions using its tool | calls/session |
|---|---|---|
| `sub-rg` | **113/120 (94%)** | 5.2 |
| `sub-sg` | **112/120 (93%)** | 3.4 |

Against §27's 47% and 41%. Removing the choice more than doubled delivery, so
§28 measures the tools rather than the agent's preference between them. **No
endpoint has been looked at.**

**Primary:** `hitRegion@5`, `sub-sg − sub-rg`, paired `boot_ci` (4,000
resamples, seed 1). MDE 0.012 at n=848 on §27's measured paired sd 0.126.

**Co-primary — cost and turns.** §27 put semgrep's own increment over ripgrep at
+4.5% and +0.48 turns *with* `Grep` present. R1's per-session mean is **$0.221**
against §27's $0.158–0.187, so the registered prediction is that **removing
native `Grep` is itself expensive** and that `sub-rg − cc` will carry most of it
— the smoke measured `sub-rg` at +42% over `cc-rg` on identical instances at
equal turn count. The mechanism to test is §27's: raw `rg` through Bash averages
25 KB a call and floods, while the native tool bounds its output.

**Secondary, Holm-corrected:** `hitFile@5`, `ctxEff`, `nDCG@500`, `recall@100`,
`precision`; `nDCG@500` and `FUH` near ceiling — print the bound.

**Registered expectation: parity on quality**, |Δ| < 0.012, with the interesting
result expected to be **cost, not accuracy**. **Registered response to a null:**
report as a bound with the conservative bias attached, and do not re-cut for a
stratum that moved.

**A gate gap fixed before R2, not overridden.** R1 failed once, on
`php-cs-fixer-8064`: the agent searched a path absent at that base commit,
semgrep exited 2, and the distress gate counted "every search empty".
`classify_usage` already labels that *bad path (tool correct)* but the all-empty
check never consulted it. triage.py's own principle is that "a gate that punishes
the tool for being right is a gate nobody can pass", so the filter now applies to
the distress check as well — for distress only. Fixing rather than overriding
matters because R2 is seven times larger and a gate overridden every run is not a
gate.

### 28.2 R2 interrupted by the credit ceiling, and a mechanism read on the 456 clean pairs (2026-08-10)

**What happened to R2.** The 848-rung ran `sub-rg` essentially to completion —
822 rows, 820 ok, the other 26 stuck on cold-cache download failures — then hit
the API's five-hour credit ceiling partway through `sub-sg`: 848 rows on disk,
**484 ok and 364 `agent_error`**, every failed row a rate-limit rejection, median
duration 0 s, median cost $0. The gate GATED OFF on exactly this (366 non-ok
rows, 392 partial instances), which is the gate doing its job. The dead cells
cost nothing and are resumable; **the registered pooled-848 analysis has not been
run** and still gets computed once, on the full frame, after recovery.

**A look at the primary on partial data, declared.** Run at the operator's
request to understand *mechanism*, on the 456 instances where both arms have
clean rows. It saw the partial-data primary: `sub-sg − sub-rg = −0.0073`
(sd 0.134, w/l 52/73, **331 exact ties**), consistent with the registered parity
expectation (|Δ| < 0.012). §28.1 has no stopping rule on endpoints, so this look
changes no decision, but it is a look and it is recorded as one. The 456 are
approximately a ladder prefix, not a random subsample, and nothing below is a
registered result. Reproduce with `eval/swexplore/mechanism.py`.

**Discovery is at ceiling; the entire contest is line-range margins.** On 454/456
pairs *both* arms land at least one gold region. File-level discovery discordance
is symmetric — sg's agent missed a gold file rg's had on 43 instances and found
one rg missed on 38, worth −9.50 and +9.07 rate-points, a wash. SWE-Explore
issues carry identifier anchors that exact match resolves as well as ranking
does, so the vocabulary-mismatch case semantic search exists for almost never
binds here. What remains is *which lines* get submitted, and that is where the
whole net −3.33 lives.

**The bucket accounting.** Every lost region attributed to a cause from the
session's own shim log and captured output; an instance's lost score is
distributed proportionally, so buckets sum to the gap. Both directions, because a
bucket is only a tool finding if the other tool does not lose the same way:

| bucket | sg lost | % of sg gap | rg lost | net sg-specific |
|---|---|---|---|---|
| line precision — right file, wrong lines | 4.37 | **27.3%** | 1.88 | **−2.49** |
| — within 32 lines (chunk edge) | 1.55 | 9.7% | 0.95 | −0.60 |
| — beyond 32 lines (wrong area) | 2.82 | 17.6% | 0.93 | −1.89 |
| noise the tool showed — submitted a non-gold file its output displayed | 2.77 | **17.3%** | 1.33 | **−1.44** |
| gold surfaced in output, never submitted | 2.29 | 14.3% | 1.82 | −0.47 |
| gold scoped away — file-scoped queries only, never surfaced | 2.81 | 17.6% | 4.80 | **+1.99 (rg worse)** |
| gold rank miss despite a repo-wide query | 1.27 | 7.9% | 0.57 | −0.70 |
| never invoked the tool | 1.27 | 7.9% | 0.40 | −0.87 |
| noise from the agent's own guess | 1.04 | 6.5% | 1.27 | +0.23 |

1. **Line precision is the sg-specific deficit — 27% of sg's losses, 2.3× rg's
   rate.** `hit_region_rate` scores exact overlap; `rg` prints `path:line:text`
   and agents copy the line into their range, while sg prints a ~32-line window
   the agent anchors to. The pure chunk-edge case is only a third of the bucket
   (jq-2650: sg walked the agent to `parser.c:3443`, gold at 3456, one window
   short; fluentd-3917: sg's agent submitted `yaml_parser.rb 1–40` against gold
   47–51 while rg's agent, shown the match line, submitted 24–53). The larger
   share is >32 lines off — a plausible chunk in the wrong part of the right
   file, accepted as the answer.
2. **sg's always-answer behavior converts to noise submissions at 2× rg's
   rate.** 99% of sg's 1,437 calls exited 0 with content; 17% of rg's 1,854
   exited 1 with nothing, and the agent reformulated on the spot. A weak match
   that fills the screen reads as an answer: 42 submitted regions in non-gold
   files that sg itself had displayed, against rg's 18.
3. **Single-file scoping is real but it is an agent behavior, not an sg defect —
   rg loses more to it than sg does.** "Gold scoped away" is 17.6% of sg's gap
   and **37.8% of rg's**, the largest rg bucket; scoping rates are identical in
   sg's winning and losing sessions (67% vs 64% file-scoped). Agents scope both
   tools to guessed paths and lose when the guess is wrong — and sg's repo-wide
   ranked search is precisely the surface that wins those points back. Query
   styles differ as expected: sg gets 4.5-word phrases, 70% file-scoped; rg gets
   1.9-word patterns, 90% path-scoped, alternation (`a|b|c`, often across several
   files in one call) on half of all calls.

Cost on the same 456: `sub-sg` $0.240/session vs `sub-rg` $0.192 (+25%), +0.4
turns — consistent with §28.1's registered prediction that the interesting result
is cost, not accuracy.

**What this buys the tool, ranked:** (1) surface the best-matching *line* inside
each chunk, not just the window — the deficit is anchoring, and the
`--decl-boost` machinery already re-reads candidate chunks cheaply; (2) make a
weak match look weak — some "no strong match" signal where rg's exit 1 now does
the agent's reformulation prompting; (3) leave repo-wide ranked search alone — it
is the bucket where sg is already winning. Caveats: "appeared in output" is a
substring match on captured stdout, attribution within an instance is
proportional rather than causal, and all of it is descriptive, on 54% of the
frame, outside the registration.

---

## 29 Acting on §28.2: fine answers, a floor, wide-by-default, and function chunking again (2026-08-10)

§28.2's bucket accounting turned into four engine changes, built in one arc.
Everything here is *mechanism landed*; the measurements that would flip the
remaining defaults are §29.4's.

### 29.1 The fine rerank (shipped, default on)

Line precision was sg's one clearly tool-specific deficit — 27.3% of its §28
losses, 2.3× ripgrep's rate, because agents anchor submitted ranges to the span
the tool prints and a 32-line chunk window ends lines away from the target.
`finalize` now scores every 4-line window of each candidate chunk by cosine
against the query (raw text both sides, i8-quantized both sides — a pure function
of query string and file bytes, so cold==warm holds with no index state threaded
in), and the best window becomes the hit's span, its passage, and its score.
Windows re-rank the candidate pool (`--fine-blend`, 1.0 = pure fine); same-file
windows electing the same lines collapse; `--no-fine` reproduces the old output
byte for byte and is the control arm. Costs ~0.5 ms, timed as `finalize:fine`.

Two consequences. Scores stopped being decorative: the maxsim head normalization
made every rank-1 fused score exactly 2.0, and the fine cosine is the first
cross-query-comparable number the pipeline emits — which is what makes §29.2
possible at all. And at blend 1.0 the fine order *owns* the list, making the §24
declaration boost invisible inside the pool (it still gates who reaches the k×3
candidates); the decl-boost parity test now pins fine off for that reason.
Whether blend 1.0 is right against 0.7-ish is a §29.4 question, registered before
looking.

### 29.2 The score floor (mechanism shipped, default off)

sg answered with content on 99% of 1,437 real §28 calls; agents submitted
non-gold files sg itself had displayed at 2× rg's rate, while rg's loud empty
misses (17% of calls) are what prompted rephrasing. `--min-score` is that missing
"colder, try again": set-level (the floor asks whether the scope contains the
concept at all — a weak tail behind a strong head is normal ranked output),
judged in the shared finalize tail, zero hits + exit 1 + a footer line naming the
refused score. Signal = best fine cosine (`--no-fine`: best chunk cosine via the
MMR vectors).

Default 0 = off, deliberately: a floor that cries wolf teaches agents to ignore
it. `best_signal` is reported in the envelope on success too, so calibration
joins score→outcome from existing artifacts: replay
`eval/queries/guesses-*.jsonl` through guessplay plus the 1,437 captured s27
sub-sg invocations, take the largest floor with ≤2% false-floor rate on
gold-hitting queries, ship that number with its measured true-negative rate.

### 29.3 desc-v10, and function chunking rebuilt (opt-in)

**desc-v10** models the pathless call as *the* way to search ("start wide; add a
path only to narrow further") and fixes the stale top-10. Grounds: agents
file-scoped ~70% of sg calls, "gold scoped away" was 17.6% of sg's §28 losses and
37.8% of rg's, and no prior description ever said when a path belongs. The
§19.2b example and tripwires carry unchanged. The SWE-Explore arms keep the
*registered* SG_LINE — v10 sits beside it un-wired (`SG_LINE_V10`) until a
campaign registers arms on it, because 364 rate-limited sub-sg cells still owe
completion under the old treatment.

**Function chunking returns** (`--chunking function`, cap `--chunk-cap` 96), five
weeks after §11.4 removed it — because §11.5's verdict was that the *instrument*
couldn't resolve the effect, and SWE-Explore's line-level gold plus guessplay now
can. The §11 design is kept where it was measured and simplified where it wasn't:
one `leaf_defs` table per language (9 grammars, PHP added; everything else
recurses, which makes containers, export wrappers, and decorated definitions fall
out for free — decorators reattach via Rule B's `@` prefix); definitions ≤ cap
emit whole, never recursed, so closures stay in context; §11.2's Rule B verbatim
(prefix table, ≤20 lines, ≤1 blank — the 0%-wrong-code rule); a 5-line min-merge
for packed accessors (§11.1's +76% chunk-count case); gaps and over-cap interiors
fall to non-overlapping window cuts, so function mode is fully disjoint — the
§11.3 postings shrink, kept. Parse failure or any ERROR node falls back to line
windows; no parser timeout ever (a timeout makes the cut a function of machine
load, which breaks cold==warm). Cache entries tag as `f{cap}w{w}o{o}`; a
grammarless build (`--no-default-features`) names them but never parses them
back, reclaiming instead of mis-serving. `Chunk` stayed three u32s, so no format
bump. Binary cost measured: 39.0 → **46.5 MiB** (+7.5 for 9 grammars; §11.3 paid
+6.6 for 8). On the frozen test corpus, function mode cuts 104 chunks where
window mode cuts 39, and a warm query stays ~4 ms.

### 29.4 What is registered to happen next, before any default flips

In order, all offline and cheap: (1) guessplay A/B — fine vs `--no-fine`,
function vs window, on the harvested real-query sets; (2) floor calibration as
specified in §29.2; (3) a `--fine-blend` sweep only if (1) shows the pure fine
order losing what the §24 boost bought. Function chunking's default flip
additionally requires re-measuring §11.3's cold-index cost on django and a
snapshot re-record reviewed case by case. A SWE-Explore rung with the new binary
comes only after those gates, and its arms register the v10 description at the
same time. Nulls are reported as bounds; no default flips on a stratum cut.

### 29.5 The offline gates, run (2026-08-10)

**Guessplay A/B, 854 real harvested agent queries, 186 instances, one pass, 2×2
(fine on/off × window/function chunking).** Paired `boot_ci`:

| contrast | file hit@5 | func hit@5 strict | func hit@5 overlap |
|---|---|---|---|
| fine − no-fine (window) | −0.007 [−0.025, +0.011] | −0.009 [−0.027, +0.009] | **−0.082 [−0.104, −0.059]** |
| function − window (no fine) | +0.000 [−0.013, +0.013] | +0.009 [−0.008, +0.027] | **−0.028 [−0.047, −0.008]** |
| function+fine − baseline | −0.015 [−0.033, +0.002] | −0.019 [−0.040, +0.002] | **−0.096 [−0.119, −0.071]** |

**Read the overlap column as geometry, not quality — §24.1 said so in advance.**
`rank_func_ovl` credits a chunk that *overlaps* the gold function at all;
`rank_func` requires the chunk's best line to fall inside it. A 32-line window
overlaps a 12-line gold function by accident constantly, and a 4-line window
cannot. So a lever that shrinks spans must drive those two metrics apart, and
§24.1 registered exactly that as the signature of changed geometry rather than
changed retrieval: strict flat, overlap down. Reporting the overlap drop as a
loss would be scoring the fine rerank for no longer getting accidental credit.

On the endpoints that survive the geometry change, both levers are **nulls**:
every strict and file CI spans zero. Do-no-harm holds, which is what the gate
asked. It does not show a gain either, and the combined arm leans negative (w/l
34/50 strict) — registered as the trigger for a `--fine-blend` sweep before the
blend default is defended, not before shipping the mechanism.

**Floor calibration, 853 replayed queries** (`eval/locbench/floorcal.py`):

| | |
|---|---|
| gold-hitting top-1 score | p5 0.486, p25 0.645, median 0.725 |
| gold-missing top-1 score | p50 0.684, p75 0.785, p95 0.888 |
| **floor 0.420** | refuses **1.9%** of gold-hitting, converts **9.3%** of gold-missing to an honest "no matches" |

Identical threshold at the n=451 half-sample, which is the stability check worth
having. The distributions overlap heavily — a wrong-but-plausible neighbourhood
embeds near the query, so a miss's median (0.684) sits just under a hit's (0.725)
— and the floor only separates in the low tail. That bounds the claim: this is a
small honest-refusal win, not a discriminator.

**Two defects the gates found, both fixed before the campaign.** The fine rerank
made the display anchor worse in a way no offline metric scores: the hit's `text`
is the best-overlap line *within the span*, and where a 32-line chunk almost
always held some line sharing a query token, a 4-line window often holds none —
so the first-wins fallback anchored **8.3% of snapshot hits on a bare `{` or
`)`**. Ranking the anchor by `(overlap, carries a word)` takes it to 0.0%. And
the floor was **inaudible under `SEMGREP_NO_HINTS`**, which every agent harness
sets: its explanation sat below that early return, so a floored search gave empty
stdout, empty stderr, exit 1 — the §16.11 shape, and the opposite of the "colder,
try again" signal the floor exists to send.

---

## 30 The powered campaign on the new engine: sub-sg against sub-rg (2026-08-10)

§29 shipped four changes and §29.5 gated them offline. None had met a real
agent. This section runs all four at once against ripgrep on SWE-Explore's
line-level gold.

### 30.0 Design

Arms are §28's substitutive pair — `Grep` removed from both, one Bash search
tool each, the 93% delivery regime, roughly 2.3× the power of the additive
pair whose 41% delivery diluted §27. `sub-rg` is the **unchanged control**
(ripgrep never touches our engine); `sub-sg` carries fine rerank + floor 0.42
+ desc-v10 + function chunking. Baseline for the same contrast on the old
engine (§28.2, partial): `sub-sg − sub-rg = −0.0073` (n=456).

**Bundled deliberately, attributing nothing individually** — a moved endpoint
says "the package moved it" and no more; §29.5's offline arm-level attribution
stands in for it. Flags reach the binary through `LOCBENCH_SG_FLAGS`, never
shown to the agent. The trap avoided: the chunking half must also reach the
**index build**, since a repo-local `.semgrep/` is exempt from cache-tag
matching and a window-chunked index answers a function-chunked search with no
error anywhere; `_index_matches` raises rather than running a half-dosed arm.

### 30.1 Pre-registration, written before R2 is funded

- **Primary**: `hitRegion@5`, `sub-sg − sub-rg`, paired `boot_ci` (4,000
  resamples, seed 1). MDE 0.012 at n=848 on §27's measured paired sd 0.126.
- **Co-primary — cost and turns**, and the prediction *reverses* §28's +25%:
  **sub-sg cost ≤ sub-rg**, because the floor abandons dead ends and a 4-line
  passage is a fraction of a 32-line one.
- **Secondary, Holm**: `hitFile@5`, `ctxEff`, `nDCG@500`, `recall@100`,
  `precision`.
- **Delivery is the headline diagnostic, not a gate.** Below ~90% sg delivery
  the accuracy endpoints "describe a different agent rather than a different
  engine", and must be reported as diluted.
- **Query-shape gate before any accuracy claim**: `queryshape.py --since` must
  show desc-v10 moved the path-scoped share down from §28's ~70%. "A
  description that changed no behaviour cannot be evidence about behaviour."
- **Registered expectation**: accuracy parity, |Δ| < 0.02. A null is reported
  as a bound with delivery attached.
- **Registered response to a cost win with an accuracy null**: report it as
  the result — cheaper at equal accuracy is the §26.3 endpoint.
- Gates between rungs are harness health only; no sequential-testing alpha.

### 30.2 R1 as a pilot: four defects, and a description that moved behaviour (2026-08-11)

**The powered contrast was not funded, and no accuracy endpoint is reported
here.** R1's 240 sessions are not the registered pooled 848 and cannot pool
with a later run, since the four fixes changed the binary. $46.93 bought four
defects and two behavioural readings.

1. **`--path` is a shape agents type** — 4 times in 511 searches, up from 1 in
   484, and the rise is **desc-v10's own doing**. Now accepted as an alias. A
   description change produced a CLI requirement.
2. **The floor's own message taught an agent to fumble a flag.** It ended "or
   pass `--min-score 0`"; an agent typed `--min-score` with no value, exited
   2, and spiralled into three empty searches. §16.10 exactly: **a footer is a
   treatment, and an agent acts on any flag it names.** A test now asserts the
   message names no flag.
3. **The registered diagnostic was unreadable** — `floored`/`best_signal`
   never reached the trace envelope. Recovered by grepping stderr: **24 of 32
   empty sg searches were floored refusals**, the floor working as configured,
   invisible to the instrument meant to see it.
4. **The gate counted those refusals as failures** — 4.8% against a 2% limit,
   three quarters of it the floor doing its job. "A gate that punishes the tool
   for being right is a gate nobody can pass." Now excluded.

Two behavioural readings, both descriptive at n=120:

| | delivery | path-scoped calls | $/session | turns |
|---|---|---|---|---|
| `sub-rg` (unchanged control) | 93% | 89% | $0.200 | 9.2 |
| `sub-sg` | 91% | **50%** | $0.194 | 9.6 |
| §28 baseline | 90% | 70% | $0.240 | — |

**desc-v10 moved query shape and the control did not** — 70% → 50% for sg
while untouched rg sat at 89%. §19's gate passing in the only form it can. It
is the first description change measured to move *scoping* rather than length
or phrasing. Cost parity where §28 measured +25%, consistent with §30.1's
co-primary, but the +0.4 turn difference runs the other way.

### 30.3 The interim investigated: the turns cost is the harness's own grep block (2026-08-11)

The s31 gate rung passed on the fixed binary and `campaign.sh`'s tail auto-ran
an analysis §30.1 did not license. **Disclosed**: at n=120, `sub-sg − sub-rg`
read hitRegion −0.0226 [−0.0438, −0.0011] (at its own MDE, w/l 10/20 against
90 exact ties, sign p=0.099) and turns +1.40 [+0.56, +2.23]. The 848 was
halted to investigate, and the investigation reattributed both endpoints.

**The +1.40 turns is the blocked-grep tax, almost in its entirety.** Not the
floor (zero-floored sessions still ran +1.18; 21 refusals in 109 sessions add
~+0.22 weighted). Not file-reopening (ΔRead +0.16/session). Not extra
searching (sg 4.63 calls/session vs rg 4.50). It is shell `grep`: sg-arm
agents start one 1.23×/session against rg's 0.18×, every one refused by the
shim, every refusal a spent turn. Dose–response **+1.18 turns per extra
blocked attempt**, which at the +1.12/session exposure gap predicts +1.33 of
the +1.40 — residual **+0.07**. Where both arms hit equal blocks sub-sg is
*faster* (−0.54).

**A substitutive design is not neutral between arms — it subsidises the arm
whose treatment resembles the thing removed.** In production nothing blocks
`grep`, so this cost is the experiment's, not the tool's. The accuracy interim
inherits the artifact: 17 of the 20 sg-loss instances carry block messages.

Loss accounting since §28.2 (`mechanism.py --run-id s31`): the line-precision
deficit — 27% of sg's §28 losses at 2.3× rg's rate, the §29.1/§29.3 target —
is **gone**; sg now loses less to line precision (0.53 pts) than rg (0.71).
The residual is led by noise-submissions (24%), too small to judge the floor
by. One data note: the raw blocked-grep total (421) is inflated by a single
runaway session that looped a process-check 254 times; the per-instance
medians and dose buckets stand without it.

### 30.4 desc-v11: the exact-match escape hatch, routed (2026-08-11)

§30.3's finding is an unmet need and sg already has the feature: `-e`. No
description ever named it because §16.10 — an *unconditional* footer mention
moved ranked share 98% → 7%, the largest posture effect measured. desc-v11
walks that back narrowly: the regime changed (in a substitutive arm the
lexical urge has nowhere to go, priced at ~1.3 turns/session); the blocked
argvs name the routing (single known identifiers → `-e`; OR-of-candidates
regexes → ranked multi-word); and the mention is conditional with the identity
staying ranked-first. Registered tripwire for any campaign running v11:
**exact-share of sg invocations**, reported beside delivery. If exact-share
crowds out ranked search the variant dies as v6 and v7 did, reported as §16.10
replicating in a new regime. Surfaces: `DESC_CONDITIONS["desc-v11"]`,
`SG_LINE_V11` + `SWEXPLORE_SG_DESC=v11`. SHIPPED stays desc-v10 until measured.

---

## 31 Multi-phrase ranked search: giving the alternation habit a surface (2026-08-11)

Agents spell OR-intent as alternation and bring it to sg: 17 of 2,049 real
ranked invocations across s27/s31 contain a pipe — today dead syntax (the
tokenizer and wordpiece encoder both drop `|`, so `"a | b"` scores identically
to the pooled `"a b"`; verified byte-identical). Shapes, verbatim from the
shim logs: grep-escaped alternation of exact names (14), pasted code line
where `||` is the language's OR (2), names + an import-line phrase (1).

So `sg "a | b"` (and the grep spelling `"a\|b"`) now runs each phrase as its
own ranked search — own centroid, own coarse scan, own BM25 list — and
interleaves, fixing the §29.1 dilution for unrelated candidates. `||` never
splits; `-e` keeps regex `|`; keyword mode is untouched; a query with no pipes
takes exactly the old path, which keeps the snapshot byte-identical. Design
decisions worth their ink: coarse scores min-max normalize *within each
phrase* (RRF is rank-based, not comparable across lists); both dedupe sites
union the retriever bitmask into the survivor, or a phrase whose only
representative dies by stride accident reads spuriously floored; the fine
window is scored against *its own retrieving phrase*; final order is
fine-score + MMR with a representation pass pinning one bounded slot for a
non-floored phrase absent from the top-k, never for a floored one. The floor
goes per-phrase — "nothing matched 'X'" names the dead branch while live
phrases still answer, a refusal that tells the agent which candidate to
abandon. The footer names no flag (§30.2). No description advertises it.

### 31.1 Registered gate: the pair-replay, before any description advertises

`eval/swexplore/pairplay.py` mines consecutive same-session ranked sg call
pairs (same scope, no `-e`) from the s27/s31 shim logs, replays `a`, `b`, and
`"a | b"`, and scores **union-coverage@5** and **turn-saved**, plus the 14 real
`\|` queries replayed verbatim. Gate: merged union-coverage within 0.05 of the
sequential union, and the verbatim-14 not worse than pooled. A failure is
reported as the §24.2 shape — a plausible single-case feature losing on the
population — and the syntax stays unadvertised dead code rather than reverting.

### 31.2 The gate ran, and the answer is no (2026-08-11)

573 consecutive same-scope pairs mined, 201 with surviving checkouts replayed,
161 scored where the sequential pair had found gold:

| condition | registered bar | measured | verdict |
|---|---|---|---|
| merged top-5 covers the sequential union | ≥95% | **68.9%** (111/161) | **FAIL** |
| verbatim `\|` queries not worse than pooled | not worse | 6/6 same (survivors) | pass |

The failure is not slot arithmetic: at `-k 10`, an equal output budget, the
merged call rescues only 23 of the 51 failures (≈83% total, still under the
bar). The merged *ranking* degrades relative to two independent searches.

**The mechanism is about agents, not ranking, and it generalises.**
Consecutive same-scope searches are frequently *reformulations* — `a` missed,
`b` corrected it — not complementary halves of one intent. Merging re-imports
the abandoned mistake as a live phrase and the representation pass then
*guarantees the mistake a slot*. "An agent's second query often supersedes its
first rather than extending it." ripgrep alternation never had this problem
because a human types `a|b` in one intentional breath.

**Disposition, per the registration.** The syntax **stays, unadvertised** —
its no-pipe path is the pre-§31 path exactly, the six surviving real-pipe
queries score identically split or pooled, `||`-protection makes pasted code
safe, and deliberate single-breath alternation gets the per-phrase floor
verdicts, the one genuinely new capability. If shim logs ever show deliberate
alternation becoming common, the gate can be re-run on that population. One
harness defect fixed while gating: `pairplay.py` keyed its checkpoint on
Python's salted `hash()`, so no key survived a restart; `hashlib` now.

---

## 32 The powered campaign, take two: the repaired bundle against ripgrep (2026-08-11)

§30 halted at its gate rung and §30.3 reattributed nearly all of the
unfavourable interim to the experiment rather than the tool. Every finding
became a fix, and §31 landed alongside. This section runs the repaired bundle.

### 32.0 What changed since §30.1, and why this is a new registration

Engine: fine rerank at pool 30 plus the `(overlap, carries-a-word)` anchor fix;
floor 0.42 with the refusal reworded to name no flag; function chunking,
`--path` accepted, §31 multi-phrase (silent). Treatment: **desc-v11**, and
**shell grep unblocked in both arms**, still shim-logged. Process: no interim
analysis, exact-share tripwire, floored rate in envelopes.

The last two treatment rows are why this is **§32 and not a §30 re-run** —
they change what the bundle *is*, and "reusing §30.1's registration would be
registering one experiment and running another." The grep unblock applies to
both arms symmetrically, which levels the §30.3 subsidy and makes the contrast
about the tools again.

### 32.1 Pre-registration, written before R1 is funded

- **Primary**: `hitRegion@5`, `sub-sg − sub-rg`, paired `boot_ci` (4,000
  resamples, seed 1), computed **once**, on the pooled 848. MDE 0.012 at
  paired sd 0.126. `campaign.sh` now refuses endpoints at intermediate rungs —
  the §30.3 disclosure made structurally impossible.
- **Co-primary — turns and cost**: **|Δturns| < 0.5**, cost parity within ±5%,
  on §30.3's dose–response predicting the turns gap collapses.
- **Secondary, Holm**: `hitFile@5`, `ctxEff`, `nDCG@500`, `recall@100`,
  `precision`.
- **Diagnostics before any accuracy claim**: delivery per arm (premise ≈90%+);
  **exact-share** (§30.4's tripwire); **grep-passthrough per arm** (the §30.3
  mechanism check — if grep *substitutes* for sg, that is a finding about the
  tool); floored rate; path-scoped share against §30's 50% / 89%.
- **Registered expectation**: accuracy parity, |Δ| < 0.02. §30's −0.023 was
  measured under the blocked-grep tax at its own MDE, and §28's ladder decayed
  +0.062 → +0.002 as n grew — "regression toward zero is the base case, not a
  hope."
- **Registered response to a cost win at accuracy parity**: report it as the
  result. Gates between rungs are harness health only; no alpha is spent.

### 32.1a Amendment, made after seeing R1's delivery — intention-to-treat

§32.1 registered delivery as a **premise**; R1 came in at **72%** for the sg
arm, and this amendment — **written after seeing that number**, disclosed
rather than folded in — changes the frame, not the test. With shell grep in
both arms the contrast is `sub-rg` = ripgrep + shell grep (two lexical tools)
against `sub-sg` = semgrep + shell grep (a semantic tool with a lexical
fallback), i.e. **"is a semantic search tool available?"** holding lexical
capability constant. Under that framing an agent choosing grep over sg is
**part of the treatment effect, not a leak in it**: "a tool nobody reaches for
delivers nothing, which is a true fact about the tool rather than a defect in
the measurement."

The primary endpoint, test, MDE and one-computation rule are **unchanged**, so
no alpha moves. Delivery is reported as an outcome, not a validity gate. The
per-protocol read is still reported as a labelled secondary — "the ITT answers
'should I ship this tool', the per-protocol answers 'does it work when used'".
Honest cost: an ITT null is compatible with a tool that works well and is
under-adopted, and this design cannot separate those.

### 32.1b Two gate calibrations, both the "punishing the tool for being right" shape

1. `sg -e "->numslots = 0" src/cluster_legacy.c` was filed as **unknown flag**
   when the dash-leading token was the agent's *query* behind a good `-e`.
   `classify_usage` now scans all tokens and returns the existing
   caller-mistake verdict.
2. `tokio-rs__tokio-4867` tripped **"every search was empty"** on one sg call
   the floor refused. A ranked search exiting 1 is by construction the floor,
   so the distress filter now excludes it — the §30.2 fix applied to the check
   that shared its blind spot.

### 32.2 The result: parity, at a small and honest cost (2026-08-12)

**848 paired instances, 1,696 sessions, $286.** The registered analysis ran
once on the pooled 848; `campaign.sh` refused endpoints at the intermediate
rung, so unlike §30 there was no interim look to disclose.

| | `sub-rg` (control) | `sub-sg` |
|---|---|---|
| delivery | 82% | **74%** |
| own tool / session | 2.94 rg | 1.87 sg |
| shell grep / session | 2.82 | **4.03** |
| exact-share (§30.4 tripwire) | — | **19%** |
| floored searches | — | 2.3% (36/1,597) |

The §30.4 tripwire **did not fire**: 19% exact-share means agents took up `-e`
for the verification job desc-v11 routed it to and left 81% of calls ranked.
desc-v11 survives its kill condition. Delivery at 74% is the §32.1a story at
full n — the sg arm runs more grep than sg (4.03 vs 1.87), and a quarter of
sessions never invoke sg at all.

**Primary — a null, and a clean one.**

```
hitRegion@5   +0.0054 [−0.0043, +0.0153]   w/l 117/111   p=0.741   MDE 0.0138
```

Inside the registered |Δ| < 0.02 band on both sides, sign test flat (620 exact
ties), and the first endpoint in this program's history **powered enough to
mean it**: MDE 0.0138 against an estimate of 0.0054. Every secondary is null,
Holm-adjusted p ≥ 0.447. **Per-protocol agrees**: on the 628 instances where sg
was actually invoked, hitRegion is +0.0066 [−0.0048, +0.0182]. Because ITT and
per-protocol land in the same place, §32.1a's worry is **resolved rather than
merely disclosed** — sg does not beat ripgrep on the instances where agents
chose to use it.

**Co-primary — the registered turns prediction is wrong, and by more than
noise.**

```
turns  +0.4811* [+0.2217, +0.7500]   w/l 375/260   p=0.000   MDE 0.374
cost   +0.0038  [−0.0029, +0.0109]   sign p=0.000  (direction without magnitude)
```

The prediction *scraped through on the number and failed on the claim*: the
gap did not vanish with the grep block, it shrank from +1.40 to +0.48. Cost is
+$0.004/session — 2.3%, sign test significant, magnitude indistinguishable
from zero, inside the registered ±5%.

**What this campaign establishes.** With a lexical tool present — the
realistic deployment — adding semantic search changes retrieval accuracy by
less than 0.015 in either direction, costs half a turn and 2% more per
session, and gets used for about a third of searches. The §29 engine work is
real and measurable offline; "it does not convert into agent-visible accuracy
on SWE-Explore's line-level gold."

**Two harness defects measured, not fixed** (the LRU): its size accounting
reads a checkout *before* `ensure_index`, so a 6 GB cap held 9.7 GB (1.6×
undercount); and its in-flight protection held **130 checkouts against 6
workers**, blocking eviction entirely. Neither affects a result. Mitigation:
delete checkouts for instances complete in **both** arms.

### 32.3 The cross-campaign ledger: five arms, and what the search tool was worth

Five arms have run the full 848 with the same model, parser and gold. The `cc`
arm is designed for external comparison — upstream's `EXPLORE_PROMPT`
byte-for-byte, their explorer, their parser, only telemetry added.

| arm | $/session | turns | HitReg | CtxEff | $ per HitReg point |
|---|---|---|---|---|---|
| `cc` — upstream, unmodified | **0.158** | 7.47 | 0.457 | 0.931 | **0.346** |
| `cc-rg` — Grep + ripgrep | 0.179 | 8.21 | 0.462 | 0.937 | 0.389 |
| `cc-sg` — Grep + semgrep | 0.187 | 8.69 | 0.458 | 0.932 | 0.407 |
| `sub-rg` — ripgrep + shell grep | 0.167 | 7.48 | 0.449 | 0.933 | 0.371 |
| `sub-sg` — semgrep + shell grep | 0.170 | 7.96 | 0.455 | 0.927 | 0.375 |
| `sub-sg` §33 re-run (control) | 0.167 | 7.77 | 0.457 | 0.924 | 0.365 |
| `sub-sgb` — + bridge expansion | 0.168 | 7.74 | 0.460 | 0.933 | 0.365 |

**Calibration holds at full n.** The paper's Claude Code row is HitReg 0.531
and its Sonnet-4.5 row 0.428; since every agentic explorer there is driven by
GPT-5.4 (§27.0), the apples-to-apples target is the lower row. `cc` lands at
**0.457**. CtxEff runs high across all our arms (0.927–0.937 against a
published 0.715–0.829) because five tightly-scoped answers are structurally
favoured by that ratio — a shape difference, not an improvement.

**The uncomfortable headline: every search tool we added cost money, and none
bought accuracy.** Adding ripgrep costs +13%, semgrep +18%, and the accuracy
movements (+0.005, +0.002) are inside the measured noise floor (MDE 0.0138).
Normalised to cost per HitReg point the untouched baseline wins outright at
0.346, and `cc-sg` is the worst of the five at 0.407.

**CtxEff cannot see this.** All five arms sit within 0.010 of each other while
differing by 18% in dollars, because CtxEff measures the *shape of returned
context*, not the turns spent obtaining it (7.47 → 8.69). "Any future work
pricing an agent tool should measure cost directly; the published metric will
report a tie."

Two cross-campaign effects: unblocking shell grep made both §32 arms cheaper
than their §27 equivalents ($0.167/$0.170 against $0.179/$0.187) — §30.3's tax
in dollars; and semgrep's increment over ripgrep fell from +$0.007 to +$0.004,
directional at that size. The §33 pair is the one *within-tool* contrast and
lands at 0.365 $/point for both arms; the control's 0.457 reproduces §32's
`sub-sg` (0.455) on an independent 848 within 0.002 — the closest thing to a
replication this programme has.

**Total programme cost: $1,365 over 8,086 agent sessions** across §27, §28,
§30, §31 and §32. The bound is the deliverable: **on SWE-Explore's line-level
gold, with a Claude Code–shaped agent, the retrieval engine is not what moves
the benchmark.** The paper's own tables move HitReg 0.428 → 0.531 by changing
the *model* — an order of magnitude larger than anything reachable by changing
search, "and it is exactly the claim §29's offline wins would have licensed if
nobody had checked."

### 32.4 Why sg misses: a per-region root-cause census (2026-08-12)

`sub-sg` covers 45.5% of gold regions; this section asks where the other 54.5%
go. A census, not a sample: every one of the 3,992 gold regions across all 848
sessions, classified from trace envelopes, the shim's captured stdout, and the
transcript. `eval/swexplore/misswhy.py` implements it.

**What the wall is not.** K=5 is not the ceiling: with a median of 4 gold
regions per instance (mean 4.7), five one-region predictions could reach 0.915
and five whole-file predictions 0.939. The floor is not it either — floored
refusals are 0.3% of the loss (5 regions, 2 sessions).

| bucket (checked in order) | sub-sg | sub-rg |
|---|---|---|
| never surfaced despite a repo-wide ranked query | **31.0%** | 6.1% |
| session never invoked the tool | 23.7% | 18.1% |
| all five slots landed on *other* gold regions | 12.2% | 12.6% |
| never surfaced; every query scoped elsewhere | 9.0% | **41.0%** |
| tool displayed the gold file; agent submitted elsewhere | 8.6% | 3.7% |
| right file, wrong lines (>32 away / ≤32) | 7.4 / 3.2% | 7.3 / 3.4% |
| agent saw the file via other channels; didn't submit | 4.7% | 7.8% |
| never surfaced; every wide query floored | 0.3% | — |

Two classifier defects fixed: Claude Code's own startup greps in the shim log
mislabelled never-searched sessions as "used grep instead", and a basename
fallback matched files the agent never encountered — fixing it moved ~150
regions back into "never surfaced", which is why the retrieval bucket reads
31.0% and not 26.3%. `mechanism.py` carried the same overcount until
2026-08-14; recomputed on s32 its retrieval bucket rises 24.1% → 30.1% and
"gold surfaced, not submitted" falls 14.4% → 8.4%. §28.2's published splits
read as upper bounds on "the tool showed it", lower bounds on "the agent
guessed".

**The headline: 93% of sg's misses are shared.** Of the 2,453 regions sg
missed, the rg arm hit only 6.8% on the same instances, and only 5.0% of sg's
retrieval-bucket misses. The arms lose in mirror image: sg fails by *ranking*
(31.0% vs 6.1%), rg by *scoping* (41.0% vs 9.0%). "The tools trade failure
modes, not failure mass."

**The retrieval bucket is deep, not shallow.** Replaying the sessions' own
repo-wide queries at k=30 (158 of 881 never-surfaced regions still had a
checkout — a convenience sample) puts gold at rank 6–10 in 7%, 11–30 in 20%,
and **beyond rank 30 or absent in 71%** (2.5% ranked ≤5 on replay). A deeper
display would recover ~2% of the loss (~0.012 rate) — below §32's MDE. The
deep misses have a texture: median gold region 5 lines; two-thirds share fewer
than half their query's identifiers with the gold *file* (22% share none), and
where overlap exists it is ubiquitous tokens (23 of 33 sampled cases had no
rare shared token). "Neither a 256-dim static embedding nor BM25 has a bridge
to cross there."

#### 32.4a Inside the ranking bucket: five stages, and which one loses gold

`eval/swexplore/rankwhy.py` reran each replayable region's own wide queries
under five engine configurations plus an *exact* probe and a *self-retrieval*
probe. First matching class wins, 158 regions:

| class | share | reading |
|---|---|---|
| vocabulary gap — absent everywhere | 54% | 70 of 86 appear in **no** configuration's top-30 |
| in-pool ordering — hybrid rank 6–30 | 23% | retrieval delivered, ordering and the k=5 fold lost it |
| gold too generic to rank — self-probe fails | 15% | 20 of 23 are ≤5-line boilerplate |
| fine rerank killed it — `--no-fine` top-5 has it | 4% | including one coarse rank-1 demoted out of display |
| fusion drowned a lexical hit — bm25-alone top-5 has it | 4% | every exemplar a test file on an identifier query |

Instance-weighting moves nothing by more than 1.5 points. Two classes came back
**empty, and both absences are findings**: no region was unsearchable (the
exact probe found every file offering a quotable line, 154/154), and *function
chunking killed nothing* — window-32 ranks equal function-chunk ranks almost
everywhere, the first direct evidence that the §29 chunking change is
retrieval-neutral on real misses.

What outranks gold: 44% source, 29% tests, 16% docs, 8% locale packs, 3%
config — only 8% shares gold's directory. NodeBB `language/*/*.json` beating
source is pure ballast; docs beating code on code-vocabulary queries is §22's
docs-over-representation seen from the failure side.

The arithmetic this closes: the engine-addressable classes sum to 31% of the
bucket, 31% × 31.0% ≈ 10% of the total loss, ~0.053 rate points *gross* before
the anchoring tax. The other 69% is unreachable by any ranking change. The six
fine-rerank kills are the deferred `--fine-blend` sweep's target set.

#### 32.4b The bucket probed for levers: what moves it, what does not (2026-08-12)

- **Bigger displays cannot fix anchoring.** 36% of shown-not-submitted regions
  had the exact gold lines on screen and converted at zero; for the rest the
  median displayed line sat 52 lines from gold. Upper bound ~0.01 gross.
- **PRF is a clean negative on the vocabulary gap**: zero rescues on the 86
  regions at 4, 8 and 16 expansion terms, even at top-30. (It lifted 3
  ordering-class regions; not a lever.)
- **An import-graph neighborhood has real reach**: 48% of vocabulary-gap golds
  sit one import hop from a top-10 hit, 58% adding same-directory —
  generously matched, so a ceiling, but the only measured reach into the 54%.
- **A transformer embedding bridges part of the gap the static table cannot.**
  On the 24 vocabulary-gap regions in the 12 smallest surviving checkouts —
  every one absent from top-30 under all five static configurations — a
  generic 33M-parameter transformer (bge-small, whole-repo 32-line windows,
  best-window cosine) puts gold at **rank ≤5 for 21% and ≤30 for 46%**. Small
  n and a generic model, so a floor on what §9.9's swap could buy.
- **The fine-blend sweep is a trade, not a win**: blend 0.25 rescues 3 of 6
  fine-kills into top-5 but drops 8 ordering-class regions out of top-30; 0.5
  is balanced and rescued little. Default stays 1.0. `--keep-coarse-top`
  (shipped, off) is protective rather than curative.

**`--bm25-pin` is the change that survived.** The shipped default mode is
*semantic*, which never consults BM25; and the raw postings head is a better
gold-finder than even `--mode bm25`'s display, because fine rerank and MMR
demote lexical winners there too. The pin runs the lexical channel in every
ranked mode and guarantees its top-N chunks a display slot each, filling from
the tail, floor still winning. At k=5, pin 5 re-surfaces **32 of the 158
replayable ranking-bucket misses (20%)**, including 8 vocabulary-gap regions
whose gold BM25 knew all along. Two implementation findings: the first version
silently no-opped warm (`load_needs` gated the postings load on mode alone), a
cold≠warm split now held by an e2e test; and two existing tests needed the pin
explicitly off, one the guard's own control arm and the other
(`a_budgeted_entry_never_answers_a_line_windowed_query`) using
span-differences-across-chunkings as its instrument, which the pin blinds.

**The gate.** `guessplay` on the real harvested agent queries (467 instances,
3,441 dir/root rows, 4,212 file rows), semantic mode, paired against the
shipped default:

| arm | scope | Δrank@5 | cluster 95% CI | p |
|---|---|---|---|---|
| `--bm25-pin 3` | dir/root | +0.011 | [+0.006, +0.017] | 0.070 |
| `--bm25-pin 5` | dir/root | **+0.014** | [+0.007, +0.021] | 0.039 |
| either | file | +0.000 | [+0.000, +0.000] | 1.000 |

Both function metrics agree (+0.009 and +0.008 at pin 5 — not chunk geometry,
§24.1), the dose is strongest on 1-word identifier queries (+0.023), and
instance-level wins outnumber losses 10 to 2. First engine change in the
programme whose real-query CI excludes zero, so **the default is now
`bm25_pin: 5`** (`970fe89`). Snapshot re-recorded: 51 of 114 cases change, top
hits stable, tail slots swapping to the lexical head. Cost: one lexical query
per ranked search (~88 ms warm at kernel scale). The honest bound: +0.014 on
replayed queries and 20% of one 31% bucket compound to roughly +0.02–0.03
hitRegion gross before the anchoring tax. "The pin is shipped because the
designated offline referee ratified it and it repairs a real defect — not
because a future campaign is predicted to detect it."

**The agent-side buckets are one mechanism wearing four labels.** Trace
reading (28 sessions) found the same behaviour everywhere: agents submit only
what they have *Read*, and the snippet-to-Read conversion is governed by their
current hypothesis, not by rank — 92 missed regions sat in sg output at rank
1, 86 with the exact gold lines displayed, and the agent finalised anyway,
usually with a slot to spare. Preferences are systematic: definitions over
call sites, hand-written over generated, implementation over tests — while
SWE-Explore's gold is frequently exactly the call site, the generated file,
the test.

**The largest tractable lever is not in the engine.** Test-file gold is hit at
**11.0%** against source's **48.0%**, identically in both arms (rg:
10.0%/47.4%). Tests are 25% of gold regions and 7% of submissions. Equalising
that split is worth roughly **+0.09 hitRegion** — six times §32's MDE. The
cause is visible in the upstream prompt: "focus on finding the ROOT CAUSE, not
just symptom locations", five framed as a cap, and the actual scoring rule
(coverage of distinct gold regions, tests included) never disclosed.
Under-submission is the same story — 40% of missed regions are in sessions
that left slots empty, and the no-tool sessions (74% ran ≥1 sg search; the
rest navigated by grep or by paths named in the issue, converging in 2–6
turns) lose to early stopping, not to search.

**What this closes.** The engine's remaining absolute deficit is one bucket —
deep ranking misses on vocabulary-disjoint queries: 31.0% of the loss gross,
115 of 464 lost points (~25%) net of what rg loses to the same bucket — and
§23.2 already bounded the document-side levers against it at +0.023.
Everything larger sits in agent behaviour under a prompt that optimises for a
different objective than the metric.

---

## 33 Mining the repo's own associations: bridge expansion and neighbor injection (2026-08-13)

§32.4a left one engine bucket standing — the vocabulary gap, 54% of ranking
misses. Two candidates, both mining the repo itself, both prototype-first with
loose pass bars, since the in-situ campaign is the referee (§21.2).

### 33.0 The prototypes, and what the engine actually got

**P1 — co-occurrence expansion → bridge-file expansion.** The pairwise-PMI
table died in its first smoke: on NodeBB the tied-PMI tail picked alphabetical
locale noise ('agreement', 'alarm', '00pm'). What worked is one level up:
score every source file by idf-weighted *coverage* of the query's tokens (≥2
covered), take the top five as a committee, mine the terms ≥2 of them agree
on. On the 158 replayable ranking misses, appended at full weight:

| class | base t5/t30 | expanded t5/t30 | Δt5 | Δt30 |
|---|---|---|---|---|
| vocab gap (G, n=86) | 0 / 8 | 3 / 21 | +3/−0 | **+17/−4** |
| all 158 | 6 / 59 | 18 / 59 | **+16/−4** | +21/−21 |

The first technique to move the G class at all. The one wart — ordering-class
regions pushed out of top-30 (−13) — is full-weight concatenation diluting,
fixed in the engine version by reduced-weight expansion terms.

**P2 — neighbor injection: a null at file level, retried per-chunk.** The
EOF-comment prototype measured +1/−0 t5 on 158, mechanically explained: the
injected line lands in the file's *last* chunk and the gold's chunk never
carries it. The per-chunk variant is P2b.

**The engine got bridge expansion, not an artifact.** No `assoc.bin` at all:
bridge selection reads the BM25 postings the index already has, and mining
reads five files. `rank/bridge.rs` does committee selection and term mining;
`rank::top_k_weighted` scores caller-weighted terms (original tokens 1.0,
expansion terms `--bridge-weight`, default 0.4); `--bridge-expand N` gates it,
default 0. In semantic mode the expanded lexical head reaches the display
through the `bm25_pin` slots — embedding, fine rerank, floor and best-line
anchor all keep the original phrases. A cold==warm e2e test pins the parity.

**Drive-by finding, then fixed**: shipped PRF (`--prf`) was warm-only, a live
cold≠warm asymmetry hidden only by the `prf_terms: 0` default. Reproduced
first (at `--prf 4` warm ranked `src/jitter.rs` third, cold `docs/cooking.md`),
then fixed. "Wiring a *second* expansion through both paths is what exposed
the first one's missing half."

**Addendum, after the engine round-trip.** The lexical-only engine version
measured a quarter of the prototype's dose (vocab-gap top-30 +4 vs +17) — the
prototype expanded the query *string*, so most of its effect rode the semantic
embedding. The shipped version expands both retrieval channels and lands at
**8→16 of 86** vocab-gap regions in top-30, +2 C-class into top-5, costing 6
F-class regions their 6–30 band slot. P2b measured +8/−2 top-5 overall but only
+1 vocab-gap; engine C3 (import extraction, embed-text augmentation, cache
re-keying, two-pass cold) is **deferred as measured but dominated**.

### 33.1 Pre-registration, written before R1 is funded

- **Arms**: `sub-sg` (control, shipped engine, `--chunking function
  --min-score 0.42`) against `sub-sgb` (identical plus `--bridge-expand 8`
  injected by the shim). One binary, so both carry `bm25_pin: 5`; the contrast
  isolates bridge expansion alone.
- **Primary**: hitRegion@5, sub-sgb − sub-sg, paired boot_ci (4,000 resamples,
  seed 1), computed ONCE on the pooled 848. MDE 0.0138. **Co-primary**: cost
  and turns parity, ±5%. **Secondary (Holm)**: hitFile@5, nDCG@500,
  recall@100, precision.
- **Diagnostics before any accuracy claim**: delivery per arm; bridge-fired
  rate; floored rate; the §30.4 exact-share tripwire.
- **Registered expectation**: the offline dose (~9% of the ranking bucket ≈ 3%
  of sg's loss ≈ +0.015 gross) sits AT the MDE before the anchoring tax, so
  **the base case is a null**. Funded because in-situ evidence outranks
  offline instruments, not because a detectable effect is predicted.
- **Blocking conditions**: guessplay bridge arms regressing real queries with
  a CI entirely below −0.01; `triage_swex` gate failures stop the ladder.

#### 33.1a Amendment: a mid-campaign engine correction, and why the treatment arm restarted

R1 passed its gate (arms symmetric at 65–70% sg adoption, exact-share 21%
both, bridge firing on 71% of treatment searches). R2 halted on the account's
weekly limit: 1,057 rows returned `agent_error` at **$0.00 and zero tokens** —
an environment stop, stripped to `results/backup` because `--resume` skips by
`instance_id` regardless of status.

Reading banked telemetry turned up a real defect. **The engine was electing
locale packs, changelogs and docs to the bridge committee** — `locks.js redis
lock helper` → `cloudflare, david, draw, nib`. Those files contain the query's
words plus thousands of unrelated ones, so one seat floods the expansion. The
prototype never had this problem because its first smoke hit it and it was
given a source-only mining corpus; "the engine shipped without the equivalent
guard, so it was measuring a degraded variant of the mechanism P1 validated."
`bridge_mining_ignores_locale_and_doc_ballast` reproduces it; `NOT_A_BRIDGE`,
a deny-list of data/prose formats, fixes it.

**Consequence, taken deliberately**: the 120 banked treatment rows ran the
pre-fix engine, and finishing under the corrected one would make `sub-sgb` a
mixture of two treatments. They are written off (~$20) and the treatment arm
restarts clean. The 502 control rows stand — `bridge_expand: 0` makes both
fixes byte-identical no-ops there. Nothing in §33.1 changes. The re-measure of
the 158-region set under the fix could not be run (the LRU evicted all but 23
repos); the fixture test plus the mechanism argument is what stands behind the
correction, "stated here rather than papered over with a number from n=23".

#### 33.1b Disclosed interim look (2026-08-14)

Run at the user's request while blocked, on the **120** instances both arms had
completed and on the **pre-ballast-fix engine**: hitRegion@5 +0.0098 [−0.0155,
+0.0364] (w/l 19/18, p=1.000); hitFile@5 −0.0016 [−0.0291, +0.0262]; nDCG@500
−0.0039 [−0.0205, +0.0103]; CtxEff −0.0124 [−0.0344, +0.0102]; cost −0.0083
[−0.0276, +0.0141]; turns +0.0167 [−0.51, +0.63].

**It licenses nothing about the primary**: paired sd 0.147 at n=120 gives an
MDE of **0.0376**, so the effect under test (~0.015) is under half of it. No
stopping decision, no design change, no revised expectation. What n=120 *can*
speak to is the co-primary — cost $0.008/session cheaper, turns flat, both
inside ±5%. The figure worth watching at full n is CtxEff, the metric that
would move first if expansion made returned context more diffuse.

#### 33.1c The gate, re-run on the corrected engine (2026-08-14)

The original guessplay gate ran against the locale-electing build and is void.
Its replacement, same corpus and instrument, on the fixed engine:

| metric (semantic, dir/root) | base | bridge-8 | Δ | cluster 95% CI |
|---|---|---|---|---|
| rank@5 (gold file displayed) | 0.418 | 0.436 | **+0.018** | **[+0.005, +0.030]** |
| rank_func@5 | 0.154 | 0.156 | +0.002 | [−0.007, +0.011] |
| rank_func_ovl@5 | 0.164 | 0.165 | +0.001 | [−0.007, +0.011] |
| any metric, file scopes | — | — | +0.000 | [+0.000, +0.000] |

**The ballast fix nearly doubled the effect and moved the CI off zero**
(+0.010 [−0.001, +0.022] before, +0.018 [+0.005, +0.030] after). The
dose–response is the right shape: +0.010 at one word, +0.017 at two, +0.028 at
three-to-four — a committee needs two covered query tokens to form, and "an
effect that tracks its own precondition is much harder to explain as noise."

**The warning, stated before the campaign rather than after.** Both function
metrics are flat while the file metric moves +0.018; they agree with each
other, ruling out the chunk-geometry artifact, so what they jointly say is
that bridge expansion **puts the right file on screen without improving
line-level precision inside it**. §33's primary is line-level, so hitFile@5 is
where this should show and hitRegion@5 should move by *less* than 0.018 —
plausibly under the 0.0138 MDE once the anchoring tax is paid.

#### 33.1d The dilution factor, computed before the data (2026-08-14)

`eval/swexplore/bridgewhy.py` (written and dry-run on the discarded pilot
rows) stratifies the paired difference on whether expansion fired.

**Only ~61% of paired instances get any exposure** — in the pilot, 47 of 120
sessions never fired it and 42 of those never invoked `sg` at all. §32.1a's
availability-is-not-use, one layer in. ITT therefore estimates roughly **0.6×
the per-protocol effect**, so §33.1c's +0.018 predicts an ITT hitFile of about
**+0.011** and hitRegion under that — both below the 0.0138 MDE. **"The
registered null is now a quantitative prediction rather than a hedge."**

**The stratification is the real test**: an effect concentrated in the fired
stratum is a mechanism; one spread evenly is a coincidence. The never-fired
stratum doubles as a pairing check and in the pilot sits at zero (+0.0067, CI
straddling, w/l 3/4). Also fixed before it could mislead: the dose split was
written on the *share* of a session's searches that expanded, which is
degenerate (median share 1.00); dose is now the **count**, which in the pilot
shows fired-once +0.037 and fired-4×+ −0.031, all CIs straddling at n=13–37.
If that survives it says expansion helps the agent who asks once and hurts the
one who keeps re-asking — the opposite of guessplay's dose curve.

#### 33.1e Two limits, and a monitor that lied (2026-08-14)

The campaign met the account's **weekly** limit at the 848-rung and, after
reset, a **session** limit within minutes. Both produced rows with
`status: agent_error`, `$0.00`, zero tokens, and both required the same
cleanup: strip non-ok rows (`--resume` skips by `instance_id` regardless of
status, so a dead row permanently skips that cell) and delete the matching
`runs/s33/<instance>/<arm>/` directories, since 1,147 dead cells counted as
"instances missing an arm" and failed the gate on an artifact.

**A methodological error worth recording.** Quota return was tested with one
small probe request, which succeeded — and was taken as evidence. It was not:
leftover quota answered the probe and 1,147 sessions then died. "A probe
measures whether one request fits; a campaign asks whether a thousand do."

**And a monitor that reported a finish that never happened.** The supervisor
grepped an append-mode log for `RUNG s33 GATED OFF`, a string already present
from the morning's failure, so it fired instantly and declared the run over.
`campaign.sh`'s own lesson — "a no-op that reports success is the worst shape
a bug can take in a campaign driver" — reproduced one layer out, in the
watcher. Fixed by recording the log's length at launch. Banked at the pause:
**542/848 control**, 0/848 treatment, $95 of the ~$340 budget.

#### 33.1f A second interim look, taken by operator error (2026-08-14)

Intending to read only exposure diagnostics, `bridgewhy.py` was run — and that
tool computes the stratified endpoints as its main output. At n=600 of 848
paired: ITT +0.0024 [−0.0078, +0.0127], w/l 76/73, MDE 0.0145; bridge fired
≥1× (n=369) +0.0045 [−0.0082, +0.0173], w/l 56/55, MDE 0.0185. No decision was
taken, no arm changed, and the registered analysis still runs once on the
pooled 848. "An undisclosed peek is what turns a registration into
decoration."

The exposure diagnostic actually wanted **confirms §33.1d's prediction on
independent data**: 62% of instances had any exposure at n=600 against 61%
from the pilot, so the ITT estimate remains ≈0.6× per-protocol. Two process
fixes: `bridgewhy.py` gains `--diagnostics-only`, and it warns when its paired
n is short of the registered target.

#### 33.1g The gate's two standing failures, attributed (2026-08-15)

**"distress attributable to the tool: 3."** All three are an agent running
`sg -e <Identifier>` three times where the identifier is simply not in the
tree (`TeleportReplicaNameEnv`, `sql.NullString`, `TmpAndSlash`) — exact mode
behaving exactly as grep would. Attributed per arm: teleport 7/6/**3** control
vs 4/2/0 treatment; navidrome 16/6/**7** vs 4/2/0; tokio-axum 6/0/0 vs
5/3/**3**. Two of three are control-arm, and expansion only touches ranked
search while `-e` bypasses ranking entirely. The check is measuring the §16.10
exact-mode miss pattern, symmetric across arms.

**"instances missing an arm: N."** Cells producing no row at all: evicted
checkouts and a re-clone starvation loop. Trading parallelism for cache
residency (WORKERS 4→2, CACHE_GB 10→14) moved it 824 → 836 in one run,
confirming the diagnosis. Neither failure bears on the endpoints; both arms
remain symmetric on the diagnostics that do — sg adoption 73% vs 72%,
exact-share 20% vs 19%.

### 33.2 The result: a null, and a dose curve that argues with it (2026-08-15)

848 instances, both arms complete, $283. The registered analysis ran once.

| endpoint | sub-sg | sub-sgb | Δ | 95% CI | p | MDE |
|---|---|---|---|---|---|---|
| **hitRegion@5** (primary) | 0.4573 | 0.4598 | **+0.0025** | [−0.0063, +0.0114] | 0.643 | 0.0130 |
| cost $/session (co-primary) | 0.1665 | 0.1676 | +0.0011 | [−0.0055, +0.0083] | 0.810 | — |
| turns (co-primary) | 7.77 | 7.74 | −0.026 | [−0.283, +0.243] | 0.632 | — |
| hitFile@5 | 0.5305 | 0.5291 | −0.0015 | [−0.0124, +0.0089] | 0.809 | 0.0152 |
| ctxEff | 0.9239 | 0.9329 | +0.0089 | [−0.0008, +0.0187] | 0.781 | 0.0139 |
| precision | 0.7490 | 0.7634 | +0.0144 | [−0.0011, +0.0293] | 0.352 | 0.0217 |

**The primary is a null**, co-primaries hold parity comfortably (cost +0.7%,
turns −0.3%), every Holm-adjusted secondary is 1.000. Bridge expansion costs
nothing and, over all 848 instances, buys nothing.

**The prediction was right, and being right about a null is the point.**
§33.1c/§33.1d forecast ITT hitFile ≈ +0.011 and hitRegion below it from two
premises fixed before the data; exposure came in at **62%** and hitRegion at
+0.0025. "A null predicted quantitatively is evidence about the world; a null
discovered afterwards is only evidence about the instrument."

**And then the dose curve**, on the count of expanded searches:

| dose | n | Δ hitRegion@5 | Δ hitFile@5 | 95% CI (file) |
|---|---|---|---|---|
| never fired | 319 | −0.0017 | −0.0067 | [−0.022, +0.010] |
| fired once | 284 | −0.0034 | −0.0014 | [−0.023, +0.021] |
| fired 2–3× | 202 | +0.0090 | −0.0041 | [−0.024, +0.016] |
| **fired 4×+** | **43** | **+0.0425** | **+0.0497** | **[+0.013, +0.088]**, p=0.013 |

The never-fired stratum sits at zero — the pairing check passing. The effect
rises monotonically with dose, reaching **+0.05 on both metrics** at four or
more firings, the only stratum whose CI excludes zero, on the file metric
§33.1c predicted would carry the mechanism.

**This is a hypothesis, not a finding.** n=43 is 2% of the campaign; the
stratum is defined by an outcome-adjacent behaviour; and the pilot's dose curve
ran the *opposite* way. "Two contradictory dose curves from the same tool are
what noise looks like at n≈40." What survives is a testable claim: **bridge
expansion pays off for agents who search repeatedly, and is inert for agents
who search once** — about *persistence*, and testable only by an arm that
manipulates search count.

**The bound.** On SWE-Explore's line-level gold, with a Claude Code-shaped
agent, repo-mined query expansion moves hitRegion@5 by less than 0.013 and
hitFile@5 by less than 0.015, at cost and turn parity — even though the same
change is worth +0.018 [+0.005, +0.030] on replayed agent queries offline.
§21.2 claims another one, and "the arithmetic of why is now measured rather
than guessed — a 0.62 dilution from non-invocation, and an agent-side
conversion step that §32.4 already showed ignores rank-1 hits."

---

## 34 The unit view: what a ranked hit looks like (2026-08-15)

Ranked hits have printed grep's `path:line:text` per passage line since the
beginning, indentation stripped per line. Screenshots from real sessions
showed the worst case: the path repeated four times to show four lines (§26.4
priced the prefix tax at roughly half of all output bytes), interior blank
lines each costing a full prefix, relative nesting destroyed by the per-line
trim, and fine windows opening on `},` / `)` / a bare `"""`.

### 34.1 The pilot: 30 real searches, 10 languages

Thirty queries, three per language across Python, Go, JavaScript, TypeScript,
Rust, Java, PHP, C, Ruby, C++, re-rendered through a prototype "unit view":
path once as a `path:start-end` header, a `line:`-numbered gutter, block
dedent (the `-C` frame's rule), edges snapped off bare closers/openers, and the
window framed by its enclosing declaration with the middle elided. It worked
everywhere the naive rendering failed — redis's 221-line `activeExpireCycle`
collapsed to signature + match + close; django's orphaned `for field, messages
in error_dict.items()` gained `class BaseModelForm` / `def _update_errors` —
and failed two known ways: a window inside a backtick template literal anchors
at column 0 and elects string content as its head, and a multi-line
signature's `) {` tail gets elected instead of the signature's first line.

### 34.2 The noise audit: 57% of rows were renderer-added

Across the 90 pilot hits: **738 rows, 320 matched, 418 (57%) added by the
renderer** — the naive prototype roughly doubled every hit, and at 1.51× the
bytes of the grep-form output it gave back everything the path dedup saved.

- **The innermost head is the entire concept.** One line per hit, nearly all
  of the de-orphaning value. Unconditional.
- **8 of 138 head rows restated the path** (`module Cop` above a hit in
  `cop/layout/…`) — the §26.4 disease reintroduced vertically. An outer head
  must add a name the path does not carry.
- **6 head rows were flow, not identity** (`) {`, `} else {`, bare `else`), and
  4 were their hit's *only* head — actively misinforming. A `) {` resolves to
  its statement start.
- **~40 of 56 close lines arrived after an elision**, restating the header
  span. A close prints only when it touches the window.
- **131 elision rows** duplicated what the gutter numbers already encode; the
  marker shrinks to a bare `⋮` row.

Calibrated, **added rows drop from ~4 to ~1.5 per hit** and the byte cost to
~1.15× the grep form. The rules shipped in `search::unit`, each pinned by a
unit test carrying its audit case, including the two pilot failures.

### 34.3 What shipped, and what still gates it

The unit view is the ranked-mode default (`SearchOptions::unit_view`, computed
in `materialize` — the one-reader rule — and rendered in `out`). Three
surfaces stay byte-identical by construction: `--no-unit` restores the bare
fine-window passage as the A/B control; an explicit passage shape
(`--passage-chars`, `--passage-lines`, `--full`) wins, which pins
`tools/snapshot.sh`'s `--passage-lines 1` recording; and `--no-fine` still
reproduces pre-§28.2 output byte for byte. Exact mode is untouched.

The honest ledger for the default-first decision: §25 measured the closest
prior treatment — a `# path:span defines:` header above unchanged passages —
as a behavioral null at 1.9× bytes, while §26 measured passage *content* with
a clean dose-response on reads-after-search; the unit view is content, not
annotation, "but that is an argument, not a measurement." §28.2 located sg's
one deficit in line precision and the unit view moves the path out of the body
rows. A `disp-unit` vs `disp-nounit` campaign on reads-after-search and
right-file-wrong-lines is the standing follow-up; if line precision regresses,
the revert is one default flip.

#### 34.4 The 26-query audit: three residual defects, two rules (2026-08-15)

26 fresh queries over the ten SWE-Explore languages plus this repo, 78 hits,
every added row checked against its source. The calibrated classes held — zero
namespace leaks, zero flow heads, zero closes-after-elision, median 1 added row
per hit, bytes at 1.04× the grep form. Three defects survived:

- **The unit-boundary straddle** (4/78 harmful, ~5% with milder cases). The
  fine window likes landing on boundaries, so windows arrive as [last
  statement, `}`, blank, next declaration]. The foreign tail misleads and the
  shallow closer drags the anchor down so the head walk finds nothing.
- **A comment elected as head** (2 observed). The second sighting changed the
  prescription: the rule is not "comments are never heads" but "a comment
  heads only its own block".
- **Anchor drag without visible harm** (~5 hits): closer-heavy windows
  resolving the head one level too far out.

Two rules fix all three, both with pinning tests: truncate the window at the
first interior closer-only line shallower than its opening line, and compute
the head-walk anchor from the window's content lines, closers excluded. The
fine-window election itself is deliberately untouched — "its boundary appetite
is scoring-side behavior with §28.2 calibration behind it, and the renderer
absorbs the symptom."

#### 34.5 The polish pass: three shapes out of 309 hits (2026-08-15)

A second live audit — 103 queries, 33 scopes, 309 hits — found **zero
misinforming defects** (§34.4's classes stayed extinct at 4× the sample) and 13
polish cases in three shapes, all fixed in `search::unit`:

- **A: the dangling `*/`** (4/309). Fixed by definition rather than by rule:
  `*/` closes something the window does not show, so it *is* a closer-only
  line and the existing snap peels it.
- **B: mid-block, opener locked out** (7/309). A window starting mid-javadoc
  usually contains the col-0 declaration it documents, so the anchor is 0 and
  no head walk reaches the `/**`. Fixed by a walk-back (≤12 lines) that
  prepends the block's top under two caps, 3 rows and 240 characters, opener
  exempt from the character cap. Gap-fill learned the same character bound.
  Python docstring middles stay out of scope by design.
- **C: namespace as innermost head** (2/309, the one rule bug). The §34.2
  path-redundancy rule only ran for outer heads.
  `module`/`namespace`/`package` lines now take the informative check at any
  position; redundant ones walk past, usually ending bare, which is accurate.

Also measured: median 1 added row per hit, a quarter of hits correctly bare,
bytes at 1.11× the grep form, and 10 of the 21 hits first flagged as
"mid-comment-open" already rendered their opener via the §34.4 own-block rule
— the detector was not looking above the window.

---

## 35 Structural signals: path boost, learned checklist, graph expansion (2026-08-15)

§32.4's census said the loss splits into a 54% vocabulary-gap bucket no
reranker can reach, a 23% ordering bucket, and a tail of self-inflicted
wounds. This campaign works both sides in ladder order, cheapest first: a
filename/path boost and a learned linear combination of signals we already
compute (ordering side), then import-graph pool expansion (§32.4b's 48–58%
reach). The §9.9 code-table re-distill is out of scope by decision, not by
evidence.

### 35.0 The probe set, regenerated before it shrank further

The §32.4a/b sets were never committed and the checkouts they replay against
are LRU'd. Regenerated from the surviving `s32` artifacts: `misswhy.py` wrote
2,453 region rows; `rankwhy.py` replayed **302** never-surfaced regions (up
from 158 — the §34 stdout parser fixes taught the replay to read the unit
view). Decomposition of the 302:

    vocab-gap        138   46%
    ordering          99   33%
    fusion-drowned    23    8%
    too-generic       23    8%
    fine-killed       15    5%
    not-searchable     4    1%

Consistent with §32.4a's 54/23/15 within resolution; the ordering bucket grew,
the direction a parser that previously dropped parsed-as-empty ranked output
would move it. The 138 vocabulary-gap regions are checked in at
`eval/queries/vocabgap-s32.jsonl`.

### 35.1 Pre-registration: the path boost

Mechanism: the decl-boost loop generalizes to one structural pass over the same
k*6 head — zero added I/O. `path_share` = |qtokens ∩ path tokens| / |qtokens|;
multiplicative alongside the decl term; `--path-boost`, default 0.0.

Predictions, written before the first run: **small positive on rank_func,
concentrated in the ordering bucket** (path tokens already reach both channels
via `path_render: Full`, so this measures the *increment* of a rank-time boost
over path-as-content); **below the bm25_pin bar**, +0.005..+0.015 on rank_func
at the best weight, taking the flag only if the CI excludes zero (only 8% of
what outranks gold shares gold's directory); **both function metrics move
together** — diverging is a bug signature (§24.1), not a finding.

Gate: `guessplay.py` on `guesses-v1-desc-all.jsonl`, arms `--path-boost 0.25 /
0.5 / 1.0` in one pass, cluster bootstrap over instances. Kill: no arm's
rank_func CI excludes zero, or any arm regresses rank_func_ovl.

### 35.2 Pre-registration: the learned checklist

The tiers combine evidence with hand-picked constants. The checklist replaces
the *final* combination — the `relevance` vector MMR consumes — with a
logistic regression over per-candidate features, trained on the harvested
guess corpus, shipped as a const weight array. Features are candidate-local
only (fine cosine, coarse fused score, reciprocal bm25 rank + missing flag,
phrase popcount, decl_share, path_share, span length, query token count), so
cold==warm holds by construction; `--learned-blend` defaults 0.0.

Protocol fixed before training: labels join the guess corpus to Loc-Bench gold
through `scoring.py`'s matchers; the split is grouped by *instance* (a
query-level split leaks); training on the desc-v5 majority slice. Two gates:
**offline held-out lift** over a fine-score-only baseline — nil lift stops the
spend, since "a model that cannot beat its strongest single feature on its own
training distribution has nothing to offer the engine"; then **guessplay**,
arms 0.25/0.5/1.0 atop the accepted §35.1 configuration, adopting only if
rank_func's CI excludes zero with rank_func_ovl not regressed.

Predictions: coefficients concentrate on fine cosine and bm25 rank; any win is
small and lives in the same 23% ordering bucket as §35.1. The failure mode to
watch is scale — `mmr_lambda` mixes the learned score against raw cosine, so
the sigmoid squash is part of the registered design, not a tuning knob.

### 35.3 Pre-registration: graph expansion

The one lever aimed at the 46% vocabulary-gap bucket: the answer shares no
words with the query, but it is wired to a file that does. §32.4b measured the
reach (48% one import hop from a top-10 hit, 58% adding same-directory) and
called it a ceiling, "generously matched".

Mechanism, fixed before any run. At build, one tree-sitter parse per supported
file extracts imports (ERROR nodes tolerated — imports are local, so `cut`'s
bail-on-error rule deliberately does not apply). Specifiers resolve against the
corpus file table by longest path-suffix, ambiguity above 4 files a deny not a
guess — the §33 locale-ballast lesson in resolver form. Edges are undirected,
stored as `graph.bin` (CSR, postcard), `has_graph` in the meta; an old index
answers `--graph-expand` with a hard error that self-heals through the
discard-and-stream path. At query time seeds are the heads of both tier-1
lists; their 1-hop neighbor files' chunks join the *scoring pool* — the lexical
side earns a real scoped BM25 score, the semantic side is scored with the same
quantized query pre-MaxSim. `--graph-weight` scales what wiring earned. This is
candidate-pool expansion, not embed-text injection (§33/C3 priced that and it
stayed dominated). Grammarless builds refuse the flag.

Gates in order: **(1) pool-recall probe** on `vocabgap-s32.jsonl` — a
legitimate offline use because pool *membership* is recall, not ranking. Kill:
fewer than **10%** of the 138 regions gain gold-in-top-30 at `--graph-expand
8`. **(2) guessplay**, arms 4/8/16, adopt only on a rank_func CI excluding zero
with rank_func_ovl not regressed and `rank:graph` p50 under ~10% of total.
**(3)** only then a live campaign arm.

Predictions: gate-1 conversion lands well under the 48–58% reach — the
resolver is exact where the census was generous, and reaching the pool is not
surviving fusion. §33.2's dilution arithmetic carries. The failure worth
watching is hub files. If gate 1 fails, the residual diagnosis is resolver
reach vs. fusion drown, and the two prescribe different follow-ups.

### 35.4 Graph expansion: the gate-1 kill, and which residual it was

Built as registered. The pool-recall probe killed it on the first rung:

    arm              vocab-gap regions   gold ≤30   gold ≤5
    --graph-expand 8       138              1 (0.7%)    0
    --graph-expand 2       138              0 (0.0%)    0
                                        [gate: ≥10%]

The mechanism is not inert — the trace envelope shows injection firing and
saturating its 256-chunk cap on ansible-scale repos — and the pre-registered
reach-vs-drown discriminator ran in the direction that settles it: fewer seeds
means less cap pressure and sharper neighborhoods, and it got *worse*. So the
failure is **reach**: the exact resolver's 1-hop neighborhoods of the actual
seed heads do not contain gold for these regions. §32.4b's 48% was measured
with generous matching from any top-10 file and warned it was a ceiling; the
conversion under exact resolution and real seeds is under 1%. (Over the full
302-region never-surfaced set the same replay puts gold ≤30 for 37% — the
ordering bucket is reachable; the vocabulary-gap bucket stays out of reach
through this door too.)

Verdict: **kill at gate 1**, as registered. `--graph-expand` stays 0, no
guessplay or campaign spend follows, and the code stays a measured-and-dormant
lever beside `--bridge-expand` and `--prf`. "The next credible attempt at this
bucket is not more neighbors but better *resolution*" — per-language module
resolution instead of path suffixes — or a different seed source, either
re-entering through this same probe, which now costs minutes.

### 35.5 The path boost: a perfect null, and why it is perfect

Gate run: 63,336 arm-rows, one binary, arms 0.25/0.5/1.0 against the empty
control, semantic and bm25, both function metrics and file rank. The result is
not a small effect with a CI over zero — it is **+0.000 in every cell with 0/0
discordant instances**, at every weight, on every metric, in both modes:
across 7,657 real queries × 3 weights, the boost never flipped a single @5
outcome in either direction.

A null that clean demands a mechanism check (§24.2), and it passed: the same
binary on the parity fixture moves `cooking.md` to rank 1 under `--path-boost
4.0` through the CLI. The boost is live. The null decomposes:

- **File scopes (55% of rows): inert by construction.** Every candidate shares
  the scope's one path, the multiplier is uniform, and a uniform multiplier
  reorders nothing. "It should have been registered as an identity."
- **Directory scopes: §29.1's invisibility argument, now measured.** At
  `fine_blend = 1.0` the fine window owns the final order, so a post-fusion
  boost acts only through candidate *membership* at the k*6 cut — and
  membership changes that flip a gold@5 outcome did not occur once in 22,995
  paired comparisons.

Verdict: **kill, as registered** — `--path-boost` stays 0.0; the
structural-boost refactor and the share threading stay, since the checklist
consumes them as features. The transferable finding is bigger than the flag:
**any post-fusion, pre-fine multiplicative reordering is a dead lever class
under the shipped fine blend** — including, retroactively, part of why §24.3's
decl weight sweep was flat. A future boost of this shape must either act on
the fine-blended order (the checklist's slot) or argue for a `fine_blend < 1`
regime first.

### 35.6 The checklist gate (in progress) — an interim look, documented before the end

The trimmed gate (semantic, control/0.5/1.0, full corpus) was stopped at the
halfway mark by request and scored — a deliberate interim look, recorded
§33.1f-style before the run finishes, so the final analysis must be read
knowing it happened.

At 198 dir/root instances (alphabetical truncation, pairing intact): blend 0.5
read rank_func +0.013 [+0.004, +0.023], ovl +0.013 [+0.004, +0.024] (the two
moving together — §24.1's ranking-not-geometry signature), file rank +0.027
[+0.015, +0.043], file scopes +0.003 not regressed. Blend 1.0: larger points,
wider CIs, discordance both ways.

The run was resumed to the registered full corpus rather than declared on the
peek: "the effect sits exactly at the +0.014 adoption bar, which is where
half-sample CIs mislead."

### 35.6 (concluded) The checklist ships: +0.012 strict on real queries, every gate green

Full corpus, 465 instances, one binary per phase, the interim look already on
the record. Semantic mode (shipped default):

    arm          scope      rank_func            rank_func_ovl        file rank
    blend 0.5    dir/root   +0.012 [+.005,+.020] +0.012 [+.005,+.020] +0.025 [+.015,+.035]
    blend 0.5    file       +0.010 [+.002,+.020] +0.011 [+.003,+.021] identity
    blend 1.0    dir/root   +0.019 [+.005,+.033] +0.022 [+.008,+.036] +0.039 [+.022,+.058]
    blend 1.0    file       +0.013 [+.000,+.026] +0.012 [-.001,+.025] identity

And the bm25 tripwire did not merely hold — it improved: +0.010 [+0.003,
+0.019] strict on directory scopes, +0.016 file rank.

Both function metrics move identically everywhere — §24.1's
ranking-not-geometry signature — and the dose is monotone, which no boost in
this program ever showed. **Adopted: `learned_blend: 0.5` default**, snapshot
re-recorded in the same commit (a pure permutation: 256 lines moved, none
added or dropped, the reorders reading as corrections — prose chunks that led
on raw score now sit below the code that computes the thing asked about). 1.0's
larger dose is measured and unshipped: its file-scope CIs touch zero, it
carries 3× the discordance, and the weights are trained on nine mostly-Python
repos — raising the dose is registered as a follow-up gated on an
off-distribution floor (cosqa or the blind ladder), not a tuning knob.

The §35 ledger, closing the campaign:

- **35.1 path boost — killed** (§35.5): a perfect null; the
  post-fusion/pre-fine boost class is dead under fine_blend 1.0.
- **35.3 graph expansion — killed at gate 1** (§35.4): reach, not drown;
  suffix resolution converts <1% of the census's generous 48%.
- **35.2 learned checklist — shipped**: the one lever that acted after the
  fine rerank, where §35.5 says a reordering signal must act — and the first
  engine change since bm25_pin whose real-query CI excludes zero, at twice the
  file-rank effect.

The through-line the three verdicts share: the §32.4 census said only ~10% of
the loss was reorder-addressable, and the checklist just collected a
measurable slice of exactly that bucket while both attempts to reach *outside*
the pool (paths as content, imports as wiring) died on contact with real
queries. The vocabulary gap still owns the majority of the loss, and the §9.9
code-teacher table remains the one unexecuted lever aimed at it.
