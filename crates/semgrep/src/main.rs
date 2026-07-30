//! semgrep — a semantic grep for agents.
//!
//! One verb, one escape hatch:
//!
//! ```text
//! semgrep "where is the retry backoff computed" src/   # ranked (default)
//! semgrep -e 'fn \w+_config' .                         # exact regex, grep semantics
//! semgrep index .                                      # build .semgrep/
//! ```
//!
//! The ranked path serves the *locate* contract (best few places, bounded
//! output); `-e` serves *enumerate* and *verify* (every match, grep exit
//! codes). Engine selection (bm25/semantic/hybrid) and tuning knobs are
//! hidden flags kept for the bench/eval harnesses — no caller should be
//! asked which engine to use.

use anyhow::Result;
use clap::{Parser, Subcommand};
use semgrep_core::cache;
use semgrep_core::keyword::KeywordOptions;
use semgrep_core::search::{Mode, SearchOptions, search};
use semgrep_core::store::{self, BuildOptions};
use semgrep_core::{ChunkParams, corpus};
use std::path::PathBuf;

/// Exact mode prints at most this many matches unless --all is given; the
/// footer reports the true total so enumeration is never silently lossy.
const EXACT_PRINT_CAP: usize = 250;

#[derive(Parser)]
#[command(
    name = "semgrep",
    version,
    about = "Search code by meaning or by name: ranked search with a grep-compatible exact mode",
    after_help = "Ranked results are the k best locations, not every match — if the answer\n\
                  isn't there, rephrase the query. Use -e when you need every occurrence\n\
                  or proof of absence."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// What to find: an identifier, a phrase, or a question (a regex with -e)
    query: Option<String>,

    /// Directory to search (default: current directory)
    path: Option<PathBuf>,

    /// Exact regex matching, grep semantics: every match, exit 1 on none
    #[arg(short = 'e', long)]
    exact: bool,

    /// Case-insensitive (exact mode)
    #[arg(short = 'i', long)]
    ignore_case: bool,

    /// Treat the pattern as a literal string, not a regex (exact mode)
    #[arg(short = 'F', long)]
    fixed_string: bool,

    /// Print every match in exact mode (default: first 250 plus a count)
    #[arg(long)]
    all: bool,

    /// Number of ranked results
    #[arg(short = 'k', long = "top", default_value_t = 10)]
    top: usize,

    /// Context lines to print around each hit line
    #[arg(short = 'C', long, default_value_t = 0)]
    context: usize,

    /// Emit JSONL ({path, start_line, end_line, line, text, score})
    #[arg(long)]
    json: bool,

    /// Print timing/memory report + per-stage provenance to stderr
    #[arg(long)]
    stats: bool,

    /// Re-walk the corpus after an indexed search to report stale files
    /// (costs a directory walk; independent of --stats)
    #[arg(long)]
    check_stale: bool,

    // ------------------------------------------------------------------
    // Hidden: engine selection + tuning, kept for the bench/eval harnesses.
    // ------------------------------------------------------------------
    /// hybrid | keyword | bm25 | semantic (harness use; -e beats this)
    #[arg(long, hide = true)]
    mode: Option<String>,

    /// Ignore (and never write) any index or cache; force the streaming path
    #[arg(long, hide = true)]
    no_index: bool,

    /// Use exact brute-force ranking even if the index has HNSW
    #[arg(long, hide = true)]
    brute_force: bool,

    /// Weight of the semantic list in hybrid fusion (BM25 = 1.0)
    #[arg(long, hide = true, default_value_t = 0.2)]
    sem_weight: f32,

    /// Disable MMR diversity reranking (return raw fused order)
    #[arg(long, hide = true)]
    no_diversify: bool,

    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity
    #[arg(long, hide = true, default_value_t = 0.75)]
    mmr_lambda: f32,

    /// Chunk window in lines (streaming path; indexed uses index params)
    #[arg(long, hide = true, default_value_t = 32)]
    window: u32,

    /// Chunk overlap in lines
    #[arg(long, hide = true, default_value_t = 8)]
    overlap: u32,

    /// PRF: expand the query with N terms mined from the first pass's top
    /// hits, then re-rank lexically (experimental, RESEARCH.md §9.3)
    #[arg(long, hide = true, default_value_t = 0)]
    prf: usize,

    /// Rerank candidates by MaxSim late interaction (experimental, §9.2)
    #[arg(long, hide = true)]
    maxsim: bool,

    /// MaxSim rerank head size (0 = auto: k*3, min 24)
    #[arg(long, hide = true, default_value_t = 0)]
    maxsim_pool: usize,

    /// MaxSim vs original-order blend within the head (1.0 = pure MaxSim)
    #[arg(long, hide = true, default_value_t = 1.0)]
    maxsim_blend: f32,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or refresh the .semgrep index for a directory
    Index {
        /// Directory to index (default: current directory)
        path: Option<PathBuf>,
        /// Also build the anny HNSW graph (faster queries, more RAM/disk)
        #[arg(long)]
        hnsw: bool,
        /// SIF-weighted chunk embeddings (experimental, RESEARCH.md §9.1)
        #[arg(long, hide = true)]
        sif: bool,
        /// SIF smoothing constant a (larger = milder weighting)
        #[arg(long, hide = true, default_value_t = 1e-3)]
        sif_a: f64,
        /// Subtract the sample-estimated common component (SIF second half)
        #[arg(long, hide = true)]
        sif_center: bool,
        /// Report index freshness instead of rebuilding
        #[arg(long)]
        status: bool,
        #[arg(long, default_value_t = 32)]
        window: u32,
        #[arg(long, default_value_t = 8)]
        overlap: u32,
    },
    /// Inspect or reclaim the search cache
    Cache {
        /// Remove dead entries (repo gone) and evict LRU down to the budget
        #[arg(long)]
        prune: bool,
        /// Remove every cached entry
        #[arg(long)]
        clear: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("semgrep: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (bytes as f64, 0);
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

fn cache_cmd(prune: bool, clear: bool) -> Result<i32> {
    if clear {
        let (n, freed) = cache::cache_clear();
        println!("cleared {n} entries, reclaimed {}", human(freed));
        return Ok(0);
    }
    if prune {
        // An explicit prune reclaims everything reclaimable, not just the
        // current generation: automatic GC only runs on a cold write, so a
        // user who only queries warm scopes would never reclaim anything.
        cache::gc_old_generations();
        let (n, freed) = cache::enforce_budget();
        println!("pruned {n} entries, reclaimed {}", human(freed));
    }
    let entries = cache::cache_status();
    let total: u64 = entries.iter().map(|e| e.bytes).sum();
    let cap = cache::cache_max_bytes();
    println!(
        "{}  ({} entries, {} of {} budget)",
        cache::cache_base().display(),
        entries.len(),
        human(total),
        human(cap)
    );
    println!("generation {}", cache::compat_key());
    for e in &entries {
        let age = if e.age_secs > 86_400 {
            format!("{}d", e.age_secs / 86_400)
        } else if e.age_secs > 3_600 {
            format!("{}h", e.age_secs / 3_600)
        } else {
            format!("{}m", e.age_secs / 60)
        };
        println!(
            "  {:>9}  {:>5} ago  {}{}",
            human(e.bytes),
            age,
            e.root.display(),
            if e.root_exists { "" } else { "   (gone — prunable)" }
        );
    }
    if !entries.is_empty() {
        println!(
            "\nsemgrep cache --prune to reclaim, --clear to remove all; \
                  SEMGREP_CACHE_MAX_BYTES sets the budget"
        );
    }
    Ok(0)
}

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Some(Cmd::Cache { prune, clear }) => cache_cmd(prune, clear),
        Some(Cmd::Index { path, hnsw, sif, sif_a, sif_center, status, window, overlap }) => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            if status {
                return index_status(&root);
            }
            let opts = BuildOptions {
                params: ChunkParams { window, overlap, ..Default::default() },
                hnsw,
                sif,
                sif_a,
                sif_center,
            };
            let t0 = std::time::Instant::now();
            let stats = store::build(&root, &opts, |done, total| {
                if done % 500 == 0 || done == total {
                    eprint!("\rindexing {done}/{total} files");
                }
            })?;
            eprintln!(
                "\rindexed {} files, {} chunks ({:.1} MB source) -> {} in {:.1}s ({:.1} MB)",
                stats.n_files,
                stats.n_chunks,
                stats.bytes_indexed as f64 / 1e6,
                store::index_dir(&root).display(),
                t0.elapsed().as_secs_f64(),
                stats.index_bytes as f64 / 1e6,
            );
            Ok(0)
        }
        None => {
            let Some(query) = cli.query.clone() else {
                anyhow::bail!("usage: semgrep <QUERY> [PATH]  (see --help)");
            };
            run_search(&cli, &query)
        }
    }
}

fn index_status(root: &std::path::Path) -> Result<i32> {
    if !store::exists(root) {
        println!("no index (run `semgrep index {}`)", root.display());
        return Ok(1);
    }
    let idx = store::LoadedIndex::load(root, store::LoadNeeds { bm25: false, hnsw: false })?;
    let stale = idx.stale_files()?;
    println!(
        "index: {} files, {} chunks, hnsw={}, stale files: {stale}",
        idx.meta.files.len(),
        idx.meta.n_chunks,
        idx.meta.has_hnsw,
    );
    Ok(if stale == 0 { 0 } else { 1 })
}

fn resolve_mode(cli: &Cli) -> Result<Mode> {
    if cli.exact {
        return Ok(Mode::Keyword);
    }
    match cli.mode.as_deref() {
        None | Some("hybrid") => Ok(Mode::Hybrid),
        Some("keyword") => Ok(Mode::Keyword),
        Some("bm25") => Ok(Mode::Bm25),
        Some("semantic") => Ok(Mode::Semantic),
        Some(other) => anyhow::bail!("unknown mode {other:?} (hybrid|keyword|bm25|semantic)"),
    }
}

fn run_search(cli: &Cli, query: &str) -> Result<i32> {
    let root = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let mode = resolve_mode(cli)?;
    // A cold ranked search is also a cache build (write-through); say so up
    // front, since large scopes make the first search visibly slower.
    if mode != Mode::Keyword && !cli.no_index && cache::discover(&root).is_none() {
        eprintln!(
            "semgrep: first ranked search of this scope — caching it (later searches are fast)"
        );
    }
    let opts = SearchOptions {
        mode,
        k: cli.top,
        no_index: cli.no_index,
        use_hnsw: !cli.brute_force,
        check_stale: cli.check_stale,
        sem_weight: cli.sem_weight,
        diversify: !cli.no_diversify,
        mmr_lambda: cli.mmr_lambda,
        prf_terms: cli.prf,
        rerank_maxsim: cli.maxsim,
        maxsim_pool: cli.maxsim_pool,
        maxsim_blend: cli.maxsim_blend,
        params: ChunkParams { window: cli.window, overlap: cli.overlap, ..Default::default() },
        keyword: KeywordOptions {
            case_insensitive: cli.ignore_case,
            fixed_string: cli.fixed_string,
            max_hits: 0,
        },
    };
    let result = search(&root, query, &opts)?;

    let shown = if mode == Mode::Keyword && !cli.all && !cli.json {
        result.hits.len().min(EXACT_PRINT_CAP)
    } else {
        result.hits.len()
    };
    for hit in &result.hits[..shown] {
        if cli.json {
            println!("{}", serde_json::to_string(hit)?);
        } else {
            println!("{}:{}:{}", hit.path, hit.line, hit.text);
            if cli.context > 0 {
                print_context(&root, hit, cli.context);
            }
        }
    }

    // A miss should be a gradient, not a dead end: when exact mode finds
    // nothing and an index makes it cheap (~100 ms), show what ranked
    // search finds for the same terms — on stderr, so stdout stays empty
    // and exit stays 1 (the "proof of absence" contract is untouched).
    let suggested = if mode == Mode::Keyword && result.hits.is_empty() {
        print_miss_suggestions(&root, query, &opts)
    } else {
        false
    };

    print_footer(query, mode, &result, shown, suggested);

    if cli.stats {
        print_stats(mode, &result);
    }
    Ok(if result.hits.is_empty() { 1 } else { 0 })
}

/// On an exact-mode miss, print the top ranked hits for the same terms to
/// stderr. Only when a fresh index exists — the fallback must never turn a
/// fast miss into a slow streaming pass. Returns whether anything printed.
fn print_miss_suggestions(root: &std::path::Path, query: &str, opts: &SearchOptions) -> bool {
    // Only when an index/cache already covers this scope (via discovery, so
    // subdir scopes and ancestor/cache entries count): the fallback must
    // never turn a fast miss into a corpus pass or a surprise cache build.
    if opts.no_index || cache::discover(root).is_none() {
        return false;
    }
    let ranked_opts = SearchOptions { mode: Mode::Hybrid, k: 3, ..opts.clone() };
    let Ok(ranked) = search(root, query, &ranked_opts) else { return false };
    if ranked.hits.is_empty() {
        return false;
    }
    eprintln!("semgrep: -e found 0 exact matches · ranked search for the same terms finds:");
    for hit in &ranked.hits {
        let text = hit.text.trim();
        let text = text.char_indices().nth(100).map(|(i, _)| &text[..i]).unwrap_or(text);
        eprintln!("semgrep:   {}:{}:{}", hit.path, hit.line, text);
    }
    eprintln!(
        "semgrep: (wrong path? broaden it · drop -e to run this ranked search on stdout)"
    );
    true
}

/// The reply teaches: every outcome names the next move, so guidance lives
/// here (paid only on the turn it's needed) instead of in the tool schema
/// (paid every session). Stderr keeps stdout grep-parseable.
fn print_footer(
    query: &str,
    mode: Mode,
    result: &semgrep_core::search::SearchResult,
    shown: usize,
    suggested: bool,
) {
    let n = result.hits.len();
    if mode == Mode::Keyword {
        let n_files = result
            .hits
            .iter()
            .map(|h| h.path.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let files = if n_files == 1 { "file" } else { "files" };
        if n == 0 {
            if !suggested {
                // Both recovery moves, not just one: a 0-hit -e can mean the
                // path was wrong just as often as the vocabulary was.
                eprintln!(
                    "semgrep: 0 matches · wrong path? broaden it · searching for a \
                     concept? drop -e and ask in plain language"
                );
            }
        } else if shown < n {
            eprintln!(
                "semgrep: showing {shown} of {n} matches in {n_files} {files} \
                 (--all for every match) · locating a concept? drop -e and ask in plain language"
            );
        } else {
            eprintln!("semgrep: {n} matches in {n_files} {files}");
        }
    } else if n == 0 {
        eprintln!(
            "semgrep: no results · try broader phrasing or a nearby concept; \
             -e '{query}' checks for the exact string"
        );
    } else {
        eprintln!(
            "semgrep: ranked top {n} of {} candidates · not it? rephrase the query, \
             or -e '<pattern>' for every exact match",
            result.report.n_chunks_considered.max(n)
        );
    }
}

fn print_stats(mode: Mode, result: &semgrep_core::search::SearchResult) {
    let r = &result.report;
    eprintln!(
        "semgrep: mode={:?} hits={} index={} hnsw={} chunks={} walk/load={}ms rank={}ms total={}ms{}",
        mode,
        result.hits.len(),
        r.used_index,
        r.used_hnsw,
        r.n_chunks_considered,
        r.walk_ms,
        r.rank_ms,
        r.total_ms,
        peak_rss_mb().map(|m| format!(" peak_rss={m:.0}MB")).unwrap_or_default(),
    );
    if !r.stages.is_empty() {
        let line = r
            .stages
            .iter()
            .map(|(name, ms)| format!("{name}={ms:.1}ms"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("semgrep: provenance: {line}");
    }
    if r.stale_files > 0 {
        eprintln!(
            "semgrep: warning: {} files changed since indexing (run `semgrep index`)",
            r.stale_files
        );
    }
}

fn print_context(root: &std::path::Path, hit: &semgrep_core::search::SearchHit, n: usize) {
    let Some(text) = corpus::read_text(&root.join(&hit.path)) else { return };
    let lines: Vec<&str> = text.lines().collect();
    let center = hit.line as usize;
    let lo = center.saturating_sub(n).max(1);
    let hi = (center + n).min(lines.len());
    for i in lo..=hi {
        if i != center {
            println!("{}-{}-{}", hit.path, i, lines[i - 1]);
        }
    }
    println!("--");
}

fn peak_rss_mb() -> Option<f64> {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) } == 0;
    if !ok {
        return None;
    }
    let ru = unsafe { ru.assume_init() };
    // macOS reports bytes; Linux reports KiB.
    let bytes = if cfg!(target_os = "macos") {
        ru.ru_maxrss as f64
    } else {
        ru.ru_maxrss as f64 * 1024.0
    };
    Some(bytes / 1e6)
}
