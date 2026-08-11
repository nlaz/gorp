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

**Verdict (2026-08-03): the config does not graduate. It stays opt-in.**

Three things were wrong with the gate as written, and the answer came out
negative anyway.

*The gate was not runnable.* `eval/locbench/replay.py` rejects `--sif` by
design, and the design is the point: it builds one index per worktree and
distinguishes conditions by query-time flags, so it can never compare two
index *builds*. Step 1 asked for something that file's architecture forbids.
Worse, `--embed-preproc` was missing from its `INDEX_FLAGS` guard, so a
`split` condition would have passed the check and then done nothing at all —
the warm path renders queries with the index's own stored setting and ignores
the flag — reporting parity from a condition that never ran. Both are now
fixed; the guard names `--embed-preproc` and the error points at guessplay.

*The instrument the gate meant had already answered.* `guessplay.py` does
reindex per config, and §16.5 ran it: champion − default = **−0.008, n.s.**
on the agent-guess board. That run predates the §16.11 fix and so had 53% of
its ranked rows forced to zero, which dilutes a difference rather than
biasing it — so the null was rechecked on the bug-free rows before being
leaned on here. It holds, and tightens: **+0.002, CI [−0.006, +0.009]**, with
semantic exactly 90 wins to 90 losses (§17.2). §15.8 corroborates (champion ≤ base in 3 of 6
corpora), and §15.9-B gives the mechanism — gold cosine **0.325 raw → 0.111
under SIF**, dropping a #1 hit past rank 40. Frequency weighting inverts on
token-poor, identifier-heavy queries, which §13.3 measured as exactly the
agent regime. The offline gain is real and the transfer failure is
predictable, which makes this the third instance of the §9.7/§10.6 pattern
rather than a surprise.

*And step 2's premise was false.* "The cache generation mechanism retires old
entries" — it does not. `compat::compat_key` covers format version, embed
dim, and the compiled table fingerprint; `cache::discover` filters on root
and chunk params. Neither carries `sif` or `embed_preproc`. Flipping the
default would have left every existing entry serving the old space
indefinitely, internally consistent and therefore invisible. Shipping it
would also have broken cold == warm outright: `search/stream.rs` has no SIF
pass, so the cold path cannot produce vectors in a SIF index's space.

The standing rule held. What it cost was that nobody checked the gate was
executable before writing it down — a gate that cannot run is
indistinguishable from a gate that passed.

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

### 16.7 The description experiment (pre-registered 2026-08-02, before the runs)

§16.5 said the big lever is the agent's default posture; §16.6 measured the
dose curve. This experiment closes the loop: candidate descriptions built
*from* the findings, run as scored A/B conditions against a fresh ripgrep
baseline — semantic-default semgrep (the §14 flip means ranked mode IS
semantic now) vs ripgrep-only, same instances, same day, same model.

Conditions (30 stratified instances × 4, sonnet, scoring on):

- **rg** — ripgrep only, the baseline, rerun fresh for a paired same-day read.
- **desc-v4** — the §7.3 winner (identity framing, `-e` as escape hatch).
- **desc-v5** — ranked identity with **no `-e` mention at all**: §16.6's
  72%-ranked posture, now scored.
- **desc-v6** — **guess-framed**: v5 plus "if you're torn between several
  possible names, put ALL your candidates in one query" — P6
  operationalized as prompt text.

Predictions:

1. **Ranked share: v5 and v6 ≥ 60%, v4 ≈ 30–40%** (the §16.6 curve
   reproduced under scoring conditions).
2. **v6 produces multi-name ranked queries** (mean ranked-query word count
   > v5's), i.e. agents fold their ladders into one query when told they may.
3. **Function-level accuracy orders with ranked share** (the §7.1
   correlational finding, now interventional): v5/v6 ≥ v4 ≥ rg on fnAcc.
4. **semgrep conditions ≥ rg on fnAcc** (§7.1's +11pp, retested under the
   semantic default).

Power caveat, stated before results: at n=30/condition, Loc-Bench file
accuracy resolves only large deltas (§11.5: 80–87% of instances carry no
engine signal). The primary read is **behavior** (ranked share, query
shape — effects measured at 10×), accuracy is directional.

### 16.8 Results (same day; 111/120 runs completed before an external stop)

27 instances present under all four conditions; behavior over every
completed run.

**Behavior — prediction 1 HIT, decisively:**

| condition | ranked | exact | ranked share | median words |
|---|---|---|---|---|
| rg | 0 | 106 (rg) | 0% | — |
| desc-v4 (identity + `-e` hatch) | 22 | 47 | **32%** | 4 |
| desc-v5 (no `-e` mention) | 77 | 10 | **89%** | 2 |
| desc-v6 (v5 + fold-ladders) | 50 | 25 | **67%** | 2 |

Deleting one sentence moved ranked share 32% → 89% — the strongest
posture lever measured in this project, now under scored conditions.
Curious detail: v5 agents used `-e` *less* than v6 despite neither being
told about it in v5 nor discouraged in v6.

**Prediction 2 — weak.** v6's ranked queries are barely more multi-name
than v5's (36% vs 32% with ≥3 name tokens; identical mean length). The
folding *does* occur ("safe deepcopy pickle RLock" is a folded ladder) but
the extra instruction added little beyond removing `-e`. The `-e` deletion
does the work; the coaching sentence is mostly inert.

**Predictions 3–4 — accuracy is flat, exactly as the power caveat
predicted.** Paired over 27 instances: fnAcc@10tol rg 0.59, every semgrep
condition 0.63; fileAcc@5 0.74–0.78 vs rg's 0.74. All semgrep conditions
sit +4pp above rg on functions with **one discordant pair** (w1/l0) —
direction right, resolution nil; §11.5's instrument limit, reproduced to
the letter. No ordering among v4/v5/v6.

**The finding that matters for the product:** behavior is controllable at
10× by description text *without any accuracy cost* — the 89%-ranked v5
agents localize exactly as well as the 32%-ranked v4 agents and the
0%-ranked rg agents, at the same median cost ($0.19–0.22/run). Semantic-
default semgrep as the agent's only tool is at parity-or-better with
ripgrep on outcome while running an entirely different (rankable,
instrument-able, dead-ladder-free) search process. The recommended tool
description is **desc-v5**: identity framing, no `-e` mention — the
escape hatch stays available for agents that know it, but undocumented.
An accuracy separation, if one exists, needs the §11.5 instrument work
(replay-based, larger n), not more agent runs at this scale.

### 16.9 The powered A/B (pre-registered 2026-08-02, before any run)

§16.8 at benchmark scale: **desc-v5 (semantic, `-e` undocumented) vs rg
(exact-term), all 560 Loc-Bench instances, one arm each, sonnet.** §11.5's
power table is the design's spine: at ψ_fn = 0.088, n=560 resolves ~4pp at
80% power — the size of the effect §16.8 observed. Smaller runs cannot
answer the question; this one also *creates* §11.5's planned discriminative
screen as a by-product.

Design registered before the first row:

- **Primary endpoint**: `func_acc@10_tol`, desc-v5 − rg, exact two-sided
  McNemar over discordant pairs. Frame: all 560 (verified: zero instances
  have file-only gold, so no exclusions).
- **Secondaries**: `file_acc@5`, `file_recall@5`, first-gold-hit search
  index, cost and searches per run.
- **Arms are intention-to-treat**: `-e` remains functional-but-undocumented
  in desc-v5; its usage is an outcome, not a protocol violation.
- **Chunking protocol**: one canonical run (`results-scale.jsonl`, one
  model), executed as `--resume` slices across subscription usage windows
  until 1,120 ok rows exist. **No peeking**: endpoints are computed once,
  at completion; interim looks are for failures and spend only.

Predictions:

1. **desc-v5 beats rg on the primary by ≥ +4pp, McNemar p < 0.05.** Honest
   power note: 80% power at ~4.5pp; if the true effect is exactly 4pp,
   power ≈ 70% — a null is informative here, not a whiff.
2. **The delta concentrates in the partial/blind tiers** of the §15.7
   instance screen (issues that don't name the gold).
3. **desc-v5 ranked share ≥ 80%** at scale (reproducing §16.8's 89%).
4. **Zero shim bypasses; `-e` share in desc-v5 ≤ 15%.**

### 16.9a Adversarial review, and the re-registration it forced (before any row)

Two red teams were run against §16.9 before spending anything — one on
design/statistics, one on the harness code. They found enough to void the
section as first written. Recorded in full because the corrections *are*
the pre-registration now; the predictions above are **retracted** and
replaced below.

**A1 — the arm label was false, and it corrupts a published claim.**
semgrep's own footer prints `not it? rephrase the query, or -e '<pattern>'
for every exact match` on stderr after *every* ranked search, with the
caller's query interpolated (`crates/semgrep/src/out.rs`, shipped
2026-07-30). So "the description never mentions `-e`" described the prompt
and not the treatment: **the tool advertised `-e` adaptively, at the moment
of failure, in the arm built to withhold it.** Two consequences. (i) The
campaign now sets `SEMGREP_NO_HINTS=1` for every condition — a new
env-gated suppression in `out.rs` — so no arm carries retry-coaching the
other lacks. (ii) **§16.6's reading is corrected**: "28% of cap-ranked
calls used the undocumented `-e` anyway — pretraining habit" has a rival
explanation that was true all along — the tool told them, and 12 of those
72 calls immediately follow a zero-result ranked query, the footer's exact
trigger. The description-gravity *direction* (72% vs 7% ranked share)
survives; the "pretraining habit" attribution does not, and is withdrawn.

**C1 — the registered effect size was arithmetically unreachable.** For a
paired binary, the marginal delta is bounded by the discordance rate
(δ ≤ ψ). §16.9 imported ψ = 0.088 from §11.5 — measured across *engine
variants*, not across these arms. The directly measured discordance for
rg vs desc-v5 (§16.8, 27 paired instances) is **ψ = 0.037**, of which the
"+4pp" headline was literally **one discordant instance**. A +4pp marginal
delta at that ψ would require b − c = 22 out of b + c = 21: impossible.
Prediction 1 as worded could not have been satisfied by any outcome.

**B1 — the harness would have silently corrupted ~11% of the frame.** 28
instance pairs share a `(repo, base_commit)`; the worktree was keyed on
that pair, so concurrent workers checked out, indexed, and
`worktree remove --force`-ed the *same directory* — deleting trees under
live agents and leaking an index into the rg arm. Fixed: trees are keyed
by `instance_id`. (Also fixed: index-build failures no longer abort the
whole invocation; `checkout_error` rows now carry `model`/`run_id` so a
later success can supersede them; the stop event is honoured between
conditions, not only at task entry.)

**The re-registration.** Primary becomes **direction + significance +
interval, not a threshold**: desc-v5 − rg on `func_acc@10_tol`, exact
McNemar, reported with the paired bootstrap CI; a co-primary
`func_recall@10_tol` (continuous, already computed by `scoring.py`) is
added because the binary endpoint discards resolution on the 96% of
instances where both arms agree. Holm correction across the four
secondaries. **Every stratum is exploratory** and reported without
significance stars — the blind tier alone (n=68, ~2–6 discordant pairs)
cannot be tested at any α. The post-treatment "search usage" stratum is
deleted (conditioning on an outcome of treatment). The `--emit-screen`
artifact is relabeled: it is a *discordance map of this run*, not a
neutral screen, and future A/Bs run on it would inherit a winner's-curse
bias.

Re-registered predictions:

1. **Direction**: desc-v5 ≥ rg on both primaries; the func_acc McNemar CI
   excludes zero. (No magnitude registered: δ ≤ ψ makes any threshold a
   claim about discordance, not about search.)
2. **Bound**: whatever the outcome, the headline is the CI's upper limit —
   "if semantic ranking has an advantage here it is below X pp".
3. **Behavior (the powered part)**: desc-v5 ranked share ≥ 80%,
   reproducing §16.8's 89% at 20× the sample.
4. **Instrumentation honesty**: zero shim bypasses, and the run reports
   *un-shimmed* search too — 21–28% of agent Bash calls are `find`/
   `python3`/`awk` content searches invisible to the shim, and that share
   is itself arm-correlated. `first_hit_search_seq` is therefore demoted
   to descriptive.

What a null will and will not license is fixed now, not after: a clean
null licenses "**parity at n=560 with an upper bound of X pp**" plus the
behavioral result; it does **not** license "semantic ranking doesn't help
agents", because the arms still differ in result exhaustiveness (10 ranked
hits vs rg's unbounded list) and both leak a fifth of their searching into
un-instrumented tools.

**Budget revision, recorded mid-run (2026-08-02).** The first 88 rows cost
**$0.363/row** against the $0.24 projected from prior campaigns, so the
full frame projects to **~$425, not ~$270**. The gap is the frame itself:
this run covers the whole benchmark, including the large repos the pilot's
50-instance sample never touched, and the semantic arm is the pricier of
the two ($0.37 vs $0.29 mean — ranked results invite follow-up queries).
Maintainer approved continuing at the revised figure. Recorded because a
cost assumption that moves 57% is a fact about the instrument, and the
next person planning an agent A/B should budget from this number rather
than from §11.5's.

**Attrition, monitored mid-run (2026-08-03).** Every `agent_error` in the
campaign is a `--max-budget-usd 1.0` cap hit at 26–36 turns — the hardest
instances, not random noise (the §16.9a review predicted the tail would
bite at n=560). Three attempts and the cell is abandoned, so those
instances leave the paired frame. **This is only safe if attrition is
symmetric, so it is being watched rather than assumed**: at 395 rows it
is 8 rg vs 6 desc-v5 budget hits, 3 vs 3 checkout errors, and 3 vs 2 among
instances missing one arm. Balanced within noise. Two consequences for the
write-up regardless of the final split: the frame is **"instances solvable
within $1 and 900 s"**, not the full benchmark, and the final attrition
table is reported per arm — an experiment that quietly shrinks its own
frame is the failure §16.9a existed to prevent.

**First-chunk observation, not an endpoint** (the no-peeking rule binds
the endpoints, not the instrumentation checks §16.9a demanded): with the
footer suppressed, desc-v5's ranked share is **100%** — zero exact-mode
calls in 43 runs, against 89% under the footer-coached §16.8 conditions.
And the un-shimmed search covariate is live and arm-correlated as the
review predicted: 11% of desc-v5's Bash calls are `find`/`python3`
content searches versus 4% of rg's.

### 16.10 Result: parity, bounded (2026-08-03)

1,115 agent runs, **556 of 560 instances paired**, $360.99, one analysis
pass. The endpoints, desc-v5 (semantic) − rg (exact):

| endpoint | semantic | rg | Δ | 95% CI | discordant |
|---|---|---|---|---|---|
| **func_acc@10_tol** (primary) | 0.674 | 0.673 | **+0.002** | [−0.018, +0.022] | 18 / 17 |
| func_recall@10_tol (co-primary) | 0.771 | 0.766 | +0.005 | [−0.014, +0.025] | 38 / 37 |
| file_acc@5 | 0.838 | 0.835 | +0.004 | [−0.014, +0.022] | 15 / 13 |
| file_acc@1 | 0.745 | 0.737 | +0.007 | [−0.009, +0.023] | 12 / 8 |
| file_recall@5 | 0.880 | 0.875 | +0.005 | [−0.011, +0.022] | 19 / 16 |
| func_acc@10_strict | 0.667 | 0.660 | +0.007 | [−0.014, +0.029] | 22 / 18 |

**This is a null, and it is the informative kind.** The registered
headline is the bound, not the p-value: **if semantic-default search has
an agent-level localization advantage over ripgrep on this benchmark, it
is smaller than 2.2 percentage points.** 357 instances were solved by
both arms, 164 by neither, and the 35 that separated them split 18–17.
Achieved discordance ψ = **0.063** — between the pilot's 0.037 and
§11.5's 0.088, so the instrument had the resolution the re-registration
claimed, and the answer is that the effect is not there to find at this
scale.

**Prediction scorecard** (the §16.9a re-registration, not the retracted
original):

1. **Direction with a CI excluding zero — MISS.** Every one of the six
   endpoints leans positive (+0.002 to +0.007) and not one clears zero.
   The 6-for-6 sign is worth *noticing and not believing*: these
   endpoints are near-duplicates computed on the same runs, so their
   agreement is one observation wearing six hats, not six confirmations.
2. **Bound — delivered**: ≤ +2.2pp on the primary, ≤ +2.5pp on recall.
3. **Ranked share ≥ 80% — HIT, 98%.** 3,385 ranked vs 85 exact calls;
   `-e` fell to **2.4%** with the footer suppressed, against 11% when
   the tool was coaching it (§16.9a A1 quantified end to end).
4. **Instrumentation — HIT.** Zero shim bypasses in 1,115 runs. The
   un-shimmed leak that looked arm-correlated early (11% vs 4%) converged
   at scale to **11% vs 12%** — the early asymmetry was small-sample
   noise, and the covariate is symmetric where it matters.

**The exploratory strata contain a trap, and it is left as one.** Bug
Reports show +0.037 with an uncorrected p=0.035 (12/3 discordant). That
is one line out of ten exploratory tests; at α=0.05 the expected number
of such lines under a global null is 0.5, and drawing one is unremarkable.
It is reported unstarred, uncorrected, and explicitly **not** a finding —
§16.9a's multiplicity fix existed to stop exactly this line from becoming
a headline. If the Bug-Report effect is real, it is a pre-registered
hypothesis for a *future* run, not a result of this one. The blind tier —
where §15.7 and §16.5 both predicted the advantage would live — shows
+0.015 with 3/2 discordant pairs: nothing, and underpowered besides.

**Cost is the one clean separation:** $182.80 vs $143.69 for identical
work — semantic-default agents cost **27% more** per instance (more
turns, ranked results invite follow-ups) for statistically identical
localization.

**What this licenses, per the registration.** Warranted: *semantic-default
semgrep as an agent's only search tool is at parity with ripgrep for
localization on Loc-Bench, n=556, with an upper bound of +2.2pp, at 27%
higher cost* — and the behavioral result, that one sentence of tool
description moves ranked usage from 7% to 98% with no accuracy
consequence either way. Not warranted: "semantic search doesn't help
agents." The arms differ in exhaustiveness as well as matching semantics,
~11% of both arms' searching leaks into un-instrumented tools, the frame
is instances solvable within $1/900 s, and — the constraint that outlives
this run — **80% of Loc-Bench instances are decided before search
matters**: 357 solved by both arms, 164 by neither. §11.5 said the
instrument was the bottleneck; §16.10 is that claim confirmed at full
scale, with the money spent to prove it rather than assume it.

**Attrition, as promised.** 4 instances lost: 3 with one arm abandoned
after 3 budget-cap failures (all 3 missing rg), 1 failing both arms on
checkout. Budget-cap failures ran 22 rg vs 16 desc-v5 — leaning toward
dropping instances where *ripgrep* struggled, i.e. against the treatment,
so the null is if anything conservative. The frame is 556/560 = 99.3%.

**By-product delivered**: `discriminative-instances.json`, the 50
instances (9%) where the arms disagreed on either endpoint — published as
a *discordance map of this run*, explicitly not a neutral screen (§16.9a
C5): selecting future A/Bs on it inherits a winner's-curse bias.

### 16.11 A bug the trajectories exposed, after the result (2026-08-03)

Reading agent trajectories to illustrate §16.10 surfaced something the
aggregate had hidden: **`semgrep "query" <single-file>` returns zero
results, always.** Root cause in `corpus::walk` — when the search root
*is* a file, `entry.path().strip_prefix(root)` yields the **empty
string** as that file's relative path, and every downstream consumer
(chunk read, hit materialization) fails on it. Exact mode takes the
keyword path and is unaffected, which is exactly why the bug survived:
`-e` on a file works, so nothing in the test suite or the snapshot
noticed.

Blast radius in this campaign, measured from the shim logs:

| | |
|---|---|
| semantic ranked searches | 3,434 |
| **scoped to a single file** | **1,610 (46.9%)** |
| of those, returned nothing | **1,610 (100%)** |
| instances that hit it ≥ once | 339 / 556 (61%) |

Scoping to a file is the natural agent move *after* locating one, so the
bug fires precisely at the follow-up step — the semantic arm spent nearly
half its searches on a call that could not succeed.

**Does it void §16.10? Measured, not assumed — and the answer is no.**
Splitting the paired frame by whether the run ever hit the bug:

| stratum | n | semantic | rg | Δ |
|---|---|---|---|---|
| hit the bug | 337 | 0.677 | 0.665 | **+0.012** |
| never hit it | 219 | 0.671 | 0.685 | **−0.014** |

Both deltas are noise, and they point in *opposite* directions — the
bug-free stratum is if anything worse for semantic. Agents recovered by
re-searching at directory scope or falling back to Read, so the failure
cost turns rather than answers. That said, it plausibly explains part of
the **27% cost premium** §16.10 reported: an arm that wastes half its
searches on a guaranteed-empty call takes more turns to get to the same
place.

**Status of the claim.** §16.10 stands as measured — it is what the
shipped binary does, and the parity finding is robust to the bug by the
stratification above. What is *not* established is how a fixed binary
would perform; that is a new experiment, and a cheap one to justify only
if something else changes too (the §9.9 model swap is the candidate).
The fix ships regardless: it is a product bug, agents scope to files
constantly, and "your search silently returns nothing" is the worst
failure mode a search tool can have.

**The process lesson, which is the reason this section exists.** Two
adversarial reviews, a smoke test, and 1,115 runs did not surface this;
*reading four trajectories* did. The reviews checked the experiment and
the harness. Nobody checked whether the tool worked on the input agents
actually give it. Add to the pre-run checklist: **replay a handful of
real agent invocations, verbatim, and look at what came back.**


## 17. Where retrieval actually fails at agent scale (2026-08-03)

§16.11 fixed the file-scope bug and left an obvious question: with the
tool working, where does semantic search actually perform worst? The
§16.10 trajectories cannot answer it. Re-classifying all 3,519 desc-v5
searches in that campaign by cause of emptiness:

| count | share of empties | cause |
|---|---|---|
| 1,993 | 95.9% | ranked search at a **file** scope — the §16.11 bug |
| 69 | 3.3% | usage error (exit 2), bad path or malformed args |
| 16 | 0.8% | exact mode, a genuine zero-match |

2,078 of 3,519 searches (59%) returned nothing, and 82 of 445 instances
(18%) never received a single non-empty result. A failure taxonomy built
on that is a taxonomy of the bug.

### 17.1 The instrument: guessplay's pre-fix run, with the bug separated out

`eval/data/locbench/guessplay.jsonl` (33,394 rows, 2026-08-02) predates
the fix, but the bug is cleanly *separable* rather than merely present —
which makes the file usable rather than scrap. Split its default-config,
primary-policy ranked rows by scope shape:

| scope shape | n | found gold @5 |
|---|---|---|
| file | 5,117 | **0 (0.0%)** |
| directory | 4,537 | 2,532 (55.8%) |

Zero out of 5,117. That is the bug measured on the offline instrument, and
it is the cleanest evidence of it anywhere in this project — a rate of
exactly zero is not a quality result, it is a structural one. Excluding
file-scoped rows leaves a **4,537-row bug-free frame**, which is what every
number below is computed on.

It also means **§16.5's guess-board numbers were computed on a sample where
53% of ranked rows were forced to zero.** That does not bias a paired
*difference* — the zeroing is config-independent — but it dilutes one, so
every §16.5 null deserved rechecking on the clean frame.

### 17.2 §16.5's champion verdict survives the correction

Rechecked, paired on identical (gid, arm, mode), hit@5:

| frame | n | default | champion | Δ | CI | w/l |
|---|---|---|---|---|---|---|
| all rows (as §16.5 reported) | 9,654 | 0.206 | 0.207 | +0.001 | [−0.003, +0.004] | 151/143 |
| **bug-free rows only** | 4,537 | 0.438 | 0.440 | **+0.002** | [−0.006, +0.009] | 151/143 |
| — semantic only | 1,300 | 0.416 | 0.416 | +0.000 | [−0.021, +0.020] | **90/90** |
| — hybrid only | 1,937 | 0.451 | 0.461 | +0.010 | [+0.001, +0.021] | 59/39 |
| — bm25 only | 1,300 | 0.441 | 0.432 | −0.009 | [−0.015, −0.004] | 2/14 |

The null is not an artifact of dilution: on the frame where the tool
worked, semantic under split-sif is **90 wins and 90 losses**, a tie to the
row. (bm25 moving at all is §14.4's prediction-6 coupling — bm25-mode output
passes through MMR, which reads the embedding matrix.) This is what makes
§14.5's verdict safe to record as a decision rather than a guess.

### 17.3 Semantic has no distinctive weakness against bm25

Paired on the 1,300 bug-free rows where both modes ran the same query:

| outcome | n | share |
|---|---|---|
| both found | 469 | 36.1% |
| **bm25 only** | 104 | 8.0% |
| **semantic only** | 72 | 5.5% |
| neither | 655 | 50.4% |

semantic 0.416 vs bm25 0.441. The discordant sets are near-symmetric, and
profiling them finds no distinguishing feature at all: both have median
length 1 word, ~50% single-word queries, and ~45% containing a code
identifier. **The question "where does semantic lose to lexical" has no
answer on real agent queries, because it does not systematically lose** —
the two trade wins on queries that look the same. The 50.4% they *both*
miss is the real target.

### 17.4 The taxonomy of misses

Splitting all 1,300 rows by whether the gold file was even reachable from
the path the agent searched:

| n | share | outcome |
|---|---|---|
| 645 | 49.6% | found, gold inside scope |
| 476 | 36.6% | **true ranking failure** — gold inside scope, not in top-5 |
| 179 | 13.8% | **unanswerable** — gold outside the searched path |

So 27% of all misses were structurally impossible: the agent pointed the
search at a tree that does not contain the answer (`tests/` when the gold
is in `src/`, a `docs/release-notes` scope for a `jmclient/` bug). No
engine and no ranking change can recover those.

Of the 476 true ranking failures, **69% share no vocabulary with the gold
at all** — no token in common with either the gold path or the gold
function name. Overlap predicts the outcome monotonically: 49.7% of
found-rows overlap gold vocabulary, ~40% of discordant rows, 30.8% of
missed rows. That is §15's blind wall, reproduced on real agent queries
rather than constructed ones, and it is a *model* problem: the embedder
cannot relate words it was never shown to relate.

### 17.5 The fix that looked obvious and is wrong

13.8% unanswerable-by-scope suggests searching the repo root instead of
whatever the agent picked. Measured, hit@5, paired:

| frame | n | agent scope | root | Δ | CI | w/l |
|---|---|---|---|---|---|---|
| all bug-free rows | 4,537 | 0.438 | 0.425 | **−0.013** | [−0.022, −0.003] | 206/263 |
| — semantic | 1,300 | 0.416 | 0.405 | −0.012 | [−0.030, +0.005] | 64/79 |
| — bm25 | 1,300 | 0.441 | 0.423 | −0.018 | [−0.035, −0.002] | 49/72 |
| file-scoped rows (pre-fix) | 5,117 | 0.000 | 0.463 | +0.463 | [+0.450, +0.478] | 2371/0 |

**Blanket widening is a net loss.** It rescues 206 rows and costs 263: the
agent's scope choice carries real information, and discarding it to escape
the 13.8% loses more than it saves. Any scope fix has to be *selective* —
conditioned on a signal that the current scope is wrong — not a default.
(The last row is the bug again, not a scope result: at a file scope
pre-fix, anywhere else was better than nothing.)

### 17.6 What this says to do next, in order

1. **The vocabulary wall is the dominant addressable failure** (69% of true
   ranking failures) and it is not a ranking-parameter problem. That points
   at the §9.9 code-teacher swap, gated on the §15 strict-blind instrument,
   which §15.10 already retained for exactly this purpose. Nothing in §9's
   lever space touches it.
2. **Scope needs a confidence signal, not a wider default.** The bounded
   piece of work is deciding when a scope looks wrong — all-weak scores over
   a small candidate pool — and saying so on stderr. 13.8% of rows are
   reachable this way and 0% are reachable by widening unconditionally.
3. **Not ranking parameters.** split-sif is null on the clean frame (§17.2),
   and semantic-vs-bm25 is a tie (§17.3). The remaining §9 levers are tuning
   a component that is not the bottleneck.
4. **Re-run the campaign only if something else changes.** §16.11's estimate
   stands: the fix plus a model change is worth one run; the fix alone
   changes emptiness, not the ceiling that §17.4 describes.

**The methodological note.** Two of this section's findings inverted an
answer that looked settled. §16.5's null was computed on a half-zeroed
sample and needed rechecking before §14.5 could lean on it; and "widen the
scope," which follows directly from the 13.8% number, is a measured
regression. Both were one paired comparison away from being written down
wrong.

## 18. The two-tiered rerun (2026-08-03)

The §16.10 campaign measured a broken tool: 47% of the treatment arm's
searches returned nothing, and the harness never noticed because the only
per-search record was `(argv, exit, stdout_bytes)`. Everything since —
§16.11's file-scope fix, four join sites plus a fifth in `out::context`,
the §17 grep-compat work, the `index` false affordance — has been verified
by tests and offline replay, never under a live agent.

So: a small instrumented tier, a gate, then the full run. Tier 1 is
underpowered on purpose. Its job is to find the next §16.11 *before* 1,100
runs are paid for, not to measure accuracy.

### 18.1 The instrument that was missing

`SEMGREP_TRACE_FILE` already existed and was built for this — it does not
perturb the argv an agent sees, so it works underneath `shim.py`, and it
catches invocations no outer flag can reach. **`run.py` never set it.** It
does now, per condition dir, and `files_walked` was added to the envelope
(it reached `SearchReport` in the §17 work but never the trace, and it is
the field that separates "empty scope" from "unreadable scope").

`eval/locbench/triage.py` reads those envelopes beside the shim logs and
gates on them, exiting nonzero so it stops a campaign rather than
describing one. Validated by running it against the **old** campaign, where
it correctly fails: 69 usage errors, **455 distress signals**, 82 instances
where every search was empty. It would have stopped that run on chunk one.

Two of its own defects surfaced in that validation: a disk figure printed
as `580.0%`, and — the one that matters — **the empty-result gate passing
vacuously at 0/0 when no traces exist**, which is the same silence the tool
exists to end. A missing trace now fails the gate.

### 18.2 Tier 0 (free, offline)

Replaying all 3,519 logged agent invocations from §16.10 against the fixed
binary, on the frozen fixture:

| | §16.10 | now |
|---|---|---|
| usage errors (exit 2) | 69 | **7** |
| returned nothing (exit 1) | 2,008 | **90** |
| returned hits (exit 0) | 1,442 | **3,422** |
| regressions | — | **0** |

(Fixture corpus, so "hits" is easy — the exit-code *shape* is the signal,
not the ranking.)

### 18.3 What tier 1 found, on its first four rows

The smoke run — two instances — reported `path_taken=built_but_missed`
twice, a shape `telemetry.rs` names precisely because it is a bug.
Reproduced deterministically: `cache::discover` refuses a non-directory
root, so a file-scoped search misses; `build_through` then builds a
**complete index for that file** and writes it; re-discovery misses again
on the same check, so the search streams anyway; and the budget sweep
deletes the fresh entry, because it judges a root dead by `root_exists:
root.is_dir()` — right for "the checkout was deleted", wrong once §16.11
made file scopes legitimate. Every file-scoped search built an index and
threw it away, on roughly half of all agent searches.

A second, more general defect fell out of it: `enforce_budget_protecting`
passed `keep` only to the LRU pass, not to the dead sweep that runs first —
so "protect the entry I just wrote" did not.

Absolute cost was ~20 ms (a one-file index is cheap), so this is waste and
churn rather than a performance headline. Serving a file scope from an
ancestor's index is the better answer and the prefix machinery already
exists; noted, not attempted.

**This is the entire case for the instrumentation.** Four rows, and it
surfaced a defect that eight weeks of tests, two adversarial reviews, and a
1,115-run campaign had not.

### 18.4 Tier 1 results (40 instances × 2 arms, $18.70)

| gate | §16.10 | tier 1 |
|---|---|---|
| ranked searches returning nothing | 59% | **0 of 138** |
| instances where every search was empty | 18% (82) | **0** |
| distress signals attributable to the tool | 455 | **0** |
| usage errors the tool is answerable for | 69 | **0** |
| leaked worktrees / non-ok rows | 4 / 1 | **0 / 0** |

The five remaining exit-2s are the tool being correct, and `triage.py`
classifies them rather than counting them: two queries beginning with a
dash (read as flags — the caller's mistake, but the message was unhelpful,
so `--` is now suggested), two `-k` with no value, one path that does not
exist in that revision. **Gating on the raw count would have failed the run
for rejecting a bad path, which is the single most useful error the tool
emits** — an agent reads "no results" as "the code is not there", and a
wrong path is the other explanation. The gate is on unrecognised *flags*,
the category where a compat gap would hide.

Accuracy, reported for completeness and **not** to be read as a result at
n=40: `func_acc@10_tol` rg 0.625 vs desc-v5 0.675 (w2/l0),
`file_acc@5` 0.775 vs 0.800 (w1/l0). Two discordant pairs decide the
primary endpoint, which is §11.5's instrument limit restated. Cost per run
$0.221 vs $0.246; searches per run 3.8 vs 3.6.

### 18.5 Tier 2 pre-registration (before the first row)

Endpoints carry forward from §16.9 unchanged: primary `func_acc@10_tol`,
exact two-sided McNemar over discordant pairs, restricted to instances with
non-empty `gold_funcs`; secondary `file_acc@5`, cost, searches per run. One
canonical file, `--resume`, no interim endpoint looks; `triage.py` per
chunk to catch a mid-flight regression rather than a post-mortem.

**The registered expectation is parity.** §16.11 measured the file-scope
bug as costing nothing (bug-hit +0.012, bug-free −0.014, both noise), and
§17 put the remaining ceiling on the vocabulary wall — 69% of true ranking
failures share no token with the gold — which is a model problem and out of
scope by decision. Tier 1's +0.050 rests on two discordant pairs and is not
evidence against that prior.

So the honest value of tier 2 is: **the §16.10 number was measured on a
broken tool, and this is what the product actually does.** A null is the
predicted result, not a disappointment. Recording that here, before the
run, is what makes it a prediction rather than a rationalisation.

### 18.6 Tier 1b: an independent 40, and what it cost to get one

Tier 1a's +0.050 rested on two discordant pairs, so §18.5 registered parity
and said so. The way to test that is a fresh sample, and asking for one
turned up a harness defect first.

**`--seed` barely moved the sample.** `stratified_sample` shuffled each
category and then `sort(key=repo)`; Python's sort is stable, so the shuffle
survived only *within* a repo while the repo order came out alphabetical for
every seed. Taking from the front picked the same alphabetically-first repos
every time: **seed 1 and seed 2 shared 37 of 40 instances.** No error, no
warning — a re-run under a new seed returns a near-duplicate and reports it
as fresh coverage, so any claim of the form "validated on an independent
sample" would have been false. Fixed by shuffling the repo *order* rather
than dropping the grouping (the interleaving is still wanted: one repo must
not dominate 40 rows). Seeds 1–3 now cover 99 distinct instances instead of
43. Tier 1a's instances came from the old sampler and are replayable only via
`--instances`, from `results-tier1.jsonl`.

**The run** (seed 2, 33 of 40 instances new, $19.84): gate passed. 165 engine
traces, **0 ranked searches returning nothing**, 0 distress signals, 0 usage
errors the tool is answerable for. The §16.11 signature did not appear in
either tier.

Three non-ok rows failed the gate first, and both causes were environmental
rather than tool defects:

- `UCL__TLOmodel-1524`, both arms — **`git-lfs: command not found`**. An LFS
  repo cannot be checked out without it, and the failure is symmetric across
  arms, so it silently shrinks the frame rather than biasing it. One instance
  in 40 is ~14 of 560 at full scale. Installed; both arms then completed.
- `Netflix__metaflow-2141` — the **`--budget-usd` guard firing at $1.02**,
  working as designed on a 33-turn run, but recorded as `agent_error`, which
  reads as a failure. It completed at `--budget-usd 1.5` in 24 searches.

**Accuracy across both tiers** (paired, ok rows only):

| | n | rg | desc-v5 | Δ | discordant |
|---|---|---|---|---|---|
| tier 1a | 40 | 0.625 | 0.675 | +0.050 | w2/l0 |
| tier 1b | 40 | 0.575 | 0.550 | **−0.025** | w1/l2 |
| **pooled distinct** | **73** | **0.616** | **0.616** | **0.000** | w2/l2 |

`func_acc@10_tol`. The sign reversed on an independent sample and the pooled
delta is exactly zero on four discordant pairs total. **This is §18.5's
registered prediction landing before the money was spent** — had tier 1a run
alone, +0.050 would have looked like a result. `file_acc@5` pooled: 0.795 vs
0.781. Cost per run is at parity (tier 1b: $0.249 rg vs $0.247 desc-v5), the
27% premium §16.10 measured having gone with the file-scope bug.

Tier 2's registration stands unchanged, now with two independent samples
behind it rather than a prior.

## 19. The description A/B: restoring the micro-example (2026-08-04)

§18 closed the engine question at parity and §17 put the ceiling on the
vocabulary wall. What neither touched is the largest lever this project has
ever measured, which is not in the engine at all: **§16.6 moved an agent's
ranked share from 72% to 7% by mentioning `-e` in one clause.** Description
effects here are an order of magnitude larger than any §9 ranking parameter.

### 19.1 What the post-fix trajectories show

The first campaign run on a working tool (366 searches over 5 runs,
2026-08-03 19:29 onward) is the first clean read of how agents drive this
tool. The distress signals are gone: empty results fell from 55–68% to ~2%,
repeated-identical-queries from 1,040 to 0, `--help` probes from 27 to 0.

What is left is the query shape:

| query length | n | share |
|---|---|---|
| 1 word | 124 | 34% |
| 2 words | 125 | 34% |
| 3 words | 40 | 11% |
| 4+ words | 77 | 21% |

**68% of queries are one or two words** — identifier guesses at a tool built
to take descriptions. That is the same population §17.3 profiled from the
other end: the queries that miss have median length 1 word, and §17.4 found
69% of true ranking failures share *no token at all* with the gold. A
one-token guess has almost no surface to overlap on.

### 19.2 The candidate, and why it is a defect rather than an idea

`desc-v5` — the description in every campaign since §16.7, 695 runs — **has
no micro-example.** `desc-v4` does.

That is an accident of derivation, not a decision. v5 was produced by cutting
`-e` out of v4 (§16.6), and the example went with it because the example was
the clause that named a mode. But §7.3's winner was ranked-as-identity
framing **plus** a micro-example, and §7.3 separately found that *agents
imitate examples more reliably than they follow rules*. What has shipped for
695 runs is half of a measured result, and the half that was dropped is the
half that demonstrates a descriptive query.

`desc-v7` is `desc-v5` with the v4 example restored and nothing else: one
inserted sentence, 237 characters identical before it and 95 after, verified
by diff rather than by eye. `-e` stays unmentioned, so this cannot
re-collapse ranked share the way §16.6 did.

### 19.2a The prior already in the logs, and its confound

`queryshape.py` reads query length by condition out of the shim logs, so the
descriptions already run can be asked prediction 1 before a row is spent
(ranked semgrep searches only):

| condition | example? | rule? | n | mean words | ≤2 words |
|---|---|---|---|---|---|
| desc-v4 | **yes** | no | 23 | **3.74** | 30% |
| desc-v5 (ships today) | no | no | 4,129 | 2.40 | 69% |
| desc-v6 | no | **yes** | 50 | 2.38 | 64% |

Two things fall out, and only one of them is trustworthy.

**The clean one is desc-v6.** It is desc-v5 plus an explicit instruction —
"describe the behavior… put ALL your candidate names in one query" — and it
moved query length by **−0.02 words**. A rule telling agents to write longer
queries did not produce longer queries. That is §7.3's finding reproduced on
an independent condition, and it is why the lever under test is an example
rather than another sentence of advice. (This also corrects a claim made in
passing while scoping this work: desc-v6 *has* been run, 27 instances — an
earlier count truncated its row and read as never-run.)

**The confounded one is desc-v4.** It is +1.34 words over desc-v5 and has
the example — but it also mentions `-e`, and calls the tool "a ranked hybrid
code search" where v5 says "a ranked code search". Three differences, one
outcome, n = 23. It is a prior, not a result, and reading it as one would be
§17's methodological note happening a third time.

desc-v7 exists to turn that confound into a single variable. The prior is
strong enough to be worth the frame and weak enough that it cannot stand in
for it.

### 19.2b What a static model does with a paraphrase (and why v7 was wrong)

Before spending a frame teaching agents to write descriptions, the obvious
question: **does the engine actually reward one?** `ese` is a *static*
embedding table — one vector per token, pooled by SIF rarity weight, word
order discarded. There is no contextual encoder, so a query's vector is a
weighted bag of its tokens' vectors, and `sif.rs`'s weight `a/(a + p(w))`
with `a = 1e-3` puts a word appearing in 1% of the corpus at 0.09 while a
rare one sits near 0.99. **A paraphrase is therefore reduced, at the engine,
to its rare tokens** — "where is the retry backoff computed" is close to
"retry backoff computed" — and if those tokens miss, nothing is left.

Measured on `guessplay.jsonl` by `eval/locbench/stylecut.py` (checked in, so
every number below re-runs from one command), restricted to the arm where the
agent wrote the ranked query itself (`ranked-own`; t1/t2 are harness
translations of grep patterns and identifier-shaped by construction), default
config, original scope, non-file scopes only — n = 413, hit@5:

| style | n | words | semantic | bm25 | hybrid |
|---|---|---|---|---|---|
| identifiers | 194 | 3.2 | 0.526 | 0.500 | **0.526** |
| plain words | 155 | 3.8 | 0.503 | 0.503 | **0.548** |
| mixed | 22 | 6.6 | 0.409 | 0.500 | 0.455 |
| paraphrase | 42 | 7.5 | 0.357 | 0.357 | **0.357** |

Stratifying by §17.4's predictor — does the query share any subtoken with the
gold function — separates knowing the name from writing it well:

| style | shares gold vocab | shares none |
|---|---|---|
| identifiers | 0.581 (n=105) | **0.461** (n=89) |
| plain words | 0.567 (n=60) | 0.537 (n=95) |
| paraphrase | 0.824 (n=17) | **0.040** (n=25) |

**A paraphrase that misses the gold's vocabulary finds it 4% of the time. An
identifier guess that also misses finds it 46%.** That is the finding, and it
inverts what desc-v7 was built on. A paraphrase is not a way around not
knowing the name — it is bimodal, superb when it happens to contain the right
rare word (0.824) and near-total failure when it does not. An identifier
guess degrades gracefully instead, because a wrong guess still shares
subtokens with the right one: `retry_backoff` and `backoff_delay` overlap
where "computed" and `backoff_delay` do not.

Two things follow. **semantic − bm25 is ≈ 0 in every stratum** — the static
table adds essentially nothing over lexical matching on real agent queries,
which is §17.3's tie localized rather than contradicted. And **query length
is the wrong endpoint**: paraphrases are the longest queries and the worst
ones, so a description that raised mean length by teaching questions would be
a regression reported as a win. `queryshape.py` reports style, and the
existing desc-v4 rows show exactly that trap — its +1.34 words is **−7pp
identifiers and +5pp paraphrase**.

**The robustness check, and the number to quote.** The four-way classifier is
fuzzy at one boundary: `cpp_appendColumnToParquet` matches neither the
snake_case nor the camelCase pattern and lands in "plain words", so that class
is a mixture of prose and unrecognised identifiers. The clean signal is the
one that does not depend on recognising code shape — **does the query contain
English function words** — which splits it into a name and a description with
no fuzzy middle. Collapsed that way, hybrid hit@5 with bootstrap CIs:

| | name-like | description |
|---|---|---|
| all | 0.536 (n=349) [0.484, 0.590] | 0.391 (n=64) [0.281, 0.516] |
| shares gold vocab | 0.576 (n=165) | 0.636 (n=33) |
| **shares no gold vocab** | **0.500** (n=184) [0.429, 0.571] | **0.129** (n=31) [0.032, 0.258] |

The overall difference is not conclusive on its own — those CIs overlap. **The
blind stratum is**, and it is the whole finding: the CIs are disjoint, and
when the query already contains the gold's vocabulary a description does
marginally *better* (0.636 vs 0.576). So descriptions are not bad; they are
**entirely dependent on lucky rare-token overlap**, which is what a static
bag-of-words model predicts and what the campaign is meant to confirm on
agents who were told which style to write.

*Caveats, because this is observational.* Agents choose how to phrase, so the
stratification is a control and not a randomisation. n is small in the cut
that carries the result (31 blind descriptions), and the narrower
paraphrase-only cut puts it at 0.040 — 1 hit in 25. Quote **0.129 vs 0.500**,
the collapsed and better-powered version; the direction is far better
established than the magnitude either way.

### 19.3 Pre-registration (amended 2026-08-04, before the first row)

Endpoints carry forward from §16.9/§18.5 unchanged: primary
`func_acc@10_tol`, exact two-sided McNemar over discordant pairs, restricted
to instances with non-empty `gold_funcs`; secondary `file_acc@5`, cost,
searches per run. One canonical file, `--resume`, no interim endpoint looks,
`triage.py` per chunk.

**What the amendment changed, and when.** §19.3 first registered *query
length* as prediction 1 and "parity or a small gain" for desc-v7. §19.2b then
measured that length is the wrong endpoint and that desc-v7's paraphrase
example demonstrates the worst-performing style. The predictions below
replace those. **No desc-v7 or desc-v8 row had been run when this was
written** — the amendment is a response to offline analysis of a pre-existing
file, not to any result from the campaign it registers. Recording the
supersession rather than editing the original in place is the point.

**The design is now five arms**, which makes it a factorial rather than an
A/B: `desc-v5` (no example) against `desc-v6` (a rule, no example) isolates
the *instruction*; v5 against `desc-v7`/`desc-v8` isolates *having* an
example; and **v7 against v8 isolates the style the example demonstrates**,
the two differing only in the 35 characters inside the example's quotes. `rg`
rides along as the incumbent control, because five arms make the marginal
cost of the sixth small and §18's null deserves a second independent look.

**Registered predictions, in falsifiable order:**

1. **Query style moves, and length is not the endpoint.** Registered floor:
   **desc-v8 raises the identifier share by ≥5pp over desc-v5**, and desc-v7
   raises the paraphrase share. Measured with `queryshape.py`, from the shim
   logs, with no scoring and no gold files. **If style does not move,
   predictions 2–4 are void rather than negative** — an unread description
   cannot be evidence about examples. A rise in mean *words* unaccompanied by
   a style shift is explicitly **not** a pass, which is the trap desc-v4's
   +1.34 words sets.
2. **desc-v8 ≥ desc-v7 on accuracy**, and this is the comparison the campaign
   exists for. §19.2b puts a blind paraphrase at 0.040 and a blind identifier
   guess at 0.461, so if agents imitate examples (§7.3) the ordering should
   survive into `func_acc@10_tol`. This is the one place a large effect would
   not be surprising.
3. **desc-v7 ≤ desc-v5.** The uncomfortable prediction, registered because it
   is what §19.2b implies and because desc-v7 is *our own* proposal from
   yesterday: an example that demonstrates paraphrasing should make things
   worse, not merely fail to help. If v7 beats v5, §19.2b's mechanism is
   wrong and the observational analysis misled us.
4. **Cost does not rise**, in any arm.

**Pre-specified subgroup, and a disclosed peek.** §19.2b's mechanism is not a
claim about queries in general — it is a claim about *blind* ones. Where the
query already carries the gold's vocabulary, a description does marginally
better (0.636 vs 0.576); the collapse to 0.129 happens only when it does not.
So the effect, if real, should concentrate in the **`blind` issue-naming tier**
(§15.7) and be absent in `named`, where the issue text hands the agent the
symbol and no phrasing advice can matter. That ordering — an effect in `blind`,
nothing in `named` — is a sharper test than the pooled delta, and a pooled null
with a `blind` effect is a pass rather than a failure.

This matters because the pooled endpoint is probably underpowered. §18.5 said
the binary "discards resolution on the ~96% of instances where both arms
agree", and §18 ended with four discordant pairs across 73 instances. The
co-primary `func_recall@10_tol` is the continuous endpoint for that reason and
should be read beside the binary, not after it.

*Disclosure:* this paragraph was written at 81 of 200 rows, after running
`ab_analyze.py` on the partial file to check the analysis path executes. That
peek showed desc-v8 and desc-v7 identical on every endpoint with **zero
discordant pairs** at n=16 — no signal in either direction, and the `blind`
stratum at n=3 was 0.000 for both arms. The subgroup follows from §19.2b's
mechanism, which predates every row; it is recorded here rather than
introduced at analysis time, and the peek is recorded because a subgroup added
after any look at the data is worth less if nobody says so.

Two failure modes named in advance. **The example's content is a confound**
(§7.3: agents imitate examples): `retry backoff` is networking vocabulary and
Loc-Bench is not mostly networking bugs, so if style moves and accuracy does
not, the next arm is a different example rather than a conclusion about
examples. And **desc-v8 conflates two changes** — identifier shape *and*
three candidate names in one query. If it wins, which of those did the work
is a further arm, not something this frame answers.

### 19.4 How to run it

The five-arm campaign §19.5 reports, kept for reproduction:

    OUT=../data/locbench/results-desc-tier1.jsonl LIMIT=40 \
    CONDITIONS=rg,desc-v5,desc-v6,desc-v7,desc-v8 eval/locbench/campaign.sh

§19.6's blind-enriched frame:

    INSTANCES=$(python3 eval/locbench/tierframe.py) \
    OUT=../data/locbench/results-desc-v8-blind.jsonl \
    CONDITIONS=rg,desc-v8 BUDGET=1.5 eval/locbench/campaign.sh

Analysis, in the order the predictions are gated:

    # The style check. --since scopes to THIS campaign's run dirs; without it
    # the sweep picks up every campaign ever run and compares arms across
    # different instances, which is not a paired comparison.
    #
    # No `--a/--b` here, and that is not an omission: a two-arm style delta is
    # impossible against rg, which has no ranked mode and therefore contributes
    # no ranked queries at all — `--b rg` reports "no ranked searches for: rg".
    # (Registered with `--b rg` and corrected on the first chunk, which is what
    # running the free check early is for.) What is available is a *within-arm
    # replication*: desc-v8's identifier share on this frame against the 65% it
    # produced in §19.5 and desc-v5's 45% baseline there. Weaker than a paired
    # delta, and the strongest form this arm pairing allows.
    python3 eval/locbench/queryshape.py --since <run-id>

    python3 eval/locbench/ab_analyze.py \
      --results ../data/locbench/results-desc-v8-blind.jsonl --a desc-v8 --b rg
    python3 eval/locbench/reweight.py \
      --results ../data/locbench/results-desc-v8-blind.jsonl --a desc-v8 --b rg

`campaign.sh` takes arms, frame and budget as parameters rather than literals;
its defaults reproduce the §16.9 frame's *shape* but run desc-v8, since a
harness whose default tests something other than what README recommends stops
being evidence about the product. Prediction 1 is answerable from the shim logs
alone, so **run `queryshape.py` after the first chunk** — before the frame is
paid for. Order matters: prediction 1 is free and gates the ones that are not.

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

**Prediction 1 (style moves): passed for v8, failed for v7.** desc-v8 raised
the identifier share **+20pp** over desc-v5 (65% vs 45%, n=161 vs 218) against
a registered floor of +5pp — the gate on everything below is met. But v7 was
registered to raise the *paraphrase* share and did the opposite: paraphrase
fell 1pp while identifiers rose 8pp. **Showing an agent a question did not make
it ask questions.** So v7 is not behaviourally the paraphrase arm it was
designed to be, and every v7 result below is weaker evidence about paraphrasing
than the design intended.

**Prediction 2 (v8 ≥ v7): directionally yes, unresolved.** Δ = +0.050
CI[−0.075, +0.175], 4 discordant to 2, p = 0.69.

**Prediction 3 (v7 ≤ v5): failed.** Δ = **+0.000** exactly, 1 discordant to 1.
An example demonstrating a paraphrase neither helped nor hurt. Registered
because the mechanism implied our own previous day's proposal was harmful; it
is not, and that is recorded as a miss rather than reinterpreted.

**Prediction 4 (cost does not rise): passed, and then some.** desc-v8 is the
*cheapest* arm and uses the fewest searches — 3.5 against rg's 5.0, a 30%
reduction. Agents given a naming example converge in fewer round-trips, which
is the §2 token argument landing in the one place it can be observed directly.

**The pooled accuracy result is a null.** +0.050 for desc-v8 over desc-v5, over
desc-v7 and over rg — the same figure three times, on 4-to-2 and 6-to-4
discordant splits with p between 0.69 and 0.75. The whole 40-instance frame
yields 4–6 discordant pairs. Reweighted to the true population, +0.044
CI[−0.099, +0.188]. §18.6's lesson applies exactly: a +0.050 on two discordant
pairs reversed on an independent 40.

**The pre-specified blind subgroup, and its honest size.** All three desc-v8
comparisons show blind +0.167 — and it is the *same* +0.167 each time, because
v5, v7 and rg all score 0.333 on the 6 blind instances while v8 scores 0.500.
**That is one instance.** The direction matches §19.2b's mechanism and the
named stratum is flat as predicted, which is the pattern registered in advance;
but a single blind instance is an anecdote with a confidence interval drawn
around it, and it is reported here only because pre-registering a subgroup
obliges reporting it whatever it says. §19.6 exists to give it 68 instances.

**desc-v6 is inert for the third time**: −0.025 pooled, −0.167 blind, and
+0.40 words with −5pp identifiers on n=245 queries. An explicit instruction to
describe behaviour and fold candidate names still does not change behaviour
where an example does. §7.3's example-beats-rule asymmetry now has three
independent replications and no counterexample.

**Limitations, none of which the numbers above disclose on their own.**

- *Retry conditioning.* 5 of 200 cells come from an arm that failed and was
  re-run until it succeeded (desc-v6 ×2, rg, desc-v5, desc-v8 ×1 each). That
  conditions those cells on termination, which plausibly correlates with
  finding the answer. Spread across arms, so it costs cleanliness rather than
  direction — but §19.6 removes it by raising the budget for every cell up
  front rather than retrying failures.
- *Budget censoring is not symmetric by construction, only by luck.* The guard
  truncates long runs, and long runs are exactly the informative ones. It fired
  on three instances; `Netflix__metaflow-2141` is the same instance §18.6
  documented.
- *An earlier claim of ours was wrong.* Re-running a failed cell at a higher
  budget was described mid-campaign as necessary for "equal budget across arms
  within an instance". It is not: the runs are stochastic (desc-v6 succeeded at
  $0.98 under a $1.00 cap and then failed at $1.51 under a $1.50 one), and a
  cap only binds when hit, so an arm finishing at $0.94 is unaffected by
  headroom it never used. The real hazard was the retry conditioning above.

**What shipped on this.** desc-v8 is now README's recommended tool description
(§6 called the tool prompt a deliverable and nothing shipped one), with the
evidence grade stated in the section: behaviour change measured, accuracy gain
directional and unconfirmed. `campaign.sh` defaults to it. **`cli.rs` keeps its
desc-v5-derived `--help` text, so `--help` and README now differ, and `--help`
still advertises "a question"** — the style §19.2b found worst when blind. That
divergence is deliberate, recorded in both places, and is what §19.6 decides.

### 19.6 Pre-registration: the blind-enriched frame (before the first row)

**The frame changes, and why.** §18.5 registered tier 2 as 560 × 2, rg against
desc-v5. This supersedes it: rg against **desc-v8**, on **204 instances in
equal strata** — all 68 `blind`, plus 68 `partial` and 68 `named`
(`tierframe.py`, seed 1). The dataset is 62/26/12, so a random 560-instance
frame spends 62% of its budget on the stratum where §19.2b predicts *no*
effect and still yields only 68 blind pairs. Equal strata buy the same 68 blind
pairs for ~$134 instead of ~$368.

**What that costs, stated up front:** a pooled mean over this frame is a mean
over a population one-third blind, and must not be quoted beside §16.9/§18.
`reweight.py` restores a comparable figure by weighting within-stratum deltas
to the true 348/144/68 shares, with a stratified bootstrap CI that will be
*wider* than the unweighted one because blind stays the noisiest stratum.

**Endpoints.** Primary: the **blind stratum**, n = 68 pairs,
`func_acc@10_tol`, exact McNemar. Secondary: partial and named strata; the
reweighted pooled estimate; `func_recall@10_tol` (continuous, because §18.5
notes the binary discards resolution wherever arms agree); cost; searches per
run — where §19.5 saw desc-v8 30% below rg and that deserves a powered test of
its own.

**Registered predictions.**

1. **An effect in `blind`, ≈0 in `named`.** This is the whole mechanism:
   §19.2b found descriptions and names indistinguishable when the query already
   carries the gold's vocabulary, and 13% against 50% when it does not. A
   pooled null with a blind effect is a **pass**. A blind null falsifies the
   mechanism on real agents, and §19.2b's observational finding should then be
   treated as a property of that offline replay rather than of agent behaviour.
2. **Searches per run stays below rg.** §19.5's 3.5 vs 5.0 is the most
   promising unregistered number in this project and is therefore exactly the
   one most likely to be noise.
3. **Cost does not rise.**

**Registered in advance because they would otherwise be tempting after the
fact:** the blind stratum is the primary *because* §19.5's blind signal was one
instance, not because it was positive; and if the blind effect appears with
`named` also moving, that is a general-competence difference rather than this
mechanism, and should be reported as failing prediction 1.

**Budget:** `--budget-usd 1.5` for every cell from the start, so no cell is
retried into existence and §19.5's retry conditioning cannot recur.

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

(First read at 407 cells gave blind +0.000 on n=67 and pooled −0.034; the last
cell moved no conclusion and only the third decimal of two arm means.)

§19.6 registered this in advance: *"A blind null falsifies the mechanism on
real agents, and §19.2b's observational finding should then be treated as a
property of that offline replay rather than of agent behaviour."* **It is a
blind null. The mechanism is falsified on real agents, and that sentence is
now binding.**

**The manipulation worked; the outcome did not follow.** This is not a failure
to move agents. The style shift replicated cleanly on a harder frame — 62%
identifier-shaped queries over 795 ranked searches, against desc-v5's 45%
baseline in §19.5. Agents read the example, imitated it, and wrote the queries
§19.2b said would find more. They did not find more. The dissociation is the
result: **a description can reliably change how an agent searches without
changing what it finds.**

**Tier-1's +0.050 reversed.** §19.5 measured desc-v8 over rg at +0.050 on 6-to-4
discordant pairs; at 204 pairs it is −0.034 on 7-to-14. The intervals overlap,
so this is a null replacing a null rather than a contradiction — but the point
estimate changed sign, which is exactly the §18.6 pattern (a +0.050 on two
discordant pairs reversing on an independent 40) happening to *our own shipped
change*, two days after we wrote §18.6 down.

**The two efficiency predictions passed, and replicated.**

- **Prediction 2 (searches below rg): passed.** 3.97 against 4.68 per run,
  median 2 against 3, paired Δ **−0.72**. §19.5 saw 3.5 against 5.0, so this is
  a second measurement of the same effect on a different frame.
- **Prediction 3 (cost does not rise): passed.** $0.281 against $0.290, paired
  Δ −$0.008.

So desc-v8 buys **fewer round-trips at no accuracy gain**, and the honest
summary of semgrep against ripgrep is unchanged from §18: **parity**, with a
negative point estimate here whose CI includes zero.

**What this does not test.** The campaign compared rg against desc-v8. It says
nothing at power about **desc-v8 against desc-v5** — the actual ship decision —
which still rests only on §19.5's +0.050 over 4-to-2 discordant pairs. Shipping
desc-v8 was justified by an argument this result removes one leg of; the
remaining legs are the replicated style shift and the replicated search
reduction, neither of which is an accuracy claim. README has been corrected
accordingly rather than left carrying a superseded number.

**Three ways this could still be wrong, in the direction of the hypothesis.**

- *The frame is deliberately hard.* 33% blind against a 12% population. The
  reweighted figure corrects for that and is also negative, so this does not
  rescue the result, but it does mean the pooled number is not comparable to
  §16.9/§18's.
- *Attrition is rg-favourable.* All 3 failures were rg cells truncated by the
  budget guard on long runs. Dropping those biases the surviving rg sample
  toward runs rg could finish — which flatters rg. 3 of 411 attempts, so the
  effect is small, but it points the same way as the result and cannot explain
  it away.
- *`func_acc@10_tol` is a blunt endpoint.* The co-primary recall is −0.025 with
  16-to-20 discordant, so the continuous measure agrees with the binary rather
  than hiding a signal inside it.

**What §19.2b now means.** Its measurement stands as a description of the
*offline replay*: on `guessplay.jsonl`, a blind paraphrase found the gold 13%
of the time against a blind name's 50%, with disjoint CIs. What does not
survive is the inference from that to agent behaviour. The likeliest
reconciliation is selection: in the replay, *which* queries an agent wrote was
already determined by what it knew, and §19.2b's stratification controlled for
vocabulary overlap with the gold but not for everything overlap proxies. An
agent instructed to write names writes names for targets it cannot name, and
those names are guesses, where the agents in the replay who wrote names were
often agents who had a name.

That is a hypothesis, not a finding, and it is the thing to test next — not
another description arm.

### 19.8 Three ways a search disappeared without being counted

Reading §19.7's own trajectories in the viewer turned up three channels through
which an agent's search vanished from every record the harness keeps. None
moves a published endpoint — all of them sit upstream of the metrics, which
score the agent's final answer — but together they are the difference between
"the tool was used 1,475 times" and what actually happened.

**1. Paths the scorer could not read (fixed).** `first_gold_hit_seq` matched a
two-component `dir/base` tail anywhere in the output. **semgrep prints paths
relative to the scope it was given; rg prints them as passed**, so
`semgrep q msal/` yields `application.py:162:` where `rg q msal/` yields
`msal/application.py:162:` — the tail matches ripgrep and misses semgrep. A
one-armed undercount: 13 of 204 desc-v8 rows had a gold hit the metric could
not see against 5 of 204 rg rows, including one where all four of the agent's
searches returned the gold file and the metric read `None`. Now resolved
against each invocation's own scope. Re-scoring moved 68 desc-v8 and 31 rg rows
and **changed no endpoint**: primary, co-primary, every secondary and the
reweighted pooled figure are identical to the digit.

**2. Calls the permission layer refused.** Claude Code evaluates a compound
command as a whole, so an agent typing `rg …; rg …; git log …` under
`Bash(rg *)` has *both searches* refused because of the `git`. The call never
executes, so the shim never runs and `n_invocations` cannot count it.
**288 refused calls across 88 tasks**; `Zulko__moviepy-2253` is the clean case,
where the rg arm ran zero real searches and its pane showed only the refusal.
Roughly symmetric (rg 19% of tasks, desc-v8 24%) and it does not move the
result — restricting to the 164 tasks where both arms genuinely searched leaves
the primary at **−0.030** against −0.034. Now counted by `capture.py` and
filterable in the viewer, which is where it should have been all along.

**3. The tool called as a tool that does not exist.** Four desc-v8 agents
emitted a structured `tool_use` block rather than a Bash command:

    {"name": "semgrep", "input": {"query": "groupby cohort rechunk order test",
                                  "path": "dask/tests/test_order.py"}}
    -> Error: No such tool available: semgrep

The input schema is the description's own signature. `semgrep "query" [path]`
reads as a spec with named slots, and an agent surrounded by JSON-schema tools
filled them in. **This happened 8 times across 4 desc-v8 tasks and 0 times to
rg in 204** — nobody mistakes ripgrep for anything but a shell command. It is
small and mostly self-correcting (3 of the 4 went on to run real searches, 3 of
the 4 answered correctly), so it does not explain the null. But it is a
*self-inflicted, one-armed* loss created by how we worded the treatment, which
is the kind of thing that is invisible in aggregate and obvious in a
trajectory.

**What to do about each.** (1) is fixed. (2) wants the allowlist widened to
permit read-only `git log`-style calls so a chained command stops costing an
arm its searches, and `run.py` recording denials into the search stats rather
than leaving them only in transcripts. (3) is a description question, not a
harness one: a line saying the tool is run *in Bash* would likely end it, at
the cost of another arm and another campaign to prove it. None of the three is
worth re-running §19.7 for — the largest, (2), moves the primary by 0.004.

**The pattern worth keeping.** All three were invisible in every table and
obvious the moment someone opened a single task and read it against its own
numbers. That is now the third time in this project (§16.11 and §17 being the
others) that trajectories caught what aggregates could not, and it is the
argument for the viewer existing at all.

### 19.9 What agents do with a pipe, and `sg`

§19.8 left two channels open and pointed at the trajectories rather than the
tables. Reading them produced a measurement, a defect, and a rename.

**The denial trigger, diagnosed.** Recovering the command behind each refusal
from the transcripts — 144 of 288 are reconstructible — the trigger is **not**
compound commands, which §19.8 guessed. It is *any binary in the command outside
the allowlist*, wherever it sits. First binaries: `python3` 62, `git` 23, `rg`
13, `find` 10, `grep` 9, `semgrep` 5, `cat`/`awk` 3 each. Of the 18 refusals
whose command *begins with the arm's own permitted tool*, nearly all die on what
they pipe or chain into, not on the tool; only 2 were the quoted-`|`-read-as-a-
pipe false positive that looked likely. **The allowlist is behaving as designed
and stays as it is.** §19.8's proposed widening is withdrawn: it would have
loosened a gate that is not the problem.

**Piping, measured.** Of commands beginning with the search tool, rg is piped in
**252 of 863 (29%)** and semgrep in **32 of 778 (4%)**. Targets: `head` 237, rg
27, grep 15, sed 9, tail 4, xargs 3, wc 3, sort 2, awk 1.

**79% of all piping is `head`, which `-k` already does** — and that is the most
plausible reading of the 7× gap. rg has no bounded mode, so an agent bounds it
by hand; semgrep is bounded by construction, so the reflex mostly falls away.
It does not fall away entirely: agents still write `-k 5 | head -30`, belt and
braces, which is a small argument that `-k` is not as legible as we think.

Of the 32 semgrep pipes, **2** wanted something `-k` cannot give, and both are
the same thing spelled two ways — narrowing to a line range, as
`awk -F: '$2 < 2297'` and `grep -E "8[0-9][0-9]|9[0-3][0-9]"`.

**The defect that made piping unsafe.** `sg -e "def " big/ --all | head -1`
printed a Rust panic — `failed printing to stdout: Broken pipe (os error 32)` —
where rg exits quietly. Rust sets `SIGPIPE` to `SIG_IGN` before `main`, so the
write returns `EPIPE` and `println!` panics. It only fires past the ~64 KB pipe
buffer, so FIXES.md #26's `-M 200` hid it in ranked mode while `--all` still
reached it. Restoring the default disposition (ripgrep's own fix, one call)
makes the process die of SIGPIPE like every other filter. **`| head` is the most
common thing anyone does to this tool and it could crash it**, unnoticed for as
long as the tool has existed, because nothing in the eval harness pipes.

**What shipped as a result.**

- **`--lines A-B`** absorbs the one pipe `-k` could not serve. It needs no second
  binary, which matters where the caller's shell may refuse one.
- **`-` reads paths from stdin**, so `find … | sg "query" -` works without
  `xargs`. Recorded as speculative: 3 xargs uses in 1,641 invocations is not
  demand, and it is here because it composes.
- **`sg`**, alongside `semgrep`. Two `[[bin]]` targets over one source: the name
  is short enough to type all session, and nine scripts plus the test harness
  resolve `semgrep` by name, so breaking them to save a symlink is a bad trade.
  Env vars, `~/.cache/semgrep`, `.semgrep/` and the `semgrep: ` stderr prefix all
  stay, which leaves `sg` printing `semgrep: …` — deliberate, and the cheap half
  of a rename whose expensive half invalidates every built index.

**desc-v9, shipped unmeasured.** desc-v8 with the name changed to `sg` and one
clause folded into the identity sentence — *a ranked code search you run with
Bash* — aimed at §19.8's third channel, agents calling the tool as a typed API.
**It changes two things at once and therefore attributes neither.** §16.6 and the
`search` name-gravity arm both say a name alone can move behaviour, so if a
later campaign moves, the honest reading is "v9 moved", not "the Bash clause
worked". That was the accepted trade for shipping now rather than spending
another frame on a defect worth 4 tasks in 204.

### 19.10 Pre-registration: three arms, and what power is actually for sale

§19.7 left two things open. **desc-v8-or-v9 against desc-v5 has never been
measured at power** — the ship decision still rests on §19.5's +0.050 over four
discordant pairs — and **desc-v9 has never been measured at all**, having
shipped unmeasured by decision (§19.9). This campaign is rg, desc-v5 and
desc-v9 on §19.7's own 204 instances.

**What a powered run can buy here, computed before proposing one.** The observed
discordant rate on `func_acc@10_tol` is 10.3%, which fixes the smallest
detectable accuracy effect at every frame size available:

| frame | smallest accuracy effect, 80% power |
|---|---|
| 204 | ±0.060 |
| 300 | ±0.050 |
| 560 — *every instance in the dataset* | ±0.038 |

Every effect this project has measured is ≤0.05, and §19.7's own −0.034 would
need **682 instances**. **Accuracy cannot be powered at any price on this
dataset.** That is not a reason to skip the campaign; it is a reason to stop
calling accuracy its primary endpoint, and to publish the bound beside every
accuracy null rather than letting a table imply an absence it cannot support.

One endpoint can be powered, and it is the one with a replicated effect:

| endpoint | observed Δ (§19.7) | instances for 80% power |
|---|---|---|
| **searches per run** | **−0.72** | **226** |
| `func_acc@10_tol` | −0.034 | 682 |
| `func_recall@10_tol` | −0.025 | 879 |
| cost per run | −0.008 | 1,801 |

**Primary: searches per run, desc-v9 vs rg**, paired within instance, bootstrap
CI over instance-level differences.

**Registered power: 76%, not 80%.** At n=204 with Δ=−0.72 and sd=3.84 the power
is 76%; 80% wants 226 instances and 90% wants 300. The frame was chosen for
exact comparability with §19.7 — `tierframe.py` at seed 1 reproduces its
instance set, verified — over the extra 22 instances. **A null here therefore
carries a real chance of being a miss rather than an absence, and saying so is
part of the registration rather than an excuse available afterwards.**

**Registered prediction:** desc-v9 uses *fewer* searches than rg, by roughly the
−0.72 of §19.7 and the −1.5 of §19.5. A positive delta falsifies the efficiency
claim outright.

**Secondaries, pre-specified and none of them powered:** `func_acc@10_tol` over
all three pairs (bounded to ±0.060, Holm-corrected across the three),
`func_recall@10_tol`, cost per run, and the blind/partial/named strata.

**What each comparison can and cannot mean.** `desc-v9 vs rg` is the product
claim. `desc-v9 vs desc-v5` is the open ship question and is **confounded by
construction** — v5→v9 bundles the naming example (§19.2b), the `sg` rename and
the Bash clause, so a difference says "v9 differs from v5" and never which part
did it. `desc-v5 vs rg` replicates §18's null on a harder frame, free with the
other two. `desc-v9 vs desc-v8` is **exploratory only**: same instances,
different campaign, so not paired within a run.

All three arms are re-run, rg included, rather than reusing §19.7's rg rows.
Nothing in the `sg`/SIGPIPE/`tool_of` work touches ripgrep, but the primary
comparison *is* desc-v9 vs rg, and $60 to have both sides produced under one set
of conditions is cheaper than arguing the difference away later.

**Registered now because it would be tempting later:** if searches fall and
accuracy stays flat, that is §19.7's dissociation replicated — the tool doing
the same work in fewer round-trips — not a disappointment. Reporting it as a
loss because the accuracy column did not move would be reading the campaign
backwards.

### 19.11 The three-arm result: a null at 44% power, and a number that reproduced

rg, desc-v5 and desc-v9 on §19.7's own 204 instances. 612 of 612 cells, 613
attempts, one `parse_error` recovered on retry, $169.62.

**The registered primary is a null, and an underpowered one.**

| | searches/run | Δ | 95% CI |
|---|---|---|---|
| **desc-v9 vs rg** | 4.15 vs 4.59 | **−0.441** | **[−0.912, +0.039]** |
| desc-v5 vs rg | 4.27 vs 4.59 | −0.314 | [−0.814, +0.172] |
| desc-v9 vs desc-v5 | 4.15 vs 4.27 | −0.127 | [−0.490, +0.225] |

The interval crosses zero by 0.039. §19.10 registered 76% power, sized on
§19.7's −0.72; the effect came in at −0.44, and **at that effect the realised
power is 44%** — 488 instances would have been needed for 80%. So this null is
closer to a coin flip than to evidence of absence, which is what §19.10
committed to saying rather than discovering afterwards. The efficiency claim is
now *weaker* than when it had two consistent point estimates behind it: −1.5,
then −0.72, now −0.44, each smaller than the last, which is the shape of a
regression to no effect at all.

**Accuracy, bounded to ±0.060 as registered:**

| | Δ `func_acc@10_tol` | discordant | p | blind stratum |
|---|---|---|---|---|
| desc-v9 vs rg | −0.044 | 7/16 | 0.093 | **+0.000** |
| desc-v9 vs desc-v5 | −0.010 | 6/8 | 0.791 | **+0.000** |
| desc-v5 vs rg | **−0.034** | 7/14 | 0.189 | **+0.000** |

**Two results survive being nulls.** `desc-v5 vs rg` came out at −0.034 — the
same figure to three decimals as §19.7's `desc-v8 vs rg`, from an independent
campaign with a different treatment arm. A number that reproduces exactly
across frames is worth more than most of the deltas in this document. And the
**blind stratum is +0.000 in all three pairs**, the third independent time it
has landed on exactly zero. §19.2b's mechanism predicted the effect would live
there; three campaigns now say it does not live anywhere.

**The ship question, answered as well as this dataset can.** desc-v9 ≈ desc-v5
on every endpoint: −0.010 accuracy, −0.127 searches, −$0.023 cost. The style
shift replicated (64% identifier-shaped queries against desc-v5's 50%, n=851
and 880). So the description reliably changed *how* agents search and moved
*nothing* about what they found — §19.7's dissociation, now on the pair that
actually shipped. Since v5→v9 bundles the example, the `sg` rename and the Bash
clause, the null is at least unambiguous: no component of it mattered enough to
show.

**What the whole §19 arc adds up to.** Six description arms, three campaigns,
~$360. Descriptions move agent behaviour reliably and measurably — the
identifier share moves 15–20pp on demand, replicated four times. **None of it
moves the answer.** The honest summary of semgrep against ripgrep is unchanged
from §18: parity, with negative point estimates whose intervals include zero.
The remaining ceiling is where §17.6 put it — the embedding model, not the
description, not the ranking parameters, and not, on this evidence, how the
agent is told to phrase a query.

## 20 Pruning the chunk before it is embedded, and budgeting by content

§14 asked what *rendering* to hand the embedder and found `split` + `sif`
(§14.4). It never asked the prior question: of the tokens in a chunk, which
ones should be there at all. Under uniform mean pooling that is not a
rhetorical distinction — every surviving token takes an equal share of the
vector, so dropping one hands its mass to the rest. Pruning is reweighting.

### 20.1 What is actually in the token stream (2026-08-05)

Rendering the vscode chunk below through the shipped pipeline turned up a
defect before any experiment ran.

```
src/vs/workbench/contrib/searchEditor/browser/searchEditorActions.ts
export function computeBackoffDelay(attempt: number): number {
  const jitter = Math.random() * BASE_DELAY_MS;
  return Math.min(MAX_DELAY_MS, 2 ** attempt * jitter);
}
```

**`function` and `export` are not in the `split-nokw` keyword table.** Nor are
`type`, `readonly`, `declare`, `null`, `undefined`, `true`, `false`, `as`,
`from`, `of` — 43 tokens missing in all, checked against the seven corpus
languages. The table has `func`, `fn` and `def` but not the spelling
TypeScript and JavaScript use, so on the corpus where §14.4 measured `split`'s
largest win the two most common boilerplate tokens in the language were being
embedded as content. `split-nokw` dropped 2 tokens from the 32 above; it
should have dropped 4.

The table is left **frozen** and the repair added beside it as
`KEYWORDS_EXTRA`, so `prune-kw` is an attributable arm rather than a silent
edit to a published condition. `the_frozen_table_really_was_missing_function_and_export`
pins the finding as a test.

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

At `prune-decl` the body reduces to `compute backoff delay attempt jitter` —
which is the intent, and which exposes the second finding: **11 of the 16
surviving tokens are the file path.** 69% of the pooled mass says where the
file lives. Prune the body and the path's share rises mechanically, so every
window in a long file converges toward one vector and within-file
discrimination fails exactly where a file has the most chunks. The path also
repeats itself — `searchEditor/` and `searchEditorActions` both say "search
editor". Hence `PathRender`, orthogonal to the tier: `full`, `dedupe`, `tail`
(last two segments), `scaled` (deduped, capped at 25% of the body's count).

Pruning is **document-side only**. A natural-language query has no declaration
sites, and the low-signal table would eat real query words — "parse a number
from a string" is three of six tokens gone. `render_query` therefore stops at
keyword pruning. This does not break the one-space invariant: that constrains
the token→vector mapping, not what content each side contributes.

### 20.2 A line is not a unit of content

`ChunkParams.window` is 32 lines. Measured over the benchmark corpora, non-
whitespace characters per 32-line window:

| corpus | p10 | median | p90 | p99 | max |
|---|---|---|---|---|---|
| vscode (ts) | 563 | **931** | 1,418 | 2,253 | 6,767 |
| linux (c/h) | 504 | **693** | 1,012 | 1,524 | 2,885 |
| tokio (rs) | 429 | **677** | 975 | 1,409 | 3,283 |
| jekyll (rb) | 386 | **675** | 874 | 1,106 | 1,419 |

A vscode chunk carries 35% more content than a linux chunk at the same line
count, the p10→p90 spread inside one corpus is 2.5×, and the worst vscode
window holds 6,767 non-whitespace characters — seven times the median, pooled
into one vector by a uniform mean. `ChunkParams.budget` cuts line-aligned
windows to a content budget instead, carrying the overlap across as a fraction
(25% at the defaults) so it is a reparameterization rather than a second
overlap policy. The unit is cAST's (arXiv 2506.15655), chosen for its reason.

Two external results bear on the sizing, and they disagree with our defaults
in the same direction. The controlled study of 864 RAG code-completion
settings (arXiv 2605.04763) found ~2,000 non-whitespace characters optimal and
**function-level chunking never Pareto-optimal**, trailing by 3.57–5.64pp;
cAST budgeted 4,000. Our median chunk is 700–930. That study also found
retriever choice (BM25 vs three dense models) worth ≤1.11pp against a
3.43–6.51pp spread between chunking strategies — if that transfers, chunking
is a larger lever than the bm25-vs-semantic axis §14 has been sweeping. It may
not transfer: every retriever there was contextual or lexical, none was a
static bag-of-words model, and no published comparison we could find tests
chunk granularity against one.

### 20.3 Pre-registration (written before the first row)

Scoring as §14: `run_eval.py`, semantic mode, paired per query, 2,000-resample
bootstrap CIs, exact sign tests, leakage printed above every table.

**Corpus assignment, and one confound.** The tiers are a *rendering* change and
run on all five sets including cosqa. The budget arm is a *chunking* change and
runs on vscode/linux/tokio/etcd only: cosqa's corpus is one short Python
function per file (20,604 docs, `eval/REPORT.md:36`), so a 32-line window and
an 800-character budget cut it into the same single chunk and the comparison is
structurally empty. Reporting a null there would be reporting the corpus.

Registered predictions, in falsifiable order:

1. **`prune-kw` gains on TS and does nothing on C.** `function`/`export` are
   6% of the example's tokens; linux has neither spelling. Floor: vscode
   semantic R@5 ≥ `split-nokw` + 0.01, and |Δ| < 0.01 on linux. A material
   linux move means the extra 43 words are doing something other than removing
   boilerplate.
2. **`prune-lex` is where a gain lives, if one does.** It removes 6 of 30
   remaining tokens. Floor: ≥ `prune-kw` on vscode and etcd `direct`. **If it
   loses on 3 of 5 corpora the tier is dead** — a hand-written stoplist that
   removes signal is not worth maintaining.
3. **`prune-decl` loses on `direct` and may win on `blind`.** It deletes the
   call-site tokens a named-identifier query matches, and 71.5% of tokio
   `direct` queries contain the gold identifier (§13.1). Registered:
   `prune-decl` < `prune-lex` on `direct`, ≥ on `blind`. Winning both means
   references are noise and the ladder should go further; losing both means
   declaration-position is the wrong axis.
4. **`prune-soft` ≥ `prune-decl` everywhere.** Weighting should dominate
   deletion when the deleted tokens are sometimes the answer. A reversal says
   dilution costs more than coverage, which would redirect the whole design.
5. **Path handling matters only at the aggressive end.** Registered: at
   `prune-lex` the three path arms sit within 0.02 of each other; at
   `prune-decl`, `scaled` ≥ `full` + 0.02. If path handling moves nothing at
   `prune-decl`, the 69% share is not costing anything and two of the three
   arms were unnecessary.
6. **SIF partially subsumes `prune-lex`.** Rarity weighting already demotes
   corpus-common tokens, which is most of the stoplist. Registered:
   Δ(`prune-lex` − `prune-kw`) is smaller with `--sif` than without, on every
   corpus. If the two are additive, the stoplist is removing something
   frequency cannot see; if SIF erases the tier entirely, the list should be
   deleted rather than tuned.
7. **The budget at parity is a no-op.** `chars-800` vs `lines-32`, |Δ R@5| <
   0.02 on all four corpora. A material *win* at parity is attributable to
   capping the tail rather than to budgeting, and §20.5 then sweeps
   800/1600/2400 to test the published optimum. A material loss means
   line-alignment interacts with overlap in a way this reparameterization got
   wrong.

**Tripwire.** bm25 cells must be identical across tier arms up to MMR, which
reads the embedding matrix (§14.4 point 6). Any other bm25 movement is a bug,
not a result.

### 20.4 How to run it

```
cargo build --release
eval/prune.sh                  # every corpus, every arm
eval/prune.sh vscode tokio     # only those
python3 eval/diff.py --base prune-kw --cand prune-lex prune-decl prune-soft
```

Results land in `eval/results/lever-<corpus>-prune-<tag>.json`, the lever
campaign's naming, so the existing comparator reads them unchanged. The script
skips any condition whose output already exists; delete the file to re-score.

### 20.5 Run 1, and the defect it was measuring instead

Four corpora completed (tokio, etcd, vscode, cosqa; linux was stopped
mid-run). Semantic mode, paired per query, 2,000-resample bootstrap CIs,
exact sign tests. Results retained as `lever-<corpus>-prune-qsym-<tag>.json`
— `qsym` for query-symmetric, which is the thing this run turned out to be
about.

**Against the incumbent `split-nokw`, R@5 on the primary cell:**

| arm | tokio | etcd | vscode | cosqa |
|---|---|---|---|---|
| `split-nokw` | 0.515 | 0.620 | 0.750 | 0.117 |
| `prune-kw` | **0.585** | **0.675** | **0.780** | 0.122 |
| `prune-lex` | 0.590 | 0.645 | 0.770 | 0.111 |
| `prune-soft` | 0.580 | 0.645 | 0.745 | 0.118 |
| `prune-uniq` | 0.580 | 0.590 | 0.755 | 0.099 |
| `prune-decl` | 0.395 | 0.420 | 0.545 | 0.072 |

`prune-kw` is +0.070 on tokio (CI [+0.030, +0.115], p=0.003) and +0.055 on
etcd (CI [+0.010, +0.100], p=0.027).

**Against the §14.4 champion `split`+`sif`, which is the bar that matters:**

| arm | tokio | etcd | vscode | cosqa |
|---|---|---|---|---|
| champion | 0.545 | 0.595 | 0.825 | 0.188 |
| `prune-kw` Δ | +0.040 n.s. | **+0.080** p=0.002 | −0.045 n.s. | **−0.066** p<0.001 |

So the headline against `split-nokw` was flattered by a weak baseline. The
repaired table beats the champion on one corpus of four and **loses on CoSQA**,
the only set with real human queries and the one §12 says to prefer for
quality claims.

**Predictions, scored:**

1. **Partial.** The repair gains, but not where registered: it was predicted as
   a TypeScript effect and the largest gain is tokio (Rust), which has no
   `function` or `export` — it has `as`, `where`, `type`, `in`, `true`,
   `false`. The 43 missing words were not a TS oversight, they were a general
   one.
2. **Failed, by its own kill condition.** `prune-lex` − `prune-kw` is +0.005,
   −0.030, −0.010, −0.012 on the four corpora: worse on three, all n.s. The
   registered floor was "if it loses on 3 of 5 the tier is dead". A
   hand-written stoplist adds nothing over fixing the keyword table.
3. **First half confirmed, hard** (−0.195 to −0.225 on `direct`, p<0.001 on
   every corpus). **Second half unsupported**: on `blind`, `prune-decl` is
   −0.015 / +0.017 / +0.000 — one nominal win, one loss, one tie, none
   significant. Registered reading: declaration-position is the wrong axis.
4. **Confirmed.** `prune-soft` beats `prune-decl` by +0.185 to +0.225
   everywhere and is statistically indistinguishable from `prune-lex`. Weight
   dominates deletion when the deleted tokens are sometimes the answer.
5. **First half holds** (path arms within 0.025 at `prune-lex`). **Second half
   is backwards**: at `prune-decl`, `scaled` is the *worst* arm on all four
   corpora (tokio 0.375 vs full 0.395, vscode 0.490 vs 0.545). Capping the path
   loses more than path dominance costs — at 69% of the tokens the path is
   still carrying signal, not crowding it out. `tail` is the best of the four
   on tokio and full on the rest.
6. **Mixed.** SIF helps CoSQA enormously (`prune-kw`+sif +0.048, p<0.001) and
   hurts etcd (−0.085, p=0.001). No clean statement about subsumption.
7. **Holds.** `chars-800` vs `lines-32` at `prune-kw`: −0.010, −0.030, +0.015,
   all n.s. The reparameterization is free, as registered. Phase 2 (the
   800/1600/2400 sweep) is therefore worth running.

**The defect.** `render_query` was documented as "the tier's normalization,
none of its pruning" and did not implement that: it kept
`Keywords::Extended`, so the query side was pruned too. On a chunk those words
are boilerplate. On a query they are English. Measured on CoSQA's 1,200 real
queries, the extended table removes **1,194 of 7,564 query tokens (15.8%),
affecting 771 queries** — against 217 tokens (2.9%) for the frozen legacy
table. `"python logging can not create file"` loses `not`; `"how to prompt an
input in python"` loses `in`; `"python mkdirs with permission"` loses `with`.

It looked like query-side damage charged to a document-side lever, falling
hardest on exactly the corpus where the arms lost. **That reading was wrong,
and §20.6 is the correction** — removing the query-side pruning was tried and
lost everywhere, including on CoSQA. What §20.5 recorded as a defect is the
better configuration. The paragraph above is left standing because the
measurement in it is real (15.8% of CoSQA query tokens do go) and only the
inference from it was mistaken.

Run 1 is retained as `lever-<corpus>-prune-qsym-<tag>.json` and run 2, which
tested the correction, as `prune-qasym-`. `qsym` is the shipped behavior.

### 20.6 The correction that lost: prune both sides or neither

§20.5 hypothesised that the prune tiers lost on CoSQA because `render_query`
was applying the extended keyword table to queries, and that queries should be
normalized and not pruned. Run 2 tested exactly that, every arm, four corpora.

**Paired Δ R@5, asymmetric (query not pruned) minus symmetric:**

| corpus | `prune-kw` | `prune-lex` | `prune-kw`+sif |
|---|---|---|---|
| tokio | **−0.040** p=0.039 | **−0.055** p=0.013 | **−0.075** p=0.001 |
| etcd | −0.020 n.s. | −0.025 n.s. | +0.000 n.s. |
| vscode | −0.010 n.s. | −0.025 p=0.062 | −0.015 n.s. |
| cosqa | −0.003 n.s. | −0.003 n.s. | **−0.014** p=0.021 |

**Every delta is negative or zero — 11 of 12, across 4 corpora — and the
CoSQA arms the change was written to rescue lost too** (`prune-soft` −0.010
p=0.012, `prune-uniq` −0.014 p<0.001, `lex-sif` −0.014 p=0.012). The
hypothesis is refuted on its own chosen corpus.

The mechanism, stated exactly, because a loose version of it predicts the
wrong things. Ranking is cosine against a fixed query, so `|q|` is constant
across documents and cancels; the score decomposes additively over the query's
tokens:

    score(d)  ∝  <C_q, d> + <K_q, d>        C = content tokens, K = keywords

Prune neither side and `<K_q, d>` is a real matching term — weak, but both
sides carry the vocabulary. Prune both and it vanishes. Prune documents only
and `K_q` survives in the query while every document has had its counterpart
deleted: word vectors are not orthogonal, so the term is still non-zero and
still *varies by document*. It is an additive term with nothing to align
with, and it reshuffles the ranking on noise. The query's mass is not "lost"
— the normalization is a constant across documents — it is converted into a
document-varying error term. Losing `not` from "python logging can not create
file" costs less than keeping a `not` that every candidate chunk has had
removed.

Query-side pruning is therefore not a feature. It is the removal of a noise
term that chunk-side pruning manufactures.

The operative rule, and it is more general than this lever: **prune the two
sides identically, or do not prune at all.** Pruning a query structurally
cannot mirror — declaration position, which prose has none of; the low-signal
table, which eats "parse a number from a string" — must therefore stay
document-side, and the tiers keep that split. Pinned by
`keyword_pruning_is_symmetric_and_the_rest_is_not`.

**Where that leaves §20 against the bar.** Symmetric `prune-kw` versus the
§14.4 champion (`split`+`sif`): tokio +0.040 n.s., etcd **+0.080 p=0.002**,
vscode −0.045 n.s., cosqa **−0.066 p<0.001**. One win, two nulls, one loss —
on the corpus §12 says to weight most. The keyword-table repair is a real
defect fixed and it is not, on this evidence, a shipping win; `split`+`sif`
survives §20 as the champion. What §20 produced instead is three negative
results worth having (the stoplist adds nothing over the repair,
declaration-position deletion costs a fifth of recall, path capping hurts),
one general rule (prune symmetrically), and one lever that is free at parity
and still untested at size — the character budget, whose 800/1600/2400 sweep
is the live thread into §20.7.

### 20.7 The symmetry confound in §20.5, and the arm that would settle it

The §20.6 mechanism revises §20.5's reading of its own predictions, so the
revision is recorded rather than left implicit.

Sort the tiers by whether a query can mirror them:

| tier | mirrorable query-side? | how it did |
|---|---|---|
| `prune-kw` | yes — one table, both sides | best prune arm |
| `prune-lex` | yes in principle, **never run that way** | −0.005 to −0.030 vs `prune-kw` |
| `prune-decl` | **no** — prose has no declaration sites | −0.195 to −0.225 |

**The ranking tracks symmetry exactly.** `prune-lex` and `prune-decl` were
both specified as document-side, on the reasoning in §20.1 that a query
cannot mirror them. For `prune-decl` that is true. For `prune-lex` it is not:
the low-signal table applies to a query as readily as to a chunk, and it was
withheld on the same intuition §20.6 has since shown to be backwards.

So §20.5's conclusions for predictions 2 and 3 — "a hand-written stoplist adds
nothing over fixing the keyword table" and "references carry signal the ladder
cannot afford to delete" — are confounded with asymmetry, and the second may
be measuring nothing but it. Both stand as *measurements of the arms as run*;
neither is safe as a statement about pruning.

One arm discriminates for the stoplist: **`prune-lex` with the low-signal
table applied to queries as well.** If it reaches `prune-kw`, asymmetry was
the whole effect and the stoplist is neutral-to-good. If it still trails, the
list removes signal and prediction 2 stands as originally read.

Nothing discriminates for `prune-decl`, and that is the more interesting
half. There is no symmetric version to run — a natural-language query has no
declaration sites to keep. If §20.6's rule is right, declaration-position
pruning is not a weak lever but a **structurally inapplicable** one for a
bag-of-words retriever: any document-side transform a query cannot mirror
buys a noise term proportional to how much it removes, and `prune-decl`
removes the most. That predicts the observed ordering
(`kw` > `lex` > `decl`) from symmetry alone, without reference to what the
tokens mean — a claim that would generalize past this implementation and past
this corpus, and one §20.8 should try to break rather than confirm.

### 20.8 The symmetry arm: a null, and a dose-response that holds

`prune-lex-sym` and `prune-uniq-sym` render documents identically to `prune-lex`
and `prune-uniq` and mirror the pruning onto the query. §20.7 asked whether
asymmetry explained the stoplist's shortfall.

**It did not.** Mirroring the low-signal table onto the query moves nothing:

| corpus | `lex` | `lex-sym` | paired Δ |
|---|---|---|---|
| tokio | 0.590 | 0.590 | +0.000 [−0.020, +0.020] |
| etcd | 0.645 | 0.635 | −0.010 [−0.030, +0.010] |
| vscode | 0.770 | 0.760 | −0.010 [−0.025, +0.000] |
| cosqa | 0.111 | 0.109 | −0.002 [−0.008, +0.004] |

All n.s. And `lex-sym` still fails to beat `prune-kw` (etcd −0.040 p=0.077,
cosqa −0.013 p=0.052). **§20.5's prediction 2 stands as originally read**: the
hand-written stoplist adds nothing over repairing the keyword table, and the
symmetry confound §20.7 raised does not rescue it. Mirrored dedupe is likewise
a null (−0.025 to +0.003, all n.s.), as §20.7 predicted for it — dedupe removes
repetitions, not a token class, so no vocabulary mismatch arises to fix.

This is *not* a contradiction of §20.6, and the reason is the useful part. The
mechanism predicts the noise term `<K_q, d>` scales with how much of the query
belongs to the pruned class. Measured on the query side:

| pruned class | share of query tokens | cost of leaving it unmirrored |
|---|---|---|
| low-signal table | 2.8–7.4% | **0.000 to −0.010** (n.s., §20.8) |
| keyword table | 14.9–15.8% | **−0.040** tokio, negative on 4/4 (§20.6) |
| non-declaration tokens | ~100% (a query is all references) | **−0.195 to −0.225** (§20.5) |

Three magnitudes, three effect sizes, monotone. The mechanism was inferred from
the middle row and it postdicts the other two, which is more than it was fitted
to do. It also sharpens the §20.7 claim about `prune-decl`: its mismatch is not
merely unmirrorable, it is *maximal* — every token a natural-language query
contains is a reference, and references are exactly what it deletes from the
documents. The prediction that would break this: a document-side transform
removing ~15% of query-mirrorable vocabulary should cost ~0.04 when unmirrored,
whatever the transform is about.

**Where §20 ends** is §20.9 — this paragraph originally read "no arm beats
`split`+`sif` on more than one corpus", which linux overturned. See below.

### 20.9 Linux, and the size sweep that went the wrong way

**A correction first.** §20.5 and §20.8 both say linux was not scored. Three
linux arms — `nokw`, `kw`, `lex` — did land, written by the interrupted run of
2026-08-04 23:39–23:42 before the query-side change, i.e. under the `qsym`
configuration, and they were swept into the `prune-qsym-*` rename with
everything else. They are valid and they change the headline.

**linux (C, 84k files, 199 `direct` queries), semantic R@5:**

| arm | R@5 | vs incumbent | vs champion (0.734) |
|---|---|---|---|
| `split-nokw` | 0.764 | — | +0.030 n.s. |
| `prune-kw` | **0.814** | +0.050 p=0.006 | **+0.080 [+0.025, +0.141] p=0.011** |
| `prune-lex` | **0.824** | +0.060 p=0.008 | **+0.090 [+0.035, +0.146] p=0.002** |

So the repair's record against the champion across five corpora is **two
significant wins (etcd +0.080, linux +0.080), two nulls (tokio, vscode), and
one significant loss (CoSQA −0.066)** — and the wins are the two largest trees.
That is a materially better result than §20.8 recorded, and it does not settle
the question: the loss is on the only corpus whose queries nobody here wrote,
which §12 says to weight most. Reporting it as a win would be picking the
favourable four-fifths.

Note also that linux is the one corpus where `prune-lex` is the best arm.
Prediction 2's kill condition was "loses on 3 of 5"; it lost on 3 of 5 and is
dead as a general lever, but the exception is the largest corpus and is not
noise.

**The sweep.** Rendering held at `prune-kw`, chunk budget swept, four corpora:

| corpus | lines-32 | chars-800 | chars-1600 | chars-2400 |
|---|---|---|---|---|
| tokio | 0.585 | 0.575 | 0.550 | 0.525 |
| etcd | 0.675 | 0.645 | 0.655 | 0.655 |
| vscode | 0.780 | 0.795 | 0.765 | 0.760 |
| linux | 0.814 | 0.804 | 0.774 | 0.759 |

**Every corpus is flat or declining as the budget grows, monotone on three of
four.** No single comparison reaches significance, but 11 of 12 against the
32-line window point down, and the two largest corpora lose the most at 2,400
(tokio −0.060, linux −0.055, both p≈0.07–0.08).

The external result does not transfer, and the reason is the same one §20.6
turned on. The controlled study that found ~2,000 characters optimal (arXiv
2605.04763) used BM25 and three transformer retrievers; cAST used contextual
embedders. Those have attention to spend across a long chunk. This engine pools
by a **uniform mean** — a bigger chunk is a strictly more diluted vector, with
no mechanism to weight the part that matters. Chunk-size guidance from the RAG
literature should be assumed not to transfer to a static bag-of-words retriever
until measured, in either direction.

The budget is still worth keeping: it is free at parity (§20.3 prediction 7,
confirmed twice), it equalises the 35% language-density gap between vscode and
linux, and it caps a tail where one 32-line vscode window holds 6,767
non-whitespace characters. It is a fix for the worst chunks, not a knob to turn
up.

**Final ledger for §20.** One shipped defect found and fixed (43 words missing
from the keyword table). One arm that beats the champion on the two largest
corpora and loses on the most trustworthy one — not a shipping decision this
evidence can make alone. Five negative results that close off directions
(stoplist, declaration pruning, path capping, dedupe, larger chunks). One rule
with a quantitative form: mirror what the query can mirror, and the cost of not
doing so scales with the unmirrored share. `split`+`sif` remains the default
until the CoSQA loss is understood.
## 21 Renderings at agent scale: the free gate

§20 measured five chunk renderings against *generated* queries and produced a
split verdict — `prune-kw` beats the §14 champion on the two largest corpora and
loses on CoSQA. §9.7's standing rule is that engine changes are gated on
agent-level evidence, and §14.5 already used the offline agent-query instrument
to refuse `split+sif` once. This section runs the four renderings against the
queries agents actually typed.

### 21.1 Pre-registration (written before the first row)

**Instrument.** `eval/locbench/guessplay.py` over `eval/queries/guesses-v1-descv9.jsonl`
— re-harvested from `runs/`, desc-v9 only: **854 ranked queries over 186
instances**. Five index configs: `default` (shipped `none`), `split`,
`prune-kw`, `prune-decl`, `champion` (`split`+`sif`). Modes semantic (shipped)
and bm25 (tripwire). Scope policy `orig`. No API spend.

**Why `guesses-v0` is not the corpus.** Its 624 ranked rows are entirely V4-era
conditions (`semgrep`, `sg-*`) with **zero** `desc-*` rows, at a 20–41% ranked
share; and 208 of the 624 (33%) are file-scoped rows written before the §16.11
fix, scoring 0.000 in every config. Re-using it would compare a fresh treatment
against a control a third of which is a hardcoded zero. The old file is retained
as the §16.5/§17.2 artefact; run 21 writes to `guessplay-v1.jsonl` from empty,
and every row carries `bin_sha256`.

**The dose, stated in the registration.** `cache::discover` returns `None` for a
non-directory root (`cache/mod.rs:74-76`), so a **file-scoped search finds no
index at all** and the cold path renders from the *search flag*
(`search/stream.rs:77,126`). Measured on the shipping corpus: **394 of 854
(46%) of desc-v9 ranked searches are file-scoped**, 334 root, 126 directory.
An index-only arm is therefore 54% treated and would report a diluted null.
Every arm here carries **both** levers — index build and injected search flag.

`--sif` exists only under `Cmd::Index` and `stream.rs` has no SIF pass, so
**`champion` is partially treatable by construction**: its file-scoped 46% gets
`split` without `sif`. It is reported as a partial arm and is not the headline.
`split` alone is the correctly-treated base of the whole §14/§20 ladder.

**P1 — the gate, and it is about power, not recall.** Define
**ψ_offline** = the share of *instances* where an arm and the control disagree
on "did any of this instance's ranked queries surface a gold file at rank ≤5".
Registered floor: an arm graduates to paid measurement only at
**ψ_offline ≥ 0.06 with |b−c|/n ≥ 0.03**. Gating on offline recall would gate on
the one quantity §9.7, §10.6 and §14.5 each showed does not transfer;
instance-level discordance is what McNemar power is made of, so ψ_offline ≈ 0 is
a positive statement that there is nothing to buy.
*Prior, measured on the old corpus before this was written*: champion vs default
is **0 of 40 discordant instances, ψ_offline = 0.000**, while query-level hit@5
moves +0.038. The registered expectation is therefore that **no arm clears P1**
and the campaign's output is a bound. Recording that in advance is the point.

**P2 — `prune-kw` ≥ control.** It is the one arm §20.6's rule says is fully
mirrorable, so it should carry no manufactured noise term. Floor: Δ hit@5 ≥ 0.00
under a **cluster bootstrap over instances** (4,000 resamples, seed 1). A
per-query interval may not be quoted: the measured design effect on this corpus
is **1.64×**, enough to flip the champion from null to significant.
*Kill:* Δ ≤ −0.02 with the interval excluding zero → `prune-kw` is dead as a
shipping candidate and §20.9's two significant wins are confirmed as a
non-transferring offline result.

**P3 — `prune-decl` loses overall, and the loss is a function of query length.**
§20.8's dose law says cost scales with the *unmirrored share of query tokens*. A
one-word identifier guess naming a declaration has an unmirrored share near
zero. Registered: pooled Δ hit@5 ≤ −0.05, **and** Δ in the 1-word stratum is
≥ pooled Δ + 0.05, monotone across {1, 2, 3–4, 5+} words. This is the
discriminating prediction: confirming it postdicts §20.5's −0.20 from query
composition alone. A flat loss across strata falsifies the dose law in this
regime.
*Confound, registered up front:* at `prune-decl` the §20.1 chunk is 69% path
tokens under the shipped `PathRender::Full`, and §20.5's prediction 5 found
capping the path made things worse. This arm measures
prune-decl-with-path-domination and cannot separate the two.

**P4 — `split` bounds the ladder.** Registered: |Δ hit@5| < 0.02. A null on
`split` bounds every document-side rendering above it. A win ≥ +0.03 reopens
§14 on agent-regime evidence for the first time.

**Tripwires (each voids the run, not the arm).**
1. **bm25 invariance.** |Δ| ≤ 0.005 in bm25 mode on every arm — the lexical
   tokenizer already does what these renderings do (`prose.rs`). Measured on the
   old corpus: Δ = +0.000, CI [−0.006, +0.007]. Any movement is a bug.
2. **One binary.** A single `bin_sha256` across every row of every config.
3. **Index readback.** After each build, `.semgrep/meta.json` must report the
   requested `{embed_preproc, sif}` or the run aborts — a failed build otherwise
   degrades into "measured the previous config", which is indistinguishable from
   a null.

**What a null will and will not license.** It **will** license withdrawing
`prune-kw` as a shipping candidate despite §20.9, and closing the document-side
rendering direction with a number rather than a pattern. It **will not** license
any claim that rendering does not matter to retrieval — §20.9's linux +0.090
[+0.035, +0.146] p=0.002 stands and is not contradicted by an agent-regime null
— nor any statement about agent *accuracy*, which this instrument does not
measure. Per §11.5 and §19.10 accuracy remains unpurchasable here: ±0.060 at
n=204, ±0.038 at all 560, ±0.15 at a 40-instance tier.

### 21.2 The gate: one arm clears it, in the losing direction

Run 2026-08-05, `guessplay-v1.jsonl`, one binary (`d89fa15f10c6abd8`), 854
desc-v9 ranked queries over 186 instances, five configs, semantic + bm25.

**A harness bug found first, and it was not the one everyone assumed.** Every
file-scoped row scored 0.000 in every config on a current binary. The cause was
`guessplay.score()` prefixing the scope path as though it were a directory —
a scope of `pkg/trainer.py` and a hit of `trainer.py` composed to
`pkg/trainer.py/trainer.py`, matching no gold. The engine was returning the hit
correctly. This had been read as the §16.11 file-scope engine bug (§17.1's
"0 of 5,117"), including in the analysis that planned this section; it is a
separate scoring defect and it was still live. Fixed; 46% of the corpus became
scoreable, base hit@5 0.000 → 0.356.

**And then the fix showed why that half cannot answer this question anyway.**
With correct scoring, all four arms return **Δ = +0.000, ψ_offline = 0.000** on
file-scoped rows, both modes, n=295. That is structural: a file scope yields
hits that all carry the scoped file's own path, so gold-*file* rank is 1 or
absent regardless of how the engine orders chunks within the file. **The
rendering cannot affect 46% of real agent searches.** Not a null — an identity.
The gate therefore rests on the 460 directory- and root-scoped queries over
148 instances, which is the complete half.

| arm | Δ hit@5 | cluster 95% CI | ψ_offline | b/c | \|b−c\|/n | P1 |
|---|---|---|---|---|---|---|
| `split` | −0.007 | [−0.027, +0.014] | 0.061 | 5/4 | 0.007 | no |
| `prune-kw` | −0.022 | [−0.055, +0.012] | 0.088 | 6/7 | 0.007 | no |
| `prune-decl` | −0.009 | [−0.065, +0.039] | **0.149** | 8/14 | **0.041** | **clears** |
| `champion` | −0.015 | [−0.049, +0.017] | 0.108 | 7/9 | 0.014 | no |

**P1 — one arm clears, pointing down.** Only `prune-decl` meets both the
ψ_offline ≥ 0.06 and the |b−c|/n ≥ 0.03 halves, and its asymmetry is 14
instances worse against 8 better (p=0.286). Every other arm moves instances
symmetrically, which is discordance without signal — exactly the condition that
inflates b+c without inflating |b−c| and therefore *reduces* McNemar power.

**P3 — falsified, and this is the result.** Registered: `prune-decl` loses
overall by ≤ −0.05. Measured: **−0.009, CI [−0.065, +0.039]** — indistinguishable
from the shipped default. Offline it lost by **0.15 to 0.28 with p<0.001 on
every one of five corpora** (§20.5). The arm the offline instrument rated worst
by a wide margin is a null in the regime this project optimizes for.
The length strata do not rescue the dose law either — −0.016 / +0.113 / −0.068 /
−0.005 across {1, 2, 3–4, 5+} words is not monotone. §20.8's dose law postdicts
the offline numbers and does not extend to real agent queries.

**P2 — missed.** `prune-kw` is the *worst* of the four at −0.022 (CI includes
zero, so the kill condition does not fire, but the floor of ≥0.00 is not met).
The arm with two significant offline wins on the largest corpora is the one that
does least well here.

**P4 — holds.** `split` at −0.007, |Δ| < 0.02. The base of the §14/§20
document-side ladder is a null on real agent queries, which bounds every
rendering above it.

**Tripwire 1 tripped, on a mis-set threshold.** `prune-decl` bm25 Δ = +0.011
against a registered ≤0.005 (CI [−0.002, +0.025], includes zero). The mechanism
is known: bm25-mode output passes through MMR, which reads the embedding matrix
(§14.4 point 6), and `prune-decl` perturbs that matrix most. §14.4 recorded the
identical tripwire as "miss as stated" for the identical reason. Registering it
a second time at a threshold that mechanism makes unreachable is the error, not
the engine. Recorded as a trip with a mis-specified threshold. Tripwires 2 and 3
passed.

**Decision: phase 2 is not bought.** The registered gate exists to answer
whether a paid frame could detect anything. It cannot. Only `prune-decl` has
enough instance-level movement to be measurable, and at ψ=0.149 a 40-instance
tier yields ~6 expected discordant pairs against the 6 all-one-way needed for
p<0.05 — a coin flip conditional on perfect asymmetry that the 8/14 split
already contradicts. The other three arms are below the floor outright.

**What this licenses.** Third confirmation of §9.7's rule (after §9.7 and
§10.6), and the first with the size of the miss measured: an offline deficit of
0.15–0.28 at p<0.001 corresponds to −0.009 [−0.065, +0.039] on real agent
queries. **Offline retrieval eval on generated queries does not predict
agent-regime behaviour for a rendering change** — not merely "gains fail to
transfer", but losses fail to transfer too, which is the stronger and more
useful form. `prune-kw` is withdrawn as a shipping candidate: §20.9's linux and
etcd wins stand as offline facts and do not survive contact with real queries.
`split`+`sif` remains the default.

**What it does not license.** Nothing about agent *accuracy*, which this
instrument does not measure. Nothing about renderings on descriptive queries —
§20.9's linux +0.090 [+0.035, +0.146] p=0.002 stands. And nothing about the 46%
of searches that are file-scoped, where no rendering can matter by construction;
if that share is worth attacking, the lever is scope handling, not rendering.

## 22 Rescuing the keyword lever, and making the file-scoped half measurable

§21.2 produced two negatives that looked terminal: `prune-kw` was the worst arm
on real agent queries (−0.022) despite two significant offline wins, and 46% of
agent searches are file-scoped, where every arm returned Δ = exactly +0.000.
Both are defects rather than findings.

### 22.1 Pre-registration (written before the first row)

**Root cause 1 — the table fires in the wrong position.** `prune-kw` deletes
tokens that are *identifier components* in a real corpus, not just syntactic
boilerplate. Measured against the 421 gold function names agents were hunting
in §21:

| rule | gold function names damaged |
|---|---|
| naive (drop the subtoken anywhere) | **20.9%** (88 of 421) |
| positional (drop only a whole-run keyword) | **0.7%** (3 of 421) |

`__init__` alone is 30 of the 88; the rest are `from_*`, `as_*`, `for_*`,
`in_*`. When an agent searches `__init__`, `prune-kw` deletes `init` from the
query *and* from every chunk, so the function is unfindable by the name it has.
`prune-kw` stays frozen (§20.5/§20.9 published it); `prune-kw-pos` is the
repair, and it also cuts queries less — 9.1% of agent query tokens against
13.7%.

**Root cause 2 — the file-scope zero is a metric artifact.** `guessplay` scored
`rank_of_gold(hits, gold_files)`. Under a file scope every hit carries that one
file's path, so the rank is 1-or-absent whatever order chunks come back in —
the rank histogram over 2,928 file-scoped rows is exactly `{1: 1050, None:
1878}`, no other value occurs. But the project's endpoint is
`func_acc@10_tol`, and *within-file chunk order decides which functions the
agent sees*. §22 scores those rows at function level: `SearchHit.line`
containment → innermost `symbols.extract` span (`sig_line..end_line`) →
`scoring.func_match(..., tolerant=True)`. Tolerant only: `symbols.extract`
yields bare leaves and 704 of 1,149 gold quals are dotted, so
`func_acc@*_strict` is not computable from leaves and is not reported.

**The design is a 2×2** over {naive, positional} × {symmetric, query-untouched},
which `default`, `prune-kw`, `prune-kw-pos` and `prune-kw-pos-q0` complete at
zero cost. `split` and `prune-decl` ride along for continuity with §21.2.

**Registered predictions:**

1. **Positional beats naive.** Floor: `prune-kw-pos` − `prune-kw` ≥ **+0.02**
   hit@5, cluster bootstrap over instances (4,000, seed 1). *Kill:* if it does
   not, the identifier-component story is wrong and the keyword lever is closed
   rather than re-tuned a third time.
2. **The gain concentrates where the table was doing damage.** Registered: Δ on
   the 21% of instances whose gold function name contains a table word is ≥ 2×
   Δ on the remainder. A uniform gain means prediction 1 passed for the wrong
   reason.
3. **The query-side axis, registered without a preferred direction.** Two
   credible mechanisms disagree. §20.6's dose law says the 9.1% unmirrored
   share costs ≈ −0.02. Against it: chunk boilerplate is *obligatory* — the
   grammar forces `def` into every function — while a query token is
   *elective*, the agent having spent one of ~5 tokens on it. §20.6 was
   measured on generated queries, and §21.2 showed that instrument mispredicts
   this regime, so its authority here is exactly what is in doubt. Two-sided:
   |Δ| ≥ 0.02 to call it either way. **A null is the most likely and most
   useful outcome** — it would put 9.1% below the dose law's detection floor
   here, and the simpler rule (do not touch the agent's query) then wins on
   parsimony rather than on performance.
4. **The `def`/`class` sub-test.** Those two tokens are 84% of the disputed
   share (169 + 84 of ~300). Registered: if `prune-kw-pos-q0` wins, the gain
   must **not** come predominantly from the 253 queries containing them — if it
   does, the effect is about syntax mimicry rather than electiveness, and the
   right lever is a two-word exception, not a policy change.
5. **Function-level scoring makes file scopes discriminative.** Floor:
   ψ_offline > 0 on file-scoped rows for at least one arm, against the current
   exact 0.000. If it is still exactly zero, within-file ordering does not reach
   the endpoint either and that half is closed on much stronger evidence.
6. **Tripwire — bm25 unchanged.** Neither new variant may move bm25 beyond the
   MMR-mediated drift §14.4 documented. The lexical tokenizer keeps its old
   callback (`token::for_each_token_with` discards the positional flag), so a
   movement means the widened `emit` leaked into BM25.
7. **Tripwire — one binary.** A single `bin_sha256` across every row.

**What a null on prediction 1 licenses.** That the keyword lever is closed: two
repairs, both measured, neither transferring. It does **not** license any claim
about renderings on descriptive queries — §20.9's linux +0.090 [+0.035, +0.146]
p=0.002 stands — nor about agent *accuracy*, which this instrument does not
measure (§11.5: unpurchasable on this benchmark at any n it can hold).

### 22.2 The repair works, and it buys nothing

Run 2026-08-05, `guessplay-v2.jsonl`, one binary (`e09664634db0c898`), 854
desc-v9 ranked queries over 186 instances, six configs, semantic + bm25, both
metrics. 10,248 rows, perfectly balanced (1,708 per config).

**P1 — passes, exactly at its floor.** `prune-kw-pos` − `prune-kw` =
**+0.022, CI [+0.010, +0.035]**, the interval excluding zero. The registered
floor was +0.02. This is the first positive result in the §20–§22 arc.

**P2 — fails, and it invalidates P1's stated mechanism.** The gain was
registered to concentrate on the instances whose gold function name the naive
table was erasing, at ≥2× the remainder. Measured:

| stratum | n | `prune-kw` | `prune-kw-pos` | Δ | 95% CI |
|---|---|---|---|---|---|
| gold name damaged | 123 | 0.423 | 0.447 | **+0.024** | [+0.000, +0.054] |
| gold name intact | 338 | 0.426 | 0.447 | **+0.021** | [+0.007, +0.036] |

1.14×, not 2×. The gain is uniform, so recovering `__init__` is *not* what
happened — §22.1 registered that reading in advance: "a uniform gain means
prediction 1 passed for the wrong reason."

**What actually happened, and it is the finding.** Positional pruning simply
deletes less, so it converges on not pruning at all:

| arm | vs `default` (no rendering) | vs `split` (no keyword pruning) |
|---|---|---|
| `prune-kw` | −0.022 [−0.055, +0.012] | −0.015 [−0.043, +0.010] |
| `prune-kw-pos` | **+0.000** [−0.030, +0.033] | **+0.007** [−0.018, +0.031] |

`prune-kw-pos` is indistinguishable from doing nothing, on both baselines and on
both metrics (function-level: −0.002 [−0.023, +0.017] vs default). **The +0.022
is not a gain over the baseline; it is the removal of a self-inflicted loss.**
The keyword table's whole measurable contribution on real agent queries is the
damage it does, and repairing it returns to parity rather than past it.

That closes the lever on its own registered terms. Two repairs — the 43 missing
words (§20.1) and the positional rule (§22.1) — each fixed a real defect, and
neither produced a rendering that beats an unrendered index in the regime this
project optimizes for. §20.9's offline wins (linux +0.090 [+0.035, +0.146]
p=0.002) stand as offline facts and remain the third instance of §9.7's rule.

**P3 — the query axis is a null, and parsimony decides it.** `prune-kw-pos-q0`
(query untouched) against `prune-kw-pos` (symmetric): −0.007 [−0.025, +0.010]
file-level, +0.004 [−0.006, +0.016] function-level, both far inside the ±0.02
needed to call it either way. §22.1 registered this outcome as the most likely
and most useful: **9.1% of query tokens is below the dose law's detection floor
in this regime**, so §20.6's rule does not extend here — not because it is
wrong, but because the effect it predicts is too small to see at this share.
The simpler rule wins on parsimony: **do not touch the agent's query.** An
agent's tokens are elective and the engine gains nothing measurable by second-
guessing them.

**P4 — moot.** Registered conditionally on `prune-kw-pos-q0` winning; it did not.

**P5 — passes, and it recovers half the corpus.** Function-level scoring makes
file scopes discriminative for the first time: ψ_offline **0.050–0.058** against
the exact 0.000 file-level scoring produced, with real discordance (3/4, 3/4,
1/5). Function-level hit@5 on file scopes is **0.193**, *higher* than
directory-scoped 0.157 — the half of agent behaviour §21.2 wrote off as
unmeasurable is both measurable and more productive than the half we were
scoring. Any future rendering or ranking work has 100% of the corpus available
to it rather than 54%.

**P6 — tripwire holds.** `prune-kw-pos` vs `prune-kw` in bm25 is **+0.000
exactly**, both metrics, ψ=0: the widened `emit` callback did not leak into the
lexical tokenizer. All arms sit at +0.002 [+0.000, +0.007] against `default`,
the identical MMR-mediated drift §14.4 documented.

**P7 — one binary** across all 10,248 rows.

**Ledger for §22.** Two of seven predictions passed as stated (P1, P5), one
failed and took P1's mechanism with it (P2), one is an informative null (P3),
two are tripwires that held (P6, P7). The keyword lever is closed: repaired, it
reaches parity with an unrendered index and no further. What §22 leaves behind
is a scoring instrument that can see every agent search rather than half of
them, and one design rule with evidence behind it — leave the agent's query
alone.

## 23 The powered agent-regime bound

§21 and §22 each ran 854 queries over 186 instances and returned nulls. A null
at that width bounds a rendering effect at roughly ±0.03, which is wider than
any effect this project has ever shipped on. §23 buys the tighter bound with
the corpus already on disk.

### 23.1 Pre-registration (written before the first row)

**Frame.** All `desc-*` conditions from `eval/data/locbench/runs/`: **7,657
ranked queries over 467 instances** (`guesses-v1-desc-all.jsonl`), against
§22's 854 over 186. **2.51× the instances**, so the cluster bootstrap narrows
by ≈1.58× and §22's key interval [−0.030, +0.033] becomes ≈[−0.019, +0.021].
467 of the dataset's 560 instances is effectively the whole benchmark.

**Arms**, chosen to answer one question — *does any rendering beat the shipped
default on real agent queries?*

| arm | what it is |
|---|---|
| `default` | shipped: raw `doc_text`, no rendering |
| `split` | the base of the §14/§20 ladder |
| `champion` | `split`+`sif`, §14.4's offline winner and the standing recommendation |
| `prune-kw-pos` | the repaired keyword lever (§22.1) |

`prune-decl` is dropped: §21.2 and §22.2 both measured it at parity, and a
third null on the same arm buys nothing. Modes semantic (shipped) and bm25
(tripwire). Both scopes, both metrics (`rank`, `rank_func`).

**Why pooling six description regimes is legitimate here.** The `desc-*` arms
differ in identifier share (desc-v5 ≈ 45–50%, desc-v8/v9 ≈ 62–65%, §19.11), so
they are not one query distribution. That widens the *population* the bound
covers rather than confounding it: the claim under test is "no rendering moves
retrieval on realistic agent queries", and a bound that holds across six
description regimes is stronger than one that holds for desc-v9 alone.
Registered check: report the per-condition cut, and if the arms disagree in
*sign* across regimes, the pooled bound is withdrawn and reported per regime.

**Registered predictions:**

1. **No rendering beats `default` at this width.** Registered: every arm's
   interval against `default` contains zero, on both metrics. *Kill:* an arm
   whose interval excludes zero on the primary metric reopens the rendering
   direction and is a shipping candidate — the outcome §20 through §22 kept
   failing to produce.
2. **The bound tightens as predicted.** Registered: the |CI| half-width on
   `champion` − `default` shrinks by 1.4–1.8× against §22's. If it does not,
   the queries are more clustered within instances than the design effect
   assumed and every interval this project has published on this corpus is
   optimistic.
3. **`champion` is not distinguishable from `default`.** The standing
   recommendation rests on §14.4's *offline* numbers. §14.5 already refused it
   once on agent-query evidence and §21.2 measured −0.015 [−0.049, +0.017].
   Registered: |Δ| < 0.02. **If `champion` loses at this width, the shipped
   default should change** — that is the one actionable outcome available here,
   and registering it in advance is what keeps it from being explained away.
4. **File scopes stay discriminative.** ψ_offline > 0 on file-scoped rows at
   function level, replicating §22.2's recovery on 2.5× the frame.
5. **Tripwire — bm25.** |Δ| ≤ 0.005 on every arm except the MMR-mediated drift
   §14.4 documented.
6. **Tripwire — one binary** across every row.

**What a clean null licenses.** "No document-side rendering moves retrieval on
real agent queries by more than ±0.02, across 7,657 queries and 467 instances
spanning six description regimes." That is a publishable bound and it closes
the rendering direction properly rather than by exhaustion. It does **not**
license any claim about agent *accuracy* (§11.5: unpurchasable here), about
ranking or chunking levers (untested), or about descriptive-query retrieval,
where §20.9's linux +0.090 [+0.035, +0.146] p=0.002 stands.

### 23.2 The bound, and the direction closes

Run 2026-08-05, `guessplay-v3.jsonl`. **62,808 rows, 7,657 ranked queries over
467 instances**, six description regimes, one binary (`eb9aec404d324b56`).
Balanced to within 176 rows across arms (the residual is exact-arm rows, which
only run under `default`).

**Semantic mode, directory- and root-scoped, against the shipped `default`:**

| arm | Δ recall@5 | 95% cluster CI | ψ | b/c |
|---|---|---|---|---|
| `split` | **−0.011** | **[−0.022, −0.002]** | 0.037 | 6/8 |
| `champion` (`split`+`sif`) | +0.005 | [−0.013, +0.023] | 0.098 | 16/21 |
| `prune-kw-pos` | −0.007 | [−0.021, +0.007] | 0.063 | 8/16 |

**P1 — holds.** No arm beats `default`; the kill condition (an interval
excluding zero *upward*) did not fire. `split` excludes zero **downward** at
the pooled n: −0.011 [−0.022, −0.002].

> **Amended by §23.3.** That significance is carried by the pooled sample, not
> replicated in the clean half. The point estimate is stable — −0.011 pooled,
> −0.012 post-fix, −0.011 pre-fix — but on post-fix data alone the interval is
> [−0.024, +0.000] and touches zero. The honest claim is **"`split` is
> consistently ≈−0.011 and reaches significance only at the pooled n"**, not
> "`split` is a significant loss". See §23.3.

**P2 — the bound tightened as registered.** §21.2's `champion` interval was
[−0.049, +0.017], half-width 0.033; here it is [−0.013, +0.023], half-width
0.018 — a **1.83×** narrowing against a registered 1.4–1.8×. Slightly better
than predicted, which means the design effect assumption was mildly
conservative rather than optimistic.

**P3 — passes, and it is the actionable one.** `champion` sits at
**+0.005, |Δ| < 0.02**. The §14.4 recommendation — carried in this file for
three sections — is **indistinguishable from doing nothing** on real agent
queries at a ±0.023 bound. §14.5 refused it once, §21.2 measured −0.015, and
this settles it at 2.5× the frame: **`split`+`sif` should not be adopted as the
default.** The shipped `EmbedPreproc::None` stands, and the reason is now a
number rather than an absence of evidence.

**P4 — file scopes stay discriminative.** ψ_offline > 0 at function level on
file-scoped rows (0.020), replicating §22.2's recovery on 2.5× the frame. And
the gap widened: function-level hit@5 is **0.272 on file scopes against 0.152
on directory scopes**. At scale, the half §21.2 wrote off is not merely
measurable but **1.8× more productive** than the half we had been scoring.

**P5, P6 — tripwires hold.** bm25 deltas are +0.002 / −0.001 / +0.001, all
within the registered 0.005. One binary across all 62,808 rows.

**The registered heterogeneity check fired, and it was mis-specified.** §23.1
said to withdraw the pooled bound if arms disagree in sign across regimes.
`split` is negative in all five (consistent); `champion` is 3+/2− and
`prune-kw-pos` 1+/3−. But **a null arm scatters around zero by construction**,
so sign disagreement among nulls is trivial and the check cannot distinguish
heterogeneity from noise. Testing the spread against sampling error instead:
`prune-kw-pos` 0.019 and `split` 0.012 against an expected 0.058 (noise);
`champion` 0.069, marginally above, driven entirely by desc-v7 — n=92, +0.054,
about 1.3 SE. The pooled bound stands, with that caveat recorded rather than
argued away. The check should have been on between-regime variance against
sampling variance, and is written that way here for reuse.

**What §23 licenses.** *No document-side rendering improves retrieval on real
agent queries by more than 0.023, across 7,657 queries and 467 instances
spanning six description regimes — and the ladder's base is 0.011 worse than no
rendering at all.* That closes the rendering direction on a measurement rather
than on exhaustion, and it retires the standing `split`+`sif` recommendation.

**What it does not license.** Nothing about agent *accuracy* (§11.5:
unpurchasable on this benchmark at any n it holds). Nothing about ranking,
chunking, or scope handling, none of which this varied. And nothing about
descriptive-query retrieval, where §20.9's linux +0.090 [+0.035, +0.146]
p=0.002 stands — that result is real, it simply describes a different task,
which is the whole finding of §21 through §23.

### 23.3 Audit of §21–§23, and one correction

Twelve checks against the raw artefacts rather than against the summaries.

**What held.**

- **Error symmetry.** 69 gids error (bad scope paths), and all 69 error in
  **all four arms** — arm-independent, so an errored row penalizes every arm
  identically. Zero pairing drops: 7,657 gids × 4 configs, all complete.
- **Independent recomputation.** The headline was recomputed from raw rows with
  fresh code and a different bootstrap seed (7, not 1). `split` −0.0113
  [−0.0214, −0.0019], `champion` +0.0046 [−0.0133, +0.0214], `prune-kw-pos`
  −0.0070 [−0.0211, +0.0071]. The published bound of 0.023 is the seed-1 upper
  bound; seed 7 gives 0.021, so **the published figure is the conservative one**.
- **Corpus provenance is exact.** 7,692 ranked `desc-*` invocations in the raw
  shim logs, 7,657 in the replayed corpus, delta **35 — exactly the
  empty-pattern residuals `harvest.py` reports**. No real query is silently
  dropped. Five random rows traced back to their originating log line by hand.
- **Replay fidelity: 98.0%.** For 500 post-fix invocations, rank-of-gold
  computed from *the agent's own stored stdout* agrees with the replay's rank
  on "gold in top-5". The 10 disagreements are tail-rank differences
  (agent 5 / replay 6) from k-truncation, not systematic. **The instrument
  reproduces what agents actually saw.**

**What did not, and the correction it forces.**

- **Half the corpus was typed against a broken tool.** `b49e818` (2026-08-03
  16:03) fixed ranked search over a single-file scope returning nothing,
  always. **50.1% of the §23 corpus predates it**, and 58.5% of those queries
  are file-scoped — so those agents got nothing back from every file-scoped
  search, and their subsequent queries are shaped by that. Replay fidelity on
  the pre-fix half is **62.6%**, and every disagreement is the same signature:
  agent found nothing, replay finds the gold at rank 1.
- **Re-run on the clean half only** (3,821 queries, 232 instances):

  | arm | pooled | post-fix only | pre-fix only |
  |---|---|---|---|
  | `split` | −0.0113 **[−0.0215, −0.0022]** | −0.0119 [−0.0244, **+0.0000**] | −0.0107 [−0.0236, +0.0019] |
  | `champion` | +0.0046 [−0.0134, +0.0225] | −0.0060 [−0.0295, +0.0157] | +0.0169 [−0.0078, +0.0418] |
  | `prune-kw-pos` | −0.0070 [−0.0212, +0.0067] | −0.0114 [−0.0302, +0.0055] | −0.0019 [−0.0205, +0.0166] |

  The **point estimates replicate** across the confound — `split` at −0.011,
  −0.012, −0.011 — so the effect is not an artefact of the broken half. What
  does not replicate is the *significance*, which is a function of n (378
  instances pooled against 194). §23.2's "`split` is a significant loss" is
  therefore amended above to a claim about the pooled estimate.
  **The ±0.023 bound is unaffected and in fact tightens post-fix** (champion's
  upper bound 0.0157), so §23's headline stands.

**Harness gaps found.**

1. **Two definitions of "is this scope a file" in one file.** `compare()` uses a
   dot-in-basename heuristic; `score()`/`_abs_hits` use `Path.is_file()` /
   suffix. They disagree on exactly one scope in the corpus — `.github`, a
   dotfile *directory* — affecting 4 rows of 30,628 (0.013%). Immaterial to
   every number here, and a latent trap: scoring and reporting must not
   disagree about what they are partitioning.
2. **`bin_sha256` fingerprints the binary, not the source.** It changed between
   the §22 and §23 runs with `crates/` byte-identical — a relink. The tripwire
   can therefore false-alarm but never false-pass (equal bytes do imply equal
   code), which is the safe direction; it should hash the source tree or record
   the `crates/` git sha instead.
3. **The pre/post-fix split is not recorded in the corpus.** Nothing in
   `guesses-*.jsonl` marks which rows were produced by a broken tool. Any future
   campaign over the harvested corpus inherits the same 50% contamination
   silently. The corpus should carry the binary or commit that served each
   query, exactly as `guessplay` rows now carry `bin_sha256`.

**What this audit does not cover.** It validates the *replay* against agent
stdout and the *arithmetic* against the raw rows. It does not validate
`symbols.extract`'s function spans against ground truth — the function-level
metric (§22.2, §23.2 P4) rests on a regex extractor that under-counts by
design, so `rank_func` figures should be read as a lower bound on within-file
discriminability rather than as a calibrated rate.

## 24 The within-file gap, and the metric that was hiding it

§23 closed the document-rendering direction with a powered bound: no rendering
improves retrieval on real agent queries by more than 0.023. What that work left
behind is an instrument that can see file-scoped searches for the first time, and
a gap nothing has ever been aimed at. §23.3 ended by warning that `rank_func`
"should be read as a lower bound rather than as a calibrated rate". This section
measures how much of a lower bound, and then tests three candidates against the
corrected instrument.

### 24.0 What the reproduction established

All 2,188 file-scoped agent searches from `guessplay-v3.jsonl` were re-executed
live against files restored from the pinned git mirrors at each instance's
`base_commit`. 2,149 completed, and **all 2,149 reproduce their recorded
`rank_func` exactly** — so the harness is faithful even though `bin_sha256` has
moved since (§23.3 finding 2: the fingerprint tracks relinks, not behaviour).

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

**The cut that matters.** Splitting by whether the query contains the gold
function's own name is the only cut tested that separates them, and it survives
both metrics:

| | n | share | strict@5 | chance@5 | lift | overlap@5 |
|---|---|---|---|---|---|---|
| names the function | 670 | 31% | 76.1% | 23.5% | **3.2×** | 87.3% |
| describes it instead | 1,479 | 69% | 42.4% | 26.5% | **1.6×** | 58.0% |

Chance is computed exactly per file, `1 − (1 − p)⁵` over the union of gold spans.
The median file-scoped query is *two words*. **69% of the traffic describes, and
there the engine is barely above chance.**

Three mechanisms were tested and ruled out. **Not name ambiguity** — the gold
name appears a median 3 times in the file when the engine finds it and 4 when it
misses, and the top-5 rate does not fall monotonically with occurrence count.
**Not the extractor going blind** — all 648 distinct (instance, scoped file, gold
function) triples were pulled from the mirrors and `symbols.extract` resolves the
gold function in **100%** of them. **Not big files** — measured as lift the engine
*improves* with size, reaching 8.7× chance above 2,000 lines, because chance falls
faster than the engine does.

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

with the spread +19.8pp on gold functions under 10 lines and +6.8pp on 30–99
line ones — the signature of chunk granularity, not of ranking. Of 160 named
top-5 misses, **75 are recovered by overlap (the measurement) and 85 are
genuine**. §22.1 chose strict deliberately, because overlap credit "would blunt
the very ordering this is built to measure", and that reasoning still holds.
What it did not anticipate is the *size* of the understatement: 14.2pp is larger
than every effect §20–§23 tried to detect. **Neither number is the truth** —
strict under-credits short functions, overlap over-credits a window that merely
brushes one — so both are emitted always, and a result that moves only one of
them is a result about the metric.

**Three candidates, each an independent flag, measured factorially.**

- **#1 — the same-file dedupe.** `hit.rs` drops any candidate whose span
  overlaps an already-kept candidate in the same file. Verified on the
  `update_sources` case: `ranked top 16 of 37 candidates`, and the chunk holding
  the declaration overlaps two higher-scoring neighbours that each contain a
  *call site*, so it is removed before ranking. Under `--overlap 0` it goes from
  absent to **rank 2**. That crude proxy is worth **+2.0pp overlap@5,
  CI [−0.001, +0.043]** across all 1,542 distinct file-scoped queries — real but
  modest, and it conflates the dedupe with a chunking change. MMR is *not* the
  cause: `--no-diversify` swaps two ranks and rescues nothing.
- **#3 — a finer, wider pass at file scope.** A file scope never resolves an
  index (`cache::discover` bails on a non-directory root), so it is always the
  streaming path — 44.7 ms over 37 chunks, with `candidate_width(k) = k*3`
  capping the pool at 30. Both the window and the cap are affordable to change
  there and nowhere else.
- **#2 — declaration-aware scoring.** `prose::declaration_sites()` exists,
  built for `PruneDecl`. §22 showed using it to *delete* tokens buys nothing;
  using it as a ranking feature is untested.

**Registered predictions:**

1. **The bracket is real and sized.** Floor: overlap@5 − strict@5 ≥ **+0.10** on
   file-scoped right-file rows, and ≥2× larger on gold functions under 10 lines
   than over 30. *This is a check on the metric fix itself* — a failure means
   the reproduction is wrong, not the engine.
2. **#1 beats its own control.** Floor: `--dedupe-overlap 0.5` −
   `--dedupe-overlap 0.0` ≥ **+0.02** overlap@5, cluster bootstrap over
   instances (4,000, seed 1). *Kill:* below +0.01 the dedupe is not the lever
   the single case suggested, and #1 ships only if it is free elsewhere.
3. **#1's gain is concentrated where the mechanism says.** Registered: the gain
   on rows where a higher-scoring neighbour chunk overlaps the gold span is ≥2×
   the gain on the remainder. A uniform gain means something else moved and
   prediction 2 passed for the wrong reason — the same discriminating shape
   §22.1's P2 used, and the one that caught P1 there.
4. **#3 helps, and helps short functions most.** Floor: ≥ **+0.02** strict@5,
   with the gain larger on gold functions under 10 lines than over 30. A flat
   profile across function length falsifies the dilution mechanism even if the
   total moves.
5. **#2 addresses the named residue.** Registered on the 85 genuine named
   misses: ≥ **20** recovered. **Two-sided on the describe half** — a
   declaration boost could plausibly hurt descriptive queries by over-weighting
   signatures, and that must not be reported as a wash.
6. **Tripwire — the directory half.** No arm may lose more than 0.01 file-level
   `rank@5` on directory scopes in the confirmation run. #1 changes ranked
   output corpus-wide and the iterate loop is blind to that by construction.
7. **Tripwire — one binary** per campaign, asserted by `bin_sha256`, and one
   `arm_flags` value per arm, which is now part of the resume key.
8. **Tripwire — cold == warm.** #3 is file-scope-only and therefore cold-only;
   #2 is not, and must be mirrored on both paths or
   `cold_and_warm_return_identical_results` fails.

**What a null on all three licenses.** That within-file ranking is not reachable
by candidate-set or scoring changes of this kind, and the remaining lever is the
*query* side — the 69% who describe rather than name. It does **not** license a
claim about the 48% of file-scoped searches aimed at the wrong file, which no
ranker reaches and which §19's tool-description instruments are the right tool
for.

**What this cannot settle.** The recoverable pool — right file, gold function
outside the top 5 — is **9–13% of all agent searches** depending on the metric,
and that is a *ceiling, not a backlog*. Some unknown share of it is the agent
having asked a different question than the benchmark grades: when a query reads
`periodic task maintenance loop` and the engine returns `_loop_coroutine` while
gold is `_send_message`, the engine was right and the query pointed elsewhere.
Separating the two needs query-intent labelling, which nothing in the harness
does. No result below should be read as if the ceiling were the target.

### 24.2 One of three, and the two that died on their own floors

Run 2026-08-06, `guessplay-v4.jsonl`, one binary (`ef37824e9d3b71e8`), 33,728
rows: a full 2×2×2 over the three candidates on the file-scoped half of the
desc-all corpus, 402 instances, semantic mode. 17,504 rows land on a file that
holds a gold function; 2,188 queries (331 instances) are paired across all
eight arms and carry every contrast below.

**Main effects, each lever averaged over the other two** (four paired
contrasts each, cluster bootstrap over instances, 4,000, seed 1):

| lever | strict@5 | overlap@5 |
|---|---|---|
| #1 dedupe 0.5 | −0.003 [−0.011, +0.005] | **−0.009 [−0.017, −0.000]** |
| #3 file-window 12 | +0.008 [−0.013, +0.028] | **−0.052 [−0.075, −0.030]** |
| #2 decl-boost 1.0 | **+0.027 [+0.006, +0.049]** | **+0.033 [+0.013, +0.052]** |

**P1 — passes, both clauses.** The bracket on the control arm is **+14.4pp**
(52.4% strict, 66.8% overlap) against a registered floor of +10.0pp, and it is
+19.8pp on gold functions under 10 lines against +7.2pp on those over 30 — a
2.75× ratio against a registered 2×. The metric fix is sound, and every number
in §22 and §23 about file scopes is a lower bound by roughly this much.

**P2 — killed, and it takes the default with it.** `--dedupe-overlap 0.5` was
registered at ≥ +0.02 overlap@5 with a kill below +0.01. Measured: **−0.009
[−0.017, −0.000]**, a small *significant loss*. The mechanism from §24.1 is
real — on the `update_sources` case the declaration chunk is deleted before
ranking and 0.5 brings it back at rank 3 — but the case is not the population.
Keeping neighbours crowds the top-k with one file's chunks more often than it
rescues the right one, which is the trade the snapshot showed plainly when 85
of 114 cases moved and three `native/ring.c` chunks took slots other files had.
**The default is reverted to 0.0 and the snapshot is byte-identical to its
pre-§24 state.** The flag stays because the arm is measured.

*What misled the plan.* §24.1 sized this lever from `--overlap 0`, which was
worth +2.0pp [−0.001, +0.043]. That proxy changes *chunking* — it makes chunks
non-overlapping — and only incidentally changes what the dedupe drops. The
gain belonged to the chunking, not to the rule under test, and the proxy
inverted the sign of the thing it was standing in for. A one-case rescue plus a
proxy is not evidence; the 2,188-query arm is.

**P3 — moot.** Registered conditionally on P2, which failed.

**P4 — the mechanism confirms while the lever fails.** `--file-scope-window 12`
was registered at ≥ +0.02 strict@5. Measured +0.008 [−0.013, +0.028]: null.
But the discriminating clause passes cleanly — the strict gain is **+4.8pp on
gold functions under 10 lines against −1.1pp on those over 30**. Dilution is
real and finer chunks do address it. They also cost more than they pay:
overlap@5 falls **−0.052 [−0.075, −0.030]**, because a 12-line chunk brushes a
gold function far less often than a 32-line one does.

That opposite movement is the most useful thing the metric fix bought. Under
strict scoring alone #3 reads as a modest win; under overlap alone it reads as
a clear loss; it is neither, and no single number could have said so. **A lever
that moves the two metrics in opposite directions is changing chunk geometry,
not retrieval quality** — and §22/§23 had no way to see that distinction.

**P5 — passes, and on both halves.** `--decl-boost 1.0` recovers **58 of the 92
named rows** the control missed at overlap@5, against a registered floor of 20.
The registration was two-sided on the describe half because over-weighting
signatures could plausibly hurt it. It does not:

| | n | Δ overlap@5 |
|---|---|---|
| query names the gold function | 685 | **+0.069 [+0.025, +0.117]** |
| query describes it instead | 1,503 | **+0.039 [+0.009, +0.066]** |

Both intervals exclude zero. The best arm in the factorial is decl-boost alone
— **56.3% strict / 71.6% overlap** against the control's 52.4% / 66.8%.

**This is the first engine change in §20–§24 to beat an unrendered index on
real agent queries.** §20–§23 spent four sections on what a chunk is *made of*
and found a bound of 0.023; this changes what a chunk is *worth* and clears it.
The reason is visible in the §24.0 failure it was built from: a chunk that
declares an identifier and a chunk that calls it were scored alike, and for a
query that names a function those are not the same answer.

**P6 — the directory half. Pending the confirmation run.** The factorial ran
`--file-scopes-only` and is blind to directory scopes by construction. That is
what §24.1's tripwire is for and it is not yet discharged.

**P7 — one binary** (`ef37824e9d3b71e8`) across all 33,728 rows, and one
`arm_flags` per arm, now part of the resume key. Verified live rather than
assumed: on the first 750 paired queries each lever changed 15–28% of rows
against the control, so no arm was a silently-unwired null.

**P8 — cold == warm** holds with the boost on, asserted by
`cold_and_warm_agree_with_the_declaration_boost`, which also asserts the boost
reorders something on the fixture — an inert boost would satisfy the equality
trivially and the test would guard nothing, which is exactly how the MaxSim
version of this bug survived until that test was written after the fact.

**Ledger for §24 so far.** Three of eight predictions pass as stated (P1, P5,
P8), one is killed on its own floor and reverts a default (P2), one is moot
(P3), one fails as a lever while confirming its mechanism (P4), one is a
tripwire that held (P7), one is outstanding (P6). Two candidates die; one
ships, pending the directory half.

### 24.3 The weight sweep (registered before the run)

§24.2 measured `--decl-boost` at **w = 1.0**, which was a first guess and never
tuned. P6 is now discharged and the lever is a shipping candidate, so the weight
gets chosen deliberately. Arms: 0.0 (control), 0.5, 1.0, 2.0, 4.0, on the 2,188
paired file-scoped queries, one binary, semantic mode.

**A sweep over the corpus that established the effect cannot also establish its
size.** Selecting the argmax of five arms on the same 2,188 queries biases the
winner's effect estimate upward by construction — the same trap §12 audited
this project for. Two commitments follow, registered here:

1. **The effect is already established and is not re-estimated by this run.**
   Its size is the independent full-corpus confirmation of §24.2 at w = 1.0:
   +0.039 [+0.015, +0.062] strict and +0.048 [+0.024, +0.072] overlap on file
   scopes, +0.017 [+0.007, +0.029] bm25 on directory scopes. Whatever the sweep
   picks, **those remain the published numbers for the lever**, and a larger
   figure produced by the selected arm is a selection artifact and is reported
   as one.
2. **The rule is parsimony, not argmax.** Take the *smallest* weight whose
   overlap@5 gain has a CI excluding zero and whose point estimate is within
   0.01 of the best arm. A bigger weight that buys nothing measurable is a
   worse default: the boost is multiplicative, so a large w lets a single
   declared token dominate a fused score, and nothing in this corpus would show
   that failure.

*Kill:* if no weight clears zero on this binary, the §24.2 result does not
replicate on a re-run and the lever is withdrawn rather than tuned.

**Result: flat, and 0.5 wins on parsimony.** Run 2026-08-06,
`guessplay-v6.jsonl`, one binary (`8bc13ebc1071f3e4`), 21,080 rows, the same
2,188 paired file-scoped queries.

| w | strict@5 | overlap@5 | Δ overlap vs w=0 |
|---|---|---|---|
| 0.0 | 52.4% | 66.8% | (control) |
| **0.5** | **56.6%** | 71.3% | **+0.046 [+0.025, +0.067]** |
| 1.0 | 56.3% | 71.6% | +0.048 [+0.023, +0.072] |
| 2.0 | 55.8% | 71.4% | +0.047 [+0.021, +0.072] |
| 4.0 | 56.4% | 71.3% | +0.045 [+0.018, +0.071] |

Every arm clears zero, so the kill does not fire and §24.2 replicates on a
second binary. The spread across an **8× range of w is 0.003** — inside the
noise of every individual interval. Registered rule takes 0.5: the smallest
weight clearing zero and within 0.01 of the best.

The flatness is the finding, not an inconvenience. A multiplicative boost whose
effect is invariant to its own magnitude is acting as a **reordering signal**,
not a score adjustment: what matters is that declaring chunks sort above calling
chunks, not by how much. That is the mechanism §24.0 described, and it also
makes the default safe — the failure mode of a large `w` (one declared token
dominating a fused score) is real but never fires here, and choosing the
smallest effective weight means it cannot start firing on a corpus this one
does not resemble.

Per §24.3's first commitment, **the published effect for the lever remains the
independent full-corpus confirmation at w = 1.0** — +0.039 [+0.015, +0.062]
strict, +0.048 [+0.024, +0.072] overlap, +0.017 [+0.007, +0.029] bm25 on
directory scopes. The 56.6% strict in the table above is the argmax of five
arms on the corpus that selected it and is not quoted as the effect size.

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

Two candidates died on floors written before the data existed, and the one that
lived did so on every cut it was measured against. Worth recording *why* the two
died, because both were argued for from a single vivid case:

- **#1 was sized by a proxy that measured something else.** `--overlap 0` was
  worth +2.0pp and looked like evidence for the dedupe rule; it changes
  chunking, and the real rule is −0.009. The proxy inverted the sign of the
  thing it stood in for.
- **#3 was right about its mechanism and wrong about its value.** Finer chunks
  demonstrably fix dilution (+4.8pp strict on gold functions under 10 lines)
  and cost more than that elsewhere (−0.052 overlap). Only the two-metric
  bracket §24.1 built could tell those apart — under either metric alone, #3
  reads as a clean result in one direction or the other.

**What §24 does not claim.** Every number here is retrieval quality on replayed
queries. §11.5 stands: whether this changes what an agent *does* is not
purchasable on this benchmark at any n it can hold, and the 9–13% recoverable
pool remains a ceiling containing an unknown share of queries that point
somewhere other than gold. What changed is that the direction §23 closed is not
the only one, and the instrument can now see the half of agent behaviour that
§21 wrote off.

### 24.5 Reproducing §24

`eval/data/` is gitignored, so the three campaign files are not in the tree.
The commands that produce them are, and `--compare-by arm_flags` reads them
back through the shipped harness rather than an ad-hoc script:

```sh
# §24.2 — the 2x2x2 (33,728 rows, ~35 min, no index builds)
ARMS="--dedupe-overlap 0.0 --file-scope-window 0 --decl-boost 0.0"
for d in 0.0 0.5; do for w in 0 12; do for b in 0.0 1.0; do
  ARMS="$ARMS;--dedupe-overlap $d --file-scope-window $w --decl-boost $b"
done; done; done
python3 eval/locbench/guessplay.py \
  --corpus eval/queries/guesses-v1-desc-all.jsonl \
  --out eval/data/locbench/guessplay-v4.jsonl \
  --file-scopes-only --configs default --modes semantic --scopes orig \
  --extra-search-flags "$ARMS"

# §24.2 P6 — the full-corpus confirmation (31,668 rows, both scopes, ~1 h)
python3 eval/locbench/guessplay.py \
  --corpus eval/queries/guesses-v1-desc-all.jsonl \
  --out eval/data/locbench/guessplay-v5.jsonl \
  --configs default --modes semantic,bm25 --scopes orig \
  --extra-search-flags ";--decl-boost 1.0"

# §24.3 — the weight sweep (21,080 rows, ~25 min)
python3 eval/locbench/guessplay.py \
  --corpus eval/queries/guesses-v1-desc-all.jsonl \
  --out eval/data/locbench/guessplay-v6.jsonl \
  --file-scopes-only --configs default --modes semantic --scopes orig \
  --extra-search-flags ";--decl-boost 0.5;--decl-boost 1.0;--decl-boost 2.0;--decl-boost 4.0"

# read any of them back, both metrics, both scopes
python3 eval/locbench/guessplay.py --out eval/data/locbench/guessplay-v5.jsonl \
  --compare ",--decl-boost 1.0" --compare-by arm_flags \
  --compare-metrics rank,rank_func,rank_func_ovl
```

Two things the comparator does *not* do, and which the §24 tables above
therefore state separately. It reports the whole scoped population, so its
file-scope rates (0.347 → 0.371 overlap@5 on v5) are diluted by the 48% of
file scopes that name a file holding no gold function and are `None` for every
arm; §24.2's rates are the right-file subset. And it contrasts one arm against
one base, so the *main effects* — each lever averaged over the other two, which
is what a 2×2×2 is for — come from averaging the four paired contrasts per
lever.

## 25 What the agent is shown, not what the engine scored

§24 shipped a ranking change and left a measurement question open. The engine
scores 32-line windows and prints **one line** per hit, so "the answer was in
the returned window" and "the agent saw the answer" are different claims that
differ by 14 points (§24.1's bracket).

Measured over 400 real file-scoped agent searches: of the 294 where the returned
window contained the answer, **77 (26%) showed the agent a line belonging to
something else** — median 7 lines away, and 64 of those inside a different
function entirely. This section tests two ways to close that, on the only
instrument that can see the difference: real agents.

### 25.1 Pre-registration (written before the first paid run)

**Neither candidate changes ranking, so `guessplay` cannot referee either.**
Replaying queries offline measures which chunks come back; this question is
about what the agent does with them. That is what makes this the first campaign
in the project worth buying.

**The two formats, costs re-measured at k=10 over 150 real agent searches:**

| | median bytes | vs today |
|---|---|---|
| today — one line per hit | 552 | 1.0× |
| `--headers` — span + declared names before each hit | 1,113 | **2.0×** |
| `--full` — every line of all 10 chunks | 11,315 | **20.5×** |

*(An earlier estimate put `--headers` at 314 bytes; that was derived from a k=3
example and is corrected here. The k=10 figure is what the campaign runs.)*

Full chunks never repeat a line: across 10,935 pairs of returned hits **zero
overlapped**, because §24.2's kept dedupe rule drops any chunk sharing a line
with a better one. That rule was retained on its own evidence; it happens to
make this format coherent.

**What is purchasable, computed before proposing the spend.** §11.5 and §19.10
both concluded agent *accuracy* is unpurchasable here — ±0.038 at all 560
instances against effects that are always ≤0.05. That has been the standing
reason not to spend, and it still holds. But the endpoint these formats target
is behavioural. Measured from the 3,502 transcripts already on disk:

| endpoint | paired sd | instances for 80% power |
|---|---|---|
| **reads-after-search per run** | 1.48 | 69 at Δ=0.50, **275 at Δ=0.25** |
| cost per run | $0.148 | 35 for a 25% change |
| input+cache tokens | 270k | 96 for a 25% change |
| `func_acc@10_tol` | — | 682 (§19.10) — never |

Baseline is **1.85 reads-after-search per run** on the shipped arm, so Δ=0.25
is a 14% reduction. *(Corrected before the run: an earlier pass counted a search
by any of the four shimmed tool names and got 1.98. `displaycmp.py` counts only
searches by the arm's **own** tool, because an arm told to type `sg` that emits
`semgrep` is escaping its treatment and must not be scored as if it had not. The
paired sd is 1.48 either way, so the registered power is unchanged.)*

**Design: four arms × 280 instances.** The three `sg` arms are byte-identical
except for a flag `shim.py` injects invisibly — "appended to the real invocation
but never shown to the agent — its commands and the logged argv stay clean" — so
the contrast is display and nothing else.

| arm | tool line | injected |
|---|---|---|
| `rg` | the rg line | — |
| `disp-line` | desc-v9 (shipped) | *(none)* — internal control |
| `disp-full` | desc-v9, identical | `--full` |
| `disp-head` | desc-v9, identical | `--headers` |

`disp-line` cannot be replaced by reusing existing desc-v9 rows: those came from
a pre-§24 binary, and comparing across binaries is the §23.3 trap exactly.

**Registered limitation.** desc-v9 tells the agent output is `path:line:text`,
which under-describes `--full`. Changing the description per arm would confound
display with the strongest lever this project has measured (§19: description
moved ranked share 7%→98%), so it is held identical and `--full` runs
*handicapped by a description that undersells it*. A win is therefore strong; a
null is ambiguous between "the format does not help" and "the agent did not know
to expect it."

**Frame: a plain random 280 of 560, seed 25, recorded as
`eval/data/locbench/display-frame-280.json` (sha256 `80bda274604a0062`)** — deliberately not
`tierframe.py`'s equal strata, which exists because §19.2b predicted the
description effect lives entirely in `blind`. No such prediction applies here,
and the primary endpoint is continuous, so every instance contributes and the
frame stays pooled-comparable to §16.9/§18. §11.5's discriminative screening is
inapplicable for the same reason: it buys McNemar power on binary accuracy.

**Registered predictions:**

1. **Primary — `--full` reduces reads-after-search.** `disp-full` vs
   `disp-line`, paired within instance, bootstrap CI over instances (4,000,
   seed 1). Direction registered (a reduction), reported two-sided, powered to
   **Δ=0.25**. *Kill:* a positive delta falsifies the mechanism — the agent
   reading *more* despite being shown more.
2. **Co-primary — cost and tokens, registered as an expected loss.** `--full`
   will cost more per run; pricing it is half the point. Powered to a 25%
   change at n=35/96, so both resolve. A cost increase with a null on P1 is the
   clearest possible reject.
3. **`--headers` buys most of it for a tenth of the bytes.** Registered:
   `disp-head` achieves ≥ half of `disp-full`'s reduction at ~2× the output
   rather than ~20×. If so, headers win on efficiency whichever reduces more.
4. **Accuracy is a bounded secondary and is not powered.** `func_acc@10_tol`
   over all pairs, Holm-corrected, with the detectable bound printed beside it.
   Registered per §19.10 so a null is never read as an absence it cannot
   support.
5. **`disp-full` vs `rg`** — the product claim, secondary, same bound.
6. **Tripwire — truncation.** Count runs whose search results appear truncated.
   `out.rs` documents that the agent's tool-result limit silently deletes hits
   ranked below a long one; at 20× bytes this is the specific way the treatment
   backfires while looking healthy.
7. **Tripwire — identical descriptions.** The three `sg` arms' recorded
   `tool_line_text` must be byte-identical. `run.py` writes it per run for
   exactly this purpose (§16 C2).
8. **Tripwire — one binary** across the campaign, and `triage.py` clean per
   tier.
9. **Gate — did the arms change query *style*?** `queryshape.py` over the shim
   logs. The intent is that arms differ only in what came *back*; a style shift
   means the display changed how agents write queries, and every downstream
   reading is then conditional on that.

**Budget, re-priced on a 6-instance × 3-arm smoke before the frame was
launched:** `disp-line` $0.280/run, `disp-head` $0.266 (0.95×), `disp-full`
$0.359 (**1.28×**, not the 4.3× a single instance had suggested). With `rg` at
its historical $0.283 that is **~$332 for 280 × 4**, inside the approved range.
The overage is registered measurement #2 and is recorded rather than absorbed.

The same smoke showed `disp-full` using *fewer* searches than the control
(3.8 vs 6.7 over 6 instances) — directionally what P1 predicts, at a sample far
too small to be evidence, and noted here only because it was visible before the
frame ran and should not be presented afterwards as though it were a
prediction.

### 25.2 Full chunks change what agents do; headers change nothing

Run 2026-08-06/08, `results-display.jsonl`, 1,156 rows over the registered
280-instance frame, **278 instances complete in all four arms** — the frame
delivered its registered power (detectable Δ=0.249 against a registered 0.25).
Spend **$295.76**, under the $317 the plan estimated and well under the $406 an
early-rows projection had feared; the tail is cheaper than the head, because
the opening chunks carry the retries.

**P1 — passes, at three times the registered effect.** Reads-after-search per
run, `disp-full` against `disp-line`, paired over 280 instances:

| endpoint | control | `--full` | Δ | 95% CI |
|---|---|---|---|---|
| **reads after a search** | 1.729 | 0.921 | **−0.807** | [−1.007, −0.611] |
| reads (all) | 3.293 | 1.821 | −1.471 | [−1.764, −1.189] |
| searches | 3.418 | 2.789 | −0.629 | [−0.932, −0.354] |
| turns | 9.13 | 6.99 | **−2.14** | [−2.72, −1.58] |
| median search bytes | 609 | 12,630 | +12,021 | [+11,150, +12,927] |

The agent opens the file after a search **47% less often**, and the whole
trajectory shortens: two fewer turns, and it searches less as well as reads
less. The registration was powered for 0.25 and the effect is 0.81.

**P2 — the cost is real, and it is not where it was expected.** `--full` costs
**+$0.042/run [+0.024, +0.059]**, about 18%. But it is not paying for volume:

| usage | control | `--full` | Δ |
|---|---|---|---|
| output tokens | 2,692 | 2,325 | **−367** [−645, −78] |
| cache **read** tokens | 272,524 | 261,316 | −11,207 (null) |
| cache **creation** tokens | 17,684 | 26,106 | **+8,421** [+6,464, +10,417] |

Twenty times the bytes per search produces *no* significant change in total
tokens read and *fewer* output tokens, because the shorter trajectory cancels
the bigger results. The entire premium is **cache creation**: each large tool
result is a new block that has to be written to the cache, and writes are the
expensive direction. §2.1's framing survives intact — "fewer, better
round-trips beat cheaper individual round-trips" — and this is that trade
priced: **2.14 fewer round-trips for 18% more dollars.**

**P3 — fails, and it is the most interesting failure here.** `--headers` was
registered to deliver ≥ half of `--full`'s reduction (so ≤ −0.40) at a tenth of
the bytes. Measured: **−0.007 [−0.175, +0.168]** on reads-after-search, +0.179
on reads, 0.000 on searches. A flat null on every behavioural endpoint, at 1.9×
the bytes.

That is a direct refutation of the reasoning that motivated it. §25.1 sized
headers from the finding that naming a chunk's declarations would surface the
gold function in **88%** of the cases where the shown line missed it. That
number was about *availability*, and it was correct. It predicted nothing,
because **an agent that is told the answer is nearby still opens the file.**
Only being handed the code removes the reason to. Availability is not use, and
88% of a gap closed on paper bought exactly zero behaviour.

**P4 — accuracy unmoved, and bounded rather than implied.** `func_acc@10_tol`:
`--full` **+0.000 [−0.032, +0.032]**, `--headers` −0.011, `rg` −0.011; McNemar
p = 1.000/0.648/0.629. `file_acc@5` likewise null. Per §19.10's rule the bound
is published beside the null: this frame resolves ±0.032, so an accuracy effect
smaller than that is not excluded and is not claimed either way. **The display
format changes the route, not the destination.**

**P5 — `--full` beats `rg` on every efficiency endpoint**, paired over 280:
reads-after-search **−0.504 [−0.682, −0.339]**, reads −0.721 [−0.964, −0.496],
searches −0.696 [−1.007, −0.386]. Note `rg` also beats the *control* on
reads-after-search (−0.304): one line of grep output is a weaker invitation to
open a file than one line of ranked output, presumably because grep's line is
the literal match. Full chunks beat both.

**P6 — truncation tripwire holds, and this is not a small thing.** Zero
truncated search results in any arm, including 12.6 KB medians. The specific
way this treatment could have backfired invisibly — the agent's tool-result
limit deleting hits ranked below a long one, which `out.rs` documents from a
659 KB incident — did not occur once in 280 runs.

**P7 — descriptions byte-identical** across the three `sg` arms
(`tool_line_sha256`, one distinct value).

**P8 — fired, and benign on inspection.** Two tripwires went off:

- *Binaries.* Eight distinct `semgrep_sha256` on a naive count, which collapses
  to **two** once historical runs on the same instances are excluded. No commit
  touched `crates/` during the campaign, and the two hashes are distributed
  near-identically across all four arms (248/41, 249/41, 253/44, 248/40). These
  are relinks of frozen source — §23.3's finding 2 exactly, that the fingerprint
  tracks the link and not the code, and can false-alarm but never false-pass.
- *Triage.* `triage.py` failed three checks at the 63-row mark: one unknown flag
  (`--iC`, an agent typo against a short compat surface), one instance whose
  every search used a nonexistent path ("tool correct"), and four non-ok rows.
  The errors were **arm-symmetric** (rg 2, line 1, full 2, head 2) with five of
  seven from a single instance failing in every arm, and a historical campaign
  fails the same gate. Recorded as fired rather than quietly passed. The check
  that would have implicated the treatment — ranked searches returning nothing,
  the §16.11 signature — was 0 of 213.

**P9 — the gate passes: agents did not change how they write.** `queryshape.py`
over the campaign's shim logs gives `disp-full` vs `disp-line` identifier share
67% vs 67%, plain-word 25% vs 27%, paraphrase 3% vs 3%. Mean words/query is
+0.42. The arms differ in what came *back*, which is what the design intended,
so the behavioural readings are not downstream of a style shift.

**Ledger.** Seven of nine as registered (P1, P2, P4, P5, P6, P7, P9), one
decisive failure (P3), one tripwire fired and diagnosed (P8).

**What ships, and what does not.** `--full` is the first change in this project
measured to alter agent behaviour: half the file-reopening, two fewer turns,
same accuracy, 18% more cost. That is a genuine trade rather than a free win,
and it is a *default* question rather than a *feature* question — the flag
exists either way. `--headers` is measured and not adopted: it costs 1.9× the
bytes and buys nothing an agent does differently.

**What this does not settle.** Accuracy is bounded at ±0.032 and untouched, so
none of this is evidence that agents *solve more*. What it buys is a shorter
route to the same answer, and the 18% is charged on a cache-write behaviour that
is a property of this harness's caching, not of the format. A different client
that does not cache tool results this way would see the token null and not the
dollar cost.

### 25.3 Three analysis bugs, caught before the data existed

Every one of these would have produced a confident wrong answer on a $296
campaign, and all three were found by running the analyser on partial data
rather than waiting for the frame:

1. **The sign was inverted against its own label.** `boot_ci` returns
   `mean(first) − mean(second)`; pairs were passed `(base, cand)` under headings
   reading `cand − base`. The primary effect would have published as **+0.81
   reads-after-search — an increase — when it is −0.81.**
2. **It swept in the smoke runs.** Same arms, same binary, not the registered
   frame; n would have grown with instances chosen after the fact.
3. **It paired on `(run, instance)`.** A resumed campaign writes a new run
   directory each time, so an instance's arms routinely land in different runs.
   That silently discarded **53 of 278 instances** — a fifth of what was paid
   for — and the fix is `ab_analyze.load`'s own rule: key by instance, latest
   run wins.

A fourth was a crash rather than a wrong answer: an interrupted run writes a
line whose `message` is a bare string, and the walker died on it — on exactly
the campaigns worth analysing.

The general lesson is the one §12 already drew and this section pays for again:
**the analysis path deserves the same pre-run verification as the treatment.**
A registered prediction protects against choosing the hypothesis after the fact.
It does nothing about an arithmetic error, and three of the four above would
have survived any amount of pre-registration.

## 26 Passages by default, at eighteen lines

§25 established that showing the whole 32-line passage instead of one line is
the first change in this project to alter agent behaviour: file-reopening fell
**1.729 → 0.921** over 1,120 sessions, sessions ran **2.14 turns** shorter,
accuracy did not move (±0.032), and it cost **+18%** — entirely in cache
*writes*, since the model read no more and wrote less.

It also killed the cheap alternative and left the most transferable lesson in
the arc. Region headers, sized from a finding that naming a passage's
declarations would surface the answer in **88%** of the cases a single line
missed, moved **nothing** (−0.007 [−0.175, +0.168]). **Availability is not
use**: being told the answer is nearby does not stop an agent opening the file,
and only being handed the code does.

That leaves 20× output as the price of a real win. §26 ships the cheaper point
on that curve and buys the campaign that says whether it holds.

### 26.1 Pre-registration (written before the first paid run)

**The shipped default is an 18-line passage, 8 before the match and 9 after.**
Chosen from a coverage/bytes curve over 232 real agent searches: 18 lines holds
**94% of the whole passage's coverage for 46% of its bytes**, and the two steps
beyond it cost 1,520 and 1,921 bytes per point gained against ~460 below.

The extra line goes *after* on measurement, not intuition. "A declaration is
followed by its body" is a plausible reason to lean forward and it is wrong:
8/9 scores 57.3%, 6/11 and 4/13 both 53.9%, and 0/17 falls to 50.0%. One line
of forward bias is the whole of it.

**Output costs, measured on 150 real searches with the shipped binary** —
the plan's estimates were derived in Python without the line-number prefix and
are corrected here to what the tool actually emits:

| arm | flags | median bytes | vs control |
|---|---|---|---|
| `pl-1` | `--passage-lines 1` | 556 | 1.0× — the pre-§26 default |
| `pl-18` | `--passage-lines 18` | 5,874 | **10.6×** — the new default |
| `pl-18k5` | `--passage-lines 18 -k 5` | 2,917 | 5.2× |
| `pl-full` | `--full` | 10,796 | 19.4× — §25's measured winner |

**Shipping before measuring, and why that is defensible here.** The default
rests on a coverage curve, which is the evidence class that failed for labels.
The difference is that this is a *reduction from a measured winner* rather than
a new idea: the mechanism §25 proved — the agent can read the code and stop —
is preserved at 18 lines, and only its sufficiency is unknown. Labels removed
the code entirely; 18 lines does not. **If the campaign shows 18 lines loses the
effect, the registered response is to move the default to the whole passage,
not to explain the result.**

**What changed for every caller**, recorded rather than discovered later: output
is ~10× larger; results are separated by a blank line; a consumer *counting*
output lines now sees ~180 rather than 10, though every non-blank line is still
`path:line:text` and parses as it always did. Three CLI tests counted lines to
count results and all three failed — the canary for exactly that breakage, now
fixed to assert what they meant. `MAX_COLUMNS` still clips every line, so the
worst case is ~36 KB against the ~64 KB of the 32-line arm where §25's
truncation tripwire measured **zero truncated results in 1,120 sessions**.

`tools/snapshot.sh` pins `--passage-lines 1` rather than re-recording. It is a
*ranking* tripwire; recording passages would bloat every case tenfold and make
it move whenever the fixture's text changes with ranking identical. The file
stays byte-comparable with every recording since §20, and that identity is also
the proof that `pl-1` reproduces the pre-§26 output exactly — which the control
arm depends on. The new display shape is pinned instead by
`the_default_result_is_an_eighteen_line_passage`.

**Design: four arms × 140 instances, ~$157** at the $0.28/run §25 measured.
Every arm passes an explicit `--passage-lines`, so no arm inherits the new
default and the contrast cannot drift with it. Frame: **140 drawn at seed 26 from the
280 instances §25 never ran** — `passage-frame-140.json`, sha256
`3a8962d12634dbce`, recorded before the first run. Zero overlap with §25's
frame, by construction rather than by luck: a plain random 140 of 560 would
share about half of it, and "independent sample" should mean what it says.

*(Overlap would not in fact threaten the primary test, which is a within-campaign
paired contrast between `pl-18` and `pl-full` and re-measures both. §25's
estimate only sets the margin. The complement is used because the claim was
made, not because the alternative was unsound.)*

**The primary test is non-inferiority, and the margin is what n buys.** Asking
"do 18 and 32 differ" and answering "not significantly" is how an underpowered
null becomes a false claim of equivalence, and this project has published
enough nulls to know the difference. At n=140 and the measured paired sd of
1.48, the margin is **0.35**.

1. **Primary — `pl-18` is non-inferior to `pl-full`.** The 95% CI on
   (`pl-18` − `pl-full`) reads-after-search must exclude **+0.35**. Against
   full's −0.807 that is "18 lines retains at least 57% of it". *Kill:* if the
   CI includes +0.35, 18 lines is not established and the default moves to the
   whole passage.
2. **Sanity — `pl-18` beats the control.** `pl-18` − `pl-1` negative with a CI
   excluding zero. A primary that passes while this fails means neither arm did
   anything and the campaign measured noise.
3. **`pl-18k5` — the same non-inferiority test at five results.** Registered
   separately and expected weaker: §25 measured ranks 6–10 carrying 10 points
   of coverage (71.3% → 81.4%).
4. **Co-primary — cost, as a prediction rather than a measurement.** `pl-18`
   should cost ~**10%** over control and `pl-18k5` ~5%, because §25.2
   established the premium is proportional to output bytes through cache
   creation. A cost that does not scale with bytes falsifies that mechanism and
   is worth more than the arm it came from.
5. **Accuracy, bounded and not powered.** `func_acc@10_tol` across all pairs
   with the detectable bound printed beside it (~±0.045 at n=140), per §19.10.
6. **Tripwire — truncation.** Zero expected: every arm is smaller than the
   32-line arm that already measured zero.
7. **Tripwire — one binary, identical descriptions** across the four arms
   (`tool_line_sha256`), and `triage.py` run and *recorded* rather than assumed
   clean — §25's fired on arm-symmetric noise and the same is expected again.
8. **Gate — `queryshape.py`.** Query style must not shift between arms; a shift
   means the display changed how agents write rather than only what they read.

### 26.2 Eighteen lines is worse, and the economy it was for does not exist

Run 2026-08-08, `results-passage.jsonl`, 579 rows, **138 of 140 instances
complete in all four arms** — the frame delivered its registered power exactly
(margin 0.353 against a registered 0.35). Spend **$140.02**, under the $157
estimated.

**P1 — fails, by 0.014.** Registered: the 95% CI on (`pl-18` − `pl-full`)
reads-after-search must exclude **+0.35**. Measured **+0.243 [+0.121,
+0.364]** — the upper bound clears the margin by fourteen thousandths.

Two things about that failure, and the second matters more than the first.
It is *not* merely a power shortfall: the interval **excludes zero**, so 18
lines is measurably worse than the whole passage rather than unproven against
it. And the point estimate says 18 lines retains 70% of the effect, while the
interval cannot rule out 55%. Both readings are in the data; the registered
test asked one question and got one answer.

| arm | reads after a search | vs control | vs `pl-full` | bytes | cost |
|---|---|---|---|---|---|
| `pl-1` | 1.564 | — | +0.800 | 605 | — |
| `pl-18k5` | 1.107 | −0.457 [−0.664, −0.250] | **+0.343 [+0.171, +0.529]** | 3,435 | **−12%** |
| `pl-18` | 1.007 | −0.557 [−0.779, −0.343] | **+0.243 [+0.121, +0.364]** | 7,167 | −3% |
| `pl-full` | 0.764 | −0.800 [−1.050, −0.564] | — | 11,636 | +5% |

**A clean dose-response.** More lines, more effect, monotonically, with every
contrast against control excluding zero. That is the shape of a real mechanism
and it is the opposite of what the coverage curve implied: 18 lines holds 94% of
the whole passage's *coverage* and only **70% of its behaviour**. §25's lesson
lands a second time — availability predicted behaviour and was wrong again,
this time about a quantity rather than a kind.

**P2 — passes.** `pl-18` beats the control by −0.557 [−0.779, −0.343], so both
arms did something and the primary is a comparison between two live treatments.

**P3 — fails clearly.** `pl-18k5` gives back +0.343 [+0.171, +0.529], well past
the margin. Registered as the weaker arm and it is.

**P4 — the prediction fails and the mechanism survives, which is the most
useful result here.** Registered: cost premium proportional to output bytes via
cache creation — `pl-18` ~10%, `pl-18k5` ~5%. Measured:

| | bytes | cache creation | output tokens | cost |
|---|---|---|---|---|
| `pl-18k5` | 5.7× | **−3,207** [−4,679, −1,661] | −240 | **−12%** [−0.046, −0.007] |
| `pl-18` | 11.8× | +915 [−981, +2,940] | −467 | −3% (null) |
| `pl-full` | 19.2× | **+5,635** [+3,065, +8,256] | −687 | +5% (null) |

Cache creation scales with bytes exactly as §25.2 said. **Cost does not**,
because the shorter trajectory's output-token saving cancels it. Passages are
not expensive: the whole passage is **+5% [−4%, +13%]**, a null, and both
shortened arms are *cheaper than showing one line*.

That is a failure to replicate §25.2's headline +18% [+2.4%, +5.9%]. The
intervals barely overlap. §25 ran 278 instances and §26 ran 138 disjoint ones
with the same binary and the same measurement, so the honest reading is that
the cost premium is smaller and noisier than one campaign suggested — and that
**the entire economic case for shortening was built on a number that did not
hold.** The lever was chosen to buy something that was not for sale.

**P5 — accuracy unmoved and bounded.** `func_acc@10_tol`: `pl-18` +0.014
[−0.022, +0.051], `pl-full` −0.007 [−0.036, +0.022], `pl-18k5` +0.000
[−0.036, +0.036]. All null at a resolution of ±0.045, per §19.10.

**P6 — truncation zero** in all four arms, as registered.

**P7 — tool lines byte-identical** across the four `sg` arms, one binary.

**P8 — query style unchanged**, so the arms differ in what came back.

**The registered response, applied.** §26.1 said: *"If the campaign shows 18
lines loses the effect, the registered response is to move the default to the
whole passage, not to explain the result."* It did, so **the default is the
whole passage.** `--passage-lines` stays as the knob, and 18 remains reachable
for anyone who wants the trade.

Writing that rule in advance was worth the whole exercise. The temptation with
+0.364 against a 0.35 margin is to observe that it misses by 0.014, that 18
lines keeps 70% of the effect at 62% of the bytes, and to keep it. Both of those
statements are true. Neither is the test that was agreed to, and the cost
argument that would have justified bending it turned out to be measuring noise.

**Ledger.** Six of eight as registered (P2, P5, P6, P7, P8, and P4's mechanism),
two decisive failures (P1, P3), and one prediction inside P4 falsified in a way
worth more than the arm that produced it.

**What §26 leaves.** A default that is now measured rather than inferred, a
knob that makes the trade available, and a second, sharper instance of the
availability trap: it is not only that naming a thing fails to change behaviour
(§25's labels) — *showing 94% of it changes only 70% as much.*

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
turns against 9.17, 36.3 s against 39.1, accuracy tied. No tradeoff argument is
needed for that comparison; it is better on every axis at once. Against the
whole passage it *is* a trade: **16% cheaper for 1.5 turns and 7 seconds.**

The mechanism is the one §25.2 found, read the other way. Richer results
monotonically cut what the model reads (269,729 → 226,623 tokens) and writes
(2,624 → 1,937) because the session shortens; what rises is **cache writes**
(17,248 → 22,883), and those are the expensive direction. `k=5` is the only arm
with *fewer* cache writes than the control (14,041): it shortens the session
without inflating each result.

**Shipped: `k=5`, `passage_lines=18`.** And this is an endpoint switch made
after seeing the data, which is exactly what pre-registration prevents. Cost was
registered as a co-primary so it is not a fished result, but the *decision rule*
was written on reads-after-search and is being overridden. Recorded here rather
than presented as the plan. Two things temper it: the −16% interval excludes
zero comfortably, and cost is nonetheless the endpoint that already failed to
replicate once (§25's +18% became §26's +5%), so a confirmation on an
independent frame is owed before the number is quoted as settled.

`desc-v9` still says "top 10" and now returns 5. The mismatch is *as measured* —
the `pl-18k5` arm ran with that description — so the description stays frozen
under its own name (§20.1's rule). A `desc-v10` saying "top 5" is a separate
arm, not an edit.

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
unit `ChunkParams::budget` already uses for chunking, for the same reason
(§20.2).

**800 characters, because it is the equivalence point.** Over 109 real agent
searches at k=5 it scores **51.4% at 2,880 bytes** against 18 lines' **51.4% at
2,853** — the same behaviour, to the search. 600 costs 2,140 and scores 48.6%:
three searches fewer out of 109, which is noise and may well be free. It is not
taken, because changing the unit *and* the effective size together would leave
the next campaign unable to say which one moved.

What it buys, across the three corpora: **5,492 / 8,413 / 2,321** — prose falls
**83%** and the worst corpus **38%**.

**What it does not buy, and the first attempt assumed it would.** It does not
equalise cost across languages; the spread stays ~3.5×. Roughly half of printed
output is the per-line `path:line:` prefix, which scales with *line count*
rather than content, so a content budget hands short-line C more lines and more
overhead — the first measurement at 600 characters actually *inverted* the
problem, making the kernel dearer than Wikipedia. Charging a `LINE_OVERHEAD`
recovers part of it; the path part is not knowable in the engine, which cannot
tell whether the CLI will print one. **The property delivered is a bounded worst
case, not a flat cost**, and saying otherwise would be claiming the thing that
was tried and failed.

Unmeasured, and it should be said plainly: every number in §26.4 is coverage
and bytes. Whether 4 lines of prose is *enough to act on* where 20 lines of C
is has not been tested on an agent, and §25's labels are the standing warning
about exactly that inference.

## 27 Claude Code with semgrep enabled, on SWE-Explore (2026-08-08)

Every agent-scale result this project has — §16 through §26 — was measured on
Loc-Bench, and measuring the measuring instrument found three limits that bound
all of them.

**It is 100% Python.** All 1,149 gold files across 560 instances are `.py`. The
whole-passage win (§25), the 800-character budget (§26.4) and desc-v9 are
Python results carrying a cross-language recommendation.

**A sixth of every campaign was inert.** Across 5,394 ok rows, **922 (17.1%)
never invoked the search tool at all** — and they *out*-score the ones that did,
0.868 against 0.771 on `file_acc@5`. Inertness tracks the issue-text tier
exactly: 23.0% in `named`, 12.8% in `partial`, 6.3% in `blind`. A session that
never searches cannot respond to a search change, so this is very likely the
mechanism behind §19.10's "agent accuracy is unpurchasable at ±0.038". That was
read as an instrument limit; a good part of it is frame composition.

**And the benchmark forces a choice nobody faces.** `run.py` removes `Grep`
from the agent entirely, so §16–§26 measured semgrep *instead of* ripgrep. The
product question — does semgrep help an agent that **still has grep** — has
never been asked here.

SWE-Explore (arXiv 2606.07297, June 2026) answers all three. 848 issues over
203 repositories in **10 languages**; the task is a ranked list of
`(path, start, end)` at K=5, which is literally semgrep's output shape; and the
gold is line-level regions derived from what *successful repair trajectories
actually read*, intersected across ≥2 trajectories and manually audited. Its
published `claude_code` explorer is stock Claude Code with ripgrep-backed
`Grep` — the control this project has never run.

### 27.0 What the setup cost, and the four defects it found

Three things about the dataset had to be established before any design was
possible, and two of them changed it.

**Checkouts are per-instance.** `fetch_repos.py` downloads one tree per
`instance_id` at its own `base_commit`, so gold line numbers are valid. This
was the gate that would have invalidated everything and it passes.

**The issue text is not in the dataset.** Upstream resolves it from a
`unify_trajs/` directory that is not in the repo and not published anywhere we
could find; without it no agent arm can run. Rebuilt from the three sets the
instances were drawn from — SWE-bench Verified (451), SWE-bench Multilingual
(182), SWE-bench Pro (215, after stripping its `instance_` id prefix) —
**848/848**. That is better provenance than a trajectory dump, and it surfaced
that Pro's statements are rewritten ("# Description:", curly quotes) rather than
raw issues, which is a query-distribution difference worth stratifying on.

**No prefetch is possible.** 19 checkouts sampled across all ten languages
average **32.1 MB** (1.2 MB axum → 113 MB teleport), extrapolating to **26.6 GB**
for 848 — against 21 GiB free, before indexes, which live inside the checkout.
So the runner fetches, indexes, runs all three arms and evicts, under a
byte-capped LRU. An LRU and not a refcount because `eval_runner.py`'s loop is
explorer-major: each instance is visited once per arm in three separate passes,
and a refcount freeing after the last arm would never free during the first.

Two properties of the gold are worth recording because they shape which metrics
mean anything. Core regions per instance: median 4, mean 4.7. Core region
*size*: **median 5 lines, p90 1,037, max 9,705** — 59.4% are ≤32 lines and
**29.8% are over 200**, because the trajectories the gold is built from include
whole-file reads. So `Rec_ℓ` is dominated by the giant regions and `HitRegion`
by the small ones, and the two answer different questions.

#### The four harness defects

The harness is SWE-Explore's own, patched: their dataset, their prompt, their
`eval.py`, plus a 98-line purely-additive patch to `eval_runner.py` and three
new files. Four defects surfaced across four smoke runs costing about four
dollars. **Every one of them would have produced a clean, publishable, wrong
number.**

1. **32 leaked MCP tools.** The first smoke exposed 36 tools to the agent, 32
   of them MCP servers from the operator's own config — Google Drive, Gmail,
   Playwright. Contamination three ways: capabilities the benchmark never
   granted, a system prompt inflated by 32 tool schemas, and a configuration
   nobody else could reproduce. The middle one is the worst, because prompt
   size *is* cache-creation tokens, which is the co-primary endpoint. Fixed
   with `--strict-mcp-config` and `--setting-sources ""`; cost per run fell
   from $0.09–0.51 to $0.03–0.16.
2. **`--permission-mode dontAsk` does not enforce `--allowedTools`.** It means
   "do not prompt", not "restrict". With Bash enabled the agent ran
   `grep -n "n_jobs" sklearn/...` directly and never touched its own tool.
   locbench never relied on the allowlist for this — it blocks `grep`/`git`
   with PATH shims (`run.py:477-494`) — and dropping those shims was an error
   made here. Left in, every Bash-enabled arm does lexical search without
   invoking its treatment, all three arms converge on shell grep, and the
   campaign reports a null that means nothing.
3. **Upstream's prompt steers away from the treatment.** `EXPLORE_PROMPT` says
   "Use Glob, Grep, and Read tools to explore the codebase." Measured
   consequence: `bash_calls` was **0** in every arm on every instance, all
   three arms returned identical answers, and the treatment was simply never
   delivered. An appended system prompt saying a tool exists does not survive a
   user prompt naming three others — §25's *availability is not use*, one level
   up, at the tool surface rather than the passage. The clause is now amended
   per arm, one tool name added in the same position, with `cc` keeping
   upstream's prompt byte-for-byte and an assertion that fires if upstream
   rewords it.
4. **Silent skips.** Transient archive-API failures under four workers dropped
   **29 of 31** instances from a pass: the fetch returned `None`, the runner
   skipped, and the arm simply came back short. Silent skips are the worst
   failure mode available here because they cost money *and* select which
   instances get measured. Now retried with backoff.

#### A correction to the paper's own numbers

The tables were first read through an automated HTML fetch, and several model
labels came back garbled. Checked against the PDF, every quoted number holds —
Claude Code at HitReg 0.531, HitFile 0.667, CtxEff 0.829, 48.0% downstream
resolve — but the models do not. The real set is **GPT-5.4**, GPT-5.4-mini,
Kimi-K2.6, Sonnet-4.5, GLM-4.7, Gemini-3-Pro, and "all agentic explorers are
driven by GPT-5.4".

**So their "Claude Code" row is the Claude Code *scaffold* routed to GPT-5.4,
not a Claude model**, and a Sonnet arm cannot reproduce it. Their own Table 5
prices the swap under a fixed scaffold: GPT-5.4 → Sonnet-4.5 moves HitReg
0.516 → 0.428 and CtxEff 0.771 → 0.715. The calibration gate is therefore
retargeted at the Sonnet row and read as a band, not an equality.

### 27.1 The pilot (n=31, exploratory)

Three arms — `cc` (Read, Glob, Grep: upstream's baseline), `cc-rg` (+ `Bash(rg *)`),
`cc-sg` (+ `Bash(sg *)`) — over a language-stratified 31 that deliberately
oversamples non-Python (6 Python of 31). Paired, `boot_ci`, 4,000 resamples,
seed 1.

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

**The third arm has already paid for itself twice.** It clears the coverage
result — Bash alone is +0.004 and −0.009, flat — so semgrep's +0.062 is not a
shell effect. And it *takes one away*: **ctxEff is a Bash effect, not a semgrep
effect** (+0.055 for `cc-rg`, +0.051 for `cc-sg`, indistinguishable). Run as two
arms, semgrep would have been credited with the context-efficiency gain, and
since CtxEff is the metric the paper's own Table 4 ranks highest (Pearson +0.950
against downstream resolve), that is exactly the claim most likely to have been
published.

Only **1 of 31** instances produced identical regions across `cc` and `cc-sg`,
so the arms genuinely diverge; the earlier 4-instance smoke that suggested
convergence was small-sample noise.

**Invocation rate**, the tripwire that decides whether any of this is
measurable: `cc-rg` 16/31 (52%), `cc-sg` 14/31 (45%), at 2.4 and 1.2 calls per
session respectively. Not the registered 70% floor — that floor was wrong and is
restated in §27.2 as a dilution factor — but far from the 0% the pre-fix smokes
measured. Per language, `sg` usage: Go 3/3, Rust 2/3, C 2/3, JS 2/3, TS 2/3,
Python 2/6, Java 1/3, Ruby 0/3, PHP 0/3.

**None of the above is a result, and §18.6 is the reason to say so.** There, an
independent second small tier reversed a sign, and the note reads: "had tier 1a
run alone, +0.050 would have looked like a result." The starred endpoints here
rest on 8–11 discordant pairs; nine endpoints carry no multiplicity correction;
the frame is not population-weighted; and `precision` moves *opposite* to
recall, which is §24.1's signature of a geometry change rather than a quality
one.

One number deserves more suspicion than the rest. Split by whether the tool was
actually invoked, `hitRegion` gains **+0.072 [+0.010,+0.142]** where `sg` ran
(n=14) and **+0.054 [−0.000,+0.110]** where it did not (n=17). A tool that is
never called cannot cause the second figure. Either it is noise at n=17, or the
amended prompt clause is doing work on its own — and the split is
post-treatment conditioning either way, so it is descriptive, not causal. Both
readings are testable at n=848, and until then the headline is soft.

**Power.** From the pilot's own paired standard deviations (hitRegion 0.126,
cost 0.097), at 80%:

| n | hitRegion MDE | cost MDE | turns MDE |
|---|---|---|---|
| 150 | 0.029 | 0.022 | 1.00 |
| 400 | 0.018 | 0.014 | 0.61 |
| **848** | **0.012** | **0.009** | **0.42** |

### 27.2 Pre-registration for the powered run (n=848)

**Provenance of this registration, stated because it matters.** The endpoints,
thresholds and analysis below were fixed in the approved plan before R1 ran and
are transcribed here unchanged. What is *not* clean: R1's interim (n=150) has
been seen, because the plan explicitly registered the independent-subset check
as something that would be computed and reported. It was registered as
descriptive and non-stopping, and it has not moved a single threshold here.
Recording that is the difference between a registration and a rationalisation.

**The ladder.** One run id (`s27`) across all rungs, each rung a longer prefix
of `bench-ladder.jsonl` (seed 27, sha `fe88b90f`), so nothing already paid for
is re-run and every rung pools. R0 n=31 → R1 n=150 → R2 n=848. Gates are
**harness health only** (`triage_swex.py`, nonzero exit); no stopping rule reads
an endpoint, so there is no sequential-testing alpha to spend.

**Primary.** `hitRegion@5`, `cc-sg − cc`, paired bootstrap (`ab_analyze.boot_ci`,
4,000 resamples, seed 1, resampling instances). MDE 0.012 at n=848, from a
paired sd of 0.126 measured on the pilot and independently confirmed at 0.128
by the full-vs-full retest control.

**Co-primary — cost.** `total_cost_usd` and `num_turns`, paired. §25.2's
registered mechanism predicted +5–10% cost with turns *flat or down*. R1
measured **+11% cost and +0.83 turns**, both with p<0.001 on the sign test, so
the turns half of that prediction is already contradicted and is recorded here
as a **failed prediction**, not adjusted to fit.

**Confound.** `cc-rg − cc` printed beside every endpoint. R1 has it flat on
coverage (+0.001 hitRegion) and *not* flat on cost (+$0.0136, +0.43 turns), so
most of the cost increase is the Bash tool rather than semgrep — the
semgrep-specific increment is about +$0.005.

**Secondary**, Holm-corrected as a family: `hitFile@5`, `ctxEff`, `nDCG@500`,
`recall@100`, `precision`. `nDCG@500` (0.971) and `FUH` (0.974) are near ceiling
and will be underpowered; the bound is printed rather than the null asserted.

**Per-language**, exploratory and population-reweighted. Every stratum was
unpowered at n=150 (Go 15, all others ≤8); at n=848 only Python (547) and Go
(84) will be, and C++ (1) never will be. Strata under n=8 are not reported.

**Tripwires.** Invocation rate is a **dilution factor, not a floor** — the
registered 70% was wrong and is withdrawn; R0 measured 45% and R1 35%, and an
effect over all instances is diluted by the non-invoking share. Truncation = 0.
`cc`'s user prompt sha256 equal to upstream's. Malformed-output rate symmetric
across arms. One binary sha256 throughout.

**Calibration** is retargeted at the paper's Sonnet-4.5 row (HitReg 0.428,
CtxEff 0.715), not its Claude Code row, because §27.0 established that every
agentic explorer there is driven by GPT-5.4. Read as a band, not an equality.

**Registered expectation, written before the pooled result.** R1's independent
119 gave **+0.010 [−0.0095, +0.0305], w/l 15/13** on the primary — the pilot's
+0.062 did not replicate, exactly as its at-MDE flag predicted. The honest
expectation for n=848 is therefore **a small positive or a null**, and the
likely deliverable is a *bound*: enabling semgrep improves region coverage by no
more than roughly 0.012 while costing about 11% more. That is a useful result
and §23.2 is the precedent for publishing one.

**Registered response to a null.** Report it as a bound with the conservative
bias attached — the gold is what grep-driven agents read, so a region semgrep
surfaces that those trajectories never needed scores as noise, and the
detectable effect is a lower bound. Do **not** re-cut for a stratum that moved.
§17.5 and §26.3 are both in the record as cases where the obvious follow-up was
the wrong move.

### 27.3 The result: a powered null on quality, at 18% more cost

848 instances, three arms, 2,544 sessions, **$444.26**. Every arm complete, zero
non-ok rows, paired on all 848.

**Primary — `hitRegion@5`, `cc-sg − cc`: +0.0018 [−0.0079, +0.0113]**, 118 wins
against 121 losses, p=0.897, MDE 0.0137. **Enabling semgrep alongside `Grep`
does not improve region coverage.** The interval is tight enough to be a bound
rather than a shrug: the true effect is no larger than about **±0.011**.

Every other quality endpoint agrees. `hitFile@5` +0.007 [−0.003, +0.017],
`ctxEff` +0.001, `nDCG@500` −0.005, `precision` +0.010. `recall@100` is +0.0047
(p=0.032 raw) and dies at Holm 0.158, flagged at its own MDE.

#### What it costs

| | sg − cc | rg − cc | **sg − rg** |
|---|---|---|---|
| cost | +$0.0286 (**+18.1%**) | +$0.0214 (+13.5%) | +$0.0072 (**+4.5%**) |
| turns | +1.225 [+0.947,+1.535] | +0.747 | +0.479 |

Both are overwhelming on the sign test — cost 626/222, turns 460/197, p<0.001.
And the confound arm earns its keep one last time: **most of the price is having
a Bash tool at all, not semgrep.** Of the 18.1%, 13.5 points are `rg`'s too;
semgrep's own increment over ripgrep is **+4.5% and half a turn**.

This also finishes off §25.2's registered prediction. It forecast +5–10% cost
with turns *flat or down*. Cost came in at 18% and turns went **up** by 1.2. The
mechanism — output bytes drive cache creation — survives in direction, but the
prediction about turns was simply wrong, and §27.2 registered it as such before
this number existed.

#### The ladder, which is the methodological result

| rung | n | `hitRegion@5`, sg − cc |
|---|---|---|
| R0 pilot | 31 | **+0.0624** [+0.0196, +0.1051] |
| R1 independent | 119 | +0.0100 [−0.0098, +0.0306] |
| R2 new only | 698 | −0.0023 [−0.0131, +0.0092] |
| **pooled** | **848** | **+0.0018** [−0.0079, +0.0113] |

A monotone decay from a starred, CI-excludes-zero, p=0.022 "finding" to nothing.
Every rung was consistent with the next; only the first was worth publishing,
and it was the only one that was wrong. The pilot's estimate sat exactly at its
own detection limit and `analyze.py` printed *"~at MDE, expect regression to the
mean"* beside it before R1 ran. **That flag was worth more than the number it
annotated**, and it is now the standing reason this project does not report an
effect whose magnitude equals its MDE.

Two pilot sub-findings also evaporated. ctxEff, which at n=31 looked like a
+0.055 *Bash* effect and was written up as one, is +0.0056 at n=848. And the
worrying +0.054 among sessions that never invoked the tool is, at scale,
+0.0058 — noise, as suspected.

#### The dilution argument does not survive either

41% of `cc-sg` sessions invoked `sg` (350/848), so the honest question is
whether the null is diluted by the 59% that could not respond. It is not:

    sg invoked      n=350   -0.0039 [-0.0184, +0.0108]
    sg not invoked  n=498   +0.0058 [-0.0069, +0.0188]

Among the sessions that **actually used the tool**, the point estimate is
*negative*. There is no hidden effect being averaged away. (Post-treatment
conditioning, so descriptive only — but it can only weaken the dilution case,
never strengthen it.)

#### Cross-language: the reason this benchmark was chosen, and it is null too

Every stratum spans zero, including the ones Loc-Bench could never test:
Python +0.001 (n=547), Go +0.000 (84), JavaScript +0.005 (40), TypeScript
−0.006 (38), Rust −0.015 (31), Java +0.018 (30), PHP +0.020 (28), C −0.017 (27),
Ruby +0.027 (22). The hypothesis that semgrep would earn its place outside
Python — the standing gap since §26.4 — is not supported.

**Calibration.** Our `cc` scores HitReg 0.457 against the paper's Sonnet-4.5 row
at 0.428 — inside the band, which is all §27.0 claimed it could be given the
scaffold and date differ. CtxEff 0.931 against 0.715 is far higher, and that
gap is unexplained; it is a reason to treat our CtxEff as non-comparable to
theirs rather than to read it as an improvement.

#### What this is, and what it is not

It is a powered answer to the product question §16–§26 never asked: **adding
semgrep to an agent that already has ripgrep-backed `Grep` buys no measurable
retrieval quality and costs 18% more.** For a tool whose case has always been
"a better primitive inside the loop" (§3.2), that is the strongest disconfirming
evidence this project has produced.

It is *not* a verdict on semgrep as a replacement for grep. Every §16–§26 result
stands: those measured semgrep *instead of* ripgrep with `Grep` removed, which
is a different question with a different answer.

Two limits are structural and were registered before the run. The gold is what
grep-driven agents read, so a region semgrep surfaces that those trajectories
never needed scores as noise — the measurable effect is a **lower bound**. And
`FUH` (0.974) and `nDCG@500` (0.965) sit near ceiling, where nothing this size
could move them.

#### Harness ledger

Four defects, each of which would have produced a clean and wrong number, and
three of whose defining property was **silence**: leaked MCP tools, an allowlist
that did not bind, a prompt that suppressed the treatment, and silent skips
(§27.0). Two more surfaced during the run itself:

- **The LRU never evicted, for an entire rung.** It tracked only what its own
  process fetched, and a `--resume`d instance never requests a checkout — so R1's
  150 trees were an invisible floor. 215 checkouts and 9.0 GB while reporting
  under a 5 GB cap. It said nothing, which is why it survived $81 of spending.
- **The evictor deleted the working directory of live agents.** It protected
  only the instance it was ensuring, not the four running in other threads.
  **432 of 848 `cc-sg` rows died at 2.7 s with 1 turn** — 51% of the treatment
  arm, and non-randomly, since it struck hardest where eviction pressure was
  highest. This one was *not* silent: `triage_swex.py` failed the run and
  refused the analysis. Compounding it, `eval_runner`'s `--resume` keys on
  instance id regardless of status, so those 432 dead cells would have been
  treated as complete had they not been stripped by hand.

The gate also fired once on the tool itself: one `sg` invocation in 484 used a
flag that does not exist (`sg "query" --path lucene-core`). The agent recovered,
ran three good searches and scored 0.6 on that instance. **The gate was
overridden deliberately and it is recorded here rather than quietly passed** —
0.2% of invocations, with no effect on any endpoint. It is still a real finding
about the compat surface: agents reach for `--path`.

## 28 Grep removed: semgrep against ripgrep, head to head (2026-08-09)

§27 answered the *additive* question and got a powered null. But the mechanism
behind that null is a **choice the agent makes**, not a property of the tool,
and three independent measurements now say the same thing about that choice:

| regime | semgrep usage |
|---|---|
| Loc-Bench `both` — rg + semgrep, routing advice in the description | **0.00 calls/session** (rg 3.51) |
| §27 `cc-sg` — semgrep + native `Grep`, tool named in the prompt | 41% of sessions, 1.4 calls |
| §27 pre-fix — semgrep available, prompt named `Grep` instead | **0%** |

With any lexical tool present, agents reach for it. §27 also showed they *add*
semgrep rather than substitute it — `Grep` usage fell only 0.48 of 3.45 while
total searching rose — so the treatment was diluted by construction and the
null was measured at ~41% delivery.

§28 removes the choice. Two arms, `Grep` gone from both, exactly one Bash search
tool each.

### 28.0 Design, and what is already known

| arm | `--tools` | allowlist | status |
|---|---|---|---|
| `cc` | `Read,Glob,Grep` | — | **already run** (§27, 848 rows, $133.97) |
| `sub-rg` | `Read,Glob,Bash` | `Bash(rg *)` | new |
| `sub-sg` | `Read,Glob,Bash` | `Bash(sg *)` | new |

Three contrasts: **`sub-sg − sub-rg`** (primary — the head-to-head with no
native tool to fall back on), **`sub-sg − cc`** (semgrep-only against stock
Claude Code, the product question), and **`sub-rg − cc`** (does removing native
`Grep` cost anything by itself — the control that keeps the other two
interpretable, exactly as `cc-rg` did in §27).

`RG_LINE`/`SG_LINE` are reused verbatim so the tool descriptions do not become a
second variable. The prompt clause drops `Grep` for the new arms, because naming
a tool the arm does not have would send the agent at something it cannot call.
Removal is enforced in **two** places and needs both: `Grep` out of `--tools`
takes away the native tool, and the PATH shims block shell
`grep`/`egrep`/`fgrep`. `--allowedTools` enforces nothing under
`--permission-mode dontAsk`, which §27.0 learned the hard way.

**The substitutive regime is not new; only this benchmark is.** Loc-Bench ran it
at scale and it was parity:

| contrast | n | delivery | file_acc@5 | func_acc@10_tol |
|---|---|---|---|---|
| desc-v5 − rg | 560 | 80% | +0.0018 [−0.0179, +0.0214] | +0.0018 [−0.0196, +0.0232] |
| desc-v9 − rg | 204 | 91% | −0.0196 [−0.0539, +0.0147] | −0.0392 [−0.0833, +0.0049] |

MDEs 0.027 and 0.030 on the first row. And §27's held-Bash contrast
(`cc-sg − cc-rg`, `Grep` present) was −0.003 [−0.013, +0.006]. **The registered
expectation is therefore parity**, |Δ| < 0.012 — recorded here before the run so
that a null is a prediction rather than a rationalisation.

What §28 genuinely adds over that prior: multi-language line-level gold instead
of Python function names, delivery near 100% instead of ~45%, and the
`sub-sg − cc` contrast nobody has measured on any benchmark.

**Harness changes, and the two that would have failed silently.** Adding arms
touched five places; two of them were latent bugs rather than new work.
`campaign.sh`'s `count_ok()` globbed every arm file under the run id, so a
two-arm rung under `s27` would have started at 2,544 ok rows against a target of
1,696, printed "rung complete" and exited **having run nothing** — a no-op that
reports success. And `triage_swex.py` gates against the *registered* arm set, so
five arms in one results directory would have failed both the "unexpected arm
labels" and "registered arms absent" checks. Both are now scoped by an explicit
`--arms`, and `analyze.py` gained `--arms`/`--contrasts` — its arm intersection
previously ignored unknown arms in silence, so it would have cheerfully
re-reported §27 while §28's rows sat unread beside it. Verified by byte-comparing
every number the parameterised analyser produces against the §27 defaults.

### 28.1 Pre-registration for the powered run, written after the R1 gate

**R1 (n=120, both arms, $53.08) passed its gate**, and its one registered
diagnostic is the premise of the whole section:

| arm | sessions using its tool | calls/session |
|---|---|---|
| `sub-rg` | **113/120 (94%)** | 5.2 |
| `sub-sg` | **112/120 (93%)** | 3.4 |

Against §27's 47% and 41%. Removing the choice more than doubled delivery, so
§28 measures the tools rather than the agent's preference between them. **No
endpoint has been looked at**; R1's job was harness health and delivery, and
that is all that has been computed.

**Primary**: `hitRegion@5`, `sub-sg − sub-rg`, paired `boot_ci` (4,000
resamples, seed 1). MDE 0.012 at n=848 on §27's measured paired sd 0.126.

**Co-primary — cost and turns.** §27 put semgrep's own increment over ripgrep
at +4.5% and +0.48 turns *with* `Grep` present. R1's per-session mean is
**$0.221** against §27's $0.158–0.187, so the registered prediction is that
**removing native `Grep` is itself expensive** and that `sub-rg − cc` will carry
most of it — the smoke measured `sub-rg` at +42% over `cc-rg` on identical
instances at equal turn count. The mechanism to test is §27's: raw `rg` through
Bash averages 25 KB a call and floods, while the native tool bounds its output.

**Secondary, Holm-corrected**: `hitFile@5`, `ctxEff`, `nDCG@500`, `recall@100`,
`precision`. `nDCG@500` and `FUH` are near ceiling — print the bound.

**Registered expectation: parity on quality.** Loc-Bench's substitutive
comparison was +0.002 [−0.018, +0.021] at n=560, and §27's held-Bash contrast
was −0.003 [−0.013, +0.006]. The prediction is |Δ| < 0.012, and the interesting
result is expected to be **cost, not accuracy**.

**Registered response to a null**: report as a bound with the conservative bias
attached, and do not re-cut for a stratum that moved.

**A gate gap fixed before R2, not overridden.** R1 failed once, on
`php-cs-fixer-8064`: the agent made a single search against a path absent at
that base commit, semgrep exited 2 with "no such file or directory", and the
distress gate counted "every search empty". `classify_usage` already labels that
case *bad path (tool correct)*, but the all-empty check never consulted it.
triage.py's own principle is that "a gate that punishes the tool for being right
is a gate nobody can pass", so the filter now applies to the distress check as
well — for distress only, since classifying those rows is `check_tool`'s job.
Fixing it rather than overriding it matters because R2 is seven times larger and
a gate overridden every run is not a gate.

### 28.2 R2 interrupted by the credit ceiling, and a mechanism read on the 456 clean pairs (2026-08-10)

**What happened to R2.** The 848-rung launched with both arms and ran
`sub-rg` essentially to completion — 822 rows, 820 ok, the other 26 instances
still stuck on cold-cache download failures — and then hit the API's five-hour
credit ceiling partway through `sub-sg`: 848 rows on disk, **484 ok and 364
`agent_error`**, every failed row a rate-limit rejection ("out of credits"),
median duration 0 s, median cost $0. The gate GATED OFF on exactly this
(366 non-ok rows, 392 partial instances) — which is the gate doing its job,
not a harness defect. The dead cells cost nothing and are resumable;
**the registered pooled-848 analysis has not been run** and still gets
computed once, on the full frame, after recovery.

**A look at the primary on partial data, declared.** What follows was run at
the operator's request to understand *mechanism*, on the 456 instances where
both arms have clean rows. It saw the partial-data primary:
`sub-sg − sub-rg = −0.0073` (sd 0.134, w/l 52/73, **331 exact ties**),
consistent with the registered parity expectation (|Δ| < 0.012). §28.1 has no
stopping rule on endpoints — gates are harness-health only — so this look
changes no decision, but it is a look and it is recorded as one. The 456 are
approximately a ladder prefix (passes run in frame order), which the
interleave-by-repo construction keeps roughly balanced; they are still not a
random subsample, and nothing below is a registered result.
Reproduce with `eval/swexplore/mechanism.py`.

**Discovery is at ceiling; the entire contest is line-range margins.** On
454/456 pairs *both* arms land at least one gold region. File-level discovery
discordance is symmetric — sg's agent missed a gold file rg's had on 43
instances and found one rg missed on 38, worth −9.50 and +9.07 rate-points
respectively, a wash. SWE-Explore issues carry identifier anchors (error
strings, function names) that exact match resolves as well as ranking does,
so the vocabulary-mismatch case semantic search exists for almost never binds
here. What remains is *which lines* get submitted, and that is where the whole
net −3.33 lives.

**The bucket accounting.** Every lost region attributed to a cause from the
session's own shim log and captured output; an instance's lost score is
distributed proportionally, so buckets sum to the gap. Both directions,
because a bucket is only a tool finding if the other tool does not lose the
same way:

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

Three findings, in order of what they are worth:

1. **Line precision is the sg-specific deficit — 27% of sg's losses, 2.3×
   rg's rate.** `hit_region_rate` scores exact overlap; `rg` prints
   `path:line:text` and agents copy the line into their range, while sg prints
   a ~32-line window the agent anchors to. The pure chunk-edge case is only a
   third of the bucket (jq-2650: sg walked the agent to `parser.c:3443`, gold
   at 3456, one window short; fluentd-3917: sg's agent submitted
   `yaml_parser.rb 1–40` against gold 47–51 while rg's agent, shown the match
   line, submitted 24–53). The larger share is >32 lines off — a plausible
   chunk in the wrong part of the right file, accepted as the answer.

2. **sg's always-answer behavior converts to noise submissions at 2× rg's
   rate.** 99% of sg's 1,437 calls exited 0 with content; 17% of rg's 1,854
   exited 1 with nothing, and the agent reformulated on the spot. A weak match
   that fills the screen reads as an answer: 42 submitted regions in
   non-gold files that sg itself had displayed, against rg's 18.

3. **Single-file scoping is real but it is an agent behavior, not an sg
   defect — rg loses more to it than sg does.** "Gold scoped away" is 17.6%
   of sg's gap and **37.8% of rg's**, the largest rg bucket; scoping rates are
   identical in sg's winning and losing sessions (67% vs 64% file-scoped).
   Agents scope both tools to guessed paths and lose when the guess is wrong —
   and sg's repo-wide ranked search is precisely the surface that wins those
   points back. Query styles differ as expected: sg gets 4.5-word phrases,
   70% file-scoped; rg gets 1.9-word patterns, 90% path-scoped, alternation
   (`a|b|c`, often across several files in one call) on half of all calls.

Cost on the same 456: `sub-sg` $0.240/session vs `sub-rg` $0.192 (+25%),
+0.4 turns — consistent with §28.1's registered prediction that the
interesting result is cost, not accuracy.

**What this buys the tool, ranked:** (1) surface the best-matching *line*
inside each chunk, not just the window — the deficit is anchoring, and the
`--decl-boost` machinery already re-reads candidate chunks cheaply; (2) make a
weak match look weak — some "no strong match" signal where rg's exit 1 now
does the agent's reformulation prompting; (3) leave repo-wide ranked search
alone — it is the bucket where sg is already winning. Caveats: "appeared in
output" is a substring match on captured stdout (common basenames can
overcount), attribution within an instance is proportional rather than causal,
and all of it is descriptive, on 54% of the frame, outside the registration.

## 29 Acting on §28.2: fine answers, a floor, wide-by-default, and function chunking again (2026-08-10)

§28.2's bucket accounting turned into four engine changes, built in one arc.
Everything here is *mechanism landed*; the measurements that would flip the
remaining defaults are §29.4's and have not run yet.

### 29.1 The fine rerank (shipped, default on)

Line precision was sg's one clearly tool-specific deficit — 27.3% of its §28
losses, 2.3× ripgrep's rate, because agents anchor submitted ranges to the
span the tool prints and a 32-line chunk window ends lines away from the
target. `finalize` now scores every 4-line window of each candidate chunk by
cosine against the query (raw text both sides, i8-quantized both sides — a
pure function of query string and file bytes, so cold==warm holds with no
index state threaded in), and the best window becomes the hit's span, its
passage, and its score. Windows re-rank the candidate pool (`--fine-blend`,
1.0 = pure fine); same-file windows electing the same lines collapse;
`--no-fine` reproduces the old output byte for byte and is the control arm.
Costs ~0.5 ms, timed as `finalize:fine`.

Two consequences worth naming. Scores stopped being decorative: the maxsim
head normalization made every rank-1 fused score exactly 2.0, and the fine
cosine is the first cross-query-comparable number the pipeline emits — which
is what makes §29.2 possible at all. And at blend 1.0 the fine order *owns*
the list, which makes the §24 declaration boost invisible inside the pool
(it still gates who reaches the k×3 candidates). The decl-boost parity test
now pins fine off for exactly that reason; whether blend 1.0 is the right
default against 0.7-ish is a §29.4 question, registered before looking.

### 29.2 The score floor (mechanism shipped, default off)

sg answered with content on 99% of 1,437 real §28 calls; agents submitted
non-gold files sg itself had displayed at 2× rg's rate, while rg's loud
empty misses (17% of calls) are what prompted rephrasing. `--min-score` is
that missing "colder, try again": set-level (the floor asks whether the
scope contains the concept at all — a weak tail behind a strong head is
normal ranked output), judged in the shared finalize tail, zero hits + exit
1 + a footer line naming the refused score. Signal = best fine cosine
(`--no-fine`: best chunk cosine via the MMR vectors).

Default 0 = off, deliberately: a floor that cries wolf teaches agents to
ignore it. `best_signal` is reported in the envelope on success too, so
calibration joins score→outcome from existing artifacts: replay
`eval/queries/guesses-*.jsonl` through guessplay plus the 1,437 captured s27
sub-sg invocations, take the largest floor with ≤2% false-floor rate on
gold-hitting queries, ship that number with its measured true-negative rate.

### 29.3 desc-v10, and function chunking rebuilt (opt-in)

**desc-v10** models the pathless call as *the* way to search ("start wide;
add a path only to narrow further") and fixes the stale top-10. Grounds:
agents file-scoped ~70% of sg calls, "gold scoped away" was 17.6% of sg's
§28 losses and 37.8% of rg's, and no prior description ever said when a path
belongs. The §19.2b example and tripwires carry unchanged. The SWE-Explore
arms keep the *registered* SG_LINE — the v10 text sits beside it un-wired
(`SG_LINE_V10`) until a campaign registers arms on it, because 364
rate-limited sub-sg cells still owe completion under the old treatment.

**Function chunking returns** (`--chunking function`, cap `--chunk-cap` 96),
five weeks after §11.4 removed it — because §11.5's verdict was that the
*instrument* couldn't resolve the effect, and SWE-Explore's line-level gold
plus guessplay now can. The §11 design is kept where it was measured and
simplified where it wasn't: one `leaf_defs` table per language (9 grammars,
PHP added; everything else recurses, which makes containers, export
wrappers, and decorated definitions fall out for free — decorators reattach
via Rule B's `@` prefix); definitions ≤ cap emit whole, never recursed, so
closures stay in context; §11.2's Rule B verbatim (prefix table, ≤20 lines,
≤1 blank — the 0%-wrong-code rule); a 5-line min-merge for packed accessors
(§11.1's +76% chunk-count case); gaps and over-cap interiors fall to
non-overlapping window cuts, so function mode is fully disjoint — the
§11.3 postings shrink, kept. Parse failure or any ERROR node falls back to
line windows; no parser timeout ever (a timeout makes the cut a function of
machine load, which breaks cold==warm). Cache entries tag as
`f{cap}w{w}o{o}`; a grammarless build (`--no-default-features`) names them
but never parses them back, reclaiming instead of mis-serving — the `c`-tag
degradation, one mode later. `Chunk` stayed three u32s, so no format bump.
Binary cost measured: 39.0 → **46.5 MiB** (+7.5 for 9 grammars; §11.3 paid
+6.6 for 8). On the frozen test corpus, function mode cuts 104 chunks where
window mode cuts 39, and a warm query stays ~4 ms.

### 29.4 What is registered to happen next, before any default flips

In order, all offline and cheap: (1) guessplay A/B — fine vs `--no-fine`,
function vs window, on the harvested real-query sets; (2) floor calibration
as specified in §29.2; (3) `--fine-blend` sweep only if (1) shows the pure
fine order losing what the §24 boost bought. Function chunking's default
flip additionally requires re-measuring §11.3's cold-index cost on django
and a snapshot re-record reviewed case by case. A SWE-Explore rung with the
new binary comes only after those gates, and its arms register the v10
description at the same time. Nulls are reported as bounds; no default
flips on a stratum cut.

### 29.5 The offline gates, run (2026-08-10)

**Guessplay A/B, 854 real harvested agent queries, 186 instances, one pass,
2×2 (fine on/off × window/function chunking).** Paired `boot_ci`, same
convention as everywhere else:

| contrast | file hit@5 | func hit@5 strict | func hit@5 overlap |
|---|---|---|---|
| fine − no-fine (window) | −0.007 [−0.025, +0.011] | −0.009 [−0.027, +0.009] | **−0.082 [−0.104, −0.059]** |
| function − window (no fine) | +0.000 [−0.013, +0.013] | +0.009 [−0.008, +0.027] | **−0.028 [−0.047, −0.008]** |
| function+fine − baseline | −0.015 [−0.033, +0.002] | −0.019 [−0.040, +0.002] | **−0.096 [−0.119, −0.071]** |

**Read the overlap column as geometry, not quality — §24.1 said so in
advance.** `rank_func_ovl` credits a chunk that *overlaps* the gold function
at all; `rank_func` requires the chunk's best line to fall inside it. A
32-line window overlaps a 12-line gold function by accident constantly, and a
4-line window cannot. So a lever that shrinks spans must drive those two
metrics apart, and §24.1 registered exactly that as the signature of changed
geometry rather than changed retrieval: strict flat, overlap down. That is
what both levers show. Reporting the overlap drop as a loss would be scoring
the fine rerank for no longer getting accidental credit.

On the endpoints that survive the geometry change, both levers are **nulls**:
every strict and file CI spans zero. Do-no-harm holds, which is what the gate
asked. It does not show a gain either, and the combined arm leans negative
(w/l 34/50 strict) — registered as the trigger for a `--fine-blend` sweep
before the blend default is defended, not before shipping the mechanism.

**Floor calibration, 853 replayed queries** (`eval/locbench/floorcal.py`):

| | |
|---|---|
| gold-hitting top-1 score | p5 0.486, p25 0.645, median 0.725 |
| gold-missing top-1 score | p50 0.684, p75 0.785, p95 0.888 |
| **floor 0.420** | refuses **1.9%** of gold-hitting, converts **9.3%** of gold-missing to an honest "no matches" |

Identical threshold at the n=451 half-sample, which is the stability check
worth having. The distributions overlap heavily — a wrong-but-plausible
neighbourhood embeds near the query, so a miss's median (0.684) sits just
under a hit's (0.725) — and the floor only separates in the low tail. That
bounds the claim: this is a small honest-refusal win, not a discriminator.

**Two defects the gates found, both fixed before the campaign.** The fine
rerank made the display anchor worse in a way no offline metric scores: the
hit's `text` is the best-overlap line *within the span*, and where a 32-line
chunk almost always held some line sharing a query token, a 4-line window
often holds none — so the first-wins fallback anchored **8.3% of snapshot
hits on a bare `{` or `)`**. Ranking the anchor by `(overlap, carries a word)`
takes it to 0.0%. And the floor was **inaudible under `SEMGREP_NO_HINTS`**,
which every agent harness sets: its explanation sat below that early return,
so a floored search gave empty stdout, empty stderr, exit 1 — the §16.11
shape, and the opposite of the "colder, try again" signal the floor exists to
send.

## 30 The powered campaign on the new engine: sub-sg against sub-rg (2026-08-10)

§29 shipped four changes and §29.5 gated them offline. None has met a real
agent. This section does that: all four on at once, against ripgrep, on
SWE-Explore's line-level gold.

### 30.0 Design

Arms are §28's substitutive pair — `Grep` removed from both, one Bash search
tool each — because that is the 93% delivery regime, roughly 2.3× the power
of the additive pair whose 41% delivery diluted §27.

| arm | treatment |
|---|---|
| `sub-rg` | **unchanged control.** ripgrep never touches our engine, so none of the four changes reach it. |
| `sub-sg` | fine rerank + floor 0.42 + desc-v10 + function chunking |

Baseline for the same contrast on the old engine, from §28.2's partial run:
`sub-sg − sub-rg = −0.0073` (n=456).

**Bundled deliberately, attributing nothing individually.** Four levers move
together, so a moved endpoint says "the package moved it" and no more. §28.1
set the precedent for accepting that when the alternative is four campaigns.
The offline arm-level attribution in §29.5 is what stands in for it.

Delivery mechanics, and the trap avoided: the engine flags reach the binary
through `LOCBENCH_SG_FLAGS`, injected by `shim.py` into the *real* invocation
and never shown to the agent, so its commands and the logged argv stay the
plain `sg "query"` an agent types. The chunking half must also reach the
**index build**, because a repo-local `.semgrep/` is exempt from cache-tag
matching by design — a window-chunked index answers a function-chunked search
with no error anywhere, treating file scopes and leaving every directory
scope untreated. `_index_matches` reads the built `meta.json` back and raises
rather than running a half-dosed arm.

### 30.1 Pre-registration, written before R2 is funded

- **Primary**: `hitRegion@5`, `sub-sg − sub-rg`, paired `boot_ci` (4,000
  resamples, seed 1). MDE 0.012 at n=848 on §27's measured paired sd 0.126.
- **Co-primary — cost and turns.** The registered prediction *reverses*
  §28's +25%: the floor abandons dead ends the agent used to pursue, and a
  4-line passage is a fraction of a 32-line one, so **sub-sg cost ≤ sub-rg**.
  This is the endpoint the section expects to move.
- **Secondary, Holm**: `hitFile@5`, `ctxEff`, `nDCG@500`, `recall@100`,
  `precision`.
- **Delivery is the headline diagnostic, not a gate**: per-arm invocation
  rate, plus the floored-search rate from the trace envelopes. Below ~90% sg
  delivery the accuracy endpoints describe a different agent rather than a
  different engine, and must be reported as diluted.
- **Query-shape gate before any accuracy claim** (§19's rule):
  `queryshape.py --since` must show desc-v10 actually moved the path-scoped
  share down from §28's ~70%. A description that changed no behaviour cannot
  be evidence about behaviour.
- **Registered expectation**: accuracy parity, |Δ| < 0.02 — §29.5's offline
  nulls predict it and the fine rerank's own mechanism (better spans, same
  files) does not obviously move a *region* metric. A null is reported as a
  bound with the delivery rate attached.
- **Registered response to a cost win with an accuracy null**: report it as
  the result. Cheaper at equal accuracy is the §26.3 endpoint this project
  already decided is the one the tool is for.
- Gates between rungs are harness health only (`triage_swex.py`), never an
  endpoint, so there is no sequential-testing alpha to spend.

### 30.2 R1 as a pilot: four defects, and a description that moved behaviour (2026-08-11)

**The powered contrast was not funded, and no accuracy endpoint is reported
here.** §30.1 binds the primary to a single computation on the pooled 848,
and R1's 240 sessions are neither that nor comparable to it — the four fixes
below changed the binary, two of them in ways that change what the agent
*does*, so these rows cannot pool with a later run. R1 stands as what the
ladder's first rung is for: $46.93 that bought four defects and two
behavioural readings.

**The gate fired on four checks and all four were real.**

1. **`--path` is a shape agents type.** §28.1 caught it once in 484 searches
   and filed it as a compat note; R1 caught it four times in 511. The rise is
   **desc-v10's own doing** — telling an agent to "add a path argument"
   invites a named flag at an interface that takes a positional. Now accepted
   as an alias. A description change produced a CLI requirement, which is not
   a direction this project had seen before.
2. **The floor's own message taught an agent to fumble a flag.** It ended "or
   pass `--min-score 0` to see weak results". One agent read that, typed
   `--min-score` with no value, exited 2, and spiralled into three
   consecutive empty searches — tripping the distress gate. This is §16.10
   exactly (naming `-e` in a footer moved ranked share 7% → 98%): **a footer
   is a treatment, and an agent acts on any flag it names.** The message now
   induces only the behaviour the floor exists for — rephrase, or widen — and
   a test asserts it names no flag.
3. **The registered diagnostic was unreadable.** §30.1 named the floored rate
   a headline diagnostic; `floored`/`best_signal` never reached the trace
   envelope, and a refusal and an empty scope both report `n_hits=0`, exit 1.
   Recovering the rate meant grepping stderr out of the search dumps, which is
   how it was recovered here: **24 of 32 empty sg searches were floored
   refusals carrying their explanation** — the floor working as configured,
   invisible to the instrument that was supposed to see it.
4. **The gate counted those refusals as failures.** "Ranked searches returning
   nothing" hit 4.8% against a 2% limit, and three quarters of it was the
   floor doing its job. Same shape as the "bad path (tool correct)" case
   §28.1 had to teach the distress check about: a gate that punishes the tool
   for being right is a gate nobody can pass. Now excluded and reported
   separately.

**Two behavioural readings, both descriptive at n=120.**

| | delivery | path-scoped calls | $/session | turns |
|---|---|---|---|---|
| `sub-rg` (unchanged control) | 93% | 89% | $0.200 | 9.2 |
| `sub-sg` | 91% | **50%** | $0.194 | 9.6 |
| §28 baseline | 90% | 70% | $0.240 | — |

**desc-v10 moved query shape and the control did not.** sg's path-scoped
share fell 70% → 50% while rg — whose description is untouched — sat at 89%
against §28's 90%. That is §19's registered gate passing in the only form it
can pass: the treated arm moved, the untreated one did not. It is also the
first description change in this project measured to move *scoping* rather
than length or phrasing.

**Cost parity, which §28 did not have.** sg ran marginally cheaper than rg
($0.194 vs $0.200) where §28 measured +25%. Consistent with §30.1's
registered co-primary prediction and with the mechanism — a 4-line passage is
a fraction of a 32-line one — but it is 120 instances of a descriptive
reading, not the registered endpoint, and the +0.4 turn difference runs the
other way.

**What a future powered run needs**: a fresh run id (R1's rows are not
poolable), a clean R1-sized gate rung on the fixed binary first, and the
§30.1 registration unchanged. Nothing in this section licenses skipping that
rung — the last one gated off, and it was right to.
