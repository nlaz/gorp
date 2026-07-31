//! The command-line surface.
//!
//! Defaults come from the engine's own defaults rather than being retyped here,
//! so `--window 32` in help text cannot drift from `ChunkParams::default()`.
//!
//! The tuning flags are hidden and grouped into one struct: they exist for the
//! bench and eval harnesses, and no caller should be asked which engine to use.
//! Grouping them means the day they graduate or die is one edit.

use clap::{Args, Parser, Subcommand};
use semgrep_core::ChunkParams;
use semgrep_core::search::SearchOptions;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "semgrep",
    version,
    about = "Search code by meaning or by name: ranked search with a grep-compatible exact mode",
    after_help = "Ranked results are the k best locations, not every match — if the answer\n\
                  isn't there, rephrase the query. Use -e when you need every occurrence\n\
                  or proof of absence."
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    /// What to find: an identifier, a phrase, or a question (a regex with -e)
    pub query: Option<String>,

    /// Directory to search (default: current directory)
    pub path: Option<PathBuf>,

    /// Exact regex matching, grep semantics: every match, exit 1 on none
    #[arg(short = 'e', long)]
    pub exact: bool,

    /// Case-insensitive (exact mode)
    #[arg(short = 'i', long)]
    pub ignore_case: bool,

    /// Treat the pattern as a literal string, not a regex (exact mode)
    #[arg(short = 'F', long)]
    pub fixed_string: bool,

    /// Print every match in exact mode (default: first 250 plus a count)
    #[arg(long)]
    pub all: bool,

    /// Number of ranked results
    #[arg(short = 'k', long = "top", default_value_t = SearchOptions::default().k)]
    pub top: usize,

    /// Context lines to print around each hit line
    #[arg(short = 'C', long, default_value_t = 0)]
    pub context: usize,

    /// Emit JSONL ({path, start_line, end_line, line, text, score})
    #[arg(long)]
    pub json: bool,

    /// Print timing/memory report + per-stage provenance to stderr
    #[arg(long)]
    pub stats: bool,

    /// Re-walk the corpus after an indexed search to report stale files
    /// (costs a directory walk; independent of --stats)
    #[arg(long)]
    pub check_stale: bool,

    /// Engine internals, for the bench and eval harnesses (see `Tuning`).
    #[command(flatten)]
    pub tuning: Tuning,
}

/// Hidden knobs. Not part of the promised interface: they exist so the harnesses
/// can hold one lever fixed while moving another, and `RESEARCH.md` cites them by
/// name. A caller who has to pick an engine has already been failed.
#[derive(Args, Clone)]
pub struct Tuning {
    /// hybrid | keyword | bm25 | semantic (harness use; -e beats this)
    #[arg(long, hide = true)]
    pub mode: Option<String>,

    /// Ignore (and never write) any index or cache; force the streaming path
    #[arg(long, hide = true)]
    pub no_index: bool,

    /// Use exact brute-force ranking even if the index has HNSW
    #[arg(long, hide = true)]
    pub brute_force: bool,

    /// Weight of the semantic list in hybrid fusion (BM25 = 1.0)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().sem_weight)]
    pub sem_weight: f32,

    /// Disable MMR diversity reranking (return raw fused order)
    #[arg(long, hide = true)]
    pub no_diversify: bool,

    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity
    #[arg(long, hide = true, default_value_t = SearchOptions::default().mmr_lambda)]
    pub mmr_lambda: f32,

    /// Chunk window in lines (streaming path; indexed uses index params)
    #[arg(long, hide = true, default_value_t = ChunkParams::default().window)]
    pub window: u32,

    /// Chunk overlap in lines
    #[arg(long, hide = true, default_value_t = ChunkParams::default().overlap)]
    pub overlap: u32,

    /// PRF: expand the query with N terms mined from the first pass's top
    /// hits, then re-rank lexically (experimental, RESEARCH.md §9.3)
    #[arg(long, hide = true, default_value_t = 0)]
    pub prf: usize,

    /// Rerank candidates by MaxSim late interaction (§9.2). ON by default in
    /// `--mode semantic`, where it is a measured win (+0.080 R@5 on etcd, CI
    /// [+0.010,+0.155]); off in hybrid, where the fused result does not move
    /// because the semantic channel carries little of it (§13.10 root cause).
    #[arg(long, hide = true)]
    pub maxsim: bool,

    /// Turn MaxSim off where it is on by default (semantic mode).
    #[arg(long, hide = true, conflicts_with = "maxsim")]
    pub no_maxsim: bool,

    /// MaxSim rerank head size (0 = auto: k*3, min 96). Lower it to trade
    /// paraphrase recall for a few ms — see rank::maxsim::AUTO_HEAD.
    #[arg(long, hide = true, default_value_t = 0)]
    pub maxsim_pool: usize,

    /// MaxSim vs original-order blend within the head (1.0 = pure MaxSim)
    #[arg(long, hide = true, default_value_t = 1.0)]
    pub maxsim_blend: f32,
}

#[derive(Subcommand)]
pub enum Cmd {
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
        #[arg(long, default_value_t = ChunkParams::default().window)]
        window: u32,
        #[arg(long, default_value_t = ChunkParams::default().overlap)]
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
