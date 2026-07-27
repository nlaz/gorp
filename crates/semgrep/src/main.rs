//! semgrep — a semantic grep for agents.
//!
//! ```text
//! semgrep "where is the retry backoff computed" src/     # hybrid (default)
//! semgrep --mode bm25 "parse config" .                   # ranked lexical
//! semgrep -e 'fn \w+_config' .                           # keyword/regex (grep-style)
//! semgrep index . --hnsw                                 # build .semgrep/
//! ```

use anyhow::Result;
use semgrep_core::index::{self, BuildOptions};
use semgrep_core::keyword::KeywordOptions;
use semgrep_core::search::{Mode, SearchOptions, search};
use semgrep_core::{ChunkParams, corpus};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "semgrep", version, about = "Semantic grep: keyword, BM25, and embedding search in one tool")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Search query (natural language, keywords, or regex with -e)
    query: Option<String>,

    /// Directory to search (default: current directory)
    path: Option<PathBuf>,

    /// hybrid | keyword | bm25 | semantic
    #[arg(long, default_value = "hybrid")]
    mode: String,

    /// Shorthand for --mode keyword (regex search)
    #[arg(short = 'e', long)]
    regex: bool,

    /// Number of results for ranked modes
    #[arg(short = 'k', long = "top", default_value_t = 10)]
    top: usize,

    /// Case-insensitive (keyword mode)
    #[arg(short = 'i', long)]
    ignore_case: bool,

    /// Treat pattern as a literal string (keyword mode)
    #[arg(short = 'F', long)]
    fixed_string: bool,

    /// Context lines to print around each hit line
    #[arg(short = 'C', long, default_value_t = 0)]
    context: usize,

    /// Emit JSONL ({path, start_line, end_line, line, text, score})
    #[arg(long)]
    json: bool,

    /// Ignore any .semgrep index; force the streaming path
    #[arg(long)]
    no_index: bool,

    /// Use exact brute-force ranking even if the index has HNSW
    #[arg(long)]
    exact: bool,

    /// Print timing/memory report + per-stage provenance to stderr
    #[arg(long)]
    stats: bool,

    /// Re-walk the corpus after an indexed search to report stale files
    /// (costs a directory walk; independent of --stats)
    #[arg(long)]
    check_stale: bool,

    /// Weight of the semantic list in hybrid fusion (BM25 = 1.0)
    #[arg(long, default_value_t = 0.2)]
    sem_weight: f32,

    /// Disable MMR diversity reranking (return raw fused order)
    #[arg(long)]
    no_diversify: bool,

    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity
    #[arg(long, default_value_t = 0.75)]
    mmr_lambda: f32,

    /// Chunk window in lines (streaming path; indexed uses index params)
    #[arg(long, default_value_t = 32)]
    window: u32,

    /// Chunk overlap in lines
    #[arg(long, default_value_t = 8)]
    overlap: u32,
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
        /// Report index freshness instead of rebuilding
        #[arg(long)]
        status: bool,
        #[arg(long, default_value_t = 32)]
        window: u32,
        #[arg(long, default_value_t = 8)]
        overlap: u32,
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

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Some(Cmd::Index { path, hnsw, status, window, overlap }) => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            if status {
                return index_status(&root);
            }
            let opts = BuildOptions {
                params: ChunkParams { window, overlap, ..Default::default() },
                hnsw,
            };
            let t0 = std::time::Instant::now();
            let stats = index::build(&root, &opts, |done, total| {
                if done % 500 == 0 || done == total {
                    eprint!("\rindexing {done}/{total} files");
                }
            })?;
            eprintln!(
                "\rindexed {} files, {} chunks ({:.1} MB source) -> {} in {:.1}s ({:.1} MB)",
                stats.n_files,
                stats.n_chunks,
                stats.bytes_indexed as f64 / 1e6,
                index::index_dir(&root).display(),
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
    if !index::exists(root) {
        println!("no index (run `semgrep index {}`)", root.display());
        return Ok(1);
    }
    let idx = index::LoadedIndex::load(root, index::LoadNeeds { bm25: false, hnsw: false })?;
    let stale = idx.stale_files()?;
    println!(
        "index: {} files, {} chunks, hnsw={}, stale files: {stale}",
        idx.meta.files.len(),
        idx.meta.n_chunks,
        idx.meta.has_hnsw,
    );
    Ok(if stale == 0 { 0 } else { 1 })
}

fn run_search(cli: &Cli, query: &str) -> Result<i32> {
    let root = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let mode = if cli.regex {
        Mode::Keyword
    } else {
        match cli.mode.as_str() {
            "hybrid" => Mode::Hybrid,
            "keyword" => Mode::Keyword,
            "bm25" => Mode::Bm25,
            "semantic" => Mode::Semantic,
            other => anyhow::bail!("unknown mode {other:?} (hybrid|keyword|bm25|semantic)"),
        }
    };
    let opts = SearchOptions {
        mode,
        k: cli.top,
        no_index: cli.no_index,
        use_hnsw: !cli.exact,
        check_stale: cli.check_stale,
        sem_weight: cli.sem_weight,
        diversify: !cli.no_diversify,
        mmr_lambda: cli.mmr_lambda,
        params: ChunkParams { window: cli.window, overlap: cli.overlap, ..Default::default() },
        keyword: KeywordOptions {
            case_insensitive: cli.ignore_case,
            fixed_string: cli.fixed_string,
            max_hits: 0,
        },
    };
    let result = search(&root, query, &opts)?;

    for hit in &result.hits {
        if cli.json {
            println!("{}", serde_json::to_string(hit)?);
        } else {
            println!("{}:{}:{}", hit.path, hit.line, hit.text);
            if cli.context > 0 {
                print_context(&root, hit, cli.context);
            }
        }
    }

    if cli.stats {
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
            eprintln!("semgrep: warning: {} files changed since indexing (run `semgrep index`)", r.stale_files);
        }
    }
    Ok(if result.hits.is_empty() { 1 } else { 0 })
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
