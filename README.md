# semgrep

A semantic grep for agents: keyword, BM25, and embedding search in one
grep-shaped tool, built on the Bog stack ([`ese`](../ese) static text
embeddings + [`anny`](../anny) HNSW). Named for its lineage with
grep/ripgrep — the incumbent agent search tools it benchmarks against.
(Not affiliated with r2c/Semgrep, the static-analysis tool.)
One tool, four modes: `keyword` (grep semantics via ripgrep's engine),
`bm25` (ranked lexical), `semantic` (embeddings), and `hybrid`
(RRF fusion, the default). Works with or without a prebuilt index.

```sh
semgrep "where is the retry backoff computed" src/   # hybrid, streaming
semgrep index . && semgrep "retry backoff" .            # hybrid, indexed (fast)
semgrep -e 'fn \w+_config' .                         # plain regex grep
semgrep --mode semantic --json -k 20 "..." docs/     # JSONL for harnesses
semgrep index . --hnsw                               # optional ANN accelerator
```

- **Design:** see [DESIGN.md](DESIGN.md)
- **Build:** `cargo build --release` (first build downloads the ese model
  weights once). Binary lands at `target/release/semgrep`.
- **Test:** `cargo test`
- **Benchmarks:** `bench/fetch-corpora.sh`, then `python3 bench/run.py` and
  `python3 bench/report.py` — speed/peak-RSS/CPU vs BSD grep, GNU grep,
  ripgrep, ugrep, ack on the Linux kernel, VS Code, and a Wikipedia subset.
- **Quality evals:** `eval/generate.py` (LLM-generated query sets),
  `eval/run_eval.py` (recall@k / MRR), `eval/agent-eval.md` (agent-task
  protocol).

Output is grep-shaped (`path:line:text`, exit 0 on hits / 1 on none) so
agents can adopt it without new habits; `--json` adds scores and chunk spans.
