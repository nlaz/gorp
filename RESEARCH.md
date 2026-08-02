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
`eval/data/lever-*.json`, compared with `eval/diff.py`. Verdicts:

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
corpora vs the §9.5 references (`eval/diff.py --base maxsim2`):

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

**Six-point pool curve** (24/32/48/64/96/128, `eval/levers.sh mp32 mp64 mp128`): no
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

## 10. Swapping the embedding table for a code-trained one (2026-07-28)

§9.9 ended with "re-distill the table from a code-aware teacher" as the
highest-leverage next experiment. The first finding is that **we should not
distill it ourselves** — it already exists, built with a stronger pipeline
than we would have run.

`minishlab/potion-code-16M-v2` is distilled from `nomic-ai/CodeRankEmbed`
(the same teacher this repo independently selected: a 12-layer/768-dim
nomic-bert trained for NL-query → code retrieval, which is exactly
semgrep's asymmetry), then *tokenlearn*-fine-tuned on 1.2M CornStack
(query, doc) pairs, then contrastive-fine-tuned with
`MultipleNegativesRankingLoss` on 1.2M more. It also does a step we had not
planned: **43k code tokens mined from CornStack are added to the tokenizer**
(63.5k vocab vs 30.5k), so whole camelCase identifiers like `getuserbyid`
are single vocab entries.

Teacher-side survey (the binding constraint is that ese implements
**WordPiece only**): `CodeRankEmbed` is WordPiece/30522/BertNormalizer with
`[UNK]/[CLS]/[SEP]` at 100/101/102 — drop-in. `jina-embeddings-v2-base-code`,
`codet5p-110m-embedding`, and `gte-modernbert-base` are all BPE, so they
would each require a new tokenizer in ese.

### 10.1 What the swap actually required

`ese/build.rs` was already model-agnostic in the important way — it consumes
any `[V × D]` safetensors matrix plus a WordPiece `tokenizer.json`. Three
changes were still needed:

1. **Build-time model selection.** `ESE_MODEL_URL` / `ESE_TOKENIZER_URL`
   env overrides (default unchanged), with `rerun-if-env-changed`, and the
   download cache keyed by model identity — otherwise two models collide on
   the same `model.safetensors` filename and the stale one silently wins.
2. **Marker vectors resolved by name, not by id.** `build.rs` hardcoded
   `100 => UNK, 101 => CLS, 102 => SEP`. BERT vocabs happen to match;
   distilled tables prune and reorder. Verified on `potion-base-8M`: ids
   100/101/102 are `¿`, `×`, `ß`. potion-code has `[UNK]` at id 1 and
   **no CLS/SEP at all** — and `encode_single` folds CLS and SEP into every
   vector, so the positional lookup would have added two arbitrary
   accented-character vectors to every embedding, scaled by `1/token_count`
   (i.e. worst on short queries). Absent markers now become zero vectors,
   which `accumulate` adds as a no-op, and the build warns.
3. **A latent trap in the Loc-Bench harness.** `ensure_index` reused an
   existing `.semgrep` if only the `sif` flag matched — never checking dims.
   A leftover 512-dim index would make every agent query bail on the dims
   check, which presents as catastrophic accuracy rather than the mechanical
   mismatch it is. Now rebuilds when the index predates the binary.

Everything downstream absorbed the change without edits: `dims` falls out of
`min(256, trunc_dims())`, `EMBED_DIM` follows, and `load_dir`'s existing
`meta.dims != EMBED_DIM` guard (`index.rs:471`) makes every stale cache entry
fail loudly. **Correction to an earlier assumption in this doc**: a table
swap is *not* silently cache-unsafe when dims change — that guard catches it.
It would only be silent for a future same-dims swap, which is the case a
model-identity field in `IndexMeta` still needs to cover.

Sizes: vocab 63,457 → 65,536 PHF slots × (8 + 256×4) = 65 MB `weights.bin`
(was 64 MB), binary 70 MB (was 69 MB). 256 dims halves `emb.bin`: kernel
index 1.3 GB → 918 MB, VS Code 74 MB → 60 MB. All 34 functional tests pass
at 256 dims, including cache transparency and `indexed_matches_unindexed`.

### 10.2 The probes: layer 3 is fixed

`tests/modelprobe.rs` and `tests/tokprobe.rs` were written as inverted
assertions — they *fail* when the space changes. Both now fail, as designed:

| probe pair | prose table (§9.9) | code table |
|---|---|---|
| `str` ~ `string` | −0.002 | **0.778** |
| `none` ~ `null` | ~0 | **0.675** |
| `fn` ~ `function` | ~0 | **0.498** |
| `regex` ~ `pattern` | ~0 | **0.454** |
| `mutex` ~ `lock` | 0.045 | **0.367** |
| `kmalloc` ~ `allocate` | ~0 | 0.214 |
| `def` ~ `function` | 0.037 | 0.082 |
| prose synonym mean | ~0.5 | 0.589 (held) |

Code-concept mean 0.438 vs prose 0.589 — the space now encodes code
relations without having lost prose synonymy. The §9.8 bait/gold MaxSim
inversion also flips (gold 3.310 > bait 3.307), though by a hair.

`identifiers_are_shredded_by_the_tokenizer` still **passes**: `scalar_None`
→ `[scalar, _, none]`, and `_` still self-matches at 1.000. Layer 1 (the
pretokenizer splitting on punctuation before any vocab lookup,
`pretokenizer.rs:115`) is untouched by a table swap, and `scalar_none` is
not among the mined tokens. Layer 2 (no context) remains unfixable by any
static model.

### 10.3 Offline results (same query sets and conditions as §9.4 base)

recall@5, `eval/data/codemodel-*.json` vs `lever-*-base.json`:

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

BM25 is unchanged everywhere, as it must be.

Three readings:

1. **The shipped default improves on both code corpora, on the metric that
   matters most.** Hybrid R@1 +0.070 (VS Code) / +0.050 (kernel), MRR@10
   +0.038 / +0.033. First-result quality is what drove the §7.1
   function-precision win and what MaxSim degraded in §9.7.
2. **Prose regresses, as a specialized model should.** Wikipedia semantic
   loses ~0.18 R@5. It is a control corpus, not a target; the hybrid path
   holds up (−0.010 direct R@5) because BM25 carries prose.
3. **The kernel paraphrase wall stands** (0.005 → 0.015 R@5, still ≤0.05).

### 10.4 Why the kernel gains so much less than VS Code

The obvious candidate is **training-language coverage**: CornStack is
Python, Java, JavaScript, Go, PHP, and Ruby. VS Code is TypeScript/
JavaScript — in distribution, and it gets +0.170 semantic direct. The Linux
kernel is C — out of distribution, and it gets +0.041. The cross-corpus
difference is *consistent with* the language hypothesis but does not isolate
it (corpus size and query sets differ too); a within-corpus split by file
extension would test it properly.

This reframes the kernel-paraphrase wall a second time. §9.9 moved it from
"query-time machinery" to "training data"; the plausible reading now is
narrower still — not "no code in the training data" but "no C". That is a
much cheaper problem than the one we thought we had.

Also note the ceiling this model sets on the hybrid path. On CoIR, its own
hybrid-with-BM25 row scores 43.36 avg vs 42.31 for BM25 alone — +1.05. Much
of what a better code embedder knows, our BM25 half already knew. The large
gains live in the pure-semantic path, and the fusion dilutes them.

### 10.5 Status

Offline is promising enough to spend the agent-level gate (§9.7: offline
gains are not evidence until they survive Loc-Bench). Running `sg-code` on
the same 50 instances as `sg-plain` in `results-ab.jsonl` — identical
prompt, harness, and driver model; the compiled-in table is the only
variable. Decision to adopt (and to hardcode the URLs, re-point the two
probe tests at the new properties, and add `model_id` to `IndexMeta`) is
gated on that result.

**Read that result as a best case.** All 50 instances are Python — 109 gold
files, every one `.py` — and Python is the first language in CornStack. Per
§10.4 this is the most favorable language setting the model has, so a win
here is an upper bound on what a C or Rust codebase would see, and it is the
same in-distribution advantage that makes VS Code (+0.170) look unlike the
kernel (+0.041). A *failure* to win in this setting would correspondingly be
strong evidence against the table. The size-stratified and language-varied
samples already queued in §7.1's follow-ups are what would bound the
general case.

### 10.6 Loc-Bench A/B result: the offline gains did not transfer (again)

50 instances, Sonnet, `sg-code` vs the stored `sg-plain` rows — identical
prompt, harness, driver, and flags; the compiled-in table is the only
variable. 47 pairs after dropping 2 baseline `parse_error` rows and 1
`agent_error`.

| paired | n | zero-search | med searches | file Acc@5 | fn Acc@10t | med cost |
|---|---|---|---|---|---|---|
| sg-plain (prose table) | 47 | 8 (17%) | 1 | **79%** | **64%** | $0.20 |
| sg-code (code table) | 47 | 14 (30%) | 2 | 70% | 57% | $0.19 |
| — both actually searched — | | | | | | |
| sg-plain | 33 | 0 | 3 | **76%** | **58%** | $0.21 |
| sg-code | 33 | 0 | 3 | 67% | 48% | $0.24 |

**The code table did not win, and by the §9.7 gate it does not graduate.**

Read it honestly, in both directions:

- **It is not a proven regression.** The entire gap is 3–4 instances.
  Discordant pairs run 4–0 (file Acc@5, all pairs) and 3–0 (both-searched),
  giving exact two-sided p = 0.125 and 0.250. This is *no evidence of
  improvement* plus weak directional evidence of harm — not a demonstrated
  loss. What is striking is the asymmetry: across both metrics and both
  subsets there is exactly **one** instance where the code table won and the
  prose table lost.
- **The headline number is partly driver noise.** Zero-search runs went 17%
  → 30%. Whether the agent searches at all is decided *before* any result
  returns, so the table cannot cause it; those runs are pure Sonnet
  stochasticity, and they are why the both-searched subset is the honest
  comparison. That subset still favors the prose table (−9pp file, −10pp
  function), so conditioning does not rescue the result.
- **n=50 cannot resolve this.** With ~30% of runs never invoking the tool,
  33 usable pairs, and a 3-instance effect, the eval is underpowered for
  anything short of a large effect. "Underpowered" cuts both ways here.

Why the large offline gains vanished — the most likely reading is that they
live in a path the default barely uses. §10.3's wins are concentrated in
**pure semantic** (+0.170 R@5 on VS Code), while **hybrid** moved only
+0.010 R@5 / +0.070 R@1. The shipped default is hybrid with `sem_weight
0.2`, and the model card's own CoIR hybrid row makes the same point
independently: dense+BM25 beats BM25 alone by +1.05 NDCG. We fused away most
of what we bought.

**Decisions:**

1. **Do not adopt as default.** No revert is needed — `build.rs` defaults to
   the prose table and the code table is opt-in via `ESE_MODEL_URL`, so the
   shipped binary is unchanged. Keep the swap mechanism: it is now tested,
   and it made this experiment cost an afternoon.
2. **Keep the §9.8/§9.9 probe tests asserting the prose-model properties**,
   since that is what ships. They remain accurate tripwires.
3. **The next question is not a better table, it is the fusion.** If the
   gains are real and pure-semantic, the test is a code table *with*
   `sem_weight` raised (or a semantic-first condition), not another table at
   0.2. That is one offline sweep plus one agent A/B.
4. **§9.7's rule holds for a second lever.** Offline retrieval gains —
   MaxSim's, and now a genuinely better embedding space — have twice failed
   to reach agent-level accuracy. The gate stays.

### 10.7 Dimensionality vs model, separated (2026-07-29)

`sg-code` confounded two changes: a code-trained table *and* 256 dims. A
third run, `sg-p256` (the shipped prose table truncated to 256 via
`ESE_DIMS`, same flags, same 50 instances), separates them into a factorial.

| condition | binary | file Acc@5 | fn Acc@10t | fn-acc GIVEN file |
|---|---|---|---|---|
| prose@512 | 72.8 MB | 79% | 64% | — |
| prose@256 | **39.0 MB** | 74% | 62% | — |
| code@256 | 73.2 MB | 70% | 57% | — |
| *both-searched subset (n=32)* | | | | |
| prose@512 | | 75% | 56% | |
| prose@256 | | 72% | **56%** | |
| code@256 | | 69% | 50% | |

On the both-searched subset prose@256 matches prose@512 on function accuracy
exactly — **zero discordant instances** — and trails 3pp on files, which is
one instance. So **§10.6's attribution was half wrong**: roughly half the
code table's 79→70 deficit is dimensionality, not the model.

Against ripgrep on the same 47 instances: prose@512 79%/64%, prose@256
74%/62%, rg 74%/57%, code@256 70%/57%. prose@256 ties rg on files while
keeping the function-level edge; code@256 is the one variant that surrenders
it. Every contrast is non-significant (p = 0.375–1.000).

**Shipped**: `Cargo.toml` pins `dim-256` (MRL prefix truncation — the
default build already truncates 1024→512). Binary 72.8 → 39.0 MB (−46%),
kernel index 1.3 GB → 918 MB. `ESE_DIMS` / `ESE_MODEL_URL` remain as
build-time overrides for future A/Bs.

## 11. Function chunking (2026-07-29)

§10.7's stratification produced the session's most interesting result and
motivated this experiment. Splitting the 47 instances by whether the issue
text *names* a gold identifier:

| stratum | | file Acc@5 | fn Acc@10t |
|---|---|---|---|
| issue NAMES the identifier (n=21) | rg | 81% | 62% |
| | prose@256 | 81% | **71%** |
| issue does NOT (n=26) | rg | 69% | 54% |
| | prose@256 | 69% | 54% |

The function-level edge comes entirely from grep's *best* case, and the two
are identical where ranked retrieval was supposed to separate. Conditional
on finding the right file (both: 35/47), semgrep names the right function
83% vs rg's 77%. **The advantage is "where in the file", not "which file"** —
which would explain why MaxSim and the code table produced nothing: both
improve *which chunks rank highest*, and file-level was already tied.

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

A 32-line window rarely cuts a function in half (11%); it **swallows ~3
whole functions**. The defect is dilution — the chunk vector is a mean over
several unrelated functions, which no better embedder can undo. This is
§9's uniform-mean-pooling pathology one level up, and it is *above* the
embedding in the pipeline.

### 11.2 Rule B: attaching leading doc without a parser

Two candidate rules for pulling a function's leading comment into its chunk:

| corpus | rule A (walk to blank line) | rule B (comment-aware, cap 20, 1 gap) |
|---|---|---|
| | doc / **code wrongly pulled** | doc / **code wrongly pulled** |
| python | 20% / 3% | 20% / **0%** |
| c | 9% / **36%** | 11% / **0%** |
| typescript | 5% / **55%** | 6% / **0%** |
| rust | 25% / 24% | **44%** / **0%** |
| ruby | 35% / 7% | 37% / **0%** |
| java | 54% / 0% | **58%** / **0%** |

The zero-language-knowledge rule A collapses on brace languages — in TS it
drags in the previous method's body 55% of the time, because methods pack
with no blank line and `}` is not a comment. **Rule B pulls 0% code
everywhere and captures more doc.** Its entire language cost is a ~10-entry
shared prefix table (`//`, `#`, `/*`, `@`, `#[`, `///`, …) — not a grammar.
Python docstrings need nothing at all: they are inside the body.

Note also that today's *overlapping* windows already capture a 3–5 line
comment block above a function most of the time, so naive function-node
chunking would have been a **regression**; Rule B exists to prevent that.

### 11.3 Implementation and measured tradeoffs

`funcchunk.rs`: tree-sitter for the one thing needing a grammar (where does
a function start?), Rule B for doc, size clamps both ways, line-window
fallback for unsupported languages, unparseable files, and every region
between functions. `Chunk` was already an arbitrary span, so nothing
downstream changed. `chunking` is recorded in `meta.json`, so a mismatched
index rebuilds rather than serving wrong spans.

**Binary** (8 grammars: py, js, ts, rust, go, c, java, ruby): +6.62 MiB,
39.0 → 45.9 MB — still 25.6 MiB below the original shipped binary, because
the dim-256 win pays for it. Note the cost is invisible until the code is
*used*; declaring the dependency alone measured 0 bytes.

**Cold path and index** (warm cache, repeats within 0.05s):

| corpus | mode | wall | chunks | index |
|---|---|---|---|---|
| django (2.9k py) | window | 0.49s | 22,341 | 14.6 MB |
| | function | 0.82s (1.7×) | 39,431 (+76%) | 19.1 MB (+31%) |
| litellm (5.1k py) | window | 1.68s | 76,740 | 53.2 MB |
| | function | 2.66s (1.6×) | 80,070 (+4%) | 48.1 MB (−10%) |
| vscode (4k ts) | window | 2.48s | 59,921 | 62.6 MB |
| | function | 2.97s (1.2×) | 68,559 (+14%) | 62.6 MB (0%) |
| linux (84k c) | window | 45.9s | 1,509,039 | 946 MB |
| | function | **64.0s (1.39×)** | 1,465,080 (−3%) | **839 MB (−11%)** |

Cold indexing costs 1.2–1.7×. Index size mostly *improves*: function chunks
carry no overlap, so BM25 postings shrink (kernel 541 → 445 MB) more than
the extra embedding rows cost; kernel RSS fell 0.78 → 0.68 GiB.

### 11.4 Result: no benefit, and the offline eval cannot referee it

**The offline eval is structurally biased here and must not be used.**
`eval/generate.py:63` samples fixed `WINDOW`-line chunks and defines ground
truth as that window's span, with queries written to be answerable by that
window — which often spans 2–3 functions that no single function chunk
contains. The eval's ground truth *is* one of the strategies under test. It
duly reported window ahead (vscode hybrid R@1 −0.050; kernel semantic R@5
−0.085, with BM25 unchanged to three decimals).

Loc-Bench, whose ground truth is the real fix's functions, is neutral:

| n=47 paired | file Acc@5 | fn Acc@10t | fn-acc GIVEN file |
|---|---|---|---|
| prose@256 window (shipped) | **74%** | **62%** | **83%** (35/47) |
| prose@256 function chunks | 70% | 57% | 82% (33/47) |
| ripgrep | 74% | 57% | 77% (35/47) |

The conditional metric — 83% → 82% — was *the* prediction, and it is flat.
Sign tests: files 0–2 (p=0.500), functions 2–4 (p=0.688). On the
both-searched subset function chunks are +3pp on functions, but that is one
instance and the full pairing contradicts it.

**Decision: not adopted, and removed from the tree** (2026-07-29). Unlike
`--maxsim`/`--sif`, which are cheap dormant flags, this one carried 8
tree-sitter grammars (+6.62 MiB) and a second code path through chunking —
too much standing cost for an unproven idea. `funcchunk.rs`, the `Chunking`
enum, the `--chunking` flags, and the grammars are gone; this section is the
record. Revisit only with an instrument that can resolve 3pp (§11.5).

### 11.5 The instrument is the bottleneck (the important finding)

Four consecutive engine changes have landed inside the noise: MaxSim
(p≈0.25), the code table (p=0.125), dims (p=0.500), chunking (p=0.500–0.688).
The reason is now measured. Across the 47 instances scored under all five
conditions:

| | file Acc@5 | fn Acc@10t |
|---|---|---|
| every condition solves it | 68% | 49% |
| every condition misses it | 19% | 30% |
| **discriminative** | **13%** | **21%** |

**80–87% of Loc-Bench instances carry no signal about the search engine.**
Measured pairwise discordance is ψ = 0.067 (file) / 0.088 (function) —
conditions agree on ~91% of instances. Required n at α=.05, 80% power:
7pp → 142 instances, 5pp → 277, 3pp → 769, 2pp → 1,729. Loc-Bench V1 holds
560, so **3pp is unreachable on this benchmark at any price**.

Screening to discriminative instances does *not* add power — McNemar depends
only on discordant pairs, and screening removes concordant ones — but it cuts
~4.5× off the cost of obtaining them.

Planned instead of more agent spend:

1. **Offline set, rebuilt and enlarged**: ~2,000 queries per corpus, ground
   truth anchored to a **symbol span** (tree-sitter, now available) rather
   than a sampled window. Fixes the §11.4 bias, resolves ~3pp instead of
   ~7pp, one-time generation cost, free to re-run.
2. **Agent launch set**: screen all 560 with neutral references (rg + plain
   semgrep), keep the ~120 discriminative; future A/Bs cost ~$25 not $116.
   Quote headline accuracy from the full sample, never the screened one.
3. **Query replay**: every agent search is logged with argv; replaying real
   queries offline removes agent stochasticity entirely at ~5× the sample
   size, for free. Do this before any further spend.

### 11.6 Cleanup, and a bug the dim-256 rollout exposed

Retired with the candidates: the `sg-code`/`sg-fnchunk` Loc-Bench conditions
(their result rows are kept), and ese's `ESE_MODEL_URL`/`ESE_TOKENIZER_URL`/
`ESE_DIMS` build overrides — `Cargo.toml` now pins `dim-256` directly, so
the env plumbing was dead weight in a sibling repo. Kept, because they are
correct independent of the experiments: `build.rs` resolving
`[UNK]`/`[CLS]`/`[SEP]` **by name** rather than by hardcoded id 100/101/102
(no behavior change for BERT vocabs, prevents silent corruption for any
other), and `ensure_index`'s binary-mtime staleness check.

**The bug**: shipping 256 dims made every pre-existing `~/.cache/semgrep`
entry unreadable, and `load_dir`'s dims check surfaced that as an error on
*every* search in a previously-cached scope — advising `re-run semgrep
index`, which isn't even the right remedy for a cache entry. That directly
contradicts §8's contract ("a cache that changes only latency is
memoization, and memoization doesn't need to be disclosed to the caller").

**The fix is structural, not a check.** Detecting incompatibility invites
drift between the detector and the loader, and `dims` is a weak proxy anyway:
a *different* table of the same width (a code-distilled 256-dim model, say)
passes a dims check and then silently scores yesterday's vectors against
today's queries. Instead, entries are namespaced by a generation key —
`~/.cache/semgrep/v2-d256-0d2d/<label>-<hash>/` — covering the format
version, the dims, and a 16-bit fingerprint of the embedding stack (obtained
by encoding a fixed probe and hashing the quantized vector, so it moves if
the table, the tokenizer, *or* the pooling changes). An entry written by an
incompatible binary sorts into a sibling directory and is never discovered.
The failure mode is "not found", so there is nothing to surface.

Three supporting changes:

- **Any** load failure on a cache entry — corruption, truncation, a missing
  `emb.bin` — falls through to the cold path, which repopulates. A repo-local
  `.semgrep` still reports, because that is an explicit artifact the user
  manages. Verified both directions.
- **Reclamation moved off the read path.** GC of stale generations and
  pre-generation flat entries runs after a write. `semgrep cache --prune`
  also runs it, since a user who only queries warm scopes never triggers a
  cold write.
- **The cache is bounded.** Entries whose root no longer exists are dropped
  (a deleted checkout previously held its index forever), then LRU eviction
  down to a 2 GB budget (`SEMGREP_CACHE_MAX_BYTES`). `semgrep cache` reports
  what is held, what it costs, and what is prunable — the README's "no size
  cap or LRU eviction, so it grows until you delete it" caveat is retired.

Regression tests: `unreadable_cache_entry_degrades_to_a_miss`,
`corrupt_cache_entry_degrades_to_a_miss`,
`cache_entries_are_namespaced_by_compat_generation`,
`cache_prunes_dead_entries_and_enforces_a_budget` (41 tests).

**Did this contaminate earlier results? No, and it was checked rather than
assumed.** No dims-mismatch error appears in any saved agent output; all 204
unexpected semgrep exits are exit 2 (agents passing grep's `-A`/`-B`, which
semgrep does not accept — present in the original 512-dim pilot too); every
harness isolates `SEMGREP_CACHE_DIR`; and Loc-Bench scores against
repo-local `.semgrep` per worktree, not cache entries. The bug could only
fire where an entry from a different-dims binary was discovered, which is the
interactive cache where it was found.

The general lesson is worth keeping: *any* future change to the embedding
table, dims, or index format invalidates every cached entry, and the cache
must absorb that silently. This was latent from the moment the cache
shipped; it took a real format change to expose it.

Agent-eval spend this session: $39.07 (sg-code $13.14, sg-p256 $13.50,
sg-fnchunk $12.43).

## 12. Adversarial audit of our own eval (2026-07-29)

The eval-v2 statistics reported semgrep beating ripgrep at p < 0.0001 on
every metric of every corpus, with discordance as lopsided as 173-0. A result
that clean, from a benchmark we wrote ourselves, is a reason to audit the
benchmark rather than celebrate.

### 12.1 The ripgrep baseline was a strawman

`run_eval.py`'s `rg_agent_style` had three compounding flaws:

1. `re.findall(r"[a-zA-Z0-9]+", ...)` — **no underscore**, so
   `blkg_rwstat_add` was shredded into blkg/rwstat/add before ripgrep saw it.
2. "Rarest" was approximated by **longest**. On
   `blkg_rwstat_add inline function choosing percpu counter…` that selects
   `function` and `choosing` — generic prose — and never the identifier.
3. The two terms were then required **on the same line**, which grep's
   line-orientation makes unlikely.

Net: on the queries where the answer's own name appears in the question, our
baseline never grepped for it. A competent agent runs `rg blkg_rwstat_add`
and lands immediately.

How often does that matter? Measured share of queries containing a *true*
identifier (snake_case or camelCase):

| corpus | direct | paraphrase |
|---|---|---|
| kernel | **66%** | 2% |
| VS Code | **70%** | 2% |
| wikipedia | 1% | 0% |

(An earlier version of this measurement counted long lowercase words like
`workaround` as identifiers and reported 93% for paraphrase — wrong, and
corrected here. The audit needed auditing.)

### 12.2 What a fair baseline costs us

`rg-strong` tries identifier-shaped tokens first, then the phrase, then the
AND/OR fallbacks. It was added **beside** the legacy condition rather than
replacing it, so the delta stays auditable instead of silently moving
published numbers.

| corpus / kind | rg (legacy) | rg-strong | semgrep | fair gap |
|---|---|---|---|---|
| kernel, direct R@5 | 0.05 | **0.32** | 0.92 | **2.9×** |
| VS Code, direct R@5 | 0.155 | **0.355** | 0.870 | **2.4×** |
| kernel, paraphrase R@5 | 0.000 | 0.000 | 0.027 | — |
| VS Code, paraphrase R@5 | 0.010 | 0.005 | 0.140 | 28× |

**The published "30× gap on identifier queries" is really ~2.9×.** Fixing the
tokenizer alone improves ripgrep 6.4× on the kernel and 2.3× on VS Code.
semgrep still wins every stratum at p < 0.0001 (kernel direct 45-0), but the
magnitude of the claim was substantially our own baseline.

Paraphrase is untouched by the fix — 0.005 vs 0.010 on VS Code, 0.000 both
ways on the kernel. There is no identifier to grep for, which is the
definition of the stratum. That asymmetry is the strongest evidence available
that the remaining advantage is a real capability difference and not an
artifact: improving the opponent closes the gap exactly where theory says it
should, and nowhere else.

### 12.3 Real queries: our conditions bracket reality, they do not represent it

Added CoSQA (`eval/fetch-cosqa.sh`): 9,020 human-written Bing queries,
relevance-labelled against 20,604 Python functions, with the **whole** corpus
written out so retrieval faces real distractors. Nobody who wrote these
queries had seen the code, so no generator could leak the answer's vocabulary
into the question.

| query set | n | has identifier | median words | tokens present in gold |
|---|---|---|---|---|
| ours, direct | 199 | 66% | 10 | — |
| ours, paraphrase | 199 | 2% | 17 | — |
| **CoSQA (real)** | **9,020** | **0%** | **6** | **42%** |

Real queries carry no identifiers, are far shorter, yet still share 42% of
their content tokens with the gold function. So `direct` is *easier* than
reality (the name is handed over) and `paraphrase` is *harder* (vocabulary
deliberately stripped, and 17 words where users type 6). Every quality claim
this project has made was anchored to one of those two poles; neither is
where users are.

Results on 1,200 sampled real queries:

| condition | R@1 | R@5 | R@10 | MRR@10 |
|---|---|---|---|---|
| **bm25** | 0.07 | **0.22** | 0.33 | **0.138** |
| hybrid (shipped) | 0.07 | 0.21 | 0.33 | 0.133 |
| semantic | 0.02 | 0.08 | 0.12 | 0.048 |
| rg / rg-strong | 0.01 | 0.03 | 0.04 | 0.013 |

Three findings:

1. **The fair baseline changes nothing here** (0.03 either way) — 0% of real
   queries contain an identifier, so a smarter grep has nothing to grab.
2. **semgrep's real-query advantage is larger than on synthetic direct
   queries**: 8.3× at R@5, 237-17 discordant, p < 0.0001. The strawman
   *understated* the tool in the regime that matters while overstating it on
   identifier queries.
3. **The semantic half contributes nothing.** BM25 alone (0.22) matches
   hybrid (0.21) and nearly triples semantic alone (0.08). On real queries
   over real Python, the win is code-aware *lexical* ranking — subtoken
   tokenization, path augmentation, ranked top-k over chunks — not
   embeddings. Consistent with §9.9, and with four straight embedding levers
   failing to transfer.

Caveat in the other direction: CoSQA labels one gold function among 20,604,
and `python condition non none` could be answered by many. Single-truth
scoring makes 0.21 a floor.

### 12.4 A cost asymmetry the accuracy tables hide

`rg-strong` is expensive *because* it is fair. A paraphrase query exhausts
all five patterns — two identifier attempts, the phrase, the AND fallback,
the OR fallback — and each is a full 1.15 GB scan. Including the legacy
condition, a kernel query that ripgrep ultimately fails to answer costs
**~8 full scans, ~25 s**, against semgrep's single ~100 ms warm query.

This is the retry-loop argument of §5.3, finally visible. Loc-Bench could not
show it because agent cost there is 91% conversation-replay cache reads
(§7.1), which swamps search entirely. A competent grep strategy means *more*
scans on failure, not fewer.

### 12.5 Decisions

- **Revise the README.** "3-27%" for ripgrep understates it (32-36% on direct
  with a fair baseline), and the 30× framing must go. Replace the single
  multiple with the split the data supports: modest advantage when the
  identifier is known, large advantage when it is not.
- **Keep both baselines.** `rg` for comparability with published numbers,
  `rg-strong` as the honest opponent. Report both.
- **CoSQA becomes a standing corpus**, and the first-class one for quality
  claims, because it is the only query set not written by us.
- **Regenerate our own sets symbol-anchored** (§11.5 item 1) and consider
  retiring `direct` entirely — a query containing the answer's name measures
  tokenizer plumbing, not retrieval.


---

## 13. A fourth corner, and a ceiling for ripgrep (2026-07-30)

§12 audited the eval and found two things: the ripgrep baseline was a strawman,
and our own query sets leak the answer into the question. It closed with two
open items — replay the queries agents actually issued (§11.5 item 3, "do this
before any further spend"), and find out how much of the remaining gap is still
the baseline. This section does both, and measures a third leak §12 missed.

### 13.1 Path leakage: the generator was shown the answer's filename

`eval/generate.py` put the file path into the prompt — *"Below is a chunk of a
file from a corpus ({path}, lines {start}-{end})"* — and semgrep's tokenizer
does path augmentation. So the generator saw the document identifier and the
scorer indexes the document identifier.

| set | kind | basename | **file stem** | dir segment | **path seg NOT in gold** |
|---|---|---|---|---|---|
| linux | direct | 1.5% | 32.7% | 48.2% | **16.1%** |
| linux | paraphrase | 0.0% | 0.0% | 25.1% | **17.1%** |
| vscode | direct | 0.0% | 22.5% | 46.0% | **12.0%** |
| vscode | paraphrase | 0.0% | 0.0% | 26.5% | **12.0%** |
| wikipedia | both | 0.0% | 0.0% | 0.0% | 0.0% |

The last column is the one that isolates the effect: a path segment the query
carries that the gold *text* does not, which identifier overlap cannot explain.

**The finding that matters is the second row.** §12.3 treated `paraphrase` as
the clean pole — vocabulary deliberately stripped, 2% identifier share. But it
leaks path segments at 17.1%, *higher* than `direct`'s 16.1%. The generator was
told to avoid the chunk's identifiers. Nothing told it to avoid the path, so it
reached for the one piece of the answer it was still allowed to see. Neither
pole is clean, and `paraphrase` is not the conservative choice it was taken for.

Caveat, recorded rather than buried: in C a file stem and an identifier prefix
are frequently the same token (`blkg-rwstat.c` ↔ `blkg_rwstat_add`), so the
`file stem` column partly re-measures §12.1's identifier leakage rather than
isolating a new effect. Only the last column is clean.

The prompt no longer passes `{path}`. The measurement above is of the sets on
disk, taken *before* that change, so the delta stays auditable — §12.2's
precedent.

`run_eval.py` now prints this table above every results table and stores it in
`--out`. §12.5 said no quality claim should be read without knowing which pole
produced it; that is now a property of the harness rather than a note in a doc.

### 13.2 Query replay: what agents actually type

497 unique ranked queries and 726 exact ones, harvested from 706 shim logs
across 42 instances, replayed offline against each instance's worktree.
`replay.py` existed but had never been run.

Four defects were fixed first, two of which would have produced a *quotable
wrong number* rather than an error:

1. **The bootstrap ignored clustering.** Queries are not independent draws —
   one instance contributed 55 of 497 (11%), the median 14. Resampling queries
   treats those 55 as 55 observations. Now resamples **instances**, and prints
   the naive interval beside the clustered one so the inflation is visible.
2. **`harvest()` mixed regexes with queries.** Its filter took `rg`
   invocations too, and those are patterns like `csrf|CSRF|X-CSRF|wtf`. That is
   390 of 887 rows measuring how BM25 tokenizes punctuation. `rg` is now
   excluded by default.
3. **No cache isolation and no index.** Conditions could be answered from
   whatever cache state a previous condition left (FIXES.md #10's shape), and
   the first condition paid a cold search while later ones ran warm. Now a
   run-local `SEMGREP_CACHE_DIR` and one `ensure_index` per worktree, with
   index-affecting flags rejected outright.
4. **`rank_of_gold` credited path-suffix matches**, so a hit on
   `tests/test_a.py` counted as finding `src/tests/test_a.py`. Now exact, with
   the loose clause instrumented: it fired **0 times in 1,491 scored queries**.

**Results** (rank of the first gold file, k=10):

| condition | hit@1 | hit@5 | hit@10 | MRR |
|---|---|---|---|---|
| hybrid | **0.254** | **0.493** | **0.610** | **0.362** |
| bm25 | 0.219 | 0.473 | 0.584 | 0.330 |
| semantic | 0.195 | 0.461 | 0.563 | 0.306 |

| pair | MRR delta | clustered 95% CI | naive 95% CI | verdict |
|---|---|---|---|---|
| hybrid − bm25 | +0.0315 | [+0.0012, +0.0630] | [+0.0134, +0.0501] | WIN, barely |
| hybrid − semantic | +0.0563 | [+0.0079, +0.1065] | [+0.0264, +0.0887] | WIN |
| bm25 − semantic | +0.0248 | [−0.0277, +0.0804] | [−0.0105, +0.0602] | inconclusive |

Two things to say about this honestly.

**The clustering correction is not cosmetic.** `hybrid − bm25` has a clustered
lower bound of **+0.0012**. The naive interval starts at +0.0134, an order of
magnitude clear of zero. Same data, same point estimate; one of them would have
been reported as a solid win and the other is a coin-flip away from
inconclusive. Every replay number in this section is the clustered one.

**This contradicts §12.3's conclusion on CoSQA, and the contradiction is the
interesting part.** There, "the semantic half contributes nothing" — bm25 0.22
matched hybrid 0.21. Here hybrid beats bm25. The difference is the query
distribution: CoSQA queries are human prose with 0% identifiers, agent queries
are half identifiers and a quarter the length. Neither result is wrong. The
lesson is that "does the semantic half earn its keep" has no corpus-independent
answer, and §12.3's finding should be read as scoped to human prose queries,
not as a general verdict.

### 13.3 The query distribution, which is a fourth corner and not a fix

§12.3 put our two synthetic poles beside real human queries. Replay adds real
*agent* queries, and they are a fourth point, not a resolution:

| set | n | identifier% | median words |
|---|---|---|---|
| ours, direct | 199 | 66% | 10 |
| ours, paraphrase | 199 | 2% | 17 |
| CoSQA (real humans) | 9,020 | 0% | 6 |
| **agent replay, ranked** | **497** | **47%** | **4** |
| **agent replay, exact (`-e`)** | **726** | **63%** | **1** |

Agents do not write prose and they do not write our paraphrases. They type
short identifier-shaped fragments — a median of *four* words ranked, *one* in
exact mode. So: replay is where this product's actual input lives, CoSQA is
where human users live, and our two generated poles are neither.

The queries are checked in at `eval/queries/replay-agent.jsonl` (strings only,
no repo content) so this distribution is reproducible from the repo.

An earlier count in this session said 880 queries at a median of 2 words. That
was wrong twice over: it parsed `argv` with an off-by-one, and it counted the
`rg` regexes as queries. The numbers above come from `replay.harvest` itself.

### 13.4 `rg-oracle`: a ceiling for ripgrep (prediction, pre-registered)

§12.2 replaced the strawman baseline with `rg-strong` and the kernel gap fell
from "30×" to ~2.9×. But `rg-strong` is still a hand-tuned query planner: two
identifiers longest-first, then the phrase, then two fallbacks. So the question
§12 left open is whether the *rest* of the gap is the engine or still the
baseline's planning.

`rg-oracle` removes the planning. It tries every content token in the query as
its own pattern and keeps whichever scored best — which requires already
knowing the answer, so **no agent can run it**. It is a ceiling, reported as
one, added beside `rg` and `rg-strong` and replacing neither.

Cost is the design problem: 12 tokens × 400 queries × a 1.15 GB kernel scan is
hours. The pruning is exact rather than heuristic — a hit only counts if it
lands in the gold file on a line overlapping the gold span ± slack, so a token
absent from that window cannot produce a correct hit at any rank and its scan
cannot change the answer. That typically leaves 1–3 tokens instead of 12.

(The cheaper-looking alternative, one `rg --json -e tok1 -e tok2 …` scan, was
rejected: rg reports the matched *text*, not which pattern matched, and `-m 1`
caps output at the first matching line per file, so a token matching later in
an already-matched file becomes invisible. An upper bound that under-credits is
not an upper bound.)

**Prediction, recorded before the run so it can be falsified:**

- CoSQA R@5: rg-strong 0.03 → oracle **0.06–0.10**, staying under bm25's 0.22.
  Real human queries carry 0% identifiers, so there is little for any token to
  find and the ceiling should stay low.
- Kernel `direct` R@5: rg-strong 0.32 → oracle **0.60–0.80**, against
  semgrep's 0.92. Two-thirds of these queries contain the gold identifier, so
  a perfect token chooser should do well — but ranked retrieval should still
  lead.
- Kernel `paraphrase` R@5: **≈0**. Vocabulary was stripped; there is no token.

**Falsification condition:** if kernel `direct` R@5 reaches **≥0.85**, the
identifier-query claim is baseline-shaped rather than engine-shaped, and §12.2's
correction did not go far enough. That would need retracting, not explaining.

### 13.5 `rg-oracle`: the result, and two ways the ceiling was wrong first

The prediction in §13.4 was recorded before the run. Here is what happened, on
four new corpora (rust/java/go/ruby, symbol-anchored ground truth, §13.6):

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

**The margin survives the ceiling.** rg-oracle is 1.8–3.6× rg-strong, so
`rg-strong` really was still leaving ripgrep performance on the table and §12.2
did not go far enough as a bound. But semgrep is **3.7–4.3× above the oracle**
on `direct` — against a ripgrep that is allowed to consult the answer before
choosing its pattern. That is the falsification test §13.4 set up, and the
claim passes it: the gap is engine-shaped, not baseline-shaped.

The pre-registered falsification condition (kernel `direct` R@5 ≥ 0.85 for the
oracle) is not met anywhere here — the highest is 0.236.

**The kernel and CoSQA oracle runs have NOT been done.** Both were started and
both were interrupted before producing any output; nothing is reported for them
above, and the §13.4 prediction for those two corpora stands untested. They are
the two with the sharpest predictions, so this is the gap in this section and
not a footnote to it. Cost estimate below, so whoever runs them budgets for it
rather than discovering it.

**bm25 ≥ hybrid on three of four corpora.** 0.710 vs 0.700 (tokio), 0.705 vs
0.695 (etcd), 0.849 vs 0.864 (commons-lang), 0.864 vs 0.886 (jekyll). The
semantic half adds nothing on code here, which agrees with §12.3's CoSQA
finding and §9.9's measurement that "on code, ese functions as a fuzzy lexical
matcher, not a semantic model." Note this does **not** contradict §13.2, where
hybrid beat bm25 on real agent queries — those are a different distribution,
and that is the whole point of having both.

**The paraphrase wall stands.** 0.065–0.182 R@5 for hybrid, on four corpora in
four languages, consistent with §9.4's kernel finding. Four years of levers
have not moved it and neither did four new corpora.

#### The ceiling was not a ceiling, twice

Worth recording because the failures were more informative than the result.

**First: a single-token vocabulary cannot bound a conjunctive one.** The oracle
tried every content token on its own. `rg_strong` also tries `A.*B` — both
tokens on one line — which is strictly *more* selective than either alone, so
gold can rank better under it than under any single token. Measured across
1,374 real queries, the "upper bound" lost to the thing it was bounding on
**53 of them (3.9%)**. Fixed by making the candidate set a superset of
`rg_strong`'s attempts, which makes the bound structural rather than hoped-for.

The fixture test asserting `rank(oracle) <= rank(rg_strong)` passed throughout.
It never constructed a case where a conjunction beat every single token. A
property test over four real corpora did, immediately.

**Second: ripgrep's output order was not deterministic.** After that fix, 12
violations remained. `rg_run` never passed `--sort`, and ripgrep parallelizes
its directory walk and emits results as workers finish. Six runs of one pattern
over etcd produced **two distinct top-10 orderings**. A rank is a position in
that list, so:

> Every `rg` and `rg-strong` number this harness has produced — including
> §12.2's fair-baseline table — carried run-to-run variance from thread
> scheduling, and no rg result was exactly reproducible.

Measured spread on rg-strong R@5 over 150 etcd queries: **0.0067 across three
runs** unsorted, **0.0000** with `--sort path`. That is small enough that it
overturns no published conclusion, and large enough to matter at the resolution
§11.5 is trying to reach — 0.67pp of pure scheduling noise against a target of
resolving 3pp effects. `--sort path` is now always passed.

CLAUDE.md says the snapshot tripwire "has caught non-determinism that no test
could see." It covers ranked output over the frozen fixture; the ripgrep
baselines were outside it, and stayed nondeterministic for the whole life of
the harness.

**What determinism costs.** `--sort path` makes ripgrep walk single-threaded.
Measured on the kernel with a warm page cache, three patterns, best of three:

| pattern | unsorted | `--sort path` | ratio |
|---|---|---|---|
| `blkg_rwstat_add` | 1.76 s | 6.50 s | 3.7× |
| `config` | 1.92 s | 8.36 s | 4.4× |
| `static` | 1.92 s | 8.46 s | 4.4× |

That is the price of a reproducible rank and it is worth paying, but it changes
what is affordable. `rg-oracle` issues up to `ORACLE_MAX_TOKENS` + ~5 patterns
per query, so a 150-query kernel oracle run is on the order of **hours**, not
minutes. On the small corpora (3–15 MB) the same run is a few minutes, which is
why §13.5's table exists and the kernel's does not yet. Budget for it, or lower
`ORACLE_MAX_TOKENS` for large corpora and say in the writeup that the ceiling
was capped.

### 13.6 Four more corpora, and what they were for

| corpus | lang | files | source | symbols | has_doc | queries |
|---|---|---|---|---|---|---|
| tokio | rust | 790 | 6.0 MB | 7,728 | 59% | 400 |
| commons-lang | java | 625 | 10.3 MB | 4,985 | 88% | 398 |
| etcd | go | 1,110 | 15.4 MB | 9,211 | 20% | 400 |
| jekyll | ruby | 166 | 3.3 MB | 1,068 | 45% | 176 |

`symbols.py` supports python/js/ts/rust/go/c/java/ruby and until now only c
and ts were exercised by a corpus — go, java, ruby and rust were tested against
hand-written fixtures alone. All four sit in the <2k-file band where §9.7 found
engine variants actually diverge; the original three are 84k, 4k and 1k files.
The `has_doc` spread (20–88%) is deliberate: it is a stratum, and a stratum
needs variance.

Ground truth is symbol-anchored, so these sets — unlike the three older ones —
can referee a chunking change without §11.4's circularity.

Running `extract()` over 22,992 real symbols found one defect the invariant
checks did not: `def self.foo` extracted the name `self` (29 of jekyll's 1,060
ruby symbols). Spans were correct, so ground truth was unaffected; the `symbol`
stratum was reporting a keyword as a method name. Invariant checks — span
ordering, EOF bounds — returned **0 violations across all four corpora** and
would never have caught it. It surfaced from a test that asserted what the name
should *be*.

### 13.7 Reproducing §12.2 against a deterministic ripgrep

§13.5 found that every rg figure this harness had produced carried thread-
scheduling variance. That makes §12.2's fair-baseline table a claim nobody
could check, so it was re-measured on the same query sets with the ordering
fixed.

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

**The conclusion holds.** The fair gap is 2.6× on the kernel and 2.4× on VS
Code against §12.2's 2.9× and 2.5×. "The published 30× is really ~3×" survives;
the third digit does not, and never could have.

**VS Code reproduces to ±0.005** — one query in 200 — in all four cells, with
both deterministic columns landing on 0.000. That is the strongest available
evidence that the harness itself is now reproducible.

**The kernel does not, and part of it is unexplained.** The rg columns move
±0.025, which is the right order for scheduling noise at 84k files (§13.5
measured 0.0067 on etcd's 1.5k). The *semgrep* column also moves — −0.021
direct, +0.013 paraphrase — and that cannot be scheduling noise, because those
modes are deterministic. Two candidates were tested and one was eliminated:

- **Index staleness: ruled out.** The kernel index predated the binary by a
  day, which looked like the answer. Rebuilding it and rescoring reproduced
  **all 1,194 ranks exactly** — every mode, every query. Staleness changed
  nothing here. (The freshness guard was kept anyway: the hazard is real and
  `locbench/run.py:220` has guarded it for a while, but it did not explain
  this.)
- **Engine drift: open.** P6 was A/B'd as retrieval-neutral on **vscode**
  (400 queries × 3 modes, all 21 metrics ±0.000) — and vscode is precisely the
  corpus that reproduces here. The kernel was never in that A/B. Kernel-only
  drift is consistent with everything observed and is not established.

Note also that §12.2's kernel "semgrep" figure of 0.92 equals this run's **bm25**
(0.920) rather than its hybrid (0.899), while §12.2's VS Code 0.870 equals this
run's **hybrid** exactly. Whether that column was ever one mode is not
recoverable from the doc.

What this costs: the kernel rows of §12.2 should be read as ±0.02, not to three
decimals. The VS Code rows are reproducible as published. Every future run is
reproducible in both, which is the point of the exercise.

### 13.8 The ceiling on real human queries (CoSQA)

The §13.4 prediction for CoSQA was **0.06–0.10 R@5, staying under bm25's 0.22**.
Result, over all 1,200 real Bing queries:

| mode | R@1 | R@5 | R@10 | MRR@10 |
|---|---|---|---|---|
| rg (legacy) | 0.012 | 0.030 | 0.051 | 0.021 |
| rg-strong | 0.012 | 0.030 | 0.051 | 0.021 |
| **rg-oracle** (ceiling) | 0.043 | **0.101** | 0.158 | 0.069 |
| semantic | 0.022 | 0.083 | 0.122 | 0.048 |
| hybrid (shipped) | 0.068 | 0.208 | 0.330 | 0.133 |
| bm25 | 0.074 | **0.222** | 0.325 | 0.138 |

**The prediction holds, and it landed one thousandth above the band** — 0.101
against a predicted ceiling of 0.10. Calling that a hit would be generous;
calling it a miss would be pedantic. It is recorded as what it is.

**§12.3's semgrep numbers reproduce exactly.** bm25 MRR 0.138, hybrid 0.133,
semantic 0.048 — all three to three decimals. The rg columns moved (MRR 0.013 →
0.021), which is the §13.5 nondeterminism showing up exactly where it should
and nowhere else: the deterministic engine reproduces, the baseline that was
never deterministic does not.

**The finding: §12.3's real-query claim has the same shape §12.1 found in the
kernel claim.** §12.3 reported semgrep's advantage on real queries as **8.3×**
at R@5 (0.22 vs 0.03). Against a ripgrep permitted to read the answer before
choosing its pattern, it is **2.2×** (0.222 vs 0.101). The ceiling is 3.4× the
`rg-strong` heuristic, so most of that 8.3× was, once again, query planning
rather than retrieval.

That is the second time this pattern has been measured. §12.2 cut "30×" to
2.9× on the kernel by fixing the baseline's tokenizer; §13.8 cuts "8.3×" to
2.2× on real queries by removing its query planning entirely. **The direction
of the claim survives both corrections. The magnitude has now been wrong
twice, in the same direction, for the same reason.** Any future gap this
project publishes should be quoted against the ceiling, not against a
heuristic we wrote.

**A ripgrep that reads the answer beats our semantic mode** — 0.101 vs 0.083
R@5, and 0.069 vs 0.048 MRR. On real human queries the embedding half is worth
less than perfect grep-token selection. §9.9 measured why: on code, ese
functions as a fuzzy lexical matcher rather than a semantic model
(`def~function` 0.037, `mutex~lock` 0.045). This is that measurement showing up
in an end-to-end score.

What survives all of it: **bm25 at 0.222 is still 2.2× the ceiling**, on the
one query set nobody on this project wrote. Ranked lexical retrieval earns its
keep on real queries; the semantic half does not; and the honest multiplier is
2.2×, not 8.3×.

### 13.9 The kernel ceiling: the falsification test resolves

This is the run §13.4 pre-registered a retraction against. Result, 199 `direct`
and 199 `paraphrase` queries over the kernel:

| condition | direct R@5 | paraphrase R@5 |
|---|---|---|
| rg (legacy) | 0.025 | 0.000 |
| rg-strong | 0.342 | 0.000 |
| **rg-oracle** (ceiling) | **0.462** | **0.000** |
| hybrid | 0.899 | 0.040 |
| bm25 | 0.920 | 0.035 |

**The claim survives.** The retraction condition was oracle `direct` R@5 ≥ 0.85.
Measured: **0.462**. §12.2's identifier-query finding stands, now against a
ripgrep permitted to read the answer before choosing its pattern.

**The prediction was wrong, and low.** §13.4 predicted 0.60–0.80 and the ceiling
came in at 0.462 — outside the band, in the direction of having *overestimated*
ripgrep. Two-thirds of these queries contain the gold identifier, and the
reasoning was that a perfect token chooser should therefore do well. It does
not, for the reason §13.8 makes concrete: picking the right token is not the
hard part on a corpus this size. `blkg_rwstat_add` is rare, but plenty of
identifiers in kernel queries appear in hundreds of files, and ripgrep returns
those in path order with no way to rank the gold one up. Recorded as a miss
rather than reframed.

**The kernel is where `rg-strong` was already nearly optimal.** The ceiling is
only **1.4×** the heuristic here, against **3.4×** on CoSQA (§13.8). That is
the expected shape and worth stating: when the query contains a rare
identifier, "grep the longest identifier" is close to the best available
strategy, so there is little headroom for an oracle to find. When the query is
ordinary English (CoSQA), token choice matters much more. The two corpora
bracket the effect.

**Against the ceiling, the fair gap is 2.0×** (0.920 vs 0.462), against 2.7×
versus `rg-strong` and §12.2's published 2.9×. The three numbers tell a
consistent story and the direction never moves.

#### The paraphrase result is the strongest evidence in this document

`rg-oracle` scores **exactly 0.000** on all 199 paraphrase queries. Not 0.005.
Zero.

A ripgrep that is allowed to inspect the answer, try every content token in the
query, and keep whichever scores best, cannot locate a single one of 199
targets once the query stops naming them. semgrep finds 4%.

§12.2 argued this asymmetry was "the strongest evidence available that the
remaining advantage is a real capability difference and not an artifact:
improving the opponent closes the gap exactly where theory says it should, and
nowhere else." That argument was made against a *heuristic* opponent. It now
holds against a *perfect* one, which is the strongest form the argument can
take: the paraphrase stratum contains no token to grep for, so no amount of
grep skill helps, and the only thing that can close it is retrieval that does
not depend on shared vocabulary.

The corollary is equally worth stating: semgrep's own paraphrase number is
0.04. Both things are true — the capability difference is real, and it is a
difference between 4% and 0%, not between good and bad. §9.4's wall stands.

### 13.10 MaxSim reranking as a default: no

Re-tested because the §9 lever numbers that first recommended MaxSim were
produced under the contaminated cache (FIXES.md #10), before rg determinism,
and before FIXES.md #9 found a NaN poisoning the reranked head — a bug
"reachable only via `--maxsim`, which is why no eval run caught it." A
recommendation resting on numbers with three known problems deserves re-running.

**14 paired comparisons, 3,071 queries, 0 wins, 1 loss.**

| set | n | conditions | result |
|---|---|---|---|
| CoSQA (real humans) | 1,200 | pool 32 / pool 96 / blend 0.5 | all **inconclusive**, +0.001 to +0.003 |
| replay (real agents) | 497 | mx48 / mx96, clustered CI | all **inconclusive** |
| tokio/commons-lang/etcd/jekyll | 1,374 | `--maxsim` | 7 inconclusive, **1 LOSS** |

The loss: jekyll `paraphrase` R@5 0.182 → 0.136, delta −0.045, CI
[−0.091, −0.011], 0-4 discordant.

**"Inconclusive" here is the well-powered kind, which is the useful part.** On
CoSQA at n=1,200 the 95% CI on the R@5 delta is about ±0.007. That does not
say "we could not tell"; it says **any effect is smaller than roughly one
point**, in either direction. The same question at n=88 (jekyll) genuinely
cannot tell, and is reported separately rather than averaged in.

**The direct-query trend is negative and consistent.** All four code corpora
move down (−0.005, −0.010, −0.011, −0.020). Pooled: −0.0116, CI
[−0.0262, +0.0029], p=0.17 — still inconclusive, but 4/4 with the same sign is
not the shape of a change about to pay off.

**It is not free.** Warm latency on small corpora, three queries averaged:

| corpus | base | maxsim |
|---|---|---|
| jekyll | 8.2 ms | **12.6 ms** |
| etcd | 8.2 ms | **12.2 ms** |
| linux | 91.5 ms | 78.8 ms |

~50% on the small corpora. The kernel row shows maxsim *faster*, which is
almost certainly noise at n=3 and is recorded rather than used — measuring it
properly would need `bench/run.py`, and it does not change the verdict either
way.

**And §9.7 stands unrefuted.** That section A/B'd MaxSim at the *agent* level
and found it actively harmful: fnAcc@10t plain 62% > mx48 59% > mx96 54%, with
agents searching *more* under maxsim (201 vs 142 calls) because worse first
results beget retries. The replay result here is inconclusive, and inconclusive
does not overturn a measured harm — replay removes the agent's decisions, which
is exactly the mechanism §9.7 blamed.

**Verdict: not a candidate for the default build.** A change that adds latency,
adds a flag, and adds a less-tested code path has to *earn* default status. The
evidence says: no win on either real-query set, one loss, a negative trend on
code, a latency cost, and a standing agent-level finding against it. §9.4's
"adopt but re-wire" should be read as superseded.

It stays available behind `--maxsim` for anyone who wants to explore the
rerank, and the numbers above are in `eval/results/` if someone wants to argue
with them.

#### Root cause: MaxSim works, on the channel that does not matter

§13.10 reported no effect and stopped there, which was not an explanation. The
mechanism, traced through `search/indexed.rs:214` and `rank/maxsim.rs`:

**MaxSim reranks the *semantic* candidate list, before RRF fusion.** That
placement is deliberate — `maxsim.rs:28` records that post-fusion reranking
"let MaxSim override BM25's exact-match signal instead of being fused with it,
which measurably hurt hybrid on code (§9.4)."

So the question is not "does the reranker work" but "does the list it reranks
decide the answer." Measured separately:

| corpus / mode | base R@5 | +maxsim | delta | verdict |
|---|---|---|---|---|
| etcd / **semantic** | 0.340 | **0.420** | **+0.080** | **WIN** (CI [+0.010,+0.155], p=0.040) |
| jekyll / **semantic** | 0.636 | **0.716** | **+0.080** | inconclusive (n=88) |
| etcd / hybrid | 0.695 | 0.675 | −0.020 | inconclusive |
| jekyll / hybrid | 0.886 | 0.875 | −0.011 | inconclusive |

**The reranker is not broken. It is a real +8pp on the list it touches** — a 24%
relative gain on etcd, and half the queries move (97/200). On the shipped
hybrid mode, 97% of queries come back completely unchanged (1,335/1,374 across
four corpora).

The causal chain is now complete, and every link is separately measured:

1. **§9.9** — on code, ese's static vectors act as a fuzzy lexical matcher, not
   a semantic model (`def~function` 0.037, `mutex~lock` 0.045).
2. **§12.3, §13.8** — so the semantic channel contributes almost nothing to the
   fused result: bm25 0.222 ≥ hybrid 0.208 on real queries, and bm25 ≥ hybrid on
   three of four code corpora. A ripgrep *oracle* (0.101) outscores semantic
   (0.083).
3. **Here** — MaxSim improves the semantic list by +8pp, and the fused output
   does not move, because the improved list was contributing little to begin
   with.

**Which means the honest verdict is narrower than §13.10's.** MaxSim is not a
bad reranker and the theory behind it is sound: one strong identifier match
should not be averaged away by boilerplate, and it isn't. The reason it cannot
earn default status is that **the bottleneck is upstream of it.** Reranking a
weak signal more cleverly does not make it a strong one.

Two things follow:

- **`--maxsim` should be the default for `--mode semantic`**, where it is a
  measured win. It is not, today.
- **The lever that would make MaxSim matter is a better code embedding**, not a
  better rerank. §10 already tried swapping the table and §9.9 explains why the
  current one is weak on code. Until that changes, work on the semantic channel
  has a low ceiling on the shipped default no matter how good the reranking is.

One prediction this investigation got wrong, recorded because it was cheap to
check and would have been a tidy story: MaxSim sums per-token similarities with
**no length normalization** (and, in a default build with no SIF stats, no IDF
either — `token_vectors(doc, None)` gives every token weight 1.0), so it should
favour longer chunks. Measured over 600 top-5 hits: mean chunk length 30.8 base
vs 30.9 with maxsim, a ratio of 1.00. The chunker emits fixed 32-line windows,
so there is no length variance for the bias to act on. The absent IDF is real
and remains a reason to distrust MaxSim on prose-heavy queries; it is not what
is happening here.

### 13.11 Post-fusion reranking, re-tested

`maxsim.rs` has carried a one-line justification since §9.5: *"This runs before
RRF, not after. Post-fusion reranking let MaxSim override BM25's exact-match
signal instead of being fused with it, which measurably hurt hybrid on code."*

That claim was worth re-testing, because the measurement behind it (§9.4,
2026-07-28) has three known problems: it was produced under the contaminated
cache (FIXES.md #10 — "every §9 lever number in RESEARCH.md was produced under
it"), before ripgrep determinism, and **before the NaN fix of FIXES.md #9** — a
bug "reachable only via `--maxsim`, which is why no eval run caught it," which
could scramble the reranked head outright.

It also tested one configuration. `blend_head` takes an alpha: 1.0 is pure
MaxSim, 0.0 keeps the incoming order. §9.4 ran at the default 1.0 — a full
override — so "overriding BM25's signal" was assumed rather than tuned. A
partial blend post-fusion had never been measured.

`--maxsim-post` now implements it, in both the warm and cold paths. Swept over
four corpora, 1,374 queries, paired against unmodified hybrid:

| blend | direct R@5 | Δ | paraphrase R@5 | Δ |
|---|---|---|---|---|
| base (no rerank) | 0.770 | — | 0.116 | — |
| 1.00 (pure MaxSim) | 0.514 | **−0.256** | 0.049 | **−0.067** |
| 0.50 | 0.719 | **−0.051** | 0.105 | −0.012 |
| 0.25 | 0.769 | −0.002 | 0.102 | **−0.015** |

**§9.4's verdict is confirmed, now on a measurement that can be trusted, and
with a mechanism.** The loss is monotone in alpha: the more MaxSim is allowed
to override the fused order, the worse the result. There is no blend where it
wins. At 0.25 it reaches "indistinguishable from doing nothing" on direct
queries — and gets there by turning itself almost off, while still losing on
paraphrase.

That shape is the same finding as §13.10 seen from the other side. MaxSim's
per-token similarity over static embeddings is a *weaker ranking signal than
BM25 fused with RRF*. Pre-fusion it improves the semantic branch (+0.08 R@5)
because that branch is weaker still. Post-fusion it is asked to improve on the
strongest list the engine produces, and it cannot. The lever is not where it
is applied; it is the quality of the signal being applied.

#### The bug this experiment produced, and what caught it

The first run of the sweep reported hybrid R@5 collapsing 0.770 → **0.058**.
That is not a bad result, it is a broken one, and the giveaway was the *shape*:
**blend 0.3 scored worse than blend 1.0.** Blend 0.3 should mostly preserve the
incoming order, so preserving it harder cannot be worse — unless the order
being preserved is upside down.

It was. `fuse` emits **higher-is-better** scores; `blend_head` consumes and
emits **lower-is-better** pseudo-distances. Pre-fusion this never mattered,
because `fuse` reads only rank *position* from the semantic list and ignores
its scores. Post-fusion, feeding one contract straight into the other inverts
the ranking. Fixed by converting at both ends (`indexed::rerank_fused`).

Two guards now exist that would have caught it immediately:
`post_fusion_rerank_at_zero_blend_is_the_identity` — at alpha 0 the rerank must
be a no-op, which is false the moment either conversion is dropped — and
`cold_and_warm_agree_under_post_fusion_reranking`.

`--maxsim-post` is kept, hidden and off. The question "would this work better
after fusion?" is a reasonable one that will be asked again; it is now one
command to answer instead of a re-implementation, and the answer is in this
table.

## 14. Semantic-first (2026-08-01)

### 14.1 The decision

**Semantic search is the product. The success criterion is semantic beating
lexical, on real queries, measured against the rg-oracle ceiling. Hybrid is off
by default until semantic carries its own weight; it returns when fusing it
back in is adding a strong signal to a stronger one, not hiding a weak one
behind BM25.** (Maintainer decision, recorded 2026-08-01.)

The default mode is now `semantic` — in the CLI (`--mode` unset), in
`SearchOptions::default()`, and in the exact-miss suggestion path, which used
to run a hidden hybrid query and now runs the default. `hybrid` remains
available as a mode flag, tuned exactly as §9.5 left it, and the harnesses
continue to report it: the fusion machinery is not being unbuilt, it is being
benched.

**What this costs today, stated up front rather than discovered later.** On
the only query set this project didn't write (§13.8, CoSQA, 1,200 real Bing
queries):

| mode | R@5 | MRR@10 |
|---|---|---|
| semantic (new default) | 0.083 | 0.048 |
| hybrid (old default) | 0.208 | 0.133 |
| bm25 (the bar) | 0.222 | 0.138 |

The default gets 2.5× worse on real queries, today. The reasoning for taking
that trade anyway:

1. **Hybrid was grading the project on a strength it didn't build.** §13.10
   measured that in hybrid, BM25 carries the fused result — 97% of queries come
   back unchanged when the semantic branch is reranked. Every published hybrid
   number was, to first order, a BM25 number. A default that looks healthy
   while its distinguishing component contributes 3% removes all pressure to
   fix that component.
2. **The thing only semantic can do is the thesis.** §13.9: a ripgrep allowed
   to read the answer before choosing its pattern scores exactly 0.000 on 199
   paraphrase queries; semantic scores 0.04. That 0.04 is the entire reason
   this project exists — and it is 0.04. The capability is real and tiny, and
   it stays tiny while the default hides it.
3. **The failure is understood and layered** (§9.8, §9.9): the prose tokenizer
   shreds identifiers, static vectors carry no context, and the space lacks
   code-concept relations. Layer 1 has a cheap, documented, never-implemented
   fix (§14.2). Layer 3 has a known fix path (re-distill from a code teacher,
   §9.9). Neither gets built while the fused default makes them look optional.

Falsifiable exit condition, so this decision can be graded rather than
re-argued: **semantic beats bm25 on CoSQA R@5** (currently 0.083 vs 0.222).
When that holds, re-measure hybrid on top of the stronger branch and decide
the default again with §9.5's sweep. If after the §14.2 campaign and a model
swap semantic still hasn't closed the gap, that is a finding about static
prose embeddings on code, and it goes here next to everything else.

Side effect worth naming: MaxSim reranking is on by default in semantic mode
(+0.080 R@5 on etcd, CI [+0.010, +0.155], §13.10), so the default search now
includes the rerank stage. Warm default latency moves from hybrid's ~115 ms to
semantic's ~53 ms plus the rerank head.

### 14.2 The hypothesis: the embedder is shown the wrong text

What the embedding stack actually sees today: `doc_text()` = the relative path,
a newline, and the raw chunk slice — operators, delimiters, decorators, string
noise and all — pushed through ese's BERT-style prose pipeline. §9.8 measured
what that does at the token level:

- `scalar_None` → `[scalar, _, none]`: snake_case shreds, and the highest-signal
  unit in the chunk never exists as a matchable token.
- Punctuation tokens are first-class: `_` matches `_` anywhere with cosine
  1.000, pure noise mass under mean pooling.
- camelCase, inconsistently, does *not* split: `computeBackoffDelay` stays one
  OOV-ish blob the wordpiece vocab fragments arbitrarily.
- The §9.8 fix path — "use semgrep's own code-aware BM25 tokenizer for the
  match units" — was documented for MaxSim and never applied to the pooled
  vectors that every semantic search actually scores.

The hypothesis (maintainer's, and §9.8's, independently): identifier words,
file-path words, and comment prose carry nearly all of a chunk's semantic
signal for a prose model; operators, values, decorators and syntax carry
little and *detract* under uniform mean pooling, because every token —
`{`, `_`, `->` — gets an equal share of the average. So: render the chunk into
the prose the model was trained on before embedding it. `get_user_name` becomes
`get user name`; punctuation contributes nothing; the query gets the identical
rendering so both sides live in the same space.

This is a layer-1 fix (§9.9's taxonomy). It cannot create code-concept
relations the space lacks — no rendering makes `mutex` near `lock` — so it
sharpens ese as the fuzzy lexical matcher it measurably is, and should move
direct/real-query scores, not the paraphrase wall.

No tree-sitter is required for this round: the code-aware tokenizer
(`text/token.rs`) already splits snake_case and camelCase and drops
punctuation, language-agnostically. A parser becomes worth its weight only for
*structural* weighting (identifier-vs-literal, signature-vs-body), which is a
follow-on lever, not a prerequisite — and §11's function-chunking result
(no benefit, and the instrument couldn't referee it) says structure bets need
better instruments before they need parsers.

The lever: `--embed-preproc <variant>` at index time, persisted in
`meta.json` exactly as `sif` is, applied identically to chunks at build, to
queries at search, and to the cold streaming path (cold == warm must survive
this, FIXES.md #11). BM25 and keyword are untouched — their tokenizer already
does this, which is part of why they win.

### 14.3 Pre-registration (written before the first run)

Conditions, per corpus — each is one index build:

| tag | render |
|---|---|
| `none` | today's raw `doc_text` (control) |
| `split` | code-aware tokens, subtokens only: `getUserName` → `get user name` |
| `split-whole` | subtokens + whole identifier: `… get user name getusername` |
| `split-nokw` | `split` minus language keywords and pure-number tokens |
| `split-sif` | `split` with `--sif` pooling (data-driven downweighting on top) |

Corpora and sets: CoSQA (primary, real queries), linux + vscode (direct and
paraphrase strata), tokio + etcd (the <2k-file band where §9.7 found variants
actually diverge). Mode under test: `semantic`. `bm25` is rerun once per corpus
as the bar and a tripwire — its pipeline is untouched, so any movement there
is a bug in the lever, not a result. Scoring: `eval/run_eval.py`, R@5 primary,
MRR@10 secondary, paired bootstrap CIs against `none`, sign tests.

Predictions, with the §13.4 convention that a miss is recorded as a miss:

1. **CoSQA semantic R@5: 0.083 → 0.11–0.16 for `split`.** Mechanism: mean
   pooling stops spending mass on punctuation pieces and shredded fragments.
   Below 0.10 the hypothesis is substantially wrong; above 0.16 I underrated
   surface noise.
2. **`split` does not reach bm25 (0.222) on CoSQA.** Layer 1 alone shouldn't
   close a 2.7× gap. If it does, §9.9's "the model is the bottleneck" needs
   revision, which would be the best possible outcome of this experiment.
3. **Kernel/vscode `direct` improves under `split`** — both sides now emit the
   same subtoken stream, strengthening exactly the fuzzy-lexical channel §9.9
   says semantic mode really is.
4. **Kernel `paraphrase` stays ≤ 0.08** (currently ~0.04). No rendering
   creates the missing relations. If paraphrase moves materially, layer 1 was
   a bigger share of the wall than three sections of forensics concluded.
5. **`split-nokw` ≥ `split` on CoSQA; `split-sif` ≈ best overall.** Keyword
   mass is noise under uniform pooling; SIF should learn most of what the
   stoplist hand-codes, making `nokw`'s edge mostly vanish under `sif`.
6. **bm25 identical to three decimals across conditions** (tripwire).

### 14.4 Results (2026-08-01, same day; eval/preproc.sh, 2,798 queries × 5–6 conditions)

Semantic mode as shipped (MaxSim on). Note the baseline correction first: §14.1
quoted CoSQA semantic at 0.083 from §13.8, but §13.8 predates MaxSim becoming
semantic mode's default — the shipped baseline this campaign measured is
**0.108**. Deltas below are against that, paired per query, 2,000-resample
bootstrap CIs, exact sign tests.

**CoSQA (1,200 real queries, the primary set):**

| condition | R@5 | Δ vs none | 95% CI | sign test |
|---|---|---|---|---|
| none | 0.108 | — | — | — |
| split | 0.116 | +0.007 | [−0.006, +0.022] | p=0.33 |
| split-whole | 0.110 | +0.002 | [−0.012, +0.016] | p=0.90 |
| split-nokw | 0.117 | +0.008 | [−0.006, +0.023] | p=0.29 |
| sif (control, added post-hoc) | 0.170 | +0.062 | [+0.043, +0.081] | 109w/35l, p≈0 |
| **split-sif** | **0.188** | **+0.080** | [+0.060, +0.099] | 133w/37l, p≈0 |
| bm25 (the bar) | 0.222 | | | |

MRR@10: 0.078 → 0.125 (split-sif, CI [+0.033, +0.060]). `split-sif` beats
`sif` alone by +0.018 (CI [+0.001, +0.037], p=0.045) — both components are
real, and they compose. **The shipped semantic mode now recovers 85% of
bm25's R@5 on real queries, from 49% at §13.8.** The gap is 1.18×, down
from 2.7×.

**The other corpora, split-sif vs none, semantic R@5:**

| corpus | lang/case | direct Δ | paraphrase Δ |
|---|---|---|---|
| vscode | TS, camelCase | 0.710 → 0.825 (+0.115, p≈0) | 0.030 → 0.090 (+0.060, p=0.012) |
| etcd | Go, camelCase | 0.420 → 0.595 (+0.175, p≈0) | −0.015 (n.s.) |
| tokio | Rust, snake_case | +0.015 (n.s.) | +0.005 (n.s.) |
| linux | C, snake_case | −0.005 (n.s.) | 0.010 → 0.035 (+0.025, 5w/0l, p=0.06) |

On vscode, `split` *alone* is +0.075 direct (p=0.006) and `split-whole`
+0.115 (p<1e-4); on CoSQA and the kernel, `split` alone is noise.

**The mechanism, resolved into two facets.** The failure was always "mean
pooling over a noisy token stream" (§9.8). Rendering fixes the *units* — and
pays exactly where ese's prose tokenizer couldn't already produce them:
camelCase corpora (TS +0.115, Go +0.175). It pays ~nothing where the
tokenizer already splits on `_` (Python, Rust, C). SIF fixes the *weights* —
and pays where the units were fine but boilerplate drowned them: +0.062 on
real Python queries. Each lever is null exactly where the other's problem
dominates, which is why no single-lever condition ever showed this and why
§9.4 — which never had a real-query set — benched SIF as a loser. CoSQA
didn't exist here until §13.8; the biggest single finding of this campaign is
that **SIF's 2026-07-28 rejection was an artifact of synthetic queries**.

**Prediction scorecard (§14.3):**

1. CoSQA split → [0.11, 0.16]: lands 0.116, inside the band — but the band's
   premise (baseline 0.083) was wrong, and against the true 0.108 baseline
   the delta is noise. Scored as a **miss**: the mechanism (punctuation-noise
   removal) barely matters on a snake_case corpus.
2. split doesn't reach bm25 on CoSQA: **hit** (even split-sif at 0.188 < 0.222).
3. direct improves under split: **half-hit** — decisively on camelCase
   corpora, null on snake_case ones. The prediction failed to condition on
   what the corpus's identifier convention leaves for the renderer to do.
4. Kernel paraphrase ≤ 0.08: **hit** (0.035 at best). And a detail worth its
   own sentence: at 0.035, semantic-with-split-sif now *ties bm25* on kernel
   paraphrase — the wall holds, but semantic no longer trails lexical behind it.
5. split-nokw ≥ split (0.117 vs 0.116, both n.s.); split-sif best overall:
   **hit**, but for the wrong reason — SIF didn't merely subsume the
   stoplist, it carried the condition.
6. bm25 unmoved: **miss as stated.** CoSQA bm25 read 0.219 under `split`
   (Δ −0.003, CI [−0.013, +0.007], n.s.). The lever cannot touch BM25
   *scoring*, but bm25-mode output passes through MMR diversification, which
   reads the (now rendered) embedding matrix. A tripwire that fires on a
   coupling you forgot is doing its job; the coupling is real, the magnitude
   is noise.

### 14.5 What graduates, and what gates it

`--embed-preproc split --sif` is the recommended index configuration for the
semantic-first campaign, and the numbers above are the §14.1 scoreboard's
first movement (CoSQA 0.108 → 0.188 against bm25's 0.222). It is **not**
being made the default build in this commit: offline gains have failed to
transfer to agent outcomes twice (§9.7, §10.6), and the standing rule is that
engine defaults move on agent-level evidence. The gate, in order of cost:

1. **Query replay** (§13.2, free): rerun the logged agent argv through a
   split-sif index vs default. If the gain shows on real agent queries, that
   is §11.5's recommended instrument saying yes.
2. If replay agrees, flip the default build and re-record the snapshot in
   that commit; the cache generation mechanism retires old entries.

Next levers, in leverage order: re-test the §10 code table *on top of*
split-sif (the table fixed the space, this fixed the stream — the two
failures were independent, so the fixes should stack); then sif-center and
`--sif-a` retuning on CoSQA (both were tuned on synthetic sets). Tree-sitter
remains unneeded: everything above is tokenizer-level. A parser earns its
place only for structural weighting (signature vs body, identifier vs
literal), and §11's lesson stands — that bet needs the replay instrument
first, not a grammar.

### 14.6 R@10, and MaxSim on top of the rendered stream (2026-08-01, follow-up)

Two questions asked after §14.4 landed: does the picture hold at deeper k,
and does MaxSim reranking still earn its place once the stream it matches
over is rendered? (MaxSim over *raw* text was §9.8's autopsy — its match
units were exactly the shredded fragments. Under `split-sif` its units are
code-aware subtokens, SIF-weighted on both sides, so its old failure mode is
gone in principle. Measured:)

**R@10, semantic, split-sif vs none:** CoSQA 0.173 → **0.286** (+0.112,
CI [+0.089, +0.135]) against bm25's 0.325 — 88% of the bar at k=10, same
shape as k=5. vscode direct +0.110, etcd direct +0.155 (both p≈0); the
snake_case corpora stay flat, as at R@5.

**MaxSim × preproc factorial** (semantic, `--no-maxsim` as the off cell;
paired within each index):

| | maxsim off | on | Δ (CI) |
|---|---|---|---|
| CoSQA, none, R@5 | 0.083 | 0.108 | +0.026 [+0.006, +0.046] |
| CoSQA, split-sif, R@5 | 0.148 | 0.188 | **+0.040** [+0.015, +0.063] |
| CoSQA, split-sif, R@10 | 0.229 | 0.286 | +0.057 [+0.031, +0.082] |
| vscode direct, none, R@5 | 0.560 | 0.710 | +0.150 [+0.095, +0.210] |
| vscode direct, split-sif, R@5 | 0.615 | 0.825 | **+0.210** [+0.150, +0.275] |

The three levers stack, and MaxSim's contribution *grows* under the rendered
index — consistent with §9.8's diagnosis that its old ceiling was the token
units, not the mechanism. A provenance detail worth keeping: the maxsim-off
none cell reads 0.083, three decimals equal to §13.8's published semantic
number — that row was, as suspected in §14.4, the maxsim-off configuration.

On separators, asked and pinned: hyphens and underscores never survive any
`split` variant — kebab-case splits like snake_case does, and no separator
character reaches ese (`kebab_and_snake_separators_are_removed_not_kept` in
`text/prose.rs`). Only the `none` baseline still shows the model punctuation.

**And a first oracle number for vscode** (2026-08-01, `oracle-vscode.json`:
rg/rg-strong/rg-oracle/hybrid rerun on the 200 `direct` queries; rg 0.155,
rg-strong 0.360, hybrid 0.870 all reproduce their published values exactly):
**rg-oracle direct R@5 = 0.540** (R@10 0.635). That slots between the kernel's
0.462 and CoSQA's 0.101×-shaped story and says something § 13.9 couldn't:
on camelCase identifier queries, even a ripgrep that reads the answer stops at
0.540 — below the *old* semantic mode (0.710), let alone the rendered index
(0.825). The §13.9 explanation transfers: choosing the right token is not the
hard part; ranking the hundreds of files that contain it is.

### 14.7 SIF vs idf weighting for the pooled vector (pre-registered 2026-08-01)

SIF weights a token by a/(a + p(w)) over *collection* frequency; BM25's idf
weights by log-scaled *document* frequency. Both say "common tokens carry
less" — with different shapes: SIF is hyperbolic (crushes stopwords hard,
saturates at 1.0 for everything rare, so `blkg` and `backoff` weigh the same),
idf is logarithmic (gentler on common terms, still discriminating among rare
ones). Since SIF turned out to be the biggest single lever on real queries
(§14.4), the natural control is: was that *SIF's weighting shape*, or just
*having any* frequency-based weighting? `--sif-idf` swaps the pooling weight
to ln((n − df + ½)/(df + ½) + 1) over per-file document frequency, everything
else identical — same rendered stream, same stats file, same query-side
pooling, MaxSim token weights included.

Conditions: `split-idf` vs the standing `split-sif` and `none`, semantic mode,
CoSQA + vscode. Predictions, before the first run:

1. **idf ≈ sif on CoSQA: |ΔR@5| ≤ 0.02, CI straddling 0.** After Σw
   normalization, pooling should care *that* boilerplate is downweighted, not
   about the precise curve doing it.
2. **Both beat `none` decisively** (replicating §14.4's +0.080 shape).
3. Weak directional guess, low confidence: if they separate, sif wins on
   CoSQA — stopword-heavy real prose rewards the harder crush of common
   tokens — and idf wins nowhere clearly.

**Result (same day): prediction 1 holds — the curves are interchangeable.**
Paired, `split-idf` vs `split-sif`:

| | Δ R@5 | 95% CI | sign test |
|---|---|---|---|
| CoSQA | −0.015 | [−0.029, +0.001] | 32w/50l, p=0.060 |
| CoSQA R@10 | +0.000 | [−0.016, +0.017] | 53w/53l, p=1.0 |
| vscode direct | +0.020 | [−0.010, +0.050] | p=0.29 |
| vscode paraphrase | +0.015 | [−0.015, +0.045] | p=0.51 |

And vs `none`, idf replicates SIF's whole gain: CoSQA +0.065 R@5 / +0.112
R@10 (both p≈0), vscode direct +0.135, paraphrase +0.075. So §14.4's biggest
lever was **having frequency-based term weighting at all**, not the SIF
functional form — a/(a+p) over collection frequency and BM25's log-df curve
land within each other's noise everywhere, with one borderline cell (CoSQA
R@5, p=0.060) leaning sif, exactly the low-confidence direction guessed in
prediction 3. Practical consequence: `--sif` stays the canonical spelling and
`--sif-idf` stays a control lever; nothing graduates from this experiment,
but the §14.4 mechanism story sharpens — the embedder didn't need BM25's
curve, it needed BM25's *idea*.

## 15. Blind search (2026-08-01): the reorientation

### 15.1 The decision

**The primary evaluation regime becomes *blind search*: queries verifiably
free of the gold's identifiers, simulating a search agent with zero prior
knowledge of the codebase's naming. Everything measured to date — every
`direct` set, CoSQA whole, the §14 scoreboard — is retained unchanged as the
*named-identifier regression board*: it may not collapse, but it no longer
defines success.** (Maintainer decision, recorded 2026-08-01.)

Why. §14's own root-cause work showed the sets mostly name things: 66–70% of
`direct` queries contain the gold identifier verbatim (§12), real CoSQA
queries share 42% of their vocabulary with the gold (§12.3), and 47% of real
agent queries carry an identifier (§13.3). On that distribution exact
matching plus idf is close to the optimal decision rule — lexical search is
being graded on its home field, and semantic search's one structural
advantage (crossing vocabulary) never comes into play. Meanwhile the two
cells that *are* vocabulary-crossing are the most interesting numbers in the
record: rg-oracle scores exactly 0.000 on kernel paraphrase while semantic
scores 0.035 — now *tied with bm25* (§14.4) — and on identifier-free CoSQA
the §13.2 verdict flips corpus-by-corpus. The capability this project exists
for lives in the blind regime, and the harness barely measures it.

The design borrows two ideas from CORE-Bench (arXiv 2409.11363, the
computational-reproducibility agent benchmark): **graded information
removal** — the same task at difficulty levels that strip context, rather
than a binary easy/hard split — and **hard verifiable gates**. The second
matters because today's `paraphrase` is only an *instruction* to the
generator, not a property of the output: measured on the sets on disk, 1–5%
of paraphrase rows still name the gold symbol verbatim, invisible to
`identifier_pct` because `leakage.identifiers()` deliberately does not count
single lowercase tokens (`flush`, `spawn`). A blind set must be blind by
construction and refused by the scorer when it is not.

### 15.2 The blindness ladder

Every level shares the same symbol-anchored gold span — one target, graded
context removal:

| level | kind | the query may contain | status |
|---|---|---|---|
| L0 | `direct` | anything, incl. the gold identifier | exists |
| L1 | `paraphrase` | shared vocabulary; identifier avoidance advisory only | exists |
| **L2** | `blind` (4–8 words), `blind_long` (12–20) | zero gold-identifier tokens — incl. lowercase symbol names and rare symbol subtokens — overlap-capped, structurally gated | **new, primary** |
| L3 | `symptom` | observable behavior only | deferred until the Loc-Bench blind screen shows the stratum matters |

Real-data anchors: the zero-gold-hit subset of CoSQA ≈ real L2; Loc-Bench
instances whose issue text names no gold identifier ≈ real L3; the ~53% of
replay-agent queries without identifiers ≈ agent-length L2.

### 15.3 The strict-blind predicate

`identifiers()` is frozen (its definition is baked into every recorded
`identifier_pct`). Blindness is decided by a new **gold-aware** predicate,
`gold_identifier_hits(query_tokens, gold_text, symbol)`: a query token t
(lowercased) is a hit if

- (a) it equals a snake_case/camelCase identifier token *of the gold span*; or
- (b) it equals the gold's own `symbol` name — the clause `identifiers()`
  cannot express — or matches it under light suffix stemming
  (ing/ed/es/s/er); or
- (c) it equals a subtoken of the symbol (split on `_`/camel) with guards:
  length ≥ 4, not a stopword, and not used as an ordinary word by the gold's
  own comments/docstrings ("plain prose" means what a comment *says* — a bare
  variable named `rwstat` is code, not prose) — `rwstat` is caught, a
  comment's `read` passes. (Prose definition refined at implementation time,
  before any run.)

`is_blind(row)` = zero hits AND per-row `gold_token_overlap` ≤ **0.5**.
Set-level gate: mean overlap over blind rows ≤ **0.25**. Both caps are
provisional until the §15.5 calibration on existing distributions (paraphrase
≈ 10–11.5%, real humans 42%), then frozen; the calibration is part of Phase 0
and happens before any blind set is scored.

### 15.4 The new success criterion

Two boards. **Blind (primary):** semantic beats bm25 on strict-blind cells —
that is the §14.1 exit condition, re-aimed at the regime the tool exists for.
**Named-identifier (regression):** every existing set keeps being run; the
§14 numbers are the floor. A change that wins blind by collapsing named does
not ship.

### 15.5 Pre-registration (written before the first Phase-0 re-cut run)

The Phase-0 re-cuts of *existing* results into blind/named strata count as
first runs. Predictions:

1. **On strict-blind generated cells, semantic (split-sif + maxsim) beats
   bm25: ΔR@5 ≥ +0.03 with CI excluding 0 on ≥3 of 6 corpora.** This is the
   reorientation's load-bearing bet — if bm25 wins even here, the §9.9
   model-swap becomes the only move left.
2. **rg-strong ≤ 0.05 R@5 on blind cells; rg-oracle collapses toward its
   kernel-paraphrase 0.000.** Blindness by construction removes what grep
   greps for.
3. **CoSQA blind re-cut: bm25's advantage shrinks or flips sign on the blind
   stratum, and widens on the named complement.**
4. **`blind_long` ≥ `blind` for semantic** (more signal to pool), **≈ for
   bm25** (length adds few new exact matches).
5. **Blind-screened Loc-Bench instances show a larger semgrep-vs-grep gap
   than the identifier-bearing complement.**

### 15.6 Phase 0: the re-cut of what was already measured (same day)

`eval/blind_cut.py` re-aggregates existing result files by the §15.3
predicate — zero scan cost, the first look at the blind regime from data
already paid for.

**CoSQA splits 847 blind / 353 named.** Champion semantic (split-sif+maxsim)
vs bm25, paired within stratum:

| stratum | n | semantic R@5 | bm25 R@5 | Δ | 95% CI |
|---|---|---|---|---|---|
| **blind** | 847 | 0.148 | 0.169 | −0.021 | [−0.045, **+0.004**] |
| named | 353 | 0.286 | 0.348 | −0.062 | [−0.110, −0.011] |

**On the real blind stratum, semantic search and bm25 are already
statistically indistinguishable** (MRR Δ −0.004, CI [−0.020, +0.010]); the
entire surviving lexical advantage is concentrated in the named 29%.
Prediction 3: direction confirmed — the gap shrinks to noise on blind, stays
decisive on named. It has not flipped sign yet; that remains the campaign's
goal. Under the raw pre-§14 index the blind gap was −0.081 — the §14 levers
closed three quarters of the *blind* gap while barely denting the named one,
which is exactly what "the levers fixed the fuzzy-lexical channel" predicts.

**The advisory paraphrase instruction leaks worse than §15.1's 1–5% verbatim
figure**: on etcd, 41/200 paraphrase rows (20%) fail strict-blind once
subtokens and the overlap cap count. (And 26/200 *direct* rows pass it —
"direct" doesn't always name.) The gate is not pedantry; a fifth of the
stratum the §13 record calls vocabulary-crossing isn't.

**Caps frozen after calibration.** Zero-hit real CoSQA queries have overlap
p50 0.33 / p90 0.60; the 0.5 per-row cap excludes 15.3% of them (the
near-verbatim tail) — strict but livable, kept. Set-mean 0.25 applies to
generated blind sets only; real-data strata use the row predicate alone.

### 15.7 Phase 2: the real-world blind strata (same day)

`eval/locbench/blind_screen.py` (output regenerable; `eval/data` is
gitignored) screens by tier — *named* (a gold function name or file stem
verbatim), *partial* (only subtokens; no gold-text prose guard exists here,
so common verbs land in this bucket and it is reported, never folded), and
*blind*.

**Real bug reports mostly name things: 348/560 Loc-Bench issues (62%) are
named, 144 (26%) partial, and only 68 (12%) truly blind.** The blind regime
is the minority of real agent work — worth stating against the reorientation
before leaning into it. The counter-fact from the same screen: **65% of
replayed agent *queries* are blind** (324/497). Agents paraphrase and probe
even when the issue names the target — the tool sees far blinder input than
the issue would suggest.

Replayed agent queries by tier (ranks recorded pre-§14, i.e. the *old*
semantic config; MRR, instance-clustered CIs):

| tier | n | bm25 | hybrid | semantic | hybrid−bm25 CI |
|---|---|---|---|---|---|
| named | 108 | 0.463 | 0.505 | 0.445 | [−0.035, +0.116] |
| partial | 65 | 0.441 | 0.468 | 0.322 | [−0.012, +0.097] |
| blind | 324 | 0.264 | 0.293 | 0.256 | [−0.006, +0.068] |

Everything gets harder blind (as it should — less to hold on to), the fused
engine leads bm25 in every tier without clearing the clustered CI at this n,
and old-semantic trails. Re-running replay against a split-sif index is the
cheap next measurement once the campaign lands.

And prediction 5's first, anecdote-grade reading, from re-stratifying the
§7.1 pilot A/B by instance tier: on the **6 blind instances**, semgrep found
the gold file 6/6 vs ripgrep's 4/6; on the 27 **named** instances ripgrep is
27/27 — the issue names the file, grep's perfect regime. Direction as
predicted, n far too small to score; the 560-instance screen is the sampling
frame for a targeted run when one is worth buying.

### 15.8 The first blind campaign: the scorecard (2026-08-02)

Six `<corpus>-blind.jsonl` sets, 4,168 queries, every blind row verified at
generation and again by the gate (blind cells: `gold_id% = 0.0` everywhere,
overlap 0.03–0.11, median 7–8 words). `eval/blind.sh`, per §15.5's registered
conditions. Blind R@5:

| corpus | rg-strong | rg-oracle | bm25 | hybrid | semantic | champion | Δ(champ−bm25), CI |
|---|---|---|---|---|---|---|---|
| tokio | 0.005 | 0.010 | 0.020 | 0.015 | 0.020 | 0.040 | +0.020 [−0.010, +0.050] |
| etcd | 0.000 | 0.012 | 0.012 | 0.012 | 0.012 | 0.006 | −0.006 [−0.023, +0.012] |
| commons-lang | 0.015 | 0.025 | 0.035 | 0.045 | 0.055 | 0.060 | +0.025 [+0.000, +0.055] |
| jekyll | 0.000 | 0.000 | 0.014 | 0.027 | 0.027 | 0.014 | ±0.000 |
| vscode | 0.005 | 0.015 | 0.035 | 0.030 | 0.030 | 0.025 | −0.010 [−0.035, +0.010] |
| linux | 0.000 | *(not run — stopped)* | 0.020 | 0.015 | 0.010 | 0.010 | −0.010 [−0.030, +0.010] |

**Prediction 1: MISS, 0/6.** Pooled over 1,042 blind rows: semantic 0.028 vs
bm25 0.024, Δ +0.004, CI [−0.007, +0.014]. On strictly-blind generated
queries **nobody can retrieve** — every engine sits at 1–6% R@5 — and the
registered consequence applies as written: *the §9.9 model swap is the only
move left* in this regime. A prose-space embedder severed from vocabulary
overlap loses its fuzzy-lexical channel exactly as grep loses its exact one.

**Prediction 2: HIT, decisively.** rg-strong ≤ 0.015 everywhere (band was
≤ 0.05); the oracle ≤ 0.025 everywhere it ran. Blindness by construction
removes what grep greps for — including an oracle-grade grep.

**Prediction 4: MISS, inverted.** `blind_long` does nothing for semantic
(Δ ±0.000) and significantly helps **bm25** (+0.015, CI [+0.002, +0.029]).
More words buy the exact matcher more lottery tickets for accidental
overlap; the pooled vector gains nothing. The reasoning behind the
prediction ("more signal to pool") was wrong about which engine is
starved for tokens.

**The synthesis, and it is the §15 finding that matters.** The blind regime
split in two under measurement:

- **Real-blind** (CoSQA's 847 zero-gold-hit human queries, overlap ≈ 0.29):
  champion semantic already at **parity** with bm25 at useful absolute levels
  (0.148 vs 0.169, CI spanning zero — §15.6). This is where users and agents
  actually live (§15.7: 65% of agent queries), the fight is winnable, and
  the §14 levers already won most of it.
- **Strict-blind** (generated, overlap ≈ 0.07 — half to a quarter of what
  real blind humans emit): a **floor for every engine**, the §13.9 paraphrase
  wall measured a third way. Distinguishing engines here is pointless until
  the embedding space knows code relations; these sets are the *instrument*
  waiting for the model experiment, not a battleground for the current stack.

Direct anchors confirm the sets are sound (bm25 0.77–0.98 when the query
names the gold). The campaign's operational conclusion: quote real-blind for
product claims, hold strict-blind as the gate the §9.9 code-teacher
re-distillation must move — it is the experiment these six sets were built
to referee, and nothing else on the §9 lever list can touch them.

### 15.9 Why the blind misses miss: forensics (2026-08-02)

`examples/why_miss.rs` (the §9.8 method, aimed at real campaign rows):
pooled cosines under raw/split/split-sif, per-query-token attribution with
SIF weights, and where each chunk's pooled mass sits. Four scenarios traced;
three failure mechanisms and one success mechanism fell out, each with
receipts.

**A — the rare words have no relations** (§9.9, confirmed on live misses).
The query's *distinctive* words — exactly the ones SIF trusts — find nothing:
`scheduled→future` 0.198, `skip→hidden` ≈0.08 (jekyll `hidden_in_the_future`,
gold cosine 0.132, rank 39); `backtrace→return` 0.312 (commons-lang
`getStackFrameList`); `offload→static` 0.229, `synchronous→async` 0.197
(tokio). The only strong gold link in the publisher miss is surface
morphology: `publication→publisher` 0.689.

**B — SIF inverts on blind queries.** The exact matches a blind query *does*
get are its domain-common words, and SIF crushes them by design:
`exception` matches the gold at **1.000** but carries weight **0.10** in a
commons-lang corpus; `posts` w=0.23 in a blog engine; `thread` w=0.19 in
tokio. Result, measured on `getStackFrameList`: gold cosine **0.325 raw →
0.111 under sif** — base semantic ranked it #1, champion dropped it past 40.
The campaign table agrees: champion ≤ base on blind cells in 3 of 6 corpora.
SIF's win on named/real queries (§14.4) is a property of *queries that
contain rare tokens*; strict-blind queries are constructed not to.

**C — prose crowds out code.** Both jekyll misses rank markdown docs, test
prose, and release notes on top: the winning chunk for "skip posts scheduled
for later publication" is a release-notes file matching `posts` 1.000 /
`later` 1.000 / `skip→skipping` 0.542. A prose model retrieves prose; in a
mixed corpus the code gold — whose vocabulary is identifiers — cannot outbid
documents that literally say the query's words.

**D — and the hits are the same mechanism pointed the right way.** Every
traced blind hit rides a *corpus-rare prose word inside the gold's own
comments*: `spawn_blocking` wins rank 1 because its doc example says "Stand
in for complex computation" and `computation` (w≈0.96 both sides) matches
1.000. The semantic channel that works on blind queries is **comment prose**,
not code.

Levers this surfaces, in order of directness: (1) re-test blind cells with
SIF off or query-side-asymmetric weighting — B says the champion config is
mis-tuned for exactly the primary regime; (2) boost comment/doc lines in the
embedded rendering (D says they carry the whole working channel — the
structural-weighting case, §14.2's deferred tree-sitter bet, now with a
mechanism); (3) the §9.9 code-teacher swap remains the only fix for A, which
is the binding constraint everywhere else.

### 15.10 Closing note: blind search is an instrument, not the product regime

Recorded 2026-08-02, maintainer decision. §15.8's own synthesis settles the
orientation question the section opened: strict-blind queries model a user
*problem statement*, but the product's user is a **coding agent** that
interprets the problem and emits vocabulary *guesses* — and 47% of real agent
queries carry an identifier (§13.3), often a wrong one. Strict-blind is
therefore re-labeled the **model-experiment instrument**: the gate the §9.9
code-teacher re-distillation must move, refereed by the six gated sets, and
not a regime any query-time work should chase. Nothing is deleted — the
sets, the §15.3 predicate, the gates, `blind.sh` and `blind_cut.py` all
stand. The primary regime becomes **agentic-guess search**, defined and
pre-registered in §16. The boards become three: guess (primary), blind
(model-experiment instrument), named-identifier (regression).

## 16. Agentic-guess search (2026-08-02): the orientation

### 16.1 The thesis and the data

The product hypothesis, stated as something falsifiable: **a coding agent
interprets a user request and guesses vocabulary; ranked search should make
those guesses land faster than exact-matching the same guesses.** The agent's
guess is the query distribution that matters — not the user's problem
statement (blind, §15.10) and not our generated paraphrases (§13.3).

The raw material already exists: the locbench shim logs hold **2,739 real
search invocations** — 609 ranked semgrep queries + 163 `search`, 1,397
`semgrep -e` exact patterns, and 570 rg calls (430 distinct patterns) that
replay deliberately excluded (§13.2). The exact and rg strata are the purest
guesses on record: alternation ladders of candidate spellings
(`writeParquet\|save_parquet\|to_parquet`) — an agent literally enumerating
its guess distribution for one intent.

And one mechanical discovery sharpens the whole program: ripgrep's regex
engine treats `\|` as a **literal pipe**, not alternation. Agents habitually
type BRE-style `\|` ladders; every such search was dead on arrival, matching
a literal `|` that occurs nowhere. The share is measured at harvest time
(prediction 5) from logged exit codes, before any replay.

### 16.2 The success criterion

Over the checked-in guess corpora (`guesses-v0.jsonl` harvested from
existing logs; `guesses-agent.jsonl` from new capture runs): **one ranked
query built from the agent's own guess must land a gold file in the top 5
more often than the agent's actual exact-mode workflow did — instance-
clustered CI excluding zero — and hybrid must not trail bm25 on the same
corpus.** Named-identifier sets remain the regression floor (§14); strict-
blind remains the model-experiment gate (§15.10) and moves only with a
model swap.

### 16.3 Method

`harvest.py` exports every invocation losslessly (pattern, flags, scopes,
frequency, order, condition — the §7.3 description-bias provenance);
`ladder.py` decomposes alternation ladders into guess-groups with two
translations (T1 = space-joined rung literals, casing preserved; T2 =
pre-split control); `guessplay.py` replays three arms per guess-group
against the instance's gold with `replay.py`'s clustered statistics:
(a) the agent's actual exact pattern (verbatim, plus `|`-normalized for
dead ladders, reported separately), (b) the ranked translation under
{bm25, semantic, hybrid} × {shipped default, §14 champion} — §15.9-B says
SIF is mis-tuned for token-poor queries, so both configs are measured —
and (c) the agents' real ranked queries re-scored under the same build.
Original scopes are primary (65% of agent calls are scoped); repo-root is
the sensitivity cut.

### 16.4 Pre-registration (written before the first harvest or replay)

1. **Hybrid-T1 beats the actual exact arm on hit@5** over all exact+rg
   guess-groups: Δ ≥ +0.05, clustered CI excluding 0.
2. **The advantage is rescue, not replacement**: rescue rate ≥ 20% (ranked
   top-10 hits among groups whose exact replay found no gold), and
   parity-or-worse where the exact arm already hits at rank 1.
3. **Hybrid ≥ bm25 on the guess corpus** (MRR delta positive; §15.7's
   direction, now at larger n under the current engine).
4. **T1 ≥ T2** for semantic/hybrid — the engine's tokenizer already splits
   identifiers; pre-splitting destroys casing signal.
5. **Dead ladders are real and rescuable**: ≥ 10% of `-e` ladder invocations
   used `\|`; the ranked translation rescues that stratum at the highest
   rate of any stratum.
6. **Exact hit@5 falls with ladder length; ranked-translation hit@5 is
   flat-to-rising in it** — a long ladder is the agent signaling it doesn't
   know the name, exactly when guess-tolerant search should win.
7. **Scope robustness**: the directions of 1–3 are unchanged between
   original-scope and repo-root replays.

### 16.5 Results (2026-08-02, same day: 2,113 guess-groups, 33,394 arm-rows)

`guessplay.py` over the full corpus, instance-clustered CIs throughout.

**P1 — significant, but smaller than registered.** One ranked hybrid query
built from the agent's own guess vs the agent's actual exact workflow, hit@5
over all 2,113 exact+rg guess-groups: **Δ +0.034, CI [+0.002, +0.071]**. The
CI excludes zero — the product effect is real — but the point estimate is
below the registered +0.05. Scored as a miss on magnitude, a hit on
direction and significance.

**P2 — miss, and the honest headline.** Rescue rate is **6.3%** (107 of
1,697 groups whose exact replay found nothing), a third of the registered
≥20%. Most wrong guesses are wrong enough that no engine rescues them.
And where the exact guess already hit rank 1 (n=232), the ranked
translation degrades it 47% of the time — replacement has real costs. The
product story this supports is narrower than the one registered: ranked
search is a better *default posture* for guessing (P1), not a reliable
safety net under any guess (P2).

**P3 — trending, not clearing:** hybrid−bm25 on the agents' own 624 ranked
queries, MRR **+0.019, CI [−0.004, +0.044]** — the §13.2 shape again,
tightened but still astride zero.

**P4 — flat** (Δ −0.010, n.s.): T1 ≈ T2; the pre-split control neither
helps nor hurts. The registered reasoning (casing signal) mattered less
than assumed.

**P5 — hit, and the campaign's most quotable mechanical fact: 19.6% of
multi-guess `-e` ladders (104/530) were dead on arrival** — BRE-style `\|`
that ripgrep's engine reads as a literal pipe. One in five of the agent's
multi-spelling exact searches never could have matched anything. The ranked
translation rescues that stratum at 12.5%, double the overall rescue rate
but below the "highest of any stratum" bet as worded.

**P6 — hit, cleanly, and the mechanism the orientation predicts.** By
ladder length, exact hit@5 falls 0.172 → 0.105 → 0.084 (1 / 2–3 / 4+ rungs)
while ranked-translation holds 0.202 → 0.148 → 0.137: **the gap widens
monotonically with how hard the agent is guessing** (+0.030 → +0.043 →
+0.053). A long ladder is the agent saying it doesn't know the name; that
is exactly where ranked search pays.

**P7 — hit:** root-scope replay agrees in direction (Δ +0.080,
CI [−0.001, +0.165]).

**Champion config: no.** The §14 split-sif index does nothing for guesses
(t1-semantic champion−default Δ −0.008, n.s.) — §15.9-B's diagnosis
(frequency weighting hurts token-poor queries) extends to the guess regime;
the shipped default is the right config for this board.

Scorecard: 3 hits (P5, P6, P7), 2 misses (P2, P4), 2 partials (P1
significant-but-small, P3 positive-but-wide). The §16.2 criterion is **not
yet met** on magnitude — the ranked-default advantage is +3.4pp, real but
modest, concentrated precisely where agents guess hardest. The next lever
is not query-time: it is making ranked mode the agent's default posture
(§7.3's framing lever is worth 3.5× more ranked usage) plus the §9.9 model
swap for the semantic branch — both now refereed by this board.

### 16.6 The capture runs: description gravity, measured clean (2026-08-02)

70 sonnet runs, 35 instances × {cap-ranked, cap-two}, `--no-score`, the
exact tool line persisted per run dir (the C2 provenance fix). The haiku
driver-diversity batch was stopped before running — noted, not replaced.
`guesses-agent.jsonl`: 359 fresh invocations.

**The starkest interface-gravity number in the record: a single
mechanics-only sentence documenting `-e` collapses ranked usage from 72%
to 7%** (cap-ranked: 28% of calls used the undocumented-but-working `-e`
anyway — pretraining habit; cap-two: 93% exact). §7.3 measured framing
*advice* worth 3.5×; this measures that merely *mentioning* the exact mode
— no advice at all — is worth ~10×. For the §16.5 conclusion ("the lever
is the agent's default posture") this is the dose-response curve.

Second finding: **median guess length is one word under both
descriptions.** Agents guess *names*, not phrases, no matter how the tool
is framed. The guess corpus is not a style artifact of v1–v4's ranked
framing; it is what agents do. (And the cap-two logs show live wrong-guess
behavior mid-ladder: `Authority(` retried as `Authority\(` — an agent
debugging its own regex escaping instead of finding code.)

