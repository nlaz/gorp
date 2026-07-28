# Research: collapsing modes & beating ripgrep as the agent search primitive

**Status:** research phase, 2026-07-27. Feeds a redesign of the CLI surface
(DESIGN.md is the v1 design this revisits). Question under study: semgrep
currently exposes four modes (`hybrid|keyword|bm25|semantic`); should the
agent-facing surface have *no modes at all* — and how much of the "smart
driver" work that Claude Code does in prompts can we push down into the tool?

---

## 1. The incumbent: how Claude Code actually drives ripgrep

Claude Code is the benchmark not just for the *engine* (rg) but for the
*prompting system* around it. The search stack is a cost-ordered ladder:

- **Glob** (file patterns → paths, near-zero tokens)
- **Grep** (ripgrep wrapper; structured params, not raw CLI)
- **Read** (500–5,000 tokens per file)
- **Explore subagent** (a cheaper model does multi-hop search in an isolated
  context and returns only conclusions — search *overhead* is kept out of the
  main loop's context)

Key mechanics of the Grep tool interface (extracted verbatim from the
installed CLI, v2.1.220 — see §1.1):

- **Default output mode is `files_with_matches`** — paths only, the cheapest
  possible result shape, **sorted by mtime descending**. Recency is Claude
  Code's only relevance ranking. `content` (matching lines, `-A/-B/-C`
  context, line numbers) and `count` are opt-in.
- **`head_limit` defaults to 250** entries/lines, with `offset` pagination
  and an in-band footer when paginated. Line width is bounded by rg's
  `--max-columns 500`. The Grep tool's whole result is capped at **20,000
  chars (~5k tokens)**; oversized results across tools aren't truncated but
  redirected — the model gets a `<persisted-output>` block with a 2 KB
  preview and a file path to the full output.
- **A delegation threshold is prompted explicitly:** "For broad codebase
  exploration or research that'll take more than 3 queries, spawn Agent with
  subagent_type=Explore. Otherwise use the Glob or Grep directly." Multi-hop
  search is expected, budgeted, and — past 3 hops — moved into a subagent's
  context so the file dumps never hit the main loop. The Explore agent is
  additionally prompted to issue parallel tool calls and comes in caller-
  specified breadths ("quick" / "medium" / "very thorough").
- System-prompt guidance steers hard away from `grep`/`find`/`cat` in Bash
  toward the dedicated tools, and toward batching independent searches as
  parallel tool calls.

### 1.1 Extracted tool prompts (Claude Code v2.1.220, verbatim)

Two prompt variants ship, gated on model; the lean one (newer models) is the
one to benchmark against. Estimated schema+description cost of the Grep tool
as rendered into the prompt: roughly 500–600 tokens (description ~90 tokens
lean; 13 parameters with multi-sentence descriptions dominate).

**Grep description (lean):**

> Content search built on ripgrep. Prefer this over `grep`/`rg` via Bash —
> results integrate with the permission UI and file links.
>
> - Full regex syntax (e.g. "log.*Error", "function\s+\w+"). Ripgrep, not
>   grep — escape literal braces (`interface\{\}`).
> - Filter with `glob` (e.g. "**/*.tsx") or `type` (e.g. "js", "py", "rust").
> - `output_mode`: "content" (matching lines), "files_with_matches" (paths
>   only, default), or "count".
> - `multiline: true` for patterns that span lines.

The legacy variant adds: "ALWAYS use Grep for search tasks. NEVER invoke
`grep` or `rg` as a Bash command", and "Use Agent tool (if available) for
open-ended searches requiring multiple rounds".

**Parameters (13):** `pattern`, `path`, `glob`, `type`, `output_mode`
(content | files_with_matches | count), `-A`/`-B`/`-C`/`context`, `-n`
(default true), `-i`, `-o`, `head_limit` ("Defaults to 250 when unspecified.
Pass 0 for unlimited (use sparingly — large result sets waste context)"),
`offset`, `multiline`. Note the token-economy language living *inside a
parameter description*.

**Grep mechanics:** bundled rg with `--hidden`, VCS dirs excluded,
`--max-columns 500`; files_with_matches → `-l`, stat'ed and sorted by mtime
desc, formatted `Found N files\n<path>…`; content mode returns rg's own
`path:line:content`; count mode appends `Found N total occurrences across M
files.`; pagination footer `[Showing results with pagination = limit: N,
offset: N]`; empty → `"No files found"` / `"No matches found"`; result cap
20,000 chars.

**Glob (lean):** "Fast file pattern matching. Supports glob patterns like
`**/*.js` or `src/**/*.ts`. Returns matching file paths sorted by
modification time." Default limit 100 files; truncation footers say how to
narrow ("Consider using a more specific path or pattern.").

**Explore agent `whenToUse` (lean):** "Read-only search agent for broad
fan-out searches — when answering means sweeping many files, directories, or
naming conventions and you only need the conclusion, not the file dumps. It
reads excerpts rather than whole files, so it locates code; it doesn't
review or audit it. Specify search breadth: 'medium' for moderate
exploration, 'very thorough' to search across multiple locations and naming
conventions."

**general-purpose agent `whenToUse`:** "…When you are searching for a keyword
or file and are not confident that you will find the right match in the
first few tries use this agent to perform the search for you." Its system
prompt encodes the search strategy itself: "search broadly when you don't
know where something lives… Start broad and narrow down. Use multiple search
strategies if the first doesn't yield results. Be thorough: check multiple
locations, consider different naming conventions, look for related files."

**System-prompt lines:** "Prefer dedicated tools over Bash when one fits…";
"make all independent tool calls in parallel"; "For broad codebase
exploration or research that'll take more than 3 queries, spawn Agent with
subagent_type=Explore"; Bash description: "Avoid using this tool to run
`find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`…".

**Read tool (the downstream cost):** 2000-line default window, `offset`/
`limit` for larger files, "When you already know which part of the file you
need, only read that part." Search quality directly gates this: a ranked
`path:line` hit lets the model do a targeted offset-read instead of a
whole-file read.

### 1.2 What the prompting compensates for

Every clause of that prompting exists to patch a weakness of exact-match
search as an agent primitive:

| Prompt/system mechanism | rg weakness it patches |
|---|---|
| `files_with_matches` default + mtime-desc sort | unbounded output on common terms (O(hits), not O(k)); no relevance signal (recency is the proxy) |
| `head_limit` 250 + pagination + 20k-char cap + persisted-output redirect | same |
| ">3 queries → spawn Explore subagent" | no ranking → triangulation is expected, so its token cost is quarantined in a subagent context |
| "start broad and narrow down; try different naming conventions" (agent prompts) | a miss returns *nothing* — the search strategy itself must live in prompt text |
| regex-syntax coaching (brace escaping, multiline flags) | query language is regex, not intent |
| parallel-tool-call guidance | each probe is cheap but low-yield, so throughput comes from batching guesses |

The design bet of this project: a tool that returns a **ranked, bounded,
deduplicated top-k for an intent-shaped query** makes most of that prompt
scaffolding unnecessary — the patch moves from the prompt (paid in input
tokens every session, executed by the expensive model every loop) into the
tool (paid once, in Rust, at ~135 ms).

---

## 2. Token economics of a search round-trip

### 2.1 Cost anatomy

Each tool call re-sends the whole conversation as input. With prompt caching
(cache reads ≈0.1× input price; Claude Code caches aggressively, ~90%+ reuse),
the *marginal* cost of one search round-trip is approximately:

```
  (tool result tokens) × full input price      ← dominant, controllable by the tool
+ (model's reasoning + next tool call) × output price   ← output tokens cost 5× input
+ (whole prior context) × 0.1 × input price    ← the tax on long loops
```

Two consequences:

1. **Tool-result size is the dominant controllable cost.** Anthropic's own
   tools-engineering guidance says exactly this and recommends concise-by-
   default output with opt-in verbosity.
2. **Every extra round-trip costs output tokens** (the model must reason and
   emit the next call — typically 100–300 output tokens ≈ 5× the price of the
   same input tokens) **plus another cache-read pass over the whole context.**
   Fewer, better round-trips beat cheaper individual round-trips.

### 2.2 Measured output volumes (VS Code corpus, this repo's bench data)

| Query | Tool | Output | ≈ tokens |
|---|---|---|---|
| `dispose` (common identifier) | `rg -n` | 1.3 MB / 1,848 lines | ~330k (must truncate) |
| `dispose` | `rg -l` (files mode) | 36 KB / 443 files | ~9–10k |
| `dispose` | `semgrep -k 10` | 1.2 KB | ~300 |
| "where is a terminal instance created" | `semgrep -k 10` (hybrid) | 1.8 KB / 10 lines | ~450 |
| same intent, keyword-ized | `rg` | 0 hits → retry loop | 0 + another round-trip |

The pattern: **rg's output cost is O(corpus × term frequency); a ranked
tool's is O(k).** rg is only cheap when the query term is rare — which is
precisely when the agent has already done the hard inference work of guessing
the right identifier. And when the guess is wrong, the "cheap" 0-hit result
costs a full extra round-trip (reasoning tokens + context re-read).

### 2.3 Where paraphrase vs keyword queries differ in cost

- **Input side:** a natural-language query is ~10–25 tokens vs ~2–5 for an
  identifier — negligible.
- **Output side (the model's own emission):** to use rg well, the model must
  *derive* keywords from intent — that inference happens in visible reasoning
  tokens, often across multiple attempts (guess identifier → 0 hits → guess
  synonym → too many hits → add path filter). Each attempt is an output-token
  spend plus a context re-read. An intent-shaped tool moves this
  derivation into the tokenizer/BM25/embedding stack, where it is free.
- **RESULTS.md quantifies the miss rate being paid for:** same NL intents,
  keyword-ized for rg (the agent-style fallback), find the target in top-5
  3–27% of the time vs 86–99% for bm25/hybrid on direct queries — on the
  kernel a 30× gap. Every miss is a full round-trip that a ranked tool never
  spends.

---

## 3. External evidence (published record)

### 3.1 Why Claude Code has no index — Anthropic's own account

Boris Cherny (creator of Claude Code), Jan 2026: *"Early versions of Claude
Code used RAG + a local vector db, but we found pretty quickly that agentic
search generally works better. It is also simpler and doesn't have the same
issues around security, privacy, staleness, and reliability."*

From the Latent Space interview (May 2025): agentic search "outperformed
everything. By a lot" in internal evals — and, critically, the concession:
*"at the cost of latency and tokens, you now have really awesome search
without security downsides."*

The argument set to beat or neutralize:

| Anthropic's argument for grep-only | semgrep's answer |
|---|---|
| single-shot RAG lost to multi-hop agentic search in evals | not proposing RAG-instead-of-agent; proposing a better *primitive inside* the loop (Augment's recommended pattern, §3.2) |
| index staleness / drift | `--check-stale` + graceful cold path (works unindexed); v2 fold watch mode |
| security/privacy of a derived index living somewhere | local index, compiled-in embedding weights, code never leaves the machine |
| ops complexity / setup | zero-config: works cold; index is an optional accelerator |
| transparency/debuggability of results | grep-shaped `path:line:text` output; `--stats` provenance |
| conceded cost: **tokens and latency** | **this is the attack surface** — O(k) output, ranked so multi-hop shrinks to ~1 hop |

### 3.2 Agentic vs semantic retrieval — who found what

- **SWE-bench (ICLR 2024):** BM25-retrieval pipeline resolved 1.96% of issues
  (Claude 2) vs 4.8% with oracle files. Single-shot retrieve-then-patch is
  the strawman that agentic loops crushed.
- **Augment Code (Sep 2025):** adding embedding tools to their SWE-bench agent
  gave *no improvement* — "agent persistence compensates for unsophisticated
  retrieval." Their caveats: SWE-bench repos are small with greppable
  identifiers; embeddings "become essential for larger codebases, less
  structured content, or more complex retrieval tasks." Their recommendation:
  **expose embeddings as a tool inside the agentic loop** — this project's
  exact shape.
- **Cursor (Nov 2025), strongest pro-hybrid numbers:** semantic search gives
  **+12.5% QA accuracy** (range 6.5–23.5% across frontier models), production
  A/B +2.6% code retention on 1,000+-file repos; "our agent makes heavy use
  of grep as well as semantic search, and the combination leads to the best
  outcomes." Their embedder is custom-trained on agent traces (relevant to
  our ese ceiling, §5.4).
- **Sourcegraph Cody (2024):** dropped embeddings for BM25 + code intel — for
  ops/privacy reasons (third-party embedding API, admin complexity, scale),
  not quality. All three of their objections vanish with local static
  embeddings.
- **Amazon Science (2026):** "Keyword search is all you need" — agentic
  keyword search reaches >90% of RAG performance without a vector DB.
- Windsurf, Cline, Devin, Sourcegraph Amp all subsequently dropped vector
  search for tool-driven search. The industry consensus is agentic-loop +
  lexical primitive; Cursor is the notable dissent, with data, on large repos.

**Synthesis:** nobody has published evidence against *ranked lexical search
as the loop primitive* — the debate is embeddings-vs-grep. RESULTS.md says
the same thing from the inside: BM25 is the headline win (0.88–0.99 R@5
direct); static embeddings add little on code (they help on prose). The
contrarian bet with published support (Cursor) is that semantic matters most
on exactly the repos where rg hurts most: big ones.

### 3.3 Tool-prompting ROI (why the prompt is part of the product)

- Anthropic, multi-agent research system post: rewriting one flaky tool's
  description → **40% decrease in task completion time** for agents using it.
  "Agent-tool interfaces are as critical as human-computer interfaces."
- Anthropic, "Writing effective tools for agents": tool responses should
  default concise with opt-in detail (their example: 206 → 72 tokens per
  response); truncation and *error messages* should actively steer the model
  ("too many matches; add a path filter") — errors are part of the economics;
  keep responses well under the 25k cap; prompt-engineering tool descriptions
  is "one of the most effective methods for improving tools."

---

## 4. The mode-collapse question

### 4.1 Current surface (v1)

`--mode hybrid|keyword|bm25|semantic` + `-e` regex alias + `-i`, `-F`, `-k`,
`--sem-weight`, `--mmr-lambda`, `--no-diversify`, `--no-index`, `--exact`,
`--window`, `--overlap`… The agent must choose a mode, and the tool
description must explain four modes ≈ four tools' worth of schema tokens and
decision burden, paid every session.

Mode choice is also a *hidden failure point*: RESULTS.md shows hybrid ≈ bm25
on direct queries and ≥ any single engine on paraphrase — i.e., **after the
weighted-RRF tuning, there is no query type where the agent picking a
specialist mode beats just using hybrid.** The remaining question is only
keyword/regex.

### 4.2 Is keyword mode worth keeping? What `-e` actually provides

Keyword mode ≈ rg (same crates, same speed — bench §1). Three things ranked
search does not currently give:

1. **Regex semantics** — structural queries (`fn \w+_config`), where the
   pattern *is* the intent. No ranked mode can express this.
2. **Exhaustiveness** — "find *all* call sites" (for a rename/refactor) needs
   every hit, not top-k. Ranked top-k is the wrong contract.
3. **Exact-match certainty** — a literal hit is proof; a ranked list is
   evidence.

But: the agent already has rg (and Claude Code will keep using its Grep tool
regardless). Duplicating rg inside semgrep buys nothing for the Claude Code
agent — it buys something for *other* harnesses where semgrep might be the
only search tool, and for the unix-user muscle-memory story.

### 4.3 Options for the collapsed surface

**A. One behavior, no flags (pure ranked):** `semgrep <query> [path]` always
runs tuned hybrid. Drop keyword mode entirely; regex/exhaustive jobs belong
to rg. Smallest possible tool description ("like grep, but you can ask in
plain language; returns the top-k most relevant locations"). Cleanest story;
gives up the drop-in-replacement claim.

**B. Auto-detect (router inside the tool):** regex-looking / quoted-literal
queries → keyword semantics; otherwise ranked. No mode flag, keeps drop-in.
Risk: misrouting is a silent failure the agent can't see or override, and
"looks like a regex" is a genuinely fuzzy classifier (`user_id` vs `\buser_id\b`
vs "user id handling"). Auto-magic that guesses wrong costs more trust than
it saves in schema tokens.

**C. One ranked behavior + one explicit escape hatch:** default is always
tuned hybrid; `-e/--regex` (grep's own flag) switches to exact grep
semantics, exhaustive output. No `--mode`, no bm25/semantic/tuning knobs in
the help. The escape hatch is *self-describing to anyone who knows grep* —
including every model, which has grep deeply in pretraining. Two sentences of
tool description cover the whole surface.

Working recommendation: **C**, with the internal engines (bm25-only,
semantic-only, fusion weights) demoted to hidden/env-var/debug flags for the
eval harness. A (pure ranked) is the fallback if agent evals show `-e` is
never chosen or is chosen wrongly. B's router can be revisited as a *visible*
behavior ("query looked like a regex; ran exact match — pass --ranked to
override") rather than a silent one.

### 4.4 What else collapses

- `--json`, `-k`, `-C` stay (harness/output shaping).
- `--no-index`, `--exact`, `--sem-weight`, `--mmr-lambda`, `--no-diversify`,
  `--window`, `--overlap` → hidden (debug/eval only). No agent should ever
  tune MMR lambda.
- `-i`/`-F` only make sense with `-e`; fold into it.
- `semgrep index` stays as-is (it's an operator command, not an agent
  decision — better yet, auto-build/refresh in the background on first ranked
  query over a large corpus; needs design for the 59 s / 1.3 GB kernel-scale
  cost).

---

## 5. The core question: push inference down into the tool?

Claude Code's architecture keeps the agent a **smart driver of a dumb-fast
tool**: the model does query formulation, result triage, and iteration; rg
does exact matching. Notably, its answer to the resulting token overhead is
not a smarter tool but a **delegated driver**: past ~3 queries, the search
loop moves into an Explore subagent whose context absorbs the file dumps and
returns only conclusions. That quarantines the cost from the main loop but
does not eliminate it — the subagent still burns the tokens (Claude Code's
own docs put agentic workloads at ~4× chat token use), still pays subagent
spawn-and-summarize overhead, and still adds wall-clock latency. The third
pole is a **smart tool**: the model states intent once; the tool does
vocabulary derivation, ranking, and dedup in-process.

A smart tool attacks the same overhead the Explore subagent quarantines —
if one ranked query usually lands, the >3-query threshold is rarely reached
and the delegation machinery (spawn, isolated context, summarize back)
goes unused. The two are complementary, not exclusive: an Explore-style
agent *equipped with* semgrep is strictly cheaper than one equipped with
grep alone. That composition (harness prompt ladder unchanged, primitive
upgraded) is the lowest-friction adoption path.

### 5.1 Where pushing down clearly wins

- **Ranking** (BM25 + fusion + MMR): replaces the model's triage-over-443-
  file-paths with a top-10. This is already built and is the measured 30×
  quality win. The cost moved from per-loop input/output tokens to 135 ms of
  Rust.
- **Vocabulary derivation**: tokenizer subtoken splitting (camelCase/snake)
  + BM25 weighting does mechanically what the model does in reasoning tokens
  ("the user said retry backoff, so grep for `backoff\|retry_delay\|jitter`").
- **Bounded output**: O(k) result contract eliminates the truncation/refine
  dance and its round-trips.
- **Dedup/diversity (MMR)**: the model currently pays tokens to notice that
  40 hits are the same vendored file.

### 5.2 Where pushing down loses (keep the agent smart)

- **Multi-hop reasoning**: "find the config parser" → read it → "now find
  callers of this specific function" — hop 2 depends on hop 1's *content*.
  No search tool can internalize this; the loop stays.
- **Exhaustive/structural queries** (§4.2): ranked top-k is the wrong
  contract; that's rg's (or `-e`'s) job.
- **Query understanding beyond retrieval**: LLM-side query expansion
  (synonyms, reformulation) is the one inference-heavy stage that measurably
  beats static embeddings on paraphrase (RESULTS.md finding 3: kernel
  paraphrase ≤ 0.05 for *every* mode — the open problem). Pushing *that*
  down would mean an LLM call inside the tool: latency, cost, and an
  API dependency inside a CLI. Wrong layer for v1; note as a server-mode
  option (§5.4).
- **Judgment about sufficiency**: only the agent knows whether hit #1
  answered the question. Keep exit codes and result shape legible so that
  judgment is cheap (grep-shaped `path:line:text` costs the model nothing to
  learn).

### 5.3 The efficiency model (to be validated by agent evals)

Let a task need the agent to *land* in the right file. With rg the expected
cost is `E[hops] × (result tokens + reasoning tokens + context re-read)`,
where E[hops] is inflated by the 73–97% top-5 miss rate on intent-shaped
queries. With ranked hybrid, E[hops] → ~1 for direct queries (R@5 0.86–0.99)
and the per-hop result cost is capped at ~450 tokens. The prediction to test
(eval/agent-eval.md is the instrument):

- **searches-to-success:** substantially fewer with semgrep-only vs rg-only
- **tokens-to-success:** dominated by avoided round-trips, not per-call size
- **failure mode to watch:** does the agent *trust* a top-10 (stop too
  early on a plausible-but-wrong hit) where rg's exhaustiveness would have
  disabused it? MMR diversity and honest exit codes are the mitigations.

### 5.4 Ceiling and levers beyond v1

- Static ese embeddings are the known ceiling on paraphrase-over-code
  (kernel ≤ 0.05). Cursor's result (embedder distilled from agent traces)
  says the ceiling is an artifact of the embedder, not the architecture.
  Levers, in escalating cost: better code embeddings; server-mode LLM query
  expansion; trace-trained embedder.
- Persistent/MCP server mode amortizes index load, makes HNSW worthwhile,
  and is where any LLM-in-the-loop stage could live without wrecking CLI
  latency.

---

## 6. The tool prompt is a deliverable

Anthropic's data (40% task-time reduction from a description rewrite) says
the tool description ships with the binary. Draft principles for semgrep's
agent-facing description, applying §3.3:

- Two-sentence core: *"Search code and docs by meaning or by keyword. Ask in
  plain language (or an exact identifier); returns the top-k most relevant
  locations as `path:line:text`."* Plus `-e` escape hatch, plus "results are
  ranked — if the first page doesn't answer, rephrase rather than paging."
- Schema stays small: the token cost of the tool definition is paid every
  session by every user. The number to beat: Claude Code's Grep tool spends
  an estimated 500–600 tokens on description + 13 parameters (§1.1), much of
  it regex coaching and output-mode plumbing that a ranked intent-shaped
  tool doesn't need. A collapsed semgrep surface (query, path, `k`,
  optional `-e`, optional context) should land under ~200.
- Steal what works from the incumbent's interface: in-band result counts
  ("Found N files"), pagination/truncation footers that say *how to narrow*,
  token-economy language inside parameter descriptions ("use sparingly —
  large result sets waste context"), and `path:line:text` shape the model
  already knows from rg.
- Errors steer: 0 hits on a ranked query should say what to try (rephrase,
  broaden path, `-e` for exact), not just exit 1.
- Truncation is visible: if k results were capped, say so in-band.

---

## 7. Real-world evals (replacing the synthetic query sets)

Survey of 2024–2026 benchmarks where search efficiency can be proven on real
repos/issues, ranked by evidence-value per unit effort:

**#1 — Loc-Bench V1 localization ablation (first, ~this week).** 560
instances from real GitHub issues (transformers, scikit-learn, sympy, …),
ground truth = the 1–10 functions modified by the real fix
(HF `czlll/Loc-Bench_V1`, Apache-2.0; LocAgent, ACL 2025). The decisive
property: **localization needs no test execution or docker** — clone repo at
base commit, run a headless agent ("output the files/functions that must be
modified"), diff against ground truth. Two conditions: {rg only} vs
{rg + semgrep}, headless `claude -p` with restricted tools or mini-swe-agent.
150–200 instances stratified by repo size, ~$20–60 per condition. Report
file/function Acc@5, searches-to-success, tokens-to-success — directly
comparable to LocAgent's published numbers (92.7% file-level Acc).

**#2 — SWE-bench Verified end-to-end subset (the headline).**
mini-swe-agent is bash-only — "swapping the search tool" is literally
putting `semgrep` on PATH plus one system-prompt line. 50–100 instances,
official harness for resolution, ~$100–300/condition. Expect a modest
resolve-rate delta (Augment's warning: SWE-bench repos are small and
greppable); the target result is **fewer tokens/steps at equal resolve
rate**, which the harness logs surface for free. SWE-bench Multilingual
(300 instances, 9 languages) is a natural add since semgrep is
language-agnostic.

**#3 — SWE-Explore (stretch, external leaderboard).** 848 instances, 203
repos, 10 languages; every "explorer" (BM25, dense, Claude Code, LocAgent…)
emits the same ranked-region list under a fixed line budget — the search
method is the swappable variable *by construction*. Their published finding
that raw BM25 is near-random while agentic search dominates is exactly the
gap semgrep's tuned hybrid should split. CC BY-NC-ND license; newest
harness. Alternative: ContextBench-Lite (500 tasks, human-annotated gold
contexts, cost-per-instance leaderboard).

Skip: Defects4J/BugsInPy (test-failure queries, not NL; heavy tooling),
CodeSearchNet (docstring-as-query is not how agents search; saturated),
RepoBench/CrossCodeEval (completion-shaped), Commit0. CoIR is a cheap
afternoon sanity check of ranking quality (MTEB-style, pip-installable) but
proves nothing about the agent loop. Cursor's Context Bench and Cognition's
SWE-grep CodeSearch Eval are private — cite, can't run. RepoQA (500
NL-description→find-the-function tasks, 5 languages) is a good cheap
secondary.

**Metric conventions to adopt:** cost–pass@1 Pareto pairs; $ per *resolved*
instance (not per attempt); median tokens / tool calls / search invocations
**conditioned on success** (pre-register this — avoids the "failed fast"
artifact); searches-to-first-useful-hit; stratify everything by repo file
count (the literature consistently shows semantic search's edge appears
above ~1k files — Cursor's threshold). Relevant motivation stat: read
operations are ~76% of mini-swe-agent token spend (SWE-Pruner), which is
precisely the spend a ranked `path:line` hit converts into targeted reads.

Full source list in the eval survey (agent report, 2026-07-27): LocAgent
arXiv 2503.09089 · ContextBench 2602.05892 · SWE-Explore 2606.07297 · CoIR
2407.02883 · RepoQA 2406.06025 · mini-swe-agent (github.com/swe-agent) ·
Cursor semsearch · Cognition SWE-grep.

### 7.1 Pilot results (50 instances × {rg, semgrep, both} × Sonnet, 2026-07-27)

Harness: `eval/locbench/` (headless `claude -p`, PATH-shim provenance,
blocker shims for grep/git — haiku demonstrably tried `git log --all
--grep=<issue#>`, which would have leaked the real fix). 96% clean runs;
the 6 failures were 2 hard instances failing in all 3 conditions (budget
cap) — instance-driven, not condition-driven. Zero shim bypasses. Blocked-
invocation audit: of ~1,614 blocked rows, 1,603 were Claude Code's own
startup probes (git config/status, IDE-process grep); Sonnet-initiated
grep/git attempts across all 150 runs: **1**. The system-prompt
"unavailable" line is near-perfectly obeyed by Sonnet-class models.
Full report: `eval/data/locbench/report.md`.

| finding | number |
|---|---|
| File Acc@5 | **75% in all three conditions** — dead even |
| Function Acc@10 (tolerant, paired n=48) | **semgrep 69% vs rg 58% (+11pp)**; on bug reports 92% vs 75% |
| Median cost / searches / output tokens | ~$0.20 / 2 / ~1.4–1.6k — no efficiency separation |
| First search surfaces a gold file | both **84%** · rg 67% · semgrep-only 41% |
| Tool choice in `both` condition | **rg 163 vs semgrep 37** (82/18) — familiarity wins |
| semgrep invocation style (all runs) | **67% used `-e` exact mode**, 33% ranked queries |

Interpretation, honestly: on this sample the §5.3 efficiency prediction did
**not** materialize — Sonnet localizes small repos in ~2 searches with
either tool, so there is no retry-loop cost for ranked search to remove.
The real signals: (a) **function-level precision** is where semgrep wins
(+11pp) — ranked chunk spans point inside the right function; grep points
at call sites; (b) in the **both** condition the agent's first search hits
gold 84% of the time — per-query tool choice beats either tool alone,
supporting the complement-not-replacement framing; (c) **interface gravity
is the product finding**: given both tools agents pick rg 82% of the time,
and even semgrep-only agents use `-e` exact mode for 2/3 of calls — grep
habits from pretraining dominate unless the prompt/footers actively steer;
the tool description alone does not flip behavior; (d) the sample is the
caveat: 39/50 repos <2k files, max 6.4k (Augment's small-repo warning
applies) — the token-efficiency thesis remains untested at the ≥10k-file
scale where rg's miss-rate should start to bite.

Follow-up analysis (shim logs): runs using ≥1 ranked query hit 68%
fnAcc@10t vs 50% for `-e`-only runs (file acc identical) — the function
win tracks ranked usage specifically. Only 21/70 ranked queries were the
agent's *first* search; 30% came immediately after a 0-output search —
ranked search is being discovered as a fallback, one wasted round-trip
late. Shipped in response (2026-07-27): (a) eval tool description
rewritten as a decision rule (exact symbol → `-e`; behavior/concept →
plain language first; 0 hits → rephrase, don't retry variants); (b)
miss-as-nudge in the CLI — `-e` with 0 hits on an indexed corpus prints
the top-3 ranked hits for the same terms on stderr (stdout empty, exit 1
— verify contract intact), collapsing the observed miss→rephrase pattern
from two calls into one.

### 7.2 Guided-prompt ablation (same 50 instances, semgrep + both, 2026-07-27)

Re-ran `semgrep` (decision-rule description + miss-nudge binary) and
`both` (explicit routing rule replacing "use whichever fits") against the
pilot rows, paired n=48 (`results-guided.jsonl`, `compare.py`). Three
findings:

1. **Instruction gravity beats interface gravity.** Agents *obeyed* the
   routing rule mechanically — but the rule's first branch ("exact symbol
   known → rg/-e") matches nearly every Loc-Bench issue (they quote
   identifiers/tracebacks), so obedience meant *less* ranked usage, not
   more: semgrep calls in guided-`both` dropped to literally 0 (from
   144/14 rg/sg); typed ranked queries in `semgrep` fell 66 → 18.
   Prompt steering works; my routing criteria were miscalibrated for
   identifier-rich tasks. Accuracy was flat either way (fAcc@5 75→77,
   fnAcc@10t 62→65 / 69→67 — noise), cost slightly down.
2. **The miss-nudge almost never fired — and exposed a real engine gap:**
   agents scope searches to subdirectories (`semgrep -e foo litellm/`) —
   **65% of all semgrep calls (124/191)** — but `search()` only checks
   `index::exists(<path arg>)`, so subdir-scoped queries silently fall to
   the cold streaming path and can never trigger index-gated behavior
   (nudge fired on only 4/69 misses). **Fix before further prompt evals:
   ancestor index discovery** (walk up to find `.semgrep` like git finds
   `.git`, then filter hits to the subtree). This also means warm-index
   perf currently evaporates for the most common agent calling pattern.
3. Zero-search runs (Glob+Read only) rose 23 → 28 of ~98 — on small
   repos, search itself is optional for a strong driver.

### 7.3 Name + framing ablation (same 50 instances, 2026-07-27)

Two more conditions on the same sample (`results-name.jsonl`): a v3
description ("one tool, two modes" menu with tradeoff context — when
exact shines vs when ranked shines — plus a micro-example, no
prescriptive rule), run under two names: `semgrep` and `search`
(identical binary, identical text, only the name differs).

| description variant | ranked share | ranked-first | fnAcc@10t |
|---|---|---|---|
| v1 pilot — ranked-as-identity ("give it anything…; `-e` for exact") | **35%** | **38%** | **69%** |
| v2 — explicit routing rule | 10% | 2% | 65% |
| v3 — modes menu + tradeoffs (`semgrep` name) | 9% | 4% | 65% |
| v3 — modes menu + tradeoffs (`search` name) | 10% | 2% | 56% |

Findings: (a) **the name-gravity hypothesis is refuted** — `search` vs
`semgrep` produced statistically identical usage (9 vs 10% ranked share);
the `-e`-everything habit is not imported by the name. (b) **Framing
hierarchy is the real lever:** the v1 description, which gives the tool a
ranked *identity* with `-e` as the escape hatch, produced **3.5× the
ranked usage** of either an explicit rule (v2) or a symmetric modes menu
with tradeoff context (v3). A symmetric "when each shines" menu reads as
a decision procedure, and on identifier-rich issues the exact branch
wins every evaluation — v2's failure in softer packaging. (c) fnAcc@10t
loosely tracks ranked share across cells (69 → 65 → 56), consistent with
§7.2's within-run correlation (ranked-using runs 68% vs 50%).

**Description design rule this yields: assert identity, don't offer a
menu.** The tool description should say what the tool *is* (ranked
search you can ask in plain language) with exact mode as a subordinate
escape hatch — not present co-equal modes with selection criteria,
however informative. v4 candidate = v1 identity framing + the
micro-example (the one element still untested in the winning frame).

Next iterations, in order of information value: (1) **implement ancestor
index discovery + subtree filtering**, then re-test the miss-nudge (it
was effectively untested at 4 firings); (2) test v4 description (v1
identity framing + micro-example); (3) re-run with the sample stratified
by repo *size*, raising the budget cap for large repos; (4) a
weak-driver run (haiku); (5) scale to 150–200 for CI-worthy deltas.

---

## 8. Design sketch: the index is a cache (2026-07-28)

Reframe: stop treating `.semgrep/` as an *artifact* the user administers
(build it, warn when stale, don't commit it, don't leave it in sibling
repos) and treat it as a *cache* the tool owns. The observation that makes
this cheap: **a build is one streaming pass — the same pass a cold search
already performs.** Cold search and index build are the same computation;
one throws the work away, the other writes it down.

| aspect | artifact (today) | cache (proposed) |
|---|---|---|
| creation | explicit `semgrep index .` | side effect of the first ranked search (write-through) |
| staleness | warned; manual full rebuild | read-repair on access (see overlay below) |
| location | `.semgrep/` inside the repo | `~/.cache/semgrep/<root-hash>/` + manifest |
| lifecycle | user-managed, unbounded | LRU, size-capped, disposable at any time |
| correctness story | results may silently lag the tree | **transparency invariant**: identical results warm or cold |
| agent surface | `index` subcommand + a decision | none — invisible |

### Mechanisms

1. **Write-through cold path.** First ranked search over an uncached root
   streams the corpus (as today) and persists the chunk table, postings,
   and embeddings it just computed. First-query cost ≈ today's cold search
   + write I/O (kernel: ~59 s → ~66 s; median real repo: <1 s). `semgrep
   index` survives only as optional prewarming (CI, humans). Progress
   note on stderr: "first search here: caching, subsequent searches
   ~100 ms" — the reply teaches, as usual.
2. **Read-repair via overlay (always-true results without incremental
   index writes).** At query time, diff the live tree against the cached
   file table (the ~1 s staleness walk we already have). Changed/deleted
   files → tombstone their chunk ids out of the warm ranking; changed/new
   files → run the *streaming* path on just that delta in memory; fuse
   the two candidate lists. The immutable base index plus a per-query
   delta overlay gives correct-as-of-now answers with zero index-format
   changes — the streaming machinery already exists. If the delta exceeds
   a threshold (say >5% of files — branch switch), treat the whole query
   as a miss: full streaming pass, write-through again. Throttle the
   staleness walk (at most once per corpus per ~60 s, recorded in meta)
   so query bursts don't pay 1 s each on big trees.
3. **Central cache dir, not repo pollution.** Keyed by canonicalized root
   (prefer the enclosing git root — which also *is* the ancestor-discovery
   fix: subdir-scoped queries resolve to the enclosing root's cache entry
   via longest-prefix match in the manifest). Kills the ".semgrep in
   sibling repos" hygiene problem, the .gitignore requirement, and
   accidental commits. LRU eviction under a size cap (default ~5 GB);
   corrupt or version-mismatched entries are misses, never errors.
4. **Concurrency:** flock per cache entry; the losing process answers via
   the streaming path rather than blocking. Atomic publish via
   write-to-tmp + rename.

### Why this dissolves standing problems

- **The fairness question (§eval/README.md):** "stateful but honest"
  upgrades to a provable property — the *cache-transparency invariant*
  (same query ⇒ same results, warm or cold, up to score ties) is
  enforceable in e2e tests. A cache that changes nothing but latency is
  memoization, and nobody argues memoization invalidates a comparison.
  Evals also get fairer vs rg: no experimenter-prebuilt index — the tool
  warms itself, and the first-search cost lands in the measured runs.
- **The staleness honesty problem:** read-repair means ranked mode never
  serves a hit that isn't true of the current tree — the one semantic gap
  vs rg (§ "stateless and always-true") closes.
- **The agent-decision problem:** `semgrep index` was the last decision
  the collapsed surface still asked of a caller. Gone. The tool
  description never mentions indexing at all.
- **RESEARCH.md §4.4's open item** ("auto-build on first ranked query;
  needs design for the 59 s kernel cost") — this is that design: the 59 s
  was being paid by the cold search anyway; write-through makes it an
  investment instead of a toll.

### Costs & risks (to measure before committing)

- First-query surprise on huge corpora (~60 s where the agent expected
  ~100 ms) — mitigated by the stderr note; measure whether agents handle
  it gracefully or time out (Loc-Bench condition with cold cache).
- Write-through I/O overhead on the cold pass (est. ~10% at kernel scale
  — measure).
- Staleness-walk overhead on warm queries (1 s on 84k files vs 135 ms
  query — hence the throttle; measure hit rates on agent bursts).
- Overlay-fusion correctness at chunk-id boundaries (tombstones must not
  disturb ranking of unchanged chunks — property-test against the
  transparency invariant).
- Repos larger than the cache cap; multi-root monorepos (manifest
  prefix-matching must pick the nearest cached root).

### 8.1 Scoped-lazy filling: index only what's been asked about

Refinement (user suggestion): since queries carry a scope, the cache
should too — fill it *subtree by subtree as scopes are actually queried*
instead of whole-repo on first contact.

The unification that makes this nearly free conceptually: **read-repair
and lazy fill are the same mechanism.** The overlay already streams
files the base index doesn't know (new files). An uncovered file — one
no query has ever touched — is the same case. Query-time diff over the
scope yields {stale, new, never-covered}; all three stream through the
in-memory delta path; write-through appends them and marks them covered.
Coverage grows monotonically along the agent's actual search paths —
the cache heats up exactly where the work is.

What it buys:

- **First-query cost proportional to the scope, not the repo.** A query
  scoped to `drivers/net/` on the kernel pays ~2 s, not 66 s. §8's
  biggest risk (the first-query surprise) mostly evaporates — the toll
  tracks what you asked for.
- **The 65% subdir-scoped calls flip from worst case to best case** —
  the funnel pattern (broad → narrow) warms precisely the hot subtrees.
- **Monorepos:** never pay for the 90% nobody searches.
- **Scoped staleness checks:** the validity walk covers the query
  subtree only — ms for a subdir vs ~1 s for the whole kernel tree.
- **Finer eviction:** cold *subtrees* can be evicted, not whole repos.

What it costs — the storage format must tolerate growth. `bm25.flat` is
a sorted immutable table; appending isn't a thing. The classic answer is
**segments** (Lucene-style): each fill/refresh writes a small immutable
segment (chunks + postings + embedding rows for the files it covers);
queries merge candidate lists across segments plus the live delta;
compaction merges segments when the count grows. Crucially, compaction
never re-embeds — vectors are copied, terms re-sorted; the expensive
work (ese, tokenization) happens exactly once per file version.
Per-segment BM25 stats approximate global idf; for subtree-scoped
queries (the common case) this is a non-issue since ranking is filtered
to the scope anyway.

Stepping stone if segments feel heavy for v1 (**scope promotion**): keep
the existing single-entry v2 format, but key entries by queried root
with containment reuse — an ancestor entry serves any descendant scope
(prefix filter); querying a *wider* scope than any entry covers builds
the wider entry and evicts its children. Transient duplication, no
format change, and the manifest logic is identical to what ancestor
discovery needs anyway.

Policy knobs: don't persist trivially small scans (cache only when the
pass cost exceeds ~a few hundred ms — "cache when the miss hurt");
compaction threshold ~8 segments; per-root segment budget under the
global LRU cap.

Sequencing: this *contains* the ancestor-discovery fix (manifest
prefix-match ≡ walk-up) — implement as one change of the index layer:
`index::discover` consults the manifest; `search()` gains write-through
and the overlay (which is also the lazy-fill path). The eval harness
then drops its explicit `ensure_index` step and lets the tool warm
itself, which is also the more honest condition. Recommended order:
scope promotion first (validates behavior with today's format), segments
when compaction pressure or monorepo use demands them.

### 8.2 Implementation status (shipped 2026-07-28)

**Parallel pass** (prerequisite — the pass is the miss latency): split
`add_doc` into parallel `tokenize_doc` + serial `add_tokenized`; per-file
read/chunk/tokenize on rayon workers in batches capped by count *and*
bytes (256 files / 16 MB — RSS stays at or below the serial baseline),
serial in-order fold preserves the chunk-id lockstep by construction.
Measured full builds: kernel 65.6 s → **45.5 s** (1.44×, CPU util
1.9×→3.2×), wikipedia 14.4 s → **8.7 s** (1.66×, RSS 732→661 MB),
vscode 3.5 s → **2.4 s**. Remaining ceiling: the read/tokenize phase and
the embed phase alternate at batch barriers instead of pipelining —
overlap them for the next step toward the wall-clock floor (~embed time).

**Cache phase 1** (scope promotion form, all shipped + tested):
`index::discover` (local `.semgrep` → git-style ancestor walk stopping at
repo boundary/$HOME → central-cache longest-prefix match);
subtree-filtered ranking (filter before truncation) with scope-relative
display paths; write-through cold ranked searches into
`$SEMGREP_CACHE_DIR` (default `~/.cache/semgrep`) keyed by canonical
root, with child-entry eviction on widening (promotion); throttled
scoped read-repair (`SEMGREP_CACHE_TTL_SECS`, default 60 s): live-tree
diff → tombstones + in-memory delta (chunk/tokenize/embed just the
drifted files) fused into both ranked lists — repair and lazy fill are
one code path. `--no-index` never reads or writes. CLI prints a
first-search teaching note; the `-e` miss-nudge now gates on discovery
(it fires for subdir scopes — previously 4/69, the §7.2 gap).

Verified: 34 tests green, incl. new e2e for ancestor-serves-subdir,
write-through transparency, promotion eviction, and read-repair
(new file found, rewritten file's old text tombstoned — always-true
results). Measured on VS Code corpus: subdir query warm at 189 ms incl.
102 ms scoped repair walk (was: full cold stream); write-through demo
148 ms cold-with-cache → 5 ms warm. **Transparency invariant, precisely:**
warm and cold return the same top-k *set* and the same top hit;
adjacent near-ties can swap order because warm scores read the
i8-quantized matrix while cold scores are f32 (quantization verified
quality-neutral in §3). Eval harness isolates `SEMGREP_CACHE_DIR` per
run. Not yet done: LRU size cap/GC for the cache base, cold-cache
Loc-Bench condition, pipelined embed overlap.

---

## 9. Retrieval-quality levers: SIF, MaxSim, multi-pass (explored 2026-07-28)

Context: the open quality problem is paraphrase-over-code (kernel R@5 ≤
0.05 for every engine, §3) and the fact that the semantic list had to be
down-weighted to 0.2 in fusion because it *diluted* BM25. Reading ese's
source explains why: **`encode_single` pools by uniform mean over
wordpiece vectors** (CLS + every token accumulated, divided by count).
A 32-line chunk's two discriminative identifiers are averaged against
hundreds of boilerplate tokens — the chunk vector is muddy by
construction. All three levers below attack this.

### 9.1 SIF term weighting (corpus-adaptive pooling)

Arora et al.'s Smooth Inverse Frequency: weight each token vector by
`a/(a + p(w))` (p(w) = corpus unigram probability, a ≈ 1e-3), then
optionally subtract the corpus's first principal component (the "common
component" every text shares). Rare tokens dominate the pool; boilerplate
nearly vanishes — the embedding-side analog of what idf does for BM25.

Fit with our architecture is unusually good:

- **The cache already stores corpus statistics.** p(w) at the wordpiece
  level is a small frequency table countable during the pass (we touch
  every token anyway); the common component is one 512-dim vector
  computable from the embedding matrix at build time. Both live in the
  cache entry → **SIF becomes corpus-adaptive**: kernel C code and
  Wikipedia prose each get their own weighting. Query-side uses the same
  table (unknown query tokens = max weight — exactly right).
- **Blocker: ese's API.** `encode` exposes only pooled vectors;
  `lookup`/`wordpiece` are private. Needs a sibling-crate extension
  (`../ese` is ours): either `encode_weighted(text, impl Fn(&str) -> f32)`
  or `for_each_token_vector(text, impl FnMut(&str, &[f32; D]))` — the
  latter also unlocks MaxSim (§9.2).
- Expected effect: semantic list stops diluting hybrid (sem_weight can
  rise from 0.2), paraphrase recall moves on prose immediately; code is
  the experiment. Cheap to validate offline against the existing 1,198
  ground-truth queries — no agents needed.

### 9.2 MaxSim reranker (late interaction, ColBERT-style)

`score(q, d) = Σ_i max_j cos(q_i, d_j)` over *token* vectors — each query
token finds its best match anywhere in the chunk, so one strong
identifier match isn't averaged away. With static embeddings this is
nearly free: doc token vectors are table lookups (no transformer), so we
can rerank the top ~128 candidates **at query time** by re-reading chunk
text (finalize re-reads it anyway) — no index-format change, no storage
blowup (ColBERT's usual cost). Rough cost: 20 query tokens × ~300 chunk
tokens × 512 dims × 128 candidates ≈ a few ms with SIMD/rayon.

Two bonuses beyond ranking:
- **Line-level localization for free:** the argmax positions say *which
  tokens* matched — feeding `materialize`'s best-line selection and
  directly extending our one proven quality edge (function-level
  precision, +11pp in §7.1).
- Composes with SIF: weight each query token's term in the sum by its
  SIF/idf weight (rare query tokens matter more).
Same ese API dependency as §9.1. As a pure reranker it's a clean A/B:
`--rerank maxsim` hidden flag, scored on the existing eval sets.

### 9.3 Multi-pass / recursive search (and the cache synergy)

Four distinct shapes, from cheapest to most structural:

1. **PRF (pseudo-relevance feedback), tool-internal.** Pass 1 hybrid →
   take top ~10 chunks → extract their most discriminative terms
   (high tf in hits, low df in corpus — the BM25 stats are loaded
   already) → append to the query → pass 2 BM25 → fuse. This is "LLM
   query expansion without the LLM": the NL query only has to land
   *near* the target semantically once; the neighborhood's vocabulary
   then powers exact lexical retrieval. Warm cost ~2× (80 → ~160 ms).
   **No new APIs — implementable today**; the cheapest paraphrase
   experiment we have.
2. **Recursive scoped drill-down.** Aggregate pass-1 chunk scores per
   directory; if results cluster (say ≥70% of top-k in one subtree),
   re-rank scoped to it — or just *say so* in the footer ("results
   cluster in litellm/integrations/ — scoping there"). This is the
   agent's measured funnel behavior (§7.2: 65% subdir-scoped) done
   inside the tool, and every scoped pass warms the lazy cache (§8.1) —
   search behavior and cache-fill are literally the same walk.
3. **Semantic→keyword handoff at the agent level** — already happens
   (30% of ranked queries followed a 0-hit exact search); PRF is the
   tool-internal version that saves the round-trip.
4. **Two-pass cold search.** Pass 1 cheap lexical scan selects candidate
   files; pass 2 embeds only those. Cuts the cold-miss cost (write-
   through latency) several-fold and addresses the 916 MB cold-BM25 RSS
   (roadmap item); pairs with §8.1 — the first pass also decides what's
   *worth* caching (hot files first, segments later).

### 9.4 Measured results (2026-07-28, full campaign: 3 corpora × 5 conditions)

Implementation: ese gained `for_each_token_vector`/`for_each_token`;
semgrep gained `index --sif` (freq pre-pass + weighted pooling, stats in
`sif.bin`, query pooled in the same space), `--maxsim` (late-interaction
rerank of the candidate pool, ~35 ms), `--prf N` (top-hit term expansion,
~32 ms). All hidden flags, default off. Full tables:
`eval/data/lever-*.json`, `eval/lever-report.py`. Verdicts:

| lever | verdict | evidence (R@5 / MRR deltas vs base) |
|---|---|---|
| **PRF** | **kill** | Harmful everywhere: kernel direct bm25 −0.27 R@5 (MRR −0.39), wiki paraphrase −0.14, vscode −0.04..−0.08. Query drift amplifies whatever the seed pass found; no paraphrase gain anywhere. A quality-gated variant (expand only when pass-1 scores are weak) could be revisited; as-is, off. |
| **MaxSim** | **adopt, but re-wire** | On the *semantic list*: consistent, large — direct +0.05/+0.10/+0.11 R@5, MRR +0.12/+0.18/+0.16 (kernel/wiki/vscode). On *hybrid* it currently reranks the fused pool, overriding BM25's exact-match signal: hurts on code (vscode −0.05), helps on prose (wiki MRR +0.07). Fix: rerank the semantic candidate list *before* fusion; let RRF fuse as usual. |
| **SIF** | **keep as MaxSim's multiplier only** | Alone: mild paraphrase gains (+0.02..0.03) but hurts code semantic direct (kernel −0.15 — hyper-rare identifiers over-focus the chunk vector, and paraphrase queries avoid exactly those tokens). With MaxSim: best-in-class — wiki hybrid 0.99 direct / 0.43 paraphrase (MRR +0.08/+0.04), vscode semantic +0.12/+0.18, semantic paraphrase 7× on vscode (0.01→0.07). |
| **kernel paraphrase** | **the wall stands** | ≤0.05 in every condition. Confirms §3 finding 3: this needs a better code embedder (or trace-trained, per Cursor) — not more query-time machinery on static embeddings. |

### 9.5 Pre-fusion re-wire + weight sweep (2026-07-28, final)

MaxSim moved pre-fusion: the semantic list's head (k×3, min 24) is
reranked by late interaction *inside* the semantic branch (similarity →
pseudo-distance keeps the list contract), then RRF fuses with untouched
BM25. Results (`lever-*-maxsim2*.json`):

- **The code-hybrid regression is gone** (vscode: was −0.05 post-fusion →
  flat R@5, MRR +0.02) while every semantic-mode gain survives
  (+0.05..0.11 R@5, +0.12..0.18 MRR across corpora). Hybrid is now
  flat-to-positive everywhere (wiki MRR +0.04; one soft cell: wiki
  paraphrase R@5 −0.03 with MRR still up). Cost ~39 ms/query.
- **SIF fails its graduation gate:** sif+maxsim2 trades direct quality
  (kernel semantic −0.07, wiki hybrid MRR −0.03 vs plain maxsim2) for
  paraphrase gains (+0.02..0.07). Not a default; stays as `index --sif`,
  documented as the paraphrase-leaning build option.
- **sem_weight 0.2 survives the sweep:** with maxsim on, w0.4/w0.6 hurt
  *everywhere* (kernel direct 0.91→0.86→0.84; wiki 0.98→0.95→0.92).
  Even a MaxSim-improved semantic list is the junior partner on
  identifier-rich queries — BM25's dominance is a property of the query
  distribution, not a defect of the fusion weight.

**Shipped defaults: unchanged** (`--maxsim` and `--sif` remain opt-in
hidden flags). The empirical route to flipping `--maxsim` on is a
Loc-Bench A/B measuring function-level Acc (the +11pp finding correlated
with semantic-list quality, and MaxSim transforms exactly that list) —
worth one condition in the upcoming re-run. sem_weight stays 0.2.

### 9.6 Knob sweep (2026-07-28: pool, blend, sif-a, centering)

Parameterized (`--maxsim-pool`, `--maxsim-blend`, `index --sif-a`,
`--sif-center`; `a` and the sample-estimated common component persist in
`sif.bin` so query pooling always matches build). 7 conditions × 3
corpora vs the §9.5 references (`tune-report.py`):

- **Pool 96 adopted as the `--maxsim` default head** (was k×3 min 24):
  semantic direct +0.03/+0.04/+0.06 R@5 (kernel/wiki/vscode), hybrid MRR
  neutral-to-positive, no real regressions. Cost 21 → 54 ms. Deeper
  candidates get rescued; the feared plausible-but-wrong promotions
  didn't materialize.
- **Blend: dead.** α = 0.75/0.5 flat-to-negative everywhere; the
  embedding order adds nothing inside the head. Pure MaxSim stays.
- **SIF a: more aggressive is better on code, hypothesis inverted.**
  a=1e-4 beats 1e-3 on both code corpora (vscode +0.02 direct/+0.01
  paraphrase; kernel semantic direct +0.05, recovering most of the −0.07
  SIF regression) but trades wiki paraphrase (−0.03) — with MaxSim
  supplying precision, the single vector can afford maximal rarity
  focus. Milder a=1e-2 is bad everywhere (vscode −0.12). Default `a`
  stays 1e-3 (SIF's documented identity is the paraphrase-leaning
  option); **use `--sif-a 1e-4` on code corpora** — doc'd, not defaulted.
- **Centering: not worth it.** Neutral on all three (one good cell —
  kernel hybrid direct 0.92, the campaign's best — but no pattern).
  Stays implemented behind `--sif-center` for future embedder work.

**Six-point pool curve** (24/32/48/64/96/128, `run-pool-sweep.sh`): no
universal knee — cells peak at different depths. Semantic *direct* keeps
creeping through 96 (kernel still rising at 128: 0.78); semantic
*paraphrase* on code **peaks at 48 and degrades past it** (vscode
0.04→0.02, MRR halves); hybrid R@5 is best at 24–48 (kernel sags to
0.88–0.89 at 64/128) while hybrid MRR peaks 64–96. Warm latency: 4.6 /
8.5 / 19 / 27 ms at pools 24/48/96/128 — not a deciding factor. The
"diminishing returns past ~30" intuition holds for the agent-facing
hybrid mode; pure semantic keeps gaining. **Narrowed Loc-Bench pool
candidates: 48 (best all-rounder) and 96 (max semantic direct)** — 32
is indistinguishable from 24, 128 costs kernel hybrid recall.

Final tuned configuration for the Loc-Bench A/B: `--maxsim` at pool 48
vs 96 (agent-level tiebreak), pure blend, normal index; SIF (`--sif-a
1e-4`) as the small-codebase hypothesis condition.

### 9.7 Loc-Bench A/B: the offline gains do not transfer (2026-07-28)

50 instances × {sg-plain, sg-mx48, sg-mx96, sg-sif(a=1e-4)+maxsim},
Sonnet, v4 description held fixed, engine flags injected by the shim
(`results-ab.jsonl`). Verdicts:

| finding | evidence |
|---|---|
| **MaxSim hurts agent-level accuracy, monotonically with pool depth** | fnAcc@10t: plain **62%** > mx48 59% > mx96 54%; fAcc@5: 77 > 71/70. Agents also searched *more* under maxsim (201 vs 142 sg calls) — worse first results beget retries. |
| **All conditions tie on 2k–10k repos** | Every cell identical (83% fAcc@5 / 75% fnAcc). Engine variants only diverge on <2k-file repos — where plain wins. |
| **SIF small-repo hypothesis: partially supported, not adoptable** | On <2k files, sif beats its maxsim base (+4pp fAcc@5, +7pp fnAcc vs mx96) — SIF's relative value does grow as repos shrink — but still trails plain (70 vs 75 fAcc@5). |
| **v4 description moved behavior as designed** | ranked-first 56% (vs pilot-v1's 38%); exact-mode calls down 125→87. But fnAcc read 62% vs pilot's 69% — at n≈47 that's ~3 instances; more ranked usage demonstrably ≠ better outcomes, and cross-run noise can't be excluded. |
| **Deltas are small** | 2–4 instances separate conditions; directions are consistent across metrics but individually within noise. |

**Decisions: the plain engine stays the default; `--maxsim` does not
graduate** (offline semantic-list gains are a misleading proxy — agents
issue identifier-shaped queries through hybrid, where BM25 carries the
ranking, and MaxSim's reorderings swap in token-similar-but-wrong chunks,
e.g. test files that repeat the identifiers). SIF remains the documented
prose/paraphrase option. The broader lesson repeats Augment's: retrieval
micro-benchmarks and agent outcomes diverge — **gate engine changes on
agent-level evals**, which this harness now makes a ~$40 question.

### 9.8 MaxSim failure forensics (2026-07-28)

Reproduced the §9.7 Deltares bait case offline with real vectors
(`tests/tokprobe.rs`, kept as a regression test: if its assertions flip,
revisit `--maxsim`). Query `scalar_None function shortcut`; gold = the
actual definition `def scalar_None(obj): return obj is None`; bait =
`regridder_function: Optional[str], if min is None and max is None:`.
Per-token argmax table:

```
query tok   → best in GOLD        → best in BAIT
scalar        scalar   1.000        str        0.210
_             _        1.000        _          1.000
none          none     1.000        none       1.000
function      (        0.115        function   1.000
shortcut      (        0.064        regridder  0.069
TOTAL         gold 3.179            bait 3.279   ← bait wins
```

Root causes, in causal order:

1. **The tokenizer shreds identifiers.** ese's prose pre-tokenizer splits
   on punctuation: `scalar_None` → `[scalar, _, none]`. The identifier —
   the highest-signal token in code — never exists as a matchable unit,
   so MaxSim can only match its fragments, which are exactly the
   fragments bait chunks share. Worse, punctuation tokens are
   first-class: `_` contributes a perfect 1.000 to *any* chunk containing
   an underscore. (camelCase, inconsistently, is *not* split:
   `computeBackoffDelay` stays whole.)
2. **Concept words don't appear in code.** The gold chunk IS a function
   but never says "function" — code says `def`/`(`. The query's concept
   word finds its literal match only where the *word* occurs (bait
   identifiers, comments), scoring 1.000 there vs 0.115 against the
   punctuation that actually expresses the concept.
3. **No contextual awareness (hypothesis confirmed).** Static vectors:
   "none" inside `scalar_None` and "none" in `min is None` are the SAME
   vector. Real ColBERT works because a transformer contextualizes each
   token before matching; per-token matching over context-free vectors
   structurally can't distinguish an identifier fragment from a keyword.
4. **No term importance (non-SIF conditions).** `token_vectors` weights
   query tokens 1.0 without SIF stats, so `_` counts as much as
   `scalar`. Consistent: the SIF condition (weighted query tokens)
   recovered about half the gap (fnAcc 54 → 59 vs plain 62).
5. **Chunk vocabulary saturation (hypothesis partially confirmed).**
   Code chunks (~300 tokens from a tiny high-frequency vocabulary) give
   nearly every chunk a perfect match for common query tokens, so
   per-token maxes saturate and the score spread that should
   discriminate collapses. It's not chunk *length* per se — it's the
   frequency distribution of code vocabulary within any sizable chunk.

Net: in the agent setting MaxSim ≈ **BM25 minus idf plus punctuation
noise** — and hybrid already *has* BM25, with idf, without the noise.
The offline gains appeared only in semantic-only mode against the
muddy-pooled-vector baseline, where any token matching helped.

**Fix path, if ever revisited** (documented, not implemented — §9.7's
gate applies): use semgrep's own code-aware BM25 tokenizer
(`tokenize.rs`: keeps whole identifiers + subtokens, drops <2-char
tokens — killing both `_`-noise and the shredding in one move) instead
of ese's prose tokenizer for the match units; always idf-weight query
tokens; contextual embeddings are the full fix but are a different
embedder, not a rerank tweak.

### 9.9 The layer below: ese's embedding space is prose-shaped (2026-07-28)

Question raised: does §9.8 extend to the static model itself — was it
trained on prose? Architecture said yes (BERT wordpiece vocab with `##`
pieces, CLS/SEP, BERT's exact normalization pipeline); a direct probe
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
code-concept relations** — `str`~`string` at −0.002 is the headline: to
a prose model, "str" is an arbitrary letter sequence, not an
abbreviation. The one code pair that scores well (`bool`~`boolean`
0.560) works through shared wordpiece *surface form*, not semantics —
which is the general story: OOV is not the problem (no probed identifier
fell to UNK; fragments cover them), the *relations* are missing.

This makes the full failure stack three layers deep, each independent:
1. §9.8: the prose tokenizer shreds identifiers (surface form)
2. §9.8: static vectors carry no context (structure)
3. §9.9: the space lacks code-concept knowledge (training distribution)

Even perfect tokenization + contextualization can't bridge query
"protect with a lock" → chunk `mutex_lock(&...)` when mutex⊥lock in the
space. It also explains the measured asymmetry precisely: semantic
*direct* on code works passably (kernel 0.68) because query identifiers
overlap chunk identifiers — **on code, ese functions as a fuzzy lexical
matcher, not a semantic model** — while paraphrase (≤0.05) is exactly
the case that needs the missing bridges. The kernel-paraphrase wall
(§3, §9.4) is hereby reframed: a training-data problem, not a
query-machinery problem, and every §9 query-time lever was bounded by it.

**The encouraging part:** a static model is just a lookup table, so the
deep fix is the cheap kind — re-distill the table from a code-aware
teacher (model2vec-style, from a code embedder), keeping ese's
architecture, speed, cold-path feasibility, and the entire semgrep stack
unchanged (same DIMENSIONS ⇒ emb.bin drop-in; the cache invalidates by
format version as usual). That is the highest-leverage next experiment
for retrieval quality — gated, per §9.7, on the agent-level eval.

### Recommended experiment order (leverage ÷ effort)

1. **PRF** — pure orchestration in `search.rs`, hidden flag, score
   tonight on existing query sets. Target: kernel/VS Code paraphrase R@5.
2. **SIF** — small ese API extension + freq table in the cache entry;
   re-score. If semantic stops diluting, raise sem_weight and re-tune.
3. **MaxSim rerank** — same ese API; `--rerank maxsim`; score recall AND
   best-line accuracy (extends the function-precision win).
4. **Drill-down/two-pass cold** — after the above settle; interacts with
   segments (§8.1) and the server mode.
All four are measurable offline with `eval/run_eval.py` before any
agent-in-the-loop (Loc-Bench) spend.

Open items:

- [x] Decide A vs C (§4.3) — **C shipped** (2026-07-27): default is tuned
      hybrid, `-e/--exact` is the grep escape hatch (with `-i`/`-F`/`--all`,
      250-match print cap + true-total footer), tuning flags hidden,
      self-teaching footers on stderr for every outcome
- [ ] Run eval #1: Loc-Bench localization ablation (§7), rg-only vs
      rg+semgrep — validates §5.3's predictions on real issues
- [ ] Then eval #2: SWE-bench Verified subset via mini-swe-agent for the
      end-to-end tokens/steps-at-equal-resolve claim
- [ ] Draft the actual tool description + MCP tool schema and count its
      tokens (target < ~200; Grep spends ~500–600)
- [ ] Decide the auto-index story (§4.4) — the zero-config claim depends on
      cold-path acceptability vs background index build
