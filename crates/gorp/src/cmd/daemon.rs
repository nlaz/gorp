//! `gorp daemon`: the server half of the warm path (workplan Phase C1/C2).
//!
//! The daemon *is* bog-serve: a `KeyedApp` keyed by root-relative file path,
//! whose pipeline derives everything the flat index derives — but inside the
//! dataflow, so `PUT`ting a file's new text retracts every chunk the old text
//! produced and inserts the new ones. BM25 postings, HNSW nodes and the
//! manifest stay consistent with zero diff logic here.
//!
//! Lifecycle: bind immediately (the sidecar is what clients discover), then
//! fill the store from the tree in the background *through our own `/batch`
//! route* — each batch is one committed transaction, so a kill -9 loses at
//! most a batch, and the requests count as activity so the idle watchdog
//! cannot fire mid-fill. A client that arrives before the fill converges sees
//! a short manifest, reads the drift as "not ready", and answers locally —
//! the readiness check and the freshness check are the same diff.
//!
//! Determinism note: `chunk_fanout` must be a pure function of the file text
//! (same rule as every other cutter — see `corpus::funcchunk`'s warning),
//! because fold retracts by re-running it. `corpus::chunk_lines` is exactly
//! that, and nothing else may cut here.

use crate::daemon::{FileDoc, Layout, ManifestEntry, SYNC_CAP};
use crate::out::PROG;
use anny::metric::Cosine;
use anyhow::Result;
use bog_serve::{Bind, KeyedApp, NoParams, SchemaDrift, TextQuery};
use fold::pipeline::{FlatMap, Keyed, Map, terminal};
use gorp_core::text::token;
use gorp_core::{Chunk, ChunkParams, EMBED_DIM, corpus, rank};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many candidates each channel contributes to fusion — gorp-core's
/// `FUSION_POOL`, and the HNSW sink's compile-time `TOP_K` below must match
/// it, which is why both are literals here rather than one being derived.
const POOL: usize = 128;

type Vect = [f32; EMBED_DIM];

/// A chunk's identity in every sink: the path plus the span. Self-describing
/// on purpose — a search result needs no chunk-table lookup to know what file
/// and lines it names, and the id space stays stable across incremental
/// updates in a way walk-order file ids cannot.
#[derive(
    Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
struct ChunkKey {
    path: String,
    start: u32,
    end: u32,
}

/// The manifest value: the same size/mtime identity `corpus::diff` compares.
#[derive(Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct Meta {
    size: u64,
    mtime: u64,
}

/// A chunk's derived pair, carried together through the fan-out so the text
/// and its vector are computed once, side by side, in one batched encode.
#[derive(Clone)]
struct ChunkPayload {
    text: String,
    vec: Vect,
}

// Plain `fn` items, not closures, so the pipeline is a nameable type that can
// be constructed twice (once to probe the lock, once for the app) — and so
// nothing can accidentally capture state, which the retraction contract
// forbids anyway.
type Tok = fn(&str, &mut Vec<u8>);
type ChunkFan = fn(&Keyed<String, FileDoc>) -> Vec<Keyed<ChunkKey, ChunkPayload>>;
type TextOf = fn(&Keyed<ChunkKey, ChunkPayload>) -> Keyed<ChunkKey, String>;
type VecOf = fn(&Keyed<ChunkKey, ChunkPayload>) -> Keyed<ChunkKey, Vect>;
type ManifestFn = fn(&Keyed<String, FileDoc>) -> Keyed<String, Meta>;
/// Mirrors `text::GorpHnsw`'s shape (M0=32, EF_SEARCH=192, EF_BUILD=128,
/// MAX_LEVEL=16) with `TOP_K` at the fusion pool.
type VecSink = terminal::search::Hnsw<ChunkKey, f32, Cosine, EMBED_DIM, 32, POOL, 192, 128, 16>;
type ChunkSinks = (
    Map<
        TextOf,
        terminal::search::Bm25<ChunkKey, String, Tok>,
        Keyed<ChunkKey, ChunkPayload>,
        Keyed<ChunkKey, String>,
    >,
    Map<VecOf, TextQuery<VecSink, Vect>, Keyed<ChunkKey, ChunkPayload>, Keyed<ChunkKey, Vect>>,
);
type Pipe = (
    FlatMap<ChunkFan, ChunkSinks, Keyed<String, FileDoc>, Keyed<ChunkKey, ChunkPayload>>,
    Map<ManifestFn, terminal::Table<String, Meta>, Keyed<String, FileDoc>, Keyed<String, Meta>>,
);

fn pipeline() -> Pipe {
    (
        FlatMap::new(
            chunk_fanout as ChunkFan,
            (
                // fold's default k1/b are gorp's own (1.2 / 0.75); the
                // tokenizer is gorp's code-aware one so `getUserName` matches
                // the same queries warm as cold.
                Map::new(
                    text_of as TextOf,
                    terminal::search::Bm25::with_tokenizer("bm25", bm25_tokens as Tok),
                ),
                Map::new(
                    vec_of as VecOf,
                    TextQuery::new(VecSink::new("vec", Cosine, 0xB06), stored_vec_of),
                ),
            ),
        ),
        Map::new(manifest_entry as ManifestFn, terminal::Table::new("manifest")),
    )
}

/// One file into its chunk payloads: cut, then embed the whole file's chunks
/// as ONE `ese::encode` batch — parallel above 16 inputs — instead of one
/// `encode_single` per chunk. This is where the fill's throughput lives: the
/// per-chunk version serialized a repo fill onto one core and never converged
/// on benchmark-scale trees (four of six locbench repos still cold at 900 s).
/// `ese::encode` is exactly `map(encode_single)`, so the vectors are
/// bit-identical to the per-chunk path and state written before this change
/// still retracts cleanly.
///
/// Default chunk params only — the store directory's params tag says so, and
/// a client posing a different chunking resolves to a different directory and
/// never reaches this daemon.
fn chunk_fanout(d: &Keyed<String, FileDoc>) -> Vec<Keyed<ChunkKey, ChunkPayload>> {
    let cuts = corpus::chunk_lines(0, &d.key, &d.val.text, &ChunkParams::default());
    let docs: Vec<String> =
        cuts.iter().map(|(_, slice)| corpus::doc_text(&d.key, slice)).collect();
    let vecs = ese::encode(docs.iter());
    cuts.into_iter()
        .zip(docs)
        .zip(vecs)
        .map(|(((c, _), text), v)| {
            Keyed::new(
                ChunkKey { path: d.key.clone(), start: c.start_line, end: c.end_line },
                ChunkPayload { text, vec: as_stored_vect(&v) },
            )
        })
        .collect()
}

fn text_of(d: &Keyed<ChunkKey, ChunkPayload>) -> Keyed<ChunkKey, String> {
    Keyed::new(d.key.clone(), d.val.text.clone())
}

fn vec_of(d: &Keyed<ChunkKey, ChunkPayload>) -> Keyed<ChunkKey, Vect> {
    Keyed::new(d.key.clone(), d.val.vec)
}

/// Encode the way both flat-index paths do: embed, normalize, then round-trip
/// through i8 quantization (`rank::as_stored`), documents and queries alike.
/// Scoring quantized-vs-quantized is the discipline behind cold == warm; a
/// daemon storing raw f32 would be a third score space for no reason.
/// Rendering is the identity because the shipped config is
/// `EmbedPreproc::None` + `PathRender::Full` (RESEARCH.md §23.2) — if that
/// ever changes, this must learn to render, and the schema fingerprint will
/// not catch it (it hashes types, not closures), so say it loudly here.
fn stored_vec_of(text: &str) -> Vect {
    as_stored_vect(&gorp_core::text::embed_query(text))
}

fn as_stored_vect(v: &[f32; EMBED_DIM]) -> Vect {
    let v = rank::as_stored(v);
    let mut out = [0.0f32; EMBED_DIM];
    out.copy_from_slice(&v);
    out
}

/// gorp's BM25 token stream, in fold's `\0`-terminated buffer convention.
fn bm25_tokens(text: &str, buf: &mut Vec<u8>) {
    buf.clear();
    token::for_each_token(text, |tok| {
        buf.extend_from_slice(tok.as_bytes());
        buf.push(0);
    });
}

fn manifest_entry(d: &Keyed<String, FileDoc>) -> Keyed<String, Meta> {
    Keyed::new(d.key.clone(), Meta { size: d.val.size, mtime: d.val.mtime })
}

/// Query for `/search`. Defaults mirror `SearchOptions::default()` so a bare
/// `?q=` asks the question the CLI would.
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SearchParams {
    q: String,
    #[serde(default = "default_k")]
    k: usize,
    /// bm25 | semantic | hybrid
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_sem_weight")]
    sem_weight: f32,
    #[serde(default = "default_pin")]
    bm25_pin: usize,
}

fn default_k() -> usize {
    gorp_core::search::SearchOptions::default().k
}
fn default_mode() -> String {
    "semantic".to_string()
}
fn default_sem_weight() -> f32 {
    gorp_core::search::SearchOptions::default().sem_weight
}
fn default_pin() -> usize {
    gorp_core::search::SearchOptions::default().bm25_pin
}

pub fn run(path: Option<PathBuf>, idle_secs: u64) -> Result<i32> {
    let root = std::fs::canonicalize(path.unwrap_or_else(|| PathBuf::from(".")))?;
    if !root.is_dir() {
        anyhow::bail!("{}: not a directory", root.display());
    }
    let params = ChunkParams::default();
    let layout = Layout::resolve(&root, &params);
    layout.claim(&root)?;

    // Probe the fjall lock before building the app: the loser of a spawn race
    // exits 0 here (its client re-reads the sidecar and connects), instead of
    // panicking inside `KeyedApp::stream`. The probe-then-reopen window is a
    // real but tiny race; a loser that slips through it panics, which the
    // spawner already treats as "connect instead".
    match fold::stream::KeyedStream::try_new(&layout.store, pipeline()) {
        Err(fold::fjall::Error::Locked) => {
            eprintln!("{PROG}: another daemon already serves {}", root.display());
            return Ok(0);
        }
        Err(e) => return Err(anyhow::anyhow!("opening {}: {e}", layout.store.display())),
        Ok(stream) => drop(stream),
    }

    eprintln!(
        "{PROG}: daemon serving {} on {} (idle timeout {idle_secs}s)",
        root.display(),
        layout.sock.display()
    );

    // The background fill: sync the tree into ourselves over our own socket,
    // SYNC_CAP files per committed batch, until the manifest diff is empty.
    // Through HTTP rather than a private channel so each batch shares the
    // route handlers' transaction semantics and counts as activity.
    let fill_sock = layout.sock.clone();
    let fill_root = root.clone();
    std::thread::spawn(move || {
        for _ in 0..600 {
            if std::os::unix::net::UnixStream::connect(&fill_sock).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut errors = 0u32;
        loop {
            match crate::daemon::sync(&fill_sock, &fill_root, &params, SYNC_CAP) {
                Ok((true, files)) => {
                    eprintln!("{PROG}: daemon warm — {files} files indexed");
                    break;
                }
                Ok((false, _)) => errors = 0,
                Err(e) => {
                    errors += 1;
                    if errors > 5 {
                        eprintln!("{PROG}: fill abandoned after repeated errors: {e:#}");
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    });

    let manifest_root = root.clone();
    let search_root = root;
    KeyedApp::stream(&layout.store, pipeline())
        .bind(Bind::Unix(layout.sock.clone()))
        .idle_timeout(Duration::from_secs(idle_secs))
        // The store is derived state the tree can re-feed; a panic per gorp
        // release would be a non-starter (workplan B4). Chunker/tokenizer/
        // model changes are invisible to the schema fingerprint, but they
        // move `compat_key`, which is in this store's *path* — so either
        // mechanism landing is a clean rebuild.
        .on_schema_drift(SchemaDrift::WipeAndRebuild)
        .get("/manifest", move |readers, _p: NoParams| {
            let _ = &manifest_root; // captured so the closure type is unique per daemon
            let ((_bm25, _vec), manifest) = readers;
            Ok(manifest
                .iter()
                .map(|(path, m): (String, Meta)| ManifestEntry {
                    path,
                    size: m.size,
                    mtime: m.mtime,
                })
                .collect::<Vec<_>>())
        })
        // The blend, warm: the daemon's own retrieval (fold's BM25 and HNSW
        // sinks), then gorp-core's OWN post-fusion pipeline — MaxSim,
        // structural boost, pins, and the whole `finalize` tail (fine rerank,
        // learned checklist, MMR, unit view). The v1 handler cut those stages
        // and the Tier-0 replay measured the cost on every query tier
        // (−0.233 recall@5 at the worst), so parity here is shared code, not
        // a promise. Only the reader-touching retrieval lives in the closure;
        // everything after is `answer`, plain data over the shared tail.
        .get("/search", move |((bm25, vec), _manifest), p: SearchParams| {
            let mode = match p.mode.as_str() {
                "bm25" => rank::Mode::Bm25,
                "semantic" => rank::Mode::Semantic,
                "hybrid" => rank::Mode::Hybrid,
                other => return Err((400, format!("unknown mode {other:?}"))),
            };
            let opts = wire_options(mode, &p);
            // Interned ids bridge ChunkKey to the pipeline's u32 space.
            let mut keys: Vec<ChunkKey> = Vec::new();
            let mut ids: HashMap<ChunkKey, u32> = HashMap::new();
            let mut intern = |key: &ChunkKey| -> u32 {
                *ids.entry(key.clone()).or_insert_with(|| {
                    keys.push(key.clone());
                    (keys.len() - 1) as u32
                })
            };
            let wants_lexical =
                matches!(mode, rank::Mode::Bm25 | rank::Mode::Hybrid) || opts.bm25_pin > 0;
            let lexical: Vec<(u32, f32)> = if wants_lexical {
                bm25.search(&p.q, POOL)
                    .iter()
                    .map(|s| (intern(&s.val), s.score as f32))
                    .collect()
            } else {
                Vec::new()
            };
            let semantic: Vec<(u32, f32)> = if mode == rank::Mode::Bm25 {
                Vec::new()
            } else {
                // Ascending distance; `fuse` turns distance into similarity
                // for semantic mode and ranks by position for hybrid.
                vec.inner()
                    .search(&vec.encode(&p.q))
                    .iter()
                    .map(|s| (intern(&s.val), s.score))
                    .collect()
            };
            Ok(answer(&search_root, keys, lexical, semantic, &opts, &p.q))
        })
        .run();
    Ok(0)
}

/// The wire's few knobs over stock defaults. The client only routes here for
/// stock configurations, so `..Default::default()` is the truth, not a guess;
/// `rerank_maxsim` mirrors the CLI's resolve rule (on for semantic mode).
fn wire_options(mode: rank::Mode, p: &SearchParams) -> gorp_core::search::SearchOptions {
    gorp_core::search::SearchOptions {
        mode,
        k: p.k.clamp(1, 100),
        sem_weight: p.sem_weight,
        bm25_pin: p.bm25_pin,
        rerank_maxsim: mode == rank::Mode::Semantic,
        ..Default::default()
    }
}

/// Everything after the sink searches, all of it gorp-core's: MaxSim on the
/// semantic list, fuse, structural boost, bm25 pins, candidates, and the
/// shared `finalize` tail. The closures adapt the flat index's id-based
/// lookups to ChunkKey ones; `vec_of` re-embeds the candidate's document,
/// which reproduces the stored vector exactly (same text, same encode, same
/// quantization round-trip) at ~30 sub-millisecond encodes per query.
fn answer(
    root: &Path,
    keys: Vec<ChunkKey>,
    lexical: Vec<(u32, f32)>,
    semantic: Vec<(u32, f32)>,
    opts: &gorp_core::search::SearchOptions,
    query: &str,
) -> Vec<gorp_core::search::SearchHit> {
    use gorp_core::search::warm;
    use gorp_core::trace::{SCHEDULE_WARM, Trace};

    let doc_of = |id: u32| -> Option<String> {
        let key = &keys[id as usize];
        chunk_text(root, key).map(|t| corpus::doc_text(&key.path, &t))
    };

    let bm25_head: Vec<u32> = lexical.iter().take(16).map(|r| r.0).collect();
    let semantic = warm::maxsim_rerank(query, semantic, opts, doc_of);
    let mut ranked = rank::fuse(opts.mode, lexical, semantic, POOL * 2, opts.sem_weight);
    let shares = warm::structural_boost(&mut ranked, query, opts, |id| {
        let key = &keys[id as usize];
        (key.path.clone(), chunk_text(root, key))
    });
    let ranked = warm::bm25_pins(ranked, &bm25_head, opts);

    // Fused ids into candidates: the daemon's analog of the indexed path's
    // `candidates()`, minus the subtree filter (a daemon always serves its
    // whole root; the CLI's own Filter narrows afterwards).
    let rank_of = |id: u32| bm25_head.iter().position(|&h| h == id).map(|i| i as u16 + 1);
    let pinned: std::collections::HashSet<u32> =
        bm25_head.iter().take(opts.bm25_pin).copied().collect();
    let share_of = |id: u32| {
        shares
            .iter()
            .find(|&&(sid, _, _)| sid == id)
            .map(|&(_, d, p)| (d, p))
            .unwrap_or((0.0, 0.0))
    };
    let limit = warm::width(opts.k);
    let mut cands: Vec<warm::Candidate> = Vec::with_capacity(limit.min(ranked.len()));
    for (id, score) in ranked {
        if cands.len() >= limit {
            if pinned.is_empty() {
                break;
            }
            if !pinned.contains(&id) {
                continue;
            }
        }
        let key = &keys[id as usize];
        let (decl_share, path_share) = share_of(id);
        cands.push(warm::Candidate {
            id,
            chunk: Chunk { file_id: 0, start_line: key.start, end_line: key.end },
            path: key.path.clone(),
            score,
            phrases: 1,
            fine: None,
            bm25_rank: rank_of(id),
            decl_share,
            path_share,
        });
    }

    let q = warm::Query::parse(query);
    let mut trace = Trace::new(SCHEDULE_WARM);
    let fin = warm::finalize(root, &q, cands, opts, "", &mut trace, |c| {
        chunk_text(
            root,
            &ChunkKey {
                path: c.path.clone(),
                start: c.chunk.start_line,
                end: c.chunk.end_line,
            },
        )
        .map(|t| rank::as_stored(&gorp_core::text::embed_query(&corpus::doc_text(&c.path, &t))))
    });
    fin.hits
}

/// A chunk's lines, re-read from the tree — chunk text is never stored, the
/// same premise as the flat index.
fn chunk_text(root: &Path, key: &ChunkKey) -> Option<String> {
    corpus::lines(
        root,
        &key.path,
        &Chunk { file_id: 0, start_line: key.start, end_line: key.end },
    )
}
