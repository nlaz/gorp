//! The machine-readable trace envelope: one JSON object per engine invocation.
//!
//! `--stats` prints two lines a human reads. This is the same information plus
//! everything a human infers from context and a program cannot — which binary,
//! which index generation, which flags, which of the several `search()` calls a
//! single command makes. Without that, comparing two runs means trusting that
//! nothing else moved.
//!
//! **Two surfaces, one object.**
//!
//! `SEMGREP_TRACE_FILE=<path>` appends a line per invocation and is the primary
//! one, for four reasons that all come down to composition: it does not perturb
//! the argv an agent sees (so it works underneath `eval/locbench/shim.py`); it
//! can be set once for a whole session; it captures invocations no outer flag
//! could reach, notably the second full `search()` that an exact-mode miss runs
//! to build its suggestion; and it works for `index` and `cache` too.
//!
//! `--stats-json` writes the same object to **stderr**. Never stdout: `--json`
//! owns stdout and `--json --stats-json` has to leave the hit stream parseable.
//!
//! Failure policy differs on purpose. A trace-file write that fails is silent —
//! opt-in telemetry must never turn a working query into a failed one. A
//! `--stats-json` failure is loud, because the user asked for it directly.

use semgrep_core::rank::Mode;
use semgrep_core::search::{SearchOptions, SearchResult};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

pub const SCHEMA: &str = "semgrep.trace/1";

/// Which of a command's engine invocations this is.
#[derive(Copy, Clone, Debug)]
pub enum Phase {
    /// The search the user asked for.
    Primary,
    /// The extra ranked search an exact-mode miss runs to suggest alternatives.
    /// Invisible to `--stats`, which only ever saw the primary result.
    Suggest,
    Index,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Primary => "primary",
            Phase::Suggest => "suggest",
            Phase::Index => "index",
        }
    }
}

/// Stable for the life of the process, so every invocation a single command
/// makes is joinable. Derived from pid and start time rather than a uuid crate.
fn query_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut h);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .hash(&mut h);
        format!("{:016x}", h.finish())
    })
}

/// Set by a harness that is driving a whole session; absent otherwise. What
/// stitches many processes into one trajectory.
fn session_id() -> Option<String> {
    std::env::var("SEMGREP_SESSION_ID").ok().filter(|s| !s.is_empty())
}

fn next_seq() -> u32 {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What built this binary. `env!` values are stamped by `build.rs`.
fn binary() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "git_sha": option_env!("SEMGREP_GIT_SHA").unwrap_or(""),
        "git_dirty": option_env!("SEMGREP_GIT_DIRTY").unwrap_or("") == "true",
        "profile": option_env!("SEMGREP_BUILD_PROFILE").unwrap_or(""),
        "embed_dim": semgrep_core::EMBED_DIM,
        // The cache generation this binary can read. Entries written by an
        // incompatible binary live under a different key and are simply not
        // found — a miss with no error, which is invisible without this field.
        "compat_key": semgrep_core::cache::compat_key(),
    })
}

/// The cache knobs as the engine actually resolved them, not as the environment
/// spelled them. A scenario that pins `SEMGREP_CACHE_TTL_SECS` and gets the
/// default anyway (the value latches in a `OnceLock` on first use) would
/// otherwise look like it had pinned it.
fn env_block() -> Value {
    json!({
        "cache_dir": semgrep_core::cache::cache_base().to_string_lossy(),
        "cache_generation": semgrep_core::cache::cache_generation().to_string_lossy(),
        "cache_max_bytes": semgrep_core::cache::cache_max_bytes(),
        "ttl_secs": std::env::var("SEMGREP_CACHE_TTL_SECS").ok(),
    })
}

fn opts_block(opts: &SearchOptions) -> Value {
    json!({
        "k": opts.k,
        "no_index": opts.no_index,
        "use_hnsw": opts.use_hnsw,
        "check_stale": opts.check_stale,
        "sem_weight": opts.sem_weight,
        "diversify": opts.diversify,
        "mmr_lambda": opts.mmr_lambda,
        "prf_terms": opts.prf_terms,
        "bridge_expand": opts.bridge_expand,
        "bridge_weight": opts.bridge_weight,
        "rerank_maxsim": opts.rerank_maxsim,
        "maxsim_pool": opts.maxsim_pool,
        "maxsim_blend": opts.maxsim_blend,
        "fine_rerank": opts.fine_rerank,
        "fine_lines": opts.fine_lines,
        "fine_blend": opts.fine_blend,
        "passage_override": opts.passage_override,
        "min_score": opts.min_score,
        "decl_boost": opts.decl_boost,
        "path_boost": opts.path_boost,
        "bm25_pin": opts.bm25_pin,
        "dedupe_overlap": opts.dedupe_overlap,
        "keep_coarse_top": opts.keep_coarse_top,
        "embed_preproc": opts.embed_preproc,
        "path_render": opts.path_render,
        "params": {
            "window": opts.params.window,
            "overlap": opts.params.overlap,
            "max_file_bytes": opts.params.max_file_bytes,
            "function": opts.params.function,
        },
        "keyword": {
            "case_insensitive": opts.keyword.case_insensitive,
            "fixed_string": opts.keyword.fixed_string,
            "max_hits": opts.keyword.max_hits,
        },
    })
}

/// Which path answered, as one word. Reconstructing this from `used_index` and
/// `wrote_cache` is possible and everybody gets it wrong once.
fn path_taken(r: &semgrep_core::search::SearchReport, mode: Mode) -> &'static str {
    if mode == Mode::Keyword {
        "keyword"
    } else if r.wrote_cache && r.used_index {
        "cold_write_through"
    } else if r.used_index {
        "warm"
    } else if r.wrote_cache {
        // Built an entry, then failed to find it: the write-through raced or
        // the params disagreed. Worth its own name because it is a bug shape.
        "built_but_missed"
    } else {
        "cold"
    }
}

fn common(phase: Phase) -> Value {
    json!({
        "schema": SCHEMA,
        "query_id": query_id(),
        "session_id": session_id(),
        "phase": phase.name(),
        "seq": next_seq(),
        "ts_unix_ms": now_ms(),
        "pid": std::process::id(),
        "argv": std::env::args().collect::<Vec<_>>(),
        "cwd": std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        "binary": binary(),
        "env": env_block(),
    })
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    base
}

/// Build the envelope for one search invocation.
pub fn search_envelope(
    phase: Phase,
    mode: Mode,
    mode_reason: &str,
    root: &Path,
    query: &str,
    opts: &SearchOptions,
    result: &SearchResult,
    exit_code: i32,
) -> Value {
    let r = &result.report;
    merge(
        common(phase),
        json!({
            "kind": "search",
            "input": {
                "root": root.to_string_lossy(),
                "root_canonical": std::fs::canonicalize(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "query": query,
                "mode": format!("{mode:?}").to_lowercase(),
                "mode_reason": mode_reason,
                "opts": opts_block(opts),
            },
            "resolution": {
                "path_taken": path_taken(r, mode),
                "used_index": r.used_index,
                "used_hnsw": r.used_hnsw,
                "wrote_cache": r.wrote_cache,
                // Engine-internal, and process-wide. The second is the honest
                // one: the CLI resolves an index twice more than the engine
                // does, and neither of those showed up anywhere before.
                "discover_calls_engine": r.discover_calls,
                "discover_calls": semgrep_core::cache::discover_calls(),
            },
            "repair": r.repair,
            "timing": {
                "stages": r.stages,
                "buckets": {
                    "walk_ms": r.walk_ms(),
                    "load_ms": r.load_ms(),
                    "build_ms": r.build_ms(),
                    "rank_ms": r.rank_ms(),
                },
                "total_ms": r.total_ms,
                "accounted_ms": r.accounted_ms(),
                // The honest residual: wall time no stage claims.
                "unattributed_ms": r.unattributed_ms(),
            },
            "results": {
                "n_hits": result.hits.len(),
                "n_chunks_considered": r.n_chunks_considered,
                // Paired with the line above this is the whole diagnosis of an
                // empty result: 0 files means the scope held nothing, files but
                // 0 chunks means nothing in it could be read — the §16.11
                // signature, which for the life of that bug was reported as an
                // ordinary miss. Without it here a trace cannot tell a caller
                // who searched the wrong directory from a tool that is broken.
                "files_walked": r.files_walked,
                "stale_files": r.stale_files,
                // A refused search and an empty scope both report n_hits=0 and
                // exit 1, and only these two fields tell them apart. §30.1
                // registers the floored rate as a headline diagnostic, so it
                // has to be readable from the envelope rather than recovered
                // by grepping stderr out of the search dumps.
                "floored": r.floored,
                "best_signal": r.best_signal,
                // §31: how the query split and how each phrase fared. In the
                // results block, not opts — the split happens inside the
                // engine and the CLI never knows it.
                "n_phrases": r.n_phrases,
                "phrase_signals": r.phrase_signals,
                "floored_mask": r.floored_mask,
                "bridge_terms": r.bridge_terms,
                "exit_code": exit_code,
            },
            // getrusage(RUSAGE_SELF) never resets, so this is a high-water mark
            // for the whole *process*, not for this invocation. On the
            // exact-miss path two searches share one number.
            "process": { "peak_rss_mb": crate::out::peak_rss_mb() },
        }),
    )
}

/// Build the envelope for an index build.
pub fn index_envelope(
    root: &Path,
    opts: &semgrep_core::store::BuildOptions,
    stats: &semgrep_core::store::BuildStats,
    exit_code: i32,
) -> Value {
    merge(
        common(Phase::Index),
        json!({
            "kind": "index",
            "input": {
                "root": root.to_string_lossy(),
                "opts": {
                    "hnsw": opts.hnsw,
                    "sif": opts.sif,
                    // Which embedding space this index is in. Without it a
                    // trace cannot tell a `split` build from a `none` one,
                    // and the two are not comparable — the warm path renders
                    // queries with the index's own setting, so a mislabelled
                    // build reads as a quality result (RESEARCH.md §14.5).
                    // Serialized, not Debug-formatted, so the token matches
                    // the one the index writes into its own meta.json and the
                    // two can be joined.
                    "embed_preproc": opts.embed_preproc,
                    "params": {
                        "window": opts.params.window,
                        "overlap": opts.params.overlap,
                    },
                },
            },
            "timing": {
                "stages": stats.stages,
                "total_ms": stats.total_ms,
                "accounted_ms": stats.stages.accounted_ms(),
                "unattributed_ms": (stats.total_ms - stats.stages.accounted_ms()).max(0.0),
            },
            "results": {
                "n_files": stats.n_files,
                "n_chunks": stats.n_chunks,
                "bytes_indexed": stats.bytes_indexed,
                "index_bytes": stats.index_bytes,
                "exit_code": exit_code,
            },
            "process": { "peak_rss_mb": crate::out::peak_rss_mb() },
        }),
    )
}

/// Write to whichever surfaces are enabled. Never fails a query.
pub fn emit(envelope: &Value, stats_json: bool) {
    let line = match serde_json::to_string(envelope) {
        Ok(l) => l,
        Err(_) => return,
    };
    if let Ok(path) = std::env::var("SEMGREP_TRACE_FILE")
        && !path.is_empty()
    {
        let _ = append_locked(Path::new(&path), &line);
    }
    if stats_json {
        crate::out::stats_json(&line);
    }
}

/// Append one line under an exclusive lock.
///
/// The lock is not optional: a concurrency scenario runs eight processes at one
/// trace file, and `O_APPEND` alone does not guarantee a whole line lands
/// intact once it exceeds the pipe-atomic size — which these envelopes do. Same
/// discipline `eval/locbench/shim.py` uses on its own log for the same reason.
fn append_locked(path: &Path, line: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is owned by `file` and live for the whole call.
    if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let res = writeln!(file, "{line}").and_then(|_| file.flush());
    // SAFETY: same fd, still live; unlock even if the write failed.
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    res
}
