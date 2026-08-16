//! The command-line surface.
//!
//! Defaults come from the engine's own defaults rather than being retyped here,
//! so `--window 32` in help text cannot drift from `ChunkParams::default()`.
//!
//! The tuning flags are hidden and grouped into one struct: they exist for the
//! bench and eval harnesses, and no caller should be asked which engine to use.
//! Grouping them means the day they graduate or die is one edit.

use clap::{Args, Parser, Subcommand};
use gorp_core::ChunkParams;
use gorp_core::search::SearchOptions;
use std::path::PathBuf;

/// The drift bound's default, taking `GORP_REPAIR_MAX_DRIFT` when it is set.
///
/// Read here, in the CLI, rather than inside the engine: the engine takes it as
/// an ordinary option so a test can cross the threshold without mutating the
/// environment of every other test in the process.
/// The provenance block behind `--version`; `-V` stays the terse line. The
/// same fields the telemetry envelope's `binary` block carries, so a bug
/// report's `--version` output and a trace file describe a build the same way.
fn long_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        let sha = option_env!("GORP_GIT_SHA").unwrap_or("");
        format!(
            "{}\ncommit: {}{}\nprofile: {}\nembed dim: {}\ncompat key: {}",
            env!("CARGO_PKG_VERSION"),
            if sha.is_empty() { "unknown" } else { sha },
            if option_env!("GORP_GIT_DIRTY") == Some("true") { " (dirty)" } else { "" },
            option_env!("GORP_BUILD_PROFILE").unwrap_or("unknown"),
            gorp_core::EMBED_DIM,
            gorp_core::cache::compat_key(),
        )
    })
}

/// `-k` bounded, because its value flows into `Vec::with_capacity` and a
/// couple of width multiplications: an absurd value must be a one-line range
/// error (which teaches the caller the legal range — the caller is usually an
/// agent that invented the number), never an allocation abort. 10,000 is far
/// past any measured real `-k` and far short of overflow.
fn parse_top(s: &str) -> Result<usize, String> {
    let v: usize = s.parse().map_err(|_| String::from("not a number"))?;
    if !(1..=10_000).contains(&v) {
        return Err(String::from("must be between 1 and 10000"));
    }
    Ok(v)
}

fn default_max_drift() -> f32 {
    std::env::var("GORP_REPAIR_MAX_DRIFT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|r: &f32| r.is_finite() && *r >= 0.0)
        .unwrap_or(SearchOptions::default().repair_max_drift)
}

#[derive(Parser)]
#[command(
    name = "gorp",
    version,
    long_version = long_version(),
    // The tool description is a deliverable, not decoration (RESEARCH.md §6):
    // identity framing as measured in §16.8/§16.10, no mention of `-e` —
    // naming the exact-mode escape hatch here moved agents off ranked search
    // wholesale; the flag stays documented on its own argument below, where
    // someone who needs it will find it. The wide-by-default sentence is the
    // desc-v10 framing (§28.2): agents scope both search tools to guessed
    // paths and lose when the guess is wrong, and nothing in any prior help
    // text ever said the pathless call is the intended one.
    about = "Ranked code search: give it an identifier, a phrase, or a question",
    after_help = "Searches the whole tree from the current directory by default — start wide,\n\
                  and add a path only to narrow further after a wide search points somewhere.\n\
                  Ranked results are the k best locations, not every match — if the answer\n\
                  isn't there, rephrase the query. -k N returns more.\n\
                  Common grep flags are accepted."
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    /// What to find: an identifier, a phrase, or a question (a regex with -e)
    pub query: Option<String>,

    /// Files or directories to search (default: current directory)
    ///
    /// More than one is grep's contract and 31% of real agent invocations use
    /// it (RESEARCH.md §17), so it is accepted: the search runs over their
    /// common ancestor and results are filtered back to the paths given.
    pub paths: Vec<PathBuf>,

    /// Same as a positional path; accepted because agents keep typing it
    ///
    /// `sg "query" --path pkg/` is a real invocation shape: §28.1 caught it
    /// once in 484 searches, and §30's R1 caught it four times in 511 — the
    /// rise is desc-v10's own doing, since telling an agent to "add a path
    /// argument" invites a named flag on an interface that takes a
    /// positional. Failing that with "unexpected argument" spends a turn to
    /// teach argv trivia. Repeatable and merged with the positionals, so
    /// `--path a --path b` behaves like `a b`.
    #[arg(long = "path", value_name = "PATH", hide = true)]
    pub path_flag: Vec<PathBuf>,

    /// Exact regex matching, grep semantics: every match, exit 1 on none
    #[arg(short = 'e', long)]
    pub exact: bool,

    /// Case-insensitive (exact mode)
    #[arg(short = 'i', long)]
    pub ignore_case: bool,

    /// Treat the pattern as a literal string, not a regex (exact mode)
    #[arg(short = 'F', long)]
    pub fixed_string: bool,

    /// Match whole words only (exact mode)
    #[arg(short = 'w', long = "word-regexp")]
    pub word: bool,

    /// Print match counts per file instead of matches (exact mode).
    /// Files with zero matches are not listed — ripgrep's rule, not GNU
    /// grep's: a recursive tool enumerating every zero-count file is noise.
    /// With --json, one {"path", "count"} object per line.
    #[arg(short = 'c', long = "count")]
    pub count: bool,

    /// Print every match in exact mode (default: first 250 plus a count)
    #[arg(long)]
    pub all: bool,

    /// Number of ranked results (bare `-k` means 20)
    ///
    /// Two defaults, because the flag's absence and its emptiness are different
    /// statements. Absent means "no opinion" and gets the engine's 5. Present
    /// with nothing after it means "more than you were going to give me" — the
    /// only thing typing `-k` at all can be asking for, since 5 is already
    /// what you get for free. 20 is that: the mode of every explicit `-k` in
    /// the measured agent corpus (31 of 98).
    ///
    /// This is not grep compatibility — neither grep nor ripgrep has `-k` in
    /// any form, so there is no other tool's behavior to honor here. It is the
    /// habit `tar -k`, `df -k` and `du -k` leave behind, where `-k` is a
    /// complete flag, arriving at a tool where it is not.
    ///
    /// A bare `-k` reads as bare only at the end of the line: given a following
    /// token it still takes it as its value, so `-k <path>` is an error about a
    /// non-numeric `--top` rather than a search. That is the shape of the data
    /// and not a compromise — across 1,554 real `-k` invocations, 1,552 are
    /// followed by a number and 2 by nothing, and not one by a path.
    #[arg(
        short = 'k',
        long = "top",
        default_value_t = SearchOptions::default().k,
        default_missing_value = "20",
        num_args = 0..=1,
        value_parser = parse_top,
    )]
    pub top: usize,

    /// Truncate each printed line to this many characters (0 = no limit)
    ///
    /// A ranked result costs k lines, so this is what bounds what k lines can
    /// cost. Without it one minified or generated line — a 374 KB single-line
    /// JSON fixture, measured in a real agent run — can outweigh every other
    /// result put together and push them out of the reader's context entirely.
    #[arg(short = 'M', long = "max-columns", default_value_t = crate::out::MAX_COLUMNS)]
    pub max_columns: usize,

    /// Context lines to print around each hit line
    #[arg(short = 'C', long, default_value_t = 0)]
    pub context: usize,

    /// Context lines to print after each hit line (overrides -C)
    #[arg(short = 'A', long = "after-context")]
    pub after_context: Option<usize>,

    /// Context lines to print before each hit line (overrides -C)
    #[arg(short = 'B', long = "before-context")]
    pub before_context: Option<usize>,

    /// Print only the paths of matching files, in rank order
    #[arg(short = 'l', long = "files-with-matches")]
    pub files_with_matches: bool,

    /// Keep only results whose path matches this glob (repeatable), e.g. -g '*.py'
    #[arg(short = 'g', long = "glob", visible_alias = "include")]
    pub glob: Vec<String>,

    /// Keep only results in this line range: A-B, A- or -B (inclusive, 1-based)
    ///
    /// The one thing agents piped this tool into `awk` and `grep` for —
    /// `… | awk -F: '$2 < 2297'` is `--lines -2297` — and the only such use that
    /// `-k` could not already serve (RESEARCH.md §19.9). Doing it here needs no
    /// second binary, which matters where the caller's shell may refuse one.
    /// `allow_hyphen_values` because `-B` is half the point: without it clap
    /// reads `--lines -100` as a stray `-1` flag and suggests `-- -1`, which is
    /// advice toward a different command. The `--lines=-100` form worked and the
    /// spaced one did not, which is exactly the kind of split nobody discovers
    /// until it wastes a turn.
    #[arg(long = "lines", value_name = "A-B", allow_hyphen_values = true)]
    pub lines: Option<String>,

    /// Colorize paths and line numbers: auto (a terminal), always, never
    ///
    /// ripgrep's flag, ripgrep's values, and ripgrep's default palette, so
    /// the two tools' output reads the same in the same terminal. `auto`
    /// also honors `NO_COLOR`. Under a pipe this is off and stdout is the
    /// same bytes it always was — the `path:line:text` contract does not
    /// bend for a display preference.
    #[arg(long, value_name = "WHEN", default_value = "auto",
          value_parser = ["auto", "always", "never"])]
    pub color: String,

    /// Emit JSONL ({path, start_line, end_line, line, text, score})
    #[arg(long)]
    pub json: bool,

    /// Print timing/memory report + per-stage provenance to stderr
    #[arg(long)]
    pub stats: bool,

    /// Emit the full machine-readable trace envelope (one JSON object per
    /// engine invocation) to stderr. Set GORP_TRACE_FILE to append it to a
    /// file instead, which also captures invocations this flag cannot see.
    /// Hidden: a harness surface, and the env var is the more capable one.
    #[arg(long, hide = true)]
    pub stats_json: bool,

    /// Re-walk the corpus after an indexed search to report stale files
    /// (costs a directory walk; independent of --stats). Hidden: an operator
    /// probe, not part of the promised search surface.
    #[arg(long, hide = true)]
    pub check_stale: bool,

    /// grep compatibility, for callers arriving with grep muscle memory.
    #[command(flatten)]
    pub compat: GrepCompat,

    /// Engine internals, for the bench and eval harnesses (see `Tuning`).
    #[command(flatten)]
    pub tuning: Tuning,
}

/// Flags gorp already satisfies unconditionally.
///
/// These are accepted and read by nothing, and that is not a stub: gorp's
/// `path:line:text` output *is* grep's `-rn` form, always. `-n` asks for line
/// numbers and gets them; `-r`/`-R` ask for recursion and get it; `-H` asks for
/// filenames and gets them. Each flag's postcondition holds the moment it is
/// accepted, so honoring it costs nothing.
///
/// Honoring their *absence* is what gorp declines to do — no `-n` would mean
/// dropping line numbers, no `-r` refusing a directory, `-h` dropping the path.
/// All three break the contract every caller parses, and `-h` is `--help`.
///
/// Hidden rather than listed: they change no behavior, and the tool prompt has a
/// budget (RESEARCH.md §6). `after_help` says once that grep flags work, which is
/// what a caller reading `--help` needs — and by RESEARCH.md §17 a caller only
/// reads `--help` when it is already stuck. `-n` alone is 88% of the flags in
/// the measured agent corpus, and every one of those calls used to exit 2.
#[derive(Args, Clone)]
pub struct GrepCompat {
    /// Print line numbers (gorp always does)
    #[arg(short = 'n', long = "line-number", hide = true)]
    pub line_number: bool,

    /// Recurse into directories (gorp always does)
    #[arg(short = 'r', long = "recursive", hide = true)]
    pub recursive: bool,

    /// Accepted for grep compatibility (gorp always recurses; it does not
    /// follow symlinks)
    #[arg(short = 'R', long = "dereference-recursive", hide = true)]
    pub dereference_recursive: bool,

    /// Print the filename with each match (gorp always does)
    #[arg(short = 'H', long = "with-filename", hide = true)]
    pub with_filename: bool,
}

/// Hidden knobs. Not part of the promised interface: they exist so the harnesses
/// can hold one lever fixed while moving another, and `RESEARCH.md` cites them by
/// name. A caller who has to pick an engine has already been failed.
#[derive(Args, Clone)]
pub struct Tuning {
    /// semantic (default) | keyword | bm25 | hybrid (harness use; -e beats this)
    #[arg(long, hide = true)]
    pub mode: Option<String>,

    /// Ignore (and never write) any index or cache; force the streaming path
    #[arg(long, hide = true)]
    pub no_index: bool,

    /// Use exact brute-force ranking even if the index has HNSW
    #[arg(long, hide = true)]
    pub brute_force: bool,

    /// Share of a scope that may drift before a cache entry is rebuilt instead
    /// of patched; 0 repairs any amount. Defaults from
    /// `GORP_REPAIR_MAX_DRIFT` when set, which is how the simulation harness
    /// sweeps it without an argv change.
    #[arg(long, hide = true, default_value_t = default_max_drift())]
    pub repair_max_drift: f32,

    /// Build and cache an index for this scope on a cold search — the
    /// pre-2026-08-16 default; caching is opt-in now. `GORP_AUTO_INDEX=1`
    /// opts a whole environment in; the `index` verb prewarms once. Hidden
    /// for the same reason the verb is: an agent that sees it opts every
    /// scope back in, which recreates the footprint the flip removed.
    #[arg(long, hide = true)]
    pub index: bool,

    /// Weight of the semantic list in hybrid fusion (BM25 = 1.0)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().sem_weight)]
    pub sem_weight: f32,

    /// Disable MMR diversity reranking (return raw fused order)
    #[arg(long, hide = true)]
    pub no_diversify: bool,

    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity
    #[arg(long, hide = true, default_value_t = SearchOptions::default().mmr_lambda)]
    pub mmr_lambda: f32,

    /// Overlap share of the shorter span at which two same-file chunks count
    /// as near-duplicates (0 = the pre-§24 rule: any shared line)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().dedupe_overlap)]
    pub dedupe_overlap: f32,

    /// Chunk window in lines when the scope is a single file, which also
    /// lifts the candidate cap there (0 = off, use --window)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().file_scope_window)]
    pub file_scope_window: u32,

    /// Prefer chunks that DECLARE a query term over chunks that merely
    /// mention it, weighted (0 = off, experimental RESEARCH.md §24.1)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().decl_boost)]
    pub decl_boost: f32,

    /// Prefer chunks whose file path carries a query term, weighted
    /// (0 = off, experimental RESEARCH.md §35.1)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().path_boost)]
    pub path_boost: f32,

    /// Learned checklist's share of the final relevance
    /// (0 = off, experimental RESEARCH.md §35.2)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().learned_blend)]
    pub learned_blend: f32,

    /// Characters shown per result, grown around the match (0 = the matched
    /// line alone; default 800). A budget in characters rather than lines
    /// because a line of prose is not a line of code — RESEARCH.md §26.4.
    /// Passing it at all opts out of the fine-window display and back into a
    /// chunk-cut passage, which is why the default lives in `SearchOptions`
    /// rather than here: the engine needs to know asked-for from defaulted.
    /// Hidden like the rest of `Tuning` — it was the one member visible in
    /// --help, an accident, not a decision.
    #[arg(long = "passage-chars", hide = true)]
    pub passage_chars: Option<u32>,

    /// Disable the fine rerank: rank and display whole chunks, the pre-§28.2
    /// behavior, byte for byte. The A/B control for eval arms.
    #[arg(long, hide = true)]
    pub no_fine: bool,

    /// Fine sub-window height in lines (RESEARCH.md §28.2)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().fine_lines)]
    pub fine_lines: u32,

    /// Fine vs coarse score blend when ordering hits (1.0 = pure fine)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().fine_blend)]
    pub fine_blend: f32,

    /// The pre-fine top candidate may be outranked but never evicted from
    /// the display (§32.4a's rank-1 kill). The A/B lever for that guard.
    #[arg(long, hide = true)]
    pub keep_coarse_top: bool,

    /// Run BM25 even in semantic mode and guarantee its top-N chunks a
    /// display slot each (0 = off; §32.4a's lexical-drown class)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().bm25_pin)]
    pub bm25_pin: usize,

    /// Bridge-file query expansion: mine up to N terms from the files that
    /// best cover the query's tokens (0 = off; §33)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().bridge_expand)]
    pub bridge_expand: usize,

    /// Weight of a bridge expansion term vs an original token's 1.0 (§33)
    #[arg(long, hide = true, default_value_t = SearchOptions::default().bridge_weight)]
    pub bridge_weight: f32,

    /// Refuse to answer when the best candidate scores below this floor:
    /// zero hits, exit 1, and a stderr line saying so (RESEARCH.md §28.2).
    /// 0 = off. Default stays 0 until the floor is calibrated on replayed
    /// real agent queries — a guessed threshold that cries wolf teaches
    /// agents to ignore the one signal this exists to send.
    #[arg(long, hide = true, default_value_t = SearchOptions::default().min_score)]
    pub min_score: f32,

    /// Lines shown per result instead of a character budget (0 = use
    /// --passage-chars). Kept so §26's campaign arms reproduce
    #[arg(long = "passage-lines", hide = true, default_value_t = SearchOptions::default().passage_lines)]
    pub passage_lines: u32,

    /// Show the whole chunk for each result — an alias for a passage wider
    /// than any chunk, kept because RESEARCH.md §25's arms were run with it
    #[arg(long, hide = true)]
    pub full: bool,

    /// Prefix each hit with its span and the names it declares
    /// (experimental, RESEARCH.md §25.1)
    #[arg(long, hide = true)]
    pub headers: bool,

    /// Disable the unit view and restore the bare fine-window passage in
    /// `path:line:text` form — the pre-§34 display, byte for byte. The A/B
    /// control for display arms (RESEARCH.md §34.3).
    #[arg(long, hide = true)]
    pub no_unit: bool,

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

    /// Rerank AFTER fusion instead of before (experimental, §13.11)
    #[arg(long, hide = true)]
    pub maxsim_post: bool,

    /// MaxSim vs original-order blend within the head (1.0 = pure MaxSim)
    #[arg(long, hide = true, default_value_t = 1.0)]
    pub maxsim_blend: f32,

    /// Prose-render text before embedding, cold path and write-through build
    /// (RESEARCH.md §14.2, §20). The warm path always follows the index's own
    /// meta.json instead. Harness use: like --sif, isolate GORP_CACHE_DIR.
    #[arg(long, hide = true, default_value = "none")]
    pub embed_preproc: String,

    /// How the path line of a chunk is rendered before embedding:
    /// full | dedupe | tail | scaled (RESEARCH.md §20.1)
    #[arg(long, hide = true, default_value = "full")]
    pub chunk_path: String,

    /// Cut chunks to this many non-whitespace characters instead of --window
    /// lines (RESEARCH.md §20.2). 0 = off, use lines.
    #[arg(long, hide = true, default_value_t = 0)]
    pub chunk_budget: u32,

    /// Chunking mode: window (default) | function — cut at definition
    /// boundaries via tree-sitter, leading comments attached, gaps
    /// window-cut (RESEARCH.md §11, §29). Streaming path; indexed scopes
    /// key their cache entries by it.
    #[arg(long, hide = true, default_value = "window")]
    pub chunking: String,

    /// Max chunk span in lines under --chunking function; an over-cap
    /// definition recurses and window-splits
    #[arg(long, hide = true, default_value_t = gorp_core::FUNC_CAP_DEFAULT)]
    pub chunk_cap: u32,
}

/// The resolved passage request. Four flags express it (`--full`,
/// `--passage-lines`, `--passage-chars`, and their absence) and all four keep
/// parsing so RESEARCH.md's arms reproduce — but their precedence lives here,
/// once, instead of being hand-resolved expression by expression in
/// `options()`.
pub struct PassageShape {
    /// Line budget; `u32::MAX` under `--full`, 0 means "use `chars`".
    pub lines: u32,
    /// Character budget, defaulted from the engine when not passed.
    pub chars: u32,
    /// Whether any passage flag was passed at all. The engine needs asked-for
    /// distinct from defaulted: an explicit passage request opts out of the
    /// fine-window/unit display (`passage_override`).
    pub explicit: bool,
}

impl Tuning {
    /// Precedence: `--full` beats `--passage-lines` beats `--passage-chars` —
    /// coarsest request wins, and a line budget bypasses the character one.
    pub fn passage_shape(&self) -> PassageShape {
        PassageShape {
            lines: if self.full { u32::MAX } else { self.passage_lines },
            chars: self.passage_chars.unwrap_or(SearchOptions::default().passage_chars),
            explicit: self.passage_chars.is_some() || self.passage_lines > 0 || self.full,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn tuning(args: &[&str]) -> Tuning {
        Cli::parse_from([&["gorp", "q"], args].concat()).tuning
    }

    /// The precedence table `passage_shape` promises: full > lines > chars,
    /// and `explicit` exactly when any of the three was passed.
    #[test]
    fn passage_shape_resolves_the_precedence_in_one_place() {
        let none = tuning(&[]).passage_shape();
        assert_eq!(none.lines, 0);
        assert_eq!(none.chars, SearchOptions::default().passage_chars);
        assert!(!none.explicit);

        let chars = tuning(&["--passage-chars", "100"]).passage_shape();
        assert_eq!((chars.lines, chars.chars, chars.explicit), (0, 100, true));

        let lines = tuning(&["--passage-lines", "6", "--passage-chars", "100"]).passage_shape();
        assert_eq!((lines.lines, lines.chars, lines.explicit), (6, 100, true));

        let full = tuning(&["--full", "--passage-lines", "6"]).passage_shape();
        assert_eq!((full.lines, full.explicit), (u32::MAX, true));
    }
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Build or refresh the .gorp index for a directory
    ///
    /// Hidden, deliberately. Searching never requires it — a cold search
    /// streams and answers on its own. Since 2026-08-16 caching is opt-in
    /// (it used to build itself on the first ranked search of a scope), and
    /// this verb is the explicit opt-in: prewarm a scope once, or set
    /// `GORP_AUTO_INDEX=1` for a whole environment. Still hidden from
    /// `--help`: agents read `--help` only when stuck (27 of 27 probes
    /// followed three or more empty searches), found a verb described as
    /// "build the index", and concluded searching requires a build step. One
    /// did exactly that mid-run (RESEARCH.md §17).
    #[command(hide = true)]
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
        /// Weight pooling by BM25-style idf over document frequency instead
        /// of a/(a+p) (experimental, RESEARCH.md §14.7)
        #[arg(long, hide = true, requires = "sif")]
        sif_idf: bool,
        /// Prose-render chunk text before embedding: none | split |
        /// split-whole | split-nokw | prune-kw | prune-lex | prune-decl |
        /// prune-soft | prune-uniq (experimental, RESEARCH.md §14.2, §20)
        #[arg(long, hide = true, default_value = "none")]
        embed_preproc: String,
        /// How the chunk's path line is rendered: full | dedupe | tail |
        /// scaled (experimental, RESEARCH.md §20.1)
        #[arg(long, hide = true, default_value = "full")]
        chunk_path: String,
        /// Cut chunks to this many non-whitespace characters instead of
        /// --window lines (experimental, RESEARCH.md §20.2). 0 = off.
        #[arg(long, hide = true, default_value_t = 0)]
        chunk_budget: u32,
        /// Report index freshness instead of rebuilding
        #[arg(long)]
        status: bool,
        #[arg(long, default_value_t = ChunkParams::default().window)]
        window: u32,
        #[arg(long, default_value_t = ChunkParams::default().overlap)]
        overlap: u32,
        /// Chunking mode: window (default) | function (RESEARCH.md §11, §29)
        #[arg(long, default_value = "window")]
        chunking: String,
        /// Max chunk span in lines under --chunking function
        #[arg(long, default_value_t = gorp_core::FUNC_CAP_DEFAULT)]
        chunk_cap: u32,
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
