# gorp — a semantic grep for agents

**Status:** v1 design, 2026-07-27, kept current on structure (last checked
2026-08-16) — the shape below is what shipped. Measured numbers here are the
v1 round; CLAUDE.md's "Known costs" is the live table, and RESEARCH.md
records what changed and why.
**Name:** `gorp` — semantic grep. Chosen for direct lineage with grep/ripgrep,
which is the incumbent agent search tool this project benchmarks against.
(Distinct from r2c Semgrep, the static-analysis tool.)

## Goal

A grep-shaped search tool that agents can drop in wherever they use `grep`/`rg`,
but that also understands *meaning*: ranked lexical search (BM25) and semantic
search (embeddings), fused into one result list. Success is measured two ways:

1. **Quality** — the tool finds the right code/docs for natural-language and
   fuzzy queries where grep finds nothing or noise.
2. **Cost** — latency, peak RSS, and CPU time close enough to ripgrep-class
   tools that adoption isn't blocked. We benchmark all of this against
   `grep`, `ripgrep`, `ugrep`, and `ack`.

## Foundation: the Bog stack

| Crate  | Role here |
| ------ | --------- |
| `ese`  | Static text embeddings (256-dim f32 via ese's `dim-256` feature; its own default is 512), compiled into the binary, CPU-only, very fast (`encode` is rayon-parallel). Makes *unindexed* semantic search feasible. |
| `anny` | HNSW ANN index. `Hnsw<f32, Cosine, 256, …>` with `to_bytes`/`from_bytes` for on-disk persistence. Cosine handles unnormalized vectors (`1 − cos`). |
| `fold` | Incremental dataflow over fjall. **Deferred to v2** (watch mode / incremental reindex). v1 indexes with a lean purpose-built format so the memory story stays clean. |

Both are consumed as path dependencies (`../anny`, `../ese`) for now.

## Search modes

| Mode       | What it is | Unindexed (cold) | Indexed (warm) |
| ---------- | ---------- | ---------------- | -------------- |
| `keyword`  | Regex/literal match, grep semantics | Parallel scan via ripgrep's own crates (`grep-regex`, `grep-searcher`, `ignore`) | Same scan (no keyword index in v1 — rg is already near-optimal here; this keeps our keyword numbers honest) |
| `bm25`     | Ranked lexical search over chunks | One-pass in-memory index build, then query | Serialized postings |
| `semantic` | Embedding similarity over chunks (default mode — semantic-first, RESEARCH.md §14) | Stream files → chunk → `ese::encode` → brute-force top-k (bounded memory: only a k-heap retained) | Default: exact rayon brute-force over the mmap'd embedding matrix (memory-light; ~1.5 GB of f32 vectors would otherwise sit resident in HNSW for a kernel-sized corpus). `gorp index --hnsw` opts into the `anny` graph for ~ms queries; benchmarks compare both. |
| `hybrid`   | Weighted reciprocal-rank fusion of `bm25` + `semantic`, opt-in via `--mode hybrid`. The shipped default instead pins the top BM25 hits into the semantic candidate set (`--bm25-pin`, RESEARCH.md §32.4b), so an identifier query still gets its exact match | Both cold paths share one corpus pass | Both warm paths |

**Why chunks for both BM25 and semantic:** one document table, one granularity,
so fusion and eval scoring are apples-to-apples, and every result maps back to
`file:line`.

### Chunking

Line-window chunks: `W` lines with `O` lines of overlap (defaults `W=32`,
`O=8`, configurable). Chunk record = `(file_id, start_line, end_line)`.
Chunk text is path-augmented before both BM25 and embedding — `corpus::doc_text`
prepends the relative path, so file names count as evidence — and any further
rendering is recorded in `meta.json` so the warm path replays exactly what the
build did. Files are skipped if binary (NUL sniff) or > max-file-size
(default 4 MiB).

### Tokenizer (BM25)

Code-aware: split on non-alphanumerics, then split `camelCase` /
`PascalCase` / `snake_case` identifiers into subtokens; lowercase; keep both
the whole identifier and its subtokens; drop tokens shorter than 2 chars.
BM25 params: k1 = 1.2, b = 0.75.

### Hybrid fusion

Weighted RRF over the BM25 and semantic lists (top-128 each): BM25
contributes `1/(60 + rank)` at weight 1.0, semantic the same at
`sem_weight` (shipped default 0.2 — tuned, not guessed), ties broken by
semantic score. Cheap, robust, no tuning of score scales.

## Index format

The index is a cache (RESEARCH.md §8): entries live under `~/.cache/gorp`
(`GORP_CACHE_DIR` to override), keyed by canonical root plus chunk
parameters. Writing one is opt-in since 2026-08-16 — a plain ranked search
streams a cold miss and leaves nothing on disk; `--index` or
`GORP_AUTO_INDEX=1` restores the write-through on a cold miss, and
`gorp index` prewarms explicitly. Discovery (and the repair/rebuild upkeep
of an entry that exists) is unconditional: opting in is about creating
state, never about reading it. A local `.gorp/` at the corpus root is
still honored by discovery when present. `meta.json` is written last — writing it is what publishes an
entry. Each entry holds:

| File | Contents |
| ---- | -------- |
| `meta.json` | Format version (v2), dims, chunk params, chunk count, `has_hnsw`, the `sif`/`embed_preproc`/`path_render` settings the warm path must replay, and the file table (path, size, mtime) for staleness detection |
| `chunks.bin` | postcard: `Vec<Chunk{file_id, start_line, end_line}>` |
| `bm25.flat` | Flat mmap-able BM25: sorted term table (binary-searched in place) + postings blob + doc lengths. Zero deserialization — provenance showed the old postcard load cost ~840 ms/query on the kernel |
| `emb.bin` | i8-quantized unit-normalized matrix, `n_chunks × 256` bytes, mmap'd. 4× smaller than f32 — the brute scan is page-fault/IO bound, so bytes-on-disk is the latency lever |
| `sif.bin` | rarity weights for the SIF pooling described in RESEARCH.md §9.1 |
| `hnsw.bin` | optional `anny` graph (`index --hnsw`). Skipped at query time when > 1 GiB: one-shot deserialize (~20 s at kernel scale) loses to the quantized brute scan until the graph has a zero-copy format; a persistent server mode would amortize it |

Staleness: on search, compare file table against the live tree and
read-repair around the drift; past `--repair-max-drift` the entry rebuilds
instead. v1 rebuild is full; incremental is the fold-based v2.

HNSW compile-time params: `DIM=256, M0=32 (M=16), K=128, EF_SEARCH=192,
EF_BUILD=128, MAX_LEVEL=16`. Runtime `k ≤ 128` served from HNSW; larger k
falls back to exact brute-force over `emb.bin`.

## CLI

```
gorp <QUERY> [PATHS…]            # search (default: ranked semantic; discovers an existing index, never writes one)
  -e, --exact                   # exact regex, grep semantics: every match, exit 1 on none
  -i / -F / -w / -c / --all     # exact mode: case / literal / whole words / counts / uncapped
  -k, --top N                   # ranked results, default 5 (bare -k means 20)
  -C, --context N               # context lines around each hit, both modes
  --json                        # JSONL: {path, start_line, end_line, line, text, score, …}
  --stats                       # print timing/memory footprint to stderr
  --index                       # opt in: cache this scope on a cold search (GORP_AUTO_INDEX=1 for an environment)
  --mode …, --no-index, …       # hidden harness knobs (see cli.rs `Tuning`)
gorp index [PATH]                # build/refresh .gorp/ (hidden; the explicit opt-in / prewarm)
gorp index --status              # index freshness report
```

Exact mode prints grep-compatible `path:line:text`. Ranked modes print a unit
view per hit (RESEARCH.md §34): a `path:start-end` header, then `line:`
numbered rows dedented as a block — the fine window plus its enclosing
declaration, `⋮` where rows were elided — in ranked order, hits separated by
a blank line (`--no-unit` restores the bare `path:line:text` passage; `score`
only in `--json`). Exit code 0 if hits, 1 if none — same contract as grep.

`gorp-core` is a library crate; the CLI is a thin wrapper, so the-library and
other Flower Computer apps can embed the engine directly.

## Benchmarks (speed, memory, CPU)

**Competitors:** BSD grep (`/usr/bin/grep`), GNU grep (`ggrep`), `rg`,
`ugrep`, `ack`. All invoked by absolute path (the dev shell wraps `grep`).

**Corpora:**
1. **Linux kernel** source tree (~1.4 GB, ~80k files) — the canonical grep
   benchmark corpus; results comparable to published rg/ugrep numbers.
2. **VS Code** repo (large TS/JS) — closest to real agent coding targets.
3. **Wikipedia subset** (~1–2 GB plain text extracted from a simplewiki dump)
   — prose/document search.

**Scenarios (per corpus):**
- Keyword: literal word; common word (many hits); rare regex; case-insensitive
  literal — gorp-keyword vs all five competitors, cold FS cache noted, warm
  measured (hyperfine handles warmup).
- BM25/semantic/hybrid: gorp only (competitors can't), measured in both
  *unindexed* (cold, includes the corpus pass/embedding) and *indexed* modes;
  plus `gorp index` build time and on-disk index size.

**Metrics captured per run:**
- Wall time: `hyperfine` (median ± σ, ≥10 runs after warmup).
- Peak RSS + user/sys CPU: `/usr/bin/time -l` (macOS) parsed to JSON; CPU
  utilization = (user+sys)/wall shows parallelism efficiency.
- Index cost: build wall time, peak RSS during build, `.gorp/` bytes on disk.

Output: per-run JSON + a generated markdown summary table. The perf harness and
the corpora live in the sibling `gorp-bench` repo and are re-runnable end to end
(`fetch-corpora.sh` then `run.sh`).

**Measured after the v2 tuning round** (kernel corpus, warm): bm25 80 ms,
semantic 80 ms, hybrid 135 ms end-to-end; peak RSS 70 MB (bm25) / ~840 MB
(semantic, quantized matrix resident). *Superseded by the dim-256 switch —
CLAUDE.md's "Known costs" carries the current numbers.* Per-stage timings via `--stats`
("performance provenance": load:meta/chunks/bm25/mmap/hnsw,
rank:bm25/embed-query/brute|ann/fuse, finalize).

**Hypotheses verified in v1 benchmarks:**
- gorp-keyword ≈ rg (same engine crates), far ahead of grep/ack.
- Unindexed BM25/semantic pay a full corpus pass — the question is whether
  ese keeps that in the "seconds, not minutes" range on 1.4 GB.
- Indexed semantic queries are ~ms but pay RSS for the loaded index; mmap of
  `emb.bin` + lazy HNSW load keep resident cost proportional to what's touched.

## Quality evals

**1. LLM-generated retrieval evals** (primary): sample chunks from each
corpus; have Claude write, per chunk, (a) a natural-language "where is the
code/passage that…" query and (b) a paraphrase query avoiding the chunk's
identifiers/keywords. Ground truth = source chunk (file + line span, with a
tolerance window). ~200 queries per corpus, spot-checked by hand. Score
recall@1/5/10, MRR@10, per mode × per tool (grep-family tools get a
best-effort keyword extraction from the NL query, which is exactly how agents
use them today — that gap is the headline metric).

**2. Agent-task evals** (secondary): ~20 tasks per corpus of the form "find
where X is handled"; run an agent (claude CLI headless) with only one search
tool available per condition; measure success rate, tool calls to success, and
tokens consumed. This measures the actual product goal: fewer, better search
round-trips for agents.

Harness in `eval/`: `generate.py` (query gen via claude CLI), `run_eval.py`
(executes queries against each tool/mode and scores recall@k / MRR), and
`results.py` for the tables.

## Milestones

M1–M4 (engine, bench harness, retrieval evals, agent-task evals) have all
shipped; the bench harness now lives in the sibling `gorp-bench` repo. What
remains is v2: fold-based incremental/watch indexing (see `FOLD.md`, and its
§9 blocker) and an MCP server mode.
