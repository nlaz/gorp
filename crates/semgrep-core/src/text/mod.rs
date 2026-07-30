//! Text into representations: tokens for the lexical engine, vectors for the
//! semantic one.
//!
//! Everything here is a pure function of a string (plus, for SIF, corpus
//! statistics). Nothing here ranks, reads files, or touches an index.

mod embed;
mod sif;
pub mod token;

pub use embed::{SemgrepHnsw, embed_query, new_hnsw};
pub use sif::{SIF_A, SifStats, embed_sif, token_vectors};
