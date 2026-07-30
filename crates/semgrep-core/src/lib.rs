//! semgrep-core: keyword, BM25, and semantic search over file trees.
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

pub const EMBED_DIM: usize = ese::DIMENSIONS;

/// Chunking parameters shared by index build and cold search.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ChunkParams {
    /// Window size in lines.
    pub window: u32,
    /// Overlap in lines between consecutive windows.
    pub overlap: u32,
    /// Files larger than this are skipped.
    pub max_file_bytes: u64,
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self { window: 32, overlap: 8, max_file_bytes: 4 * 1024 * 1024 }
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
