# Changelog

Findings and performance improvements, newest first. Measured numbers are
medians on an M-series Mac; "kernel" = Linux 6.9 source (1.15 GB, 1.51M
chunks). Full data: `docs/RESEARCH.md` and `eval/REPORT.md`; the query sets
are checked in under `eval/queries/`, while `eval/data/` is gitignored scratch
and will not be in a fresh clone.

## 2026-08-16 — colour on a terminal, and a gutter that is two spaces

Display only; ranking is untouched (`tools/snapshot.sh --check` is
byte-identical, and it records `--json` besides).

- **Colour**: `--color auto|always|never` (default `auto`), ripgrep's flag and
  ripgrep's three values. `auto` means stdout is a terminal, `NO_COLOR` is
  unset, and `TERM` is not `dumb` — under a pipe, a harness, or `--json` the
  bytes are exactly what they were, and stripping the escapes from a coloured
  run gives the piped run byte for byte (asserted, not assumed).
- **Three hues and a neutral, not ripgrep's two.** Blue names the file, cyan
  counts the lines and goes bold on the one the engine chose, green is your
  own words coming back, and grey is everything the view added around the
  answer. Sixteen-colour codes only (`94`, `2;36`, `1;36`, `90`, `1;92`), so
  the terminal's own theme still decides what "blue" looks like. Furniture is
  grey `90` rather than SGR `2`: dim is the one attribute terminals routinely
  drop, and a "recedes" state that can render identical to "matters" is not a
  state.
- **Emphasis, for a tool with no exact match.** ripgrep bolds the substring
  its regex matched; gorp has no such substring, so weight carries the
  engine's reasoning instead. **Bold is what it chose** — the best line of the
  span (`SearchHit::line`), the one thing gorp knows that grep cannot.
  **Grey is what it added** — de-orphaning rows and `-C` context, everything
  outside the scored window. **Green is where your words landed**, matched
  through the engine's own tokenizer, so `getUserName` lights up for "user
  name" and a purely semantic hit lights up nowhere — which is a true
  statement about that hit, not a failure. Exact mode emphasises the literal
  and never its subtokens (`-e compute_backoff_delay` leaves a nearby `delay`
  alone); a real regex gets no emphasis until the matcher's spans are plumbed
  through. Path emphasis included, because BM25 counts path tokens as
  evidence and a file that ranked on its name should say so.
- **The no-path gutter is two spaces, not a tab.** `264:  text` where it used
  to be `264:<TAB>text`. A tab renders at whatever tab stop the consumer
  happens to use, and a tab inside the row's own text re-aligned everything
  after it. **Consumers that recognise a path-less hit row by `^\d+:\t` must
  be updated** — in-tree ones were; gorp-bench's `scoring.py`
  (`_HIT_LINE_NOPATH`) is the one outside this repo.

## 2026-08-16 — caching is opt-in

A plain ranked search no longer writes anything to disk: a cold miss streams
and leaves nothing behind, where it used to write a `~/.cache/gorp` entry
through on the first ranked search of every scope. gorp runs in many
short-lived environments, and each one kept a cache it would never read
again. Ranking is untouched (`tools/snapshot.sh --check` is byte-identical;
cold == warm still holds).

- **Opting in**: `gorp index [DIR]` prewarms a scope once; `GORP_AUTO_INDEX=1`
  restores the old first-search write-through for a whole environment; the
  hidden `--index` flag does it for one invocation (hidden for the same
  reason the verb is — an agent that sees it opts every scope back in).
- **Reading is unchanged**: an existing entry — local `.gorp/`, ancestor, or
  cache — still answers warm, and its repair/rebuild upkeep still runs.
  Opting in gates *creating* state, never reading it.
- The harnesses that exist to exercise warm behavior opt themselves in:
  `run_eval.py` passes `--index`, the sim harness and `replay_traces.py` set
  `GORP_AUTO_INDEX=1`, and so does `tools/snapshot.sh`. Engine option:
  `SearchOptions::write_through`, recorded in the trace envelope.

## 2026-08-16 — the flag surface says what it does, and does what it says

Four changes, no default behavior moved (`tools/snapshot.sh --check` is
byte-identical):

- **Defect fixed: `-e` plus any filter truncated to the ranked k.** Multiple
  paths, `-g`, or `--lines` cut exact-mode output to 5 matches and restated
  the footer totals to match, so the loss was self-consistent and invisible.
  Truncation now keys off *mode*, not filter presence: ranked mode keeps its
  top-k contract (the over-fetch makes that mandatory), exact mode
  enumerates. The "results narrowed to the paths given" notice is ranked-only
  now — in exact mode, filtering to the paths given is grep's ordinary
  contract.
- **`-C/-A/-B` work in ranked mode.** They were advertised generally and
  silently did nothing outside `-e`. Context joins the unit view as ordinary
  rows — the header span widens with them, so first/last row == header span
  still holds and no second row grammar appears — and extends `--no-unit`
  passages in their own shape. Display-side only (`out.rs`), opt-in only, and
  never rides `--json` (schema frozen).
- **`-w` and `-c`, exact mode.** `-w`/`--word-regexp` from the engine's own
  word-boundary builder, so `-wF` word-matches the literal (ripgrep
  semantics). `-c`/`--count` prints `path:count` per file — counts collected
  in the scan, so they survive the 250-line retention bound — with grep's
  precedence (`-c` beats `-l`) and single-file rule (bare count, `-H`
  restores the path). Zero-count files are not listed. In ranked mode both
  are accepted by construction: `-w` changes nothing (ranked matching is
  subtoken-based) and `-c` counts the k ranked hits in rank order.
- **The visible surface now matches the README table.** `--passage-chars`
  (the one `Tuning` member accidentally left visible), `--stats-json`
  (`GORP_TRACE_FILE` is the capable surface), and `--check-stale` are hidden
  but fully functional; every eval-arm knob keeps parsing so RESEARCH.md's
  arms reproduce. The `-k` help text stops claiming a default of 10 (it is
  5), and DESIGN.md's CLI sketch drops flags that never shipped
  (`--reindex`, `-e` as `--regex`).

## 2026-08-15 — ranked hits print as a unit view

Ranked output stops repeating `path:line:` on every passage line and prints a
*unit view* per hit (RESEARCH.md §34): the path once as a `path:start-end`
header, `line:`-numbered rows dedented as a block, the matched window snapped
off bare closers/openers and framed by its enclosing declaration, `⋮` where
rows were elided, hits separated by a blank line. Calibrated by a noise audit
of 30 real searches across ten languages: ~1.5 added rows per hit, ~1.15× the
bytes of the old form (whose `path:line:` prefixes were ~half of all output
bytes, §26.4).

- **Exact mode is untouched** — `-e` stays grep's `path:line:text`, and so do
  exit codes.
- `--no-unit` restores the previous ranked display byte for byte (the A/B
  control); explicit passage shapes (`--passage-chars`, `--passage-lines`,
  `--full`) and `--no-fine` still win over the unit view, so every measured
  display arm reproduces.
- `--json` gains an optional `unit_rows` field on ranked hits; absent under
  `--no-unit` and in exact mode, so existing consumers see the schema they
  always saw.
- Standing follow-up (§34.3): a disp-unit/disp-nounit campaign on
  reads-after-search and line-precision — the §28.2 deficit this format could
  plausibly touch, since body rows no longer carry a copyable `path:line:`.

## 2026-08-03 — the tool now accepts what agents actually type

semgrep promised a "grep-shaped contract" and delivered it on output only. On
input it rejected grep. Measured against the §16.10 campaign's ripgrep arm
(2,074 real agent invocations): **`-n` appeared 1,829 times — 88% of every flag
typed — and exited 2 every time.** So did `-A`/`-B` (142), `-g`/`--glob`/
`--include`/`--type` (273), `-l` (118), `-r`/`-R` (48), and the **31% of calls
that passed more than one path**. Replaying the 69 argv vectors that were usage
errors in that campaign: **69 → 10, 86% recovered**, the remainder being
genuinely malformed input.

- `-n`, `-r`, `-R`, `-H` are accepted. Not stubs — semgrep's `path:line:text`
  output *is* grep's `-rn` form, so each flag's postcondition already holds
  unconditionally. Honoring their *absence* is what semgrep declines to do:
  no `-n` would mean dropping line numbers, no `-r` refusing a directory. They
  stay out of `--help` (they change nothing); one `after_help` line says grep
  flags work. Combined shorts (`-rn`, `-rln`) follow for free.
- **Multiple paths.** The search runs over their common ancestor and results
  are filtered back to the paths given, over-fetching so the top-k usually
  still fills. Filtering applies to *hits*, never the corpus walk — a walk
  filter would poison a cache entry every later search of that scope reuses.
- `-A`/`-B` asymmetric context, `-l` for paths only, `-g`/`--include` globs.
- **`index` is out of `--help`.** The index is a cache that builds itself, and
  the README has always said so — but agents read `--help` only when stuck (27
  of 27 probes followed three or more empty searches), found a verb offering to
  "build the index", and concluded search needs a build step. One did exactly
  that mid-campaign. Hidden, not removed; the harnesses still call it.
- **An empty result now says why.** A ranked search over a readable scope
  essentially cannot return zero, so zero means the corpus was empty — and
  "rephrase the query" sent callers to fix the one thing that was not wrong.
  `SearchReport` gained `files_walked`, and the footer now separates "nothing
  to search under this path" from **"found N files here but could read none of
  them"** — the exact signature of the §16.11 bug, which reported an ordinary
  miss for its entire life. This is the guard that closes the *class*.
- An invalid `-e` pattern now points at `-F`. Typing a call, `-e 'foo('`, is
  the common way to reach a regex parse error, and the error alone is a wall
  of regex internals with no way out.
- Fixed a fifth instance of the §16.11 join bug, in `out::context` — the CLI
  crate, which the earlier sweep of `semgrep-core` did not reach. `-C` on a
  single-file scope printed no context.

`preflight.py` now asserts this whole matrix, with a control that an unknown
flag still fails, so it is a tripwire rather than a one-time audit. Snapshot
unchanged (114 cases).

## 2026-08-03 — where retrieval actually fails, once the bug is subtracted

The §16.11 fix left the obvious question: with the tool working, where is
semantic search weakest? The §16.10 trajectories cannot say — 96% of their
2,078 empty searches were the file-scope bug, and 18% of instances never got
a single non-empty result. `guessplay.jsonl` can, because there the bug is
*separable*: file-scoped ranked rows found gold **0 out of 5,117 times** while
directory-scoped rows found it 55.8% of the time, so dropping the former
leaves a clean 4,537-row frame. Full analysis: RESEARCH.md §17.

On that frame, three answers, two of which invert something that looked
settled:

- **Semantic has no distinctive weakness against bm25.** Paired on 1,300
  rows: bm25-only 8.0%, semantic-only 5.5%, and the two discordant sets have
  identical query profiles (median 1 word, ~50% single-word, ~45% carrying an
  identifier). They trade wins on queries that look the same. The 50.4% they
  *both* miss is the real target.
- **27% of misses are structurally unanswerable** — the agent searched a tree
  that does not contain the gold. Of the true ranking failures that remain,
  69% share no vocabulary with the gold at all, which is §15's blind wall on
  real queries and a model problem, not a ranking-parameter one.
- **Searching the repo root instead would make things worse**: −0.013,
  CI [−0.022, −0.003], rescuing 206 rows and costing 263. The agent's scope
  choice carries information. A scope fix has to be selective.

Also rechecked: §16.5's split-sif null was computed on that half-zeroed
sample. It survives correction and tightens to +0.002, CI [−0.006, +0.009],
with semantic at exactly 90 wins to 90 losses — which is what makes the
§14.5 verdict below a decision rather than a guess.

## 2026-08-03 — the tool stops advertising `-e`; split-sif does not graduate

Two decisions that had been sitting unresolved since the §16.10 campaign.

**Shipped: the desc-v5 posture.** The ranked footer used to end every search
with `-e '<query>' checks for the exact string`. That clause was the strongest
behavior lever measured in this project — suppressing it moved an agent's
ranked share from 7% to 98% at no cost in accuracy (RESEARCH.md §16.10), a
larger dose than any tool description. It is gone from both ranked footers,
and `about`/`after_help` now carry the measured desc-v5 text. Exact mode is
unchanged and still documented on its own flag; it just isn't pitched after
every ranked search. The keyword-mode footers still name `-e`, but only to
steer off it, which is the same posture. `eval/locbench/preflight.py` gained a
tripwire so the clause cannot come back unnoticed.

**Not shipped: `--embed-preproc split --sif` as the default build.** §14.5
gated it on a query replay, and running that gate down produced three
findings. The gate was not executable — `replay.py` shares one index across
conditions and structurally cannot compare index *builds*. The instrument it
meant (`guessplay.py`) had already answered: champion − default −0.008, n.s.
(§16.5), with §15.9-B's mechanism — SIF drops a gold chunk's cosine 0.325 →
0.111 on token-poor identifier queries, which is the agent regime. And the
gate's step 2 rested on a false premise: the cache generation does *not*
retire entries on an embed-config change, so the flip would have left existing
entries serving the old space invisibly. The config stays opt-in; §14.5 now
records the verdict.

Three latent defects that investigation exposed, all fixed: `--embed-preproc`
was missing from `replay.py`'s `INDEX_FLAGS`, so a `split` condition would
pass the guard and then do nothing (the warm path follows the index's own
meta) while reporting parity; `ensure_index` compared only `sif` when deciding
to reuse an index, silently serving a `none` build to a `split` condition; and
the index telemetry envelope recorded `sif` but not `embed_preproc`, so a
trace could not tell which embedding space a build was in.

## 2026-08-03 — fix: ranked search over a single file returned nothing, always

`semgrep "query" <file>` was empty 100% of the time, for as long as the
ranked path has existed. `corpus::walk` strips the root off itself when the
root IS a file, producing an empty relative path, and four separate
`root.join(rel)` sites then looked for `<file>/<file>`. Exact mode used the
keyword path and "worked", except it printed hits as `:9:text` with no
filename — which is why no test, snapshot, or review caught it.

Scoping to a file is what an agent does immediately after finding one, so the
bug fired at the follow-up step: **1,610 of 3,434 semantic searches (47%) in
the §16.10 campaign, touching 61% of instances**. Path resolution now lives in
one `corpus::resolve` helper, and a regression test covers file-as-root in
every mode, cold and warm. Found by reading agent trajectories *after* the
campaign; `eval/locbench/preflight.py` now replays real agent invocation
shapes before any run so the next one is caught in seconds, not in hindsight.

*Amended 2026-08-03:* "path resolution now lives in one helper" was not yet
true when written — `cache::repair` kept two raw `d.root.join(...)` sites with
the identical silent-skip failure. They were unreachable, but only because
`cache::discover` refuses a non-directory root, a guard three layers away that
the repair code cannot see; serving a file-scoped query from an ancestor index
would have resurrected the bug. Both now go through `resolve`, and the claim
is true as stated. The deeper cause is also worth naming: `corpus::read_text`
maps every IO error to the same `None` it uses for binary files, so a path bug
that made *every* file unreadable was indistinguishable from a corpus of
`.png`s. Nothing yet detects "walked N files, read 0 of them" — that blind
spot is the class of bug, and it is still open.

## 2026-08-01 — embed preprocessing: semantic recovers 85% of bm25 on real queries

The §14.3 campaign (5 corpora, 2,798 queries, paired stats): rendering code
as prose before embedding (`--embed-preproc split`) plus SIF pooling takes
CoSQA semantic R@5 from 0.108 to **0.188** (CI [+0.060, +0.099], 133w/37l)
against bm25's 0.222 — the gap falls from 2.7× (§13.8) to 1.18×. Two facets
of one failure: rendering fixes the token *units* and pays on camelCase
corpora (vscode direct +0.115, etcd +0.175); SIF fixes the *weights* and
pays on real snake_case queries (+0.062) — and §9.4's rejection of SIF turns
out to have been an artifact of synthetic query sets. Kernel paraphrase wall
stands (0.035 at best) but semantic now ties bm25 behind it. Not yet the
default build: gated on query replay (§14.5), per the twice-learned §9.7
lesson. Full record: RESEARCH.md §14.4.

## 2026-08-01 — semantic-first: the default mode is now `semantic`

Maintainer decision, recorded with its cost in RESEARCH.md §14: the success
criterion for this project is semantic search beating lexical on real queries,
and a hybrid default whose fused result is 97% BM25 (§13.10) was hiding the
component under test. `--mode hybrid` remains, tuned as before; the exact-miss
suggestion path follows the default. Known cost today: CoSQA R@5 0.208 → 0.083
until the §14.2 preprocessing campaign and the model work close the gap —
that trade is the point, not an oversight. MaxSim reranking is on in semantic
mode, so it is now part of the default search.

## 2026-07-30 — reorganization: layers, and the defects it surfaced

Restructured `semgrep-core` into six layers with a one-way dependency rule
(`corpus` → `text` → `rank` → `store` → `cache` → `search`; `keyword` apart), and
split the CLI into flags / verbs / output. The two files that held 56% of the code
are gone: no engine file is over 265 lines and no function over 60. Tests went
41 → 101, plus 45 pytest cases for the harness scorers, which had none.

The reorganization was a means, not the point. Thirteen defects surfaced, nine of
them invisible to reading — they appeared only once a snapshot tripwire, a
property test, or a measurement was pointed at the code. Full ledger in
`FIXES.md`; the ones that changed behavior:

- **Cache entries ignored chunk parameters.** One search with a non-default
  `--window` wrote an entry that every later search of that scope was served
  from, silently returning spans of the wrong size. The eval harness sweeps
  `--window` against the same cache ordinary use has, so **every §9 lever number
  was measured through a contaminated cache** and deserves a re-run.
- **Cold and warm searches did not agree.** The cold path scored full-precision
  cosine over f32 embeddings; the warm path scored i8 dot products over the
  quantized matrix. 37 of 54 query/mode pairs differed. The parity test that
  existed compared the first hit only and could not see it. Cold now quantizes as
  the build does: **0 of 54 differ**, asserted over the full top-k.
- **The repair overlay was inconsistent with its own base** in the same way, and
  computed BM25 idf over its own handful of files rather than the corpus. Repair
  now matches a full rebuild on 147 of 147 comparisons for hit set, with two
  order divergences pinned by name.
- **BM25 scores were not reproducible.** Two `HashMap` iteration orders feeding
  non-associative f32 addition meant near-tied chunks swapped rank between runs
  of the same binary on the same corpus.
- **One unreadable file NaN'd an entire reranked head** (`--maxsim` only).
- Interrupted cache builds leaked entries that `cache --status` could not see and
  `cache --prune` could not free; index publication was not atomic; read-repair
  wrote into a committed `.semgrep/`.

Removed ~110 lines of unreachable f32 embedding code, dead since format v2, and
collapsed seven eval campaign scripts into two.

New guardrails: `tools/snapshot.sh` byte-compares ranked output over a frozen
fixture corpus (it caught the reproducibility bug); `crates/semgrep/tests/cli.rs`
covers the process contract, which had no tests; `tests/docs.rs` checks that
source comments citing `RESEARCH.md §N` still resolve.

## 2026-07-27 — index format v2: provenance-driven perf round

Added per-stage timing provenance to every query (`--stats` prints
`load:{meta,chunks,bm25,mmap,hnsw} rank:{bm25,embed-query,brute|ann,fuse}
finalize`). It attributed the warm path almost entirely to three costs, each
now fixed:

| Bottleneck | Was | Fix | Now |
|---|---|---|---|
| `load:hnsw`: 3.1 GB graph eagerly deserialized every query, all modes | 20–35 s | lazy per-mode component loading; one-shot CLI skips graphs > 1 GiB | 0 ms |
| `load:bm25`: postcard → HashMap of 319 MB | 839 ms | `bm25.flat`: mmap'd sorted term table binary-searched in place, zero deserialization | 0.1 ms |
| `rank:brute`: f32 scan was page-fault/IO bound (188k faults, 0.45× CPU) — not compute bound | 2.9 s | i8-quantized unit-normalized vectors; 4× smaller matrix stays cache-resident | 65 ms |

**Net (kernel, warm, end-to-end): bm25 710→80 ms, semantic 4,060→80 ms,
hybrid 4,040→135 ms.** BM25 query RSS 1.2 GB→70 MB. Index on disk 3.4→1.3 GB;
build 59 s. Small corpora warm ≤ 20 ms. Quantization verified quality-neutral
on the 400-query VS Code eval (MRR within 0.004 of f32).

Other findings this round:
- The brute-force "compute" optimization tried first (pre-normalized vectors
  + 1-FMA dot kernel) bought only ~5% — profile before optimizing: the scan
  was IO-bound, so bytes-on-disk was the real lever.
- anny HNSW ranks well warm (496 ms at kernel scale) but `from_bytes` on a
  3.1 GB graph costs ~20 s, so it loses to the quantized brute scan in a
  one-shot CLI. Needs a zero-copy/mmap graph format or a persistent server
  mode. HNSW build also peaked at 8.8 GB RSS.
- `--check-stale` split out of `--stats`: the staleness re-walk cost ~1 s per
  query on 84k files and was silently polluting totals.
- Cold streaming got ~10% slower (kernel hybrid 53→59 s) from path-augmented
  chunk text — accepted as the price of the quality gains below.

## 2026-07-27 — retrieval-quality tuning round

Evals: 200 LLM-generated chunks → 400 queries per corpus (direct +
paraphrase), ground truth ±10 lines, vs agent-style rg fallback.

Changes, each validated on the evals:
- **Path-augmented chunks**: the relative file path is prepended to each
  chunk before BM25 tokenization and embedding. Largest single quality win —
  e.g. VS Code hybrid R@1 0.49→0.63 before any fusion tuning.
- **Weighted RRF** (`--sem-weight`, default 0.2 after a 0.2–0.8 sweep):
  equal-weight fusion let the weaker semantic list dilute BM25 (hybrid R@5
  0.76 vs bm25's 0.86 on VS Code). At 0.2, hybrid ≈ bm25 on direct queries
  and ≥ any single engine on paraphrase (Wikipedia paraphrase R@10 0.52 vs
  bm25 0.47).
- **MMR diversity reranking** (`--mmr-lambda 0.75`, on by default) +
  same-file span-overlap dedupe: spreads top-k across different files or
  regions. Ablation: free on single-truth recall (paraphrase R@10 actually
  +0.02).

**Net hybrid MRR@10: VS Code 0.595→0.753, Wikipedia 0.789→0.880.**

Findings:
- Ranked lexical search is the headline: BM25/hybrid top-5 recall 88–99% on
  identifier queries vs 3–27% for the same intents keyword-ized into rg
  (kernel: 0.92 vs 0.03 — 30×).
- Static embeddings underperform on paraphrased *code* (kernel paraphrase
  ≤ 0.05 R@5 for every method; VS Code semantic-paraphrase 0.01) while fine
  on prose (0.24–0.41). Open problem: better code embeddings or LLM query
  expansion, not more fusion tuning.

## 2026-07-27 — v1: engine, CLI, benchmark + eval harnesses

- `semgrep-core` + `semgrep` CLI: keyword mode on ripgrep's engine crates
  (`grep-regex`/`grep-searcher`/`ignore`), chunk-based BM25 with a
  camelCase/snake_case-aware tokenizer, semantic mode on ese embeddings
  (anny HNSW optional), RRF hybrid. Indexed (`.semgrep/`) and unindexed
  (single streaming pass, top-k heap) paths; grep-compatible output + JSONL.
- Benchmark harness (`bench/`) vs BSD/GNU grep, ripgrep, ugrep, ack on
  kernel/VS Code/Wikipedia — wall, peak RSS, CPU via `/usr/bin/time -l`.
  Harness lessons: BSD/GNU grep need `-E` (BRE broke the regex cells), and
  ugrep detects `/dev/null` and short-circuits (0.00 s cells) — all tools now
  write to real files.
- Eval harness (`eval/`): parallel LLM query generation, recall@k/MRR
  scoring, agent-task protocol.
- v1 findings: semgrep-keyword ties rg (same engine; 1.72 vs 1.86 s kernel).
  Cold *semantic* search of the whole kernel needs no index at all — 20 s /
  154 MB (ese embeds 1.5M chunks on the fly) — while cold *BM25* was the
  memory hog (39 s / 916 MB of in-memory postings). BSD grep: 44 s / 511 MB.
