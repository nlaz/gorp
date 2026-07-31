# fold as the repair overlay's store — evaluation and design

Companion to `SIMULATION.md`, which measured the problem, and
`RCA-FJALL-LOCK.md`, which documents the one blocker. `DESIGN.md:26` deferred
fold to v2; this is the record of what ending that deferral would actually
involve, written while the evidence was in hand.

Nothing here is implemented. It is a design and a set of verified facts, so that
the next person to pick this up does not have to re-derive them.

---

## 1. Why look at fold at all

Simulation testing measured read-repair as a cliff (`SIMULATION.md` §1.3): at 50%
corpus drift a query costs more than a full cold pass, and it **never
amortizes** — the tenth identical query costs what the first did. Overlay memory
is likewise unbounded, 9 MB → 63 MB across the drift sweep.

Both follow from one property: **the overlay is rebuilt every query and discarded
at process exit.** Tombstones, the union id space in `search/rows.rs`, the
per-query rebuild — all of it is scaffolding that simulates mutation on an
immutable index. The scaffolding exists because there is nowhere to put the patch.

`fold` is a differential dataflow engine over fjall with native retraction
(`push(data, +1 | -1)`) and reads that see one consistent snapshot across every
sink. That is the shape of an overlay.

## 2. Verified facts about fold

Read from the source at `../fold` (and `../ram`, its most evolved consumer), not
from the README. Recorded because several of them contradict the obvious design.

| Fact | Consequence |
|---|---|
| `Bag<D>` makes the postcard-encoded **value the key**, refcounted | Wrong sink for us: a 256-byte i8 vector would live in the key, and retraction would require reconstructing those exact bytes |
| `BagReader` exposes only `iter()` and `contains()`, fields private | No prefix or range scan; "all rows for path P" is a full scan |
| `Bm25Reader` reads `n_docs`/`total_len` from **its own keyspace** (`b"S"`) | Its idf is computed over the overlay alone — exactly the scale mismatch `rank::Rest` exists to fix (FIXES #13). Using it would reintroduce a solved bug |
| `Push<D>` tuple impl is **2-ary and homogeneous** | Heterogeneous sinks need `FlatMap`→`Option`, or a hand-written enum-routing node |
| `NodeInitializer::keyspace_with` panics on duplicate sink names | Naming discipline is enforced at open |
| `Bag::init` is O(1); fjall's open replays the journal, O(unflushed) | Opening a non-HNSW graph is cheap and roughly constant |
| `Hnsw` holds `Arc<RwLock<HnswState>>`, graph resident, and `anny`'s `Scalar` is implemented for **f32/f64 only** | fold's HNSW terminal cannot take our i8 vectors, and would be resident anyway — which `DESIGN.md:71` already rejected for a per-query CLI. **Irrelevant to this design**, because the overlay is small enough to brute-force, which is what we already do |
| `fold::Stream::new` `.unwrap()`s the fjall open | A lock conflict is a **panic**. See `RCA-FJALL-LOCK.md` |
| `fold`'s `Cargo.lock` pins **fjall 3.1.5, which is yanked** | Must be moved before a clean lockfile can resolve. The lock code is byte-identical in 3.1.5/3.1.6/3.1.8, so nothing here depends on which |

**The house pattern for heterogeneous sinks is ram's**
(`../ram/crates/ram-core/src/graph.rs`): one `enum Event` deriving only `Clone`,
a struct with `impl Push<Event>` dispatching by hand in a `match`, a named
readers struct instead of nested tuples, and `pub type RamStream =
fold::Stream<Event, RamGraph>`. Its per-path sink
(`ram-core/src/sinks/composition.rs`, 85 lines) is the template to copy — not
`fold::terminals::Bag`.

## 3. The design: fold as a store, not a scorer

Keep the base index exactly as it is — mmap'd, i8, untouched. Replace only the
in-memory overlay.

**The decision that makes it safe:** semgrep's scorers stay. fold holds rows;
`rank_lexical`, `rank_semantic`, `rows.rs` and fusion are unchanged. Swapping in
fold's BM25 would move every score and invalidate every published eval number for
no gain we can currently justify — and per §2 it would reintroduce FIXES #13.

**Sinks** (inside a cache entry so recursive `dir_bytes` charges it and eviction
takes it along; under `cache_base()` for a repo-local `.semgrep/`, because *a
search must not write into the user's tree* — the rule `repair::check_marker`
already establishes for `last_check`, FIXES #7):

- a path-keyed KV sink of `CoverRow { path, size, mtime, chunks }`, where
  `(size, mtime)` is the pair `corpus::diff` compares, so a row is reusable iff
  it still matches the live tree — the whole amortization rests on that;
- one cell holding the base's `build_id`, so a rebuilt base drops the overlay
  wholesale.

Deleted files need no row: `corpus::diff` produces their tombstones from the file
table every query, free. A file that chunks to nothing gets a row with an empty
chunk list, which is how "covered but contributes nothing" stays representable.

**The seam.** Split `repair::scope` into `index_paths` (read → chunk → tokenize →
embed, batched at 1024 like `store/build/embed.rs`) and `assemble` (rows +
tombstones → `Repair`). Both the durable path and the in-memory fallback call
them, so there is one implementation of the expensive work and one place where
delta ids are assigned. That second point matters: `delta.chunks[j]` /
`paths[j]` / `vecs[j]` / the j-th BM25 doc must all describe one chunk, because
`rank::top_k_within` returns `j` and `Rows::delta_index` consumes it. It is the
chunk-id lockstep invariant in miniature and it breaks the same way.

This refactor lands with **no new dependencies** and is where the ordering change
(globally sorting stale paths, so fjall's key order and the fallback's order
agree) is proved harmless.

## 4. The blocker

fjall takes an exclusive advisory lock — **even for readers** — and opening the
database writes (journal recovery ends in `persist(SyncAll)`). At most one
process may have it open at all. See `RCA-FJALL-LOCK.md` for the full analysis
and the proposed upstream change.

Downstream this makes the overlay a **degradable fast path**: preflight our own
advisory lock (a single non-blocking `try_lock`, so contention costs microseconds
instead of fjall's 200 ms retry), and on failure fall back to today's in-memory
repair. Never worse than the status quo; better whenever it is available.

Whether that is good enough is an empirical question, not an argument. Extend
`eval/sim/scenarios.py::s9` to record the overlay hit rate in realistic sessions.
If it is low, the answer is the resident server `RESULTS.md` already wants, not a
workaround.

## 5. Two measurements taken while evaluating

**Binary growth is not a risk.** semgrep's 39.06 MB is 88.7% `__const` — the
compiled-in ese weights table. All of its machine code is 2.3 MB of `__text`.
Attributing text symbols by originating crate in
`../the-library/target/release/library-ingest` (a real optimized binary linking
fold + fjall) puts the entire storage stack at **0.68 MB**: fjall 0.17,
lsm_tree 0.33, support crates 0.07, fold 0.11. The method accounted for 17.1 of
that binary's 17.9 MB `__text`.

So expect **~+2%**, against `bench/report.py`'s `SIZE_BUDGET = 1.15`. Allow
0.7–1.2 MB since semgrep uses `lto = "thin"` where the-library hoists fold's
`lto = "fat"`. The real costs of adding fold are cold-build compile time
(~50–60 crates) and supply-chain surface, not size. A crate count is a bad proxy
when a binary is 89% static data.

**`corpus::walk` is single-threaded.** `corpus/mod.rs:25` uses
`ignore::WalkBuilder::new(root).build()`, while `keyword.rs:43` already uses
`.build_parallel()` on the same trees in the same crate. The walk is ~7 ms on
tokio and ~1 s on the 84k-file kernel — and it is *why the TTL exists*, which is
why answers can be stale by design (`SIMULATION.md` §1.7). Parallelizing it is
independent of everything here and probably the best ratio of value to risk
available right now.

One hazard if it is parallelized: today's `sort_by(path)` is **not a total
order**. `to_string_lossy` maps invalid UTF-8 to `U+FFFD` and `\` is rewritten to
`/`, so two distinct entries can collide on one `String`. Serial, that is merely
reproducible; parallel, a stable sort would preserve *thread arrival order* and
chunk ids would shift between runs of the same tree. Sort on
`(path, size, mtime)` and land that change **on its own, still serial**, so
ordering is isolated from parallelism.

## 6. What to measure before committing to this

1. ~~**fjall open cost per process.**~~ **Partly answered — it is cheap.**
   Measured against `fjall 3.1.8` at semgrep's release profile
   (`opt-level = 3`, `lto = "thin"`, `codegen-units = 4`), timing only the
   `open()` call: **0.38–0.96 ms warm**, 9.8 ms on a cold page cache. Against a
   1.77 ms warm query that is roughly +45%, not the multiple-milliseconds
   disaster that would have killed the design.

   Caveat: that is an **empty** database. Journal replay is O(unflushed), so the
   figure holds only if the writer checkpoints — which we want anyway, since a
   flushed store is also what makes a shared-lock reader (RCA §6.1) see
   everything committed. Re-measure against a populated overlay before relying
   on it. What still needs measuring is the *snapshot read* of a real overlay,
   which is O(overlay size) — see risk 2 below.
2. **Overlay hit rate under realistic concurrency** (§4).
3. **Whether the journal grows without bound** across a long session, given we
   would not checkpoint on the query path. If it does, checkpoint on the *write*
   branch only — which also makes a shared-lock reader (RCA §6.1) see everything
   committed.

## 7. Sequencing

Ordered by dependency, and each of the first four is worth having whether or not
fold ever lands:

1. `corpus::walk` total sort key (serial), then parallel — §5
2. recursive `cache/budget.rs::dir_bytes` — an fjall store is a *subdirectory*,
   so without this the overlay's bytes are invisible to the budget enforcer and
   the cache grows unbounded while reporting itself under budget. Pre-registered
   as sim finding `s5c`
3. the delta-size threshold RESEARCH.md §8 specifies and `repair.rs` never
   implemented — reuse the existing corrupt-entry arm at `search/mod.rs:222`
   (`remove_dir_all` + `stream::run`), which converges on the next query via the
   existing write-through
4. `repair::index_paths` / `assemble` — §3, no new dependencies
5. `fold::Stream::try_new` (cross-repo, additive) and the overlay itself

Note that (3) fixes the *first* query after a branch switch and persistence fixes
queries 2..N. They are complementary, and (3) is ~15 lines with no dependency, no
lock, and no new crates.

## 8. Correction to `SIMULATION.md`'s framing

§1.3 argued the threshold from a **single-query** crossover — repairing costs
more than a cold pass past ~50% drift. That understates it. Because a rebuild
makes every *subsequent* query fast while repair keeps charging, the honest frame
is amortized:

| drift | query | extra vs clean | rebuild pays for itself after |
|---:|---:|---:|---:|
| 1% | 14.1 ms | +5.4 | 24 queries |
| 5% | 35.1 ms | +26.4 | **5 queries** |
| 10% | 41.7 ms | +33.0 | 4 queries |
| 25% | 90.6 ms | +81.9 | 2 queries |
| 50% | 131.1 ms | +122.4 | 1 query |

Which means RESEARCH.md §8's original 5% threshold is better justified than the
measurement appeared to say — not worse. A threshold must still sit well above
the edit-then-search loop (three files of 865 is 0.35%), and 5% does.
