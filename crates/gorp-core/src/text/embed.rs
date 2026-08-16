//! Whole-string embedding, and the HNSW instantiation those vectors go into.

use crate::EMBED_DIM;
use anny::hnsw::Hnsw;
use anny::metric::Cosine;

/// HNSW instantiation used for `.gorp/hnsw.bin`.
/// M0=32 (M=16), compile-time top-K=128, EF_SEARCH=192, EF_BUILD=128.
pub type GorpHnsw = Hnsw<f32, Cosine, EMBED_DIM, 32, 128, 192, 128, 16>;

pub fn new_hnsw() -> GorpHnsw {
    GorpHnsw::new(Cosine, 0xB06)
}

pub fn embed_query(query: &str) -> [f32; EMBED_DIM] {
    ese::encode_single(query)
}
