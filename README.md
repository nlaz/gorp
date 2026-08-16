# gorp

Coding agents search with ripgrep, so every question they have about a
codebase gets squeezed into a regex guess first. When the guess misses —
wrong identifier, wrong vocabulary — the agent gets nothing and burns a
retry loop: another guess, another tool call, more tokens.

gorp skips the guess. Hand it the question and it ranks the likely places:
same input shape as grep, top-k results instead of every match, each hit
printed with its enclosing declaration. When you do want a regex, `-e` is
grep, byte for byte.

```sh
gorp "where is the retry backoff computed" src/   # ranked — no setup, no index step
gorp -e 'fn \w+_config' .                         # exact regex, grep semantics
gorp --json -k 20 "auth middleware" .             # JSONL for harnesses
```

The name is for the lineage with grep and ripgrep. No relation to
r2c/Semgrep, the static-analysis tool.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/nlaz/gorp/main/install.sh | sh
```

That puts one ~48 MB self-contained binary in `~/.local/bin`
(`GORP_INSTALL_DIR` to override, `GORP_VERSION=v0.1.0` to pin) — the
embedding table and nine tree-sitter grammars are compiled in, so there is
no daemon, no GPU, and no runtime download. Prebuilt for macOS
(arm64/x86_64) and Linux (x86_64/arm64); tarballs are on the
[releases page](https://github.com/nlaz/gorp/releases).

Building from source requires two sibling repos (`ese`, `anny`) that are
currently private; the prebuilt binaries are built from the same sources by
[the release workflow](.github/workflows/release.yml).

## Usage

`gorp <query> [paths…]` — the query is an identifier, a phrase, or a
question; paths default to the current directory. Exit 0 on hits, 1 on none.

| flag | does |
|---|---|
| `-k N` | ranked results to return (default 5; bare `-k` means 20) |
| `-e` | exact regex mode, grep semantics: every match, exit 1 on none |
| `-i` / `-F` / `-w` | exact mode: ignore case / literal string / whole words only |
| `-c` / `--all` | exact mode: per-file counts only / print every match |
| `-C` / `-A` / `-B N` | context lines around each hit, both modes |
| `-l` | matching paths only, in rank order |
| `-g GLOB` (`--include`) | keep only paths matching the glob (repeatable) |
| `--lines A-B` | keep only results in a line range |
| `-M N` | truncate printed lines at N characters (default 200; 0 = off) |
| `--json` | JSONL: one object per hit — `{path, start_line, end_line, line, text, score, …}` |
| `--stats` | per-stage timing and provenance, on stderr |
| `gorp cache` | show what the cache holds; `--prune` reclaims, `--clear` empties |

Grep flags like `-n`/`-r`/`-H` are accepted by construction, because agents
type grep flags at anything shaped like grep. stdout carries only results
and pipes cleanly; hints ride on stderr (`GORP_NO_HINTS=1` silences them).
Printed lines are capped (`-M`) with a per-hit passage budget on top, so one
minified file can't flood an agent's context.

## Point your agent at it

Paste this into your agent's system prompt:

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

This wording is measured, not decorative — one clause in it moved an
agent's ranked-search share from 7% to 98%, so it ships verbatim. The
example is a name guess rather than a question on purpose: agents imitate
the example, and a wrong name still shares subtokens with the right one
where a description may share nothing.

## How it works

Files are cut into overlapping 32-line chunks and scored by two engines
over the same chunk table: code-aware BM25 (splits `camelCase`/`snake_case`
into subtokens, counts file paths as evidence) and static 256-dim
embeddings ([ese](https://github.com/flowercomputers/ese), i8-quantized).
The default mode ranks semantically with the top BM25 hits pinned into the
candidates, so a query that is really an identifier still gets its exact
match; a fine rerank then picks the best window inside each chunk, a
declaration boost favors definitions over call sites, and MMR spreads the
results across files.

There is no index verb in normal use, because a cold search and an index
build are the same streaming pass — so the first ranked search in a scope
writes its work down to `~/.cache/gorp` and later searches mmap it. Before
serving from cache, gorp diffs the scope against the live tree, so results
are always true of the code as it is right now; warm and cold return the
same answer, enforced by tests. Nothing lands in your repo, the cache is
bounded at 2 GB, and `gorp cache` shows, prunes, or clears it.

Full design in [docs/DESIGN.md](docs/DESIGN.md); the research log behind
every default in [docs/RESEARCH.md](docs/RESEARCH.md).

## Performance

Ranked search, end-to-end including process start, M-series Mac:

| ranked query | first time in a scope | cached |
|---|---|---|
| VS Code repo (49 MB, 4k files) | 2.5 s | **10 ms** |
| kernel `drivers/net/` (145 MB) | 3.9 s | **20 ms** |
| whole Linux kernel (1.15 GB, 84k files) | 32 s | **~140 ms** |

Exact mode is ripgrep's own engine crates, so it gives up nothing to the
incumbent: 1.72 s over the kernel against ripgrep's 1.86 s. The harness
behind these numbers lives in
[gorp-bench](https://github.com/nlaz/gorp-bench).

## Does it work?

Ripgrep can only find code you can already name. Both rows below are the
same 199 kernel functions, asked for two ways (recall@5: how often the
right code is in the top 5):

| you're looking for… | ripgrep | gorp |
|---|---|---|
| something you can name — `blkg_rwstat_add percpu counter` | 0.34 | **0.92** |
| something you can only describe — "helper that increments the right per-cpu statistic" | 0.00 | 0.04 |

On 1,200 real human queries (CoSQA), gorp lands the target in the top 5
~2.2× as often as `rg-oracle` — a ceiling baseline that reads the answer
before choosing its pattern, which no real tool can do — and ~7× ripgrep
as agents actually use it. Caveats, because we went looking for them: 4%
on describe-only queries is the difference between possible and impossible,
not good and great — the embedding table is prose-trained and a
code-distilled replacement is queued. Most of the win is code-aware ranking
rather than embeddings; BM25 alone scores about the same on CoSQA. And with
a real agent driving, end-to-end accuracy is parity with ripgrep at the
file level, +11pp at the function level, with fewer search round-trips per
task (4.0 vs 4.7).

Full methodology, every corpus, and the bugs we found in our own baselines:
[eval/REPORT.md](eval/REPORT.md).

## License

MIT (see [LICENSE](LICENSE)). The binary embeds an embedding table derived
from [static-retrieval-mrl-en-v1](https://huggingface.co/sentence-transformers/static-retrieval-mrl-en-v1)
by the Sentence Transformers project, licensed Apache-2.0 — see
[NOTICE](NOTICE).
