//! semgrep-core: keyword, BM25, and semantic search over file trees.
//!
//! The engine works in two modes:
//! - **unindexed**: a single streaming pass over the corpus (chunk → tokenize →
//!   embed), retaining only postings and a top-k heap.
//! - **indexed**: a `.semgrep/` directory built by [`index::build`] holding BM25
//!   postings, a raw embedding matrix (mmap'd at query time), and optionally an
//!   anny HNSW graph.

pub mod bm25;
pub mod corpus;
pub mod index;
pub mod keyword;
pub mod search;
pub mod semantic;
pub mod tokenize;

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
