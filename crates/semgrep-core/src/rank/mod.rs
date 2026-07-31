//! Query plus representations, into an ordered list of chunk ids.
//!
//! This layer never touches the filesystem and never knows where a
//! representation came from: it scores what it is handed. `bm25` is the lexical
//! engine and its in-memory store, `vec` the vector kernels, `topk` the
//! selection machinery, `maxsim` late-interaction reranking.

pub mod bm25;
pub mod maxsim;
pub mod prf;
mod topk;
mod vec;

pub use bm25::{Rest, top_k, top_k_within};
pub use fuse::{Mode, fuse};
pub use maxsim::{blend_head as maxsim_blend_head, head_size as maxsim_head_size, maxsim};
pub use mmr::mmr_order;
pub use topk::{TopK, brute_force_top_k_i8};
pub use vec::{as_stored, dequantize_i8, distance, dot_distance_i8, normalize, quantize_i8};

pub mod fuse;
pub mod mmr;
