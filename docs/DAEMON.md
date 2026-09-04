# The per-repo daemon: design as built, measurements, and plan

Status 2026-08-26, branch `sam/fold-daemon` (uncommitted). The `daemon` cargo
feature is implemented end to end and gated green: the Tier-0 quality replay
shows statistical parity with the local engine on all three query tiers, 323
tests pass with the feature on (318 off), and `tools/snapshot.sh` is
byte-identical under both builds. This document is the record of what was
built, what was measured, and what is deliberately still open. The original
proposal is bog-kit's `docs/gorp-daemon-workplan.md`; where this document and
that one disagree, this one describes what actually shipped.

## 1. What it is

An opt-in, per-repo warm path: `gorp daemon <root>` serves ranked queries
over a unix socket from a fold-backed index that updates incrementally on
file change. The searching CLI spawns-or-connects; the daemon is **never on a
query's critical path** — no socket, a dead daemon, a filling daemon, a
drifted store, an ineligible query, or any transport failure all fall through
silently to the ordinary engine (tier-1 flat index or tier-0 stream), which
answers as it always has.

Design invariants, held throughout:

- The fjall file lock is the only mutual-exclusion primitive. Spawn races
  resolve themselves: one spawner wins the lock, the loser exits 0 and its
  client connects to the winner.
- Every client-side failure is a `None`, not an error.
- The per-query manifest diff is **both** the freshness sync and the
  readiness check (§4). Correctness never depends on the daemon being warm.
- Everything after fusion is gorp-core's own code (`search::warm`, §5). The
  daemon owns retrieval; it does not own ranking.

## 2. Architecture

### Server (`crates/gorp/src/cmd/daemon.rs`)

The daemon *is* bog-serve: a `KeyedApp` keyed by root-relative file path,
value `FileDoc { text, size, mtime }`. The pipeline derives everything the
flat index derives, inside the dataflow, so an upsert of a file's new text
retracts every chunk the old text produced and inserts the new ones — BM25
postings, HNSW nodes, and the manifest stay consistent with zero diff logic
in gorp:

    keyed(path, FileDoc)
      ├─ FlatMap: corpus::chunk_lines → per-chunk (text, stored vector)
      │    ├─ Bm25("bm25")            gorp's code-aware tokenizer, k1/b defaults
      │    └─ Hnsw<256>("vec")        GorpHnsw's shape, TOP_K = FUSION_POOL (128)
      └─ Map → Table("manifest")      path → {size, mtime}

Chunking is `corpus::chunk_lines` with default params only — the store
directory's params tag says so, and a client posing a different chunking
resolves to a different directory (§3). Embedding is per-file batched
`ese::encode` (parallel above 16 chunks), then the same
normalize→quantize→dequantize round-trip (`rank::as_stored`) both flat paths
use, documents and queries alike. `ese::encode` is exactly
`map(encode_single)`, so vectors are bit-identical to any per-chunk
formulation.

Custom routes: `GET /search` (the blend, §5) and `GET /manifest` (the sync's
view of what the daemon believes). Writes go through bog-serve's stock
`POST /batch`, one transaction per batch. Lifecycle is bog-serve's: bind →
sidecar → idle watchdog (default 30 min; checkpoint, remove sidecar+socket,
exit 0) and `SchemaDrift::WipeAndRebuild` on pipeline change.

**Self-fill.** The daemon binds immediately — the sidecar is what clients
discover — then fills itself from the tree in the background *through its own
`/batch` route*: each batch is one committed transaction (kill -9 loses at
most a batch), and the HTTP traffic counts as activity so the idle watchdog
cannot fire mid-fill. Fill times measured on the locbench trees (M-series
mac): metaflow 696 files / 7 s, sqlglot 293 / 17 s, nautobot 2,254 / 16 s,
mlflow 3,800 / 34 s.

### Client (`crates/gorp/src/daemon.rs`, hook in `cmd/search.rs`)

Per eligible ranked query: read the sidecar (dead pid = absent) → sync (§4)
→ `GET /search` over a hand-rolled HTTP/1.1-over-UDS client (no new
dependencies; `connection: close`, chunked-transfer aware) → forward the hits
through the CLI's normal filter/output/telemetry path.

Eligibility — the daemon serves one question, and anything else answers
locally: ranked mode, k ≤ 100, single phrase (18% of harvested agent queries
carry a `|` and split), default chunk params, stock retrieval knobs
(`prf`, `bridge-expand`, `min-score`, `embed-preproc`, `chunk-path` all at
defaults), not `--no-index`, not `--check-stale`, directory scope.

Spawning: `GORP_DAEMON=1` spawns on the first search; unset spawns on the
second ranked search of a scope within 10 minutes (an mtime marker under the
cache root, the `last_check` idiom); `GORP_DAEMON=0` disables connect and
spawn both — which is what `tools/snapshot.sh` pins, and what the eval
harness pins on its baseline arm. The spawn is detached (own session, stdio
to a log under the store dir) and never waited on.

## 3. Layout, keying, environment

Store: `<cache root>/srv/<compat_key>/<label>-<hash>-<params_tag>/` holding
`store/` (fjall), `store.serve.json` (bog-serve's sidecar), `log`, `recent`
(spawn marker), `root.txt` (the hash-bucket claim, same collision discipline
as cache entries — `cache::daemon_dir` shares `entry_stem` with
`cache_entry_dir` so the two cannot drift). `srv/` is a *sibling* of the
generation directories: the budget enforcer and generation GC never see it,
while `compat_key` in the path still makes a binary upgrade a clean
wipe-and-rebuild rather than an error, and the params tag keeps a `--window`
sweep from ever poisoning it.

The socket does **not** live there: `sockaddr_un` caps paths at ~104 bytes on
macOS and the cache directory is routinely deeper. It gets a short hashed
name under the temp dir — safe because nothing derives the socket path;
clients read it from the sidecar the daemon wrote after binding.

Provenance: a daemon-served query reports `daemon=true` on the `--stats`
line, `"path_taken": "daemon"` and `"daemon": true` in the trace envelope,
and `SearchReport::daemon`. This exists because a daemon answer and a flat
warm answer both say `used_index=true`, and the first benchmark run proved a
condition that cannot verify itself will silently measure the wrong arm
(§6.1).

## 4. The sync: freshness and readiness are one diff

Before querying, the client walks the tree (`corpus::walk`, the same
TTL-free walk repair pays), fetches `/manifest`, and `corpus::diff`s the two.
Empty diff → query. Small diff (≤256 files, ≤1 MiB of text per batch) → push
it in one transaction, then query — which is why an agent that edits then
searches sees its edit with no sleep and no watcher. Large diff (a filling
daemon, a branch switch) → push one installment and answer locally; repeated
queries and the daemon's own fill loop converge it. A daemon that is not
current is therefore *bypassed*, never trusted — readiness is not a flag, it
is the same comparison freshness is.

Three defects were found here by the first benchmark run, all of the class
"works on toy corpora, wedges on real ones":

1. **Serial embedding.** The pipeline encoded one chunk at a time on one
   core; four of six benchmark repos never finished filling in 900 s. Fixed
   by per-file batched encode (bit-identical vectors). metaflow: never → 7 s.
2. **axum's ~2 MiB body limit**, enforced by closing the connection
   mid-upload, which the client saw as a bare broken pipe and the fill saw as
   five retries and abandonment. Fixed on both sides: byte-capped batches
   here, and a 64 MiB `DefaultBodyLimit` in bog-serve (a document store
   whose documents are caller-sized needs headroom, not a hidden ceiling).
   sqlglot (one 3 MiB file): never → 17 s.
3. **Unreadable-file removes never converge.** The walk admits binaries that
   `read_text` refuses; syncing them as `remove` left them eternally "added",
   and 560 of them wedged nautobot's sync window solid while the manifest sat
   frozen. Fixed by upserting empty text with the real size/mtime — zero
   chunks, recorded identity, converging diff, exactly the flat build's
   semantics for the same files. nautobot: never → 16 s.

## 5. Ranking parity: `search::warm`

The daemon owns retrieval (fold's BM25 and HNSW sinks answer instead of the
flat index); everything after fusion is gorp-core's own pipeline, exported
behind the `daemon` feature as `search::warm`: `finalize` and `Candidate`
re-exported, thin wrappers for the structural boost / bm25 pins / candidate
width, and one new orchestration (`warm::maxsim_rerank`, the public
`rank::maxsim*` helpers over a caller-supplied document reader). The handler
runs: MaxSim on the semantic list (semantic mode, mirroring the CLI's resolve
rule) → `rank::fuse` → structural boost → bm25 pins → candidates → the whole
`finalize` tail (dedupe, fine rerank, learned checklist, MMR, unit view,
display pins), all under `SearchOptions::default()`. MMR's vectors are
re-embeds of the candidate documents, which reproduce the stored vectors
exactly. The wire format is literal `SearchHit` JSON — the CLI's `--json`
contract by construction, unit rows and all.

This was not the v1 shape. v1 returned the raw fused list (the workplan's
sanctioned open question: "skip rerank on the warm path and measure whether
it matters"), and the measurement came back unambiguous — see §6.2.

## 6. Measurements

### 6.1 Instrument

`eval/replay_traces.py`, extended with a self-verifying `--daemon` arm: 1,975
real harvested agent queries (of the 10,460 in `traces-v1.jsonl`) whose
target repos were materialized at pinned SHAs — metaflow, openlibrary,
sisyphus, sqlglot, nautobot, mlflow ×2. Per tree the harness pre-warms one
daemon (warmth read from the daemon's own log, not probe queries), replays
that tree's rows, and records **per row** whether the daemon actually
answered. Baseline arm: same rows, `GORP_DAEMON=0`, flat cache. Scoring:
rank of first gold file, paired per row, bootstrap CI + sign test, per tier
and never pooled.

The provenance requirement is not hygiene, it is load-bearing: the first
daemon-arm run reported plausible-looking tier numbers while only 466/1,975
rows had actually touched a daemon (the fills were failing, §4) — without
per-row provenance that run would have stood as a full daemon-arm result.

### 6.2 Quality: v1 vs shared tail (Δrecall@5 vs local, daemon-served rows)

| tier | n | v1 (coarse blend) | shared tail | sign (shared) |
|---|---|---|---|---|
| blind | 1,399 | −0.036 [−0.059, −0.014] * | **−0.006 [−0.013, +0.001]** | 9W/17L, p=0.17 |
| guess | 103 | −0.233 [−0.350, −0.107] * | **−0.019 [−0.049, +0.000]** | 0W/2L, p=0.50 |
| golden | 138 | −0.109 [−0.188, −0.029] * | **−0.007 [−0.022, +0.000]** | 0W/1L, p=1.00 |

Verdicts: the v1 cut of the post-fusion stages cost recall on **every** tier
— worst on guess-tier queries, whose reward path runs through the
declaration boost and the checklist's path-share feature — and sharing the
tail recovered essentially all of it (disagreements: 344 rows → 29 of 1,640
served, >98% agreement). Every shared-tail CI includes zero.

The residual is the retrieval front-end itself: the daemon's HNSW is an
*approximate* top-128 where the flat path brute-forces the whole matrix, so
a marginal candidate occasionally differs before the shared tail sees it.
That is the inherent cost of the incremental index, now **measured** at well
under a point of recall@5 rather than assumed. Consequence for invariants:
`cold == warm` does not extend to the daemon and cannot exactly — the
daemon's answer space is "the shared pipeline over approximate retrieval",
and the replay gate is its parity test.

### 6.3 Cost (small-corpus smoke, M-series mac)

Warm daemon query end-to-end (walk + manifest diff + `/search`): ~4 ms
against the same query's ~5 ms cold stream on a toy corpus — the daemon's
latency case is corpus-scale, where the cold alternative is 20 s and the
flat-warm alternative still pays discovery + mmap + (past the TTL) a repair
walk per invocation. Not yet measured at that scale: that is Tier 1 (§8).

## 7. Known gaps and deliberate deferrals

- **CI cannot build the feature yet.** The workflows now check out a single
  `nlaz/bog-kit` mirror (replacing `nlaz/ese` + `nlaz/anny`); that mirror
  must exist and carry bog-kit's additions before CI goes green. gorp now
  builds against `../bog-kit/{anny,ese,fold,serve}` — the old standalone
  `../ese`/`../anny` checkouts are stale forks. The enabling additions live
  in bog-kit: `Hnsw::{to_bytes,from_bytes}` (anny),
  `for_each_token{,_vector}` (ese), the request-body limit (serve) — all
  uncommitted there as of this writing.
- **Default chunk params only.** A non-default `--window` client skips the
  daemon rather than getting a second store; fine until someone actually
  wants a function-chunked daemon.
- **`gorp cache --prune/--clear` and a live daemon** have no protocol: the
  sweeps don't touch `srv/` (by construction), and `--clear` removes the
  cache root out from under any running daemon. Decide the ergonomics when
  it bites.
- **A3 (HNSW graph persistence in fold)** remains open upstream. Every
  daemon start rebuilds the graph from the persisted vectors during fill;
  at the measured fill rates this is invisible at repo scale, and the
  generous idle timeout makes cold starts rare. Revisit at kernel scale.
- **The multi-phrase 18%** always answers locally. Teachable later (the
  shared tail already handles phrases); v1 keeps the daemon's one question
  simple.

## 8. Benchmark plan (the tiers)

- **Tier 0 — quality gate (done, passing).** The replay above; push-button,
  ~35 min, self-verifying. Re-run after any daemon-side ranking change.
- **Tier 1 — latency and freshness (next).** Deterministic scripted sessions
  under `eval/sim/`: interleave K edits with N searches at three corpus
  scales, three conditions (as-shipped cold, `GORP_AUTO_INDEX=1` flat warm,
  daemon), measuring per-query wall time and edit-visibility. This is where
  the daemon's actual claims get numbers; no agent variance, ~free to run.
  Note the daemon's real competitor here is `GORP_AUTO_INDEX=1`, not grep.
- **Tier 2 — live agents (last, only if Tier 1 earns it).** 30–50
  SWE-bench-style instances, same model/harness/prompt, PATH shim swapping
  the tool per condition (`rg-strong`, gorp as-shipped, gorp + daemon),
  primary metric task success, secondaries from shim logs +
  `GORP_TRACE_FILE` (tokens, wall time, searches per session,
  time-to-first-gold). Paired per instance, clustered by task. A single
  two-agents-one-feature race is a demo, not a measurement — record one for
  the story after the campaign produces the number.
