//! gorp-core: keyword, BM25, and semantic search over file trees.
//!
//! Two ranked paths, one contract. **Cold**: a single streaming pass over the
//! corpus (chunk → tokenize → embed), keeping only postings and a top-k heap.
//! **Warm**: an index answers — a directory of BM25 postings, a quantized
//! embedding matrix mmap'd at query time, and optionally an HNSW graph. The
//! index is a cache, so the cold path also writes one on its way through, and
//! a warm answer is repaired against the live tree before it is served.

// Layers, bottom up. Each may call downward and not upward: `rank` never
// touches the filesystem, `store` never ranks, `cache` never scores, `search`
// orchestrates rather than computes. `keyword` is the exact-match escape hatch
// and stands apart from all of it.
pub mod cache;
pub mod corpus;
pub mod keyword;
pub mod rank;
pub mod search;
pub mod store;
pub mod text;

// A leaf every layer may use. It lived under `search/` and was reached upward
// into from `cache::repair` and needed downward by `store::build` — which is
// what a leaf module is for.
pub mod trace;

pub const EMBED_DIM: usize = ese::DIMENSIONS;

/// Default max chunk span for function-boundary chunking, in lines
/// (RESEARCH.md §11.1: 89% of functions are ≤32 lines; ≤96 covers the tail).
/// Lives here rather than in the feature-gated module so the CLI's flag
/// default exists — and cannot drift from the engine's — in every build.
pub const FUNC_CAP_DEFAULT: u32 = 96;

/// Chunking parameters shared by index build and cold search.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ChunkParams {
    /// Window size in lines.
    pub window: u32,
    /// Overlap in lines between consecutive windows.
    pub overlap: u32,
    /// Files larger than this are skipped.
    pub max_file_bytes: u64,
    /// Non-whitespace characters per chunk. When set, windows are cut to this
    /// budget instead of [`window`](Self::window) lines (RESEARCH.md §20.2).
    ///
    /// A line is not a unit of content: measured over the benchmark corpora, a
    /// 32-line window holds a median 931 non-whitespace characters in vscode
    /// against 693 in linux, and within vscode the p10→p90 spread is 2.5× with
    /// a worst case of 6,767 — one chunk seven times the median, pooled into
    /// one vector by a uniform mean. Budgeting by content equalizes both. The
    /// unit is cAST's (arXiv 2506.15655), chosen for the same reason.
    ///
    /// Chunks stay line-aligned regardless: [`Chunk`] addresses lines, and
    /// `corpus::lines` re-reads by line number.
    #[serde(default)]
    pub budget: Option<u32>,
    /// Function-boundary chunking with this max chunk span in lines
    /// (RESEARCH.md §11, §29). `None` = line windows; wins over
    /// [`budget`](Self::budget) when both are set.
    ///
    /// §11.1 measured why: a 32-line window rarely cuts a function in half
    /// (11%) but *swallows ~3 whole functions* — the defect is dilution, not
    /// truncation. When set, supported languages cut one chunk per
    /// definition (leading comment attached, §11.2's Rule B), gaps between
    /// definitions fall to `window`-sized cuts, and anything unparseable
    /// falls back to line windows. Not feature-gated in serde: every binary
    /// must round-trip a meta.json that carries it, grammars or not.
    #[serde(default)]
    pub function: Option<u32>,
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self {
            window: 32,
            overlap: 8,
            max_file_bytes: 4 * 1024 * 1024,
            budget: None,
            function: None,
        }
    }
}

impl ChunkParams {
    /// The same chunking at a different window, with the overlap held at the
    /// same *share* of the window rather than the same line count.
    ///
    /// Overlap exists so a match is not split across a boundary, which is a
    /// property of the window it sits in; carrying 8 lines onto a 12-line
    /// window would overlap by two thirds and trip the near-duplicate rule in
    /// `hit::finalize` on every neighbour. Clears `budget` and `function`,
    /// which measure the same thing in different units and would otherwise
    /// silently win.
    pub fn with_window(self, window: u32) -> Self {
        let share = if self.window == 0 { 0.0 } else { self.overlap as f32 / self.window as f32 };
        Self {
            window,
            overlap: (window as f32 * share).round() as u32,
            budget: None,
            function: None,
            ..self
        }
    }
}

/// A chunk is a line window into one file. Text is never stored — it is
/// re-read from the file when a chunk surfaces as a result.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Chunk {
    pub file_id: u32,
    /// 1-based, inclusive.
    pub start_line: u32,
    /// 1-based, inclusive.
    pub end_line: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FileMeta {
    /// Path relative to the corpus root, `/`-separated.
    pub path: String,
    pub size: u64,
    /// mtime as seconds since epoch (0 if unavailable).
    pub mtime: u64,
}
