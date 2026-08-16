//! Exact top-k selection over the quantized embedding matrix, and the heap
//! that the streaming path accumulates into.

use super::vec::dot_distance_i8;
use crate::EMBED_DIM;
use rayon::prelude::*;

/// Exact top-k over an i8-quantized embedding matrix (`n × EMBED_DIM` bytes,
/// mmap'd `emb.bin` in index format v2). Returns (chunk_id, distance) ascending.
pub fn brute_force_top_k_i8(query: &[i8], matrix: &[i8], k: usize) -> Vec<(u32, f32)> {
    brute_force_top_k_i8_where(query, matrix, k, None)
}

/// [`brute_force_top_k_i8`] over a subset of rows.
///
/// `allow` decides membership *before* the top-k heap sees a row, which is the
/// whole point: filtering the result afterwards can only return what a
/// corpus-wide top-k happened to include, and for a narrow scope that is
/// routinely nothing (SIMULATION.md §1.7). Skipping also makes a scoped query
/// faster — the dot product is never computed for a row that cannot be returned.
pub fn brute_force_top_k_i8_where(
    query: &[i8],
    matrix: &[i8],
    k: usize,
    allow: Option<&(dyn Fn(u32) -> bool + Sync)>,
) -> Vec<(u32, f32)> {
    let n = matrix.len() / EMBED_DIM;
    if n == 0 || k == 0 {
        return Vec::new();
    }
    let chunk_rows = 32 * 1024;
    let mut best: Vec<(u32, f32)> = (0..n.div_ceil(chunk_rows))
        .into_par_iter()
        .map(|block| {
            let start = block * chunk_rows;
            let end = (start + chunk_rows).min(n);
            let mut local = TopK::new(k);
            for row in start..end {
                if allow.is_some_and(|f| !f(row as u32)) {
                    continue;
                }
                let v = &matrix[row * EMBED_DIM..(row + 1) * EMBED_DIM];
                local.push(row as u32, dot_distance_i8(query, v));
            }
            local.into_sorted()
        })
        .reduce(Vec::new, |mut a, b| {
            a.extend(b);
            a
        });
    best.sort_unstable_by(|x, y| x.1.total_cmp(&y.1).then(x.0.cmp(&y.0)));
    best.truncate(k);
    best
}

/// Incremental top-k accumulator for streaming (unindexed) search: feed
/// (id, distance) pairs as chunks are embedded, keep only k best.
pub struct TopK {
    k: usize,
    // max-heap by distance so the worst kept item is on top
    heap: std::collections::BinaryHeap<HeapItem>,
}

struct HeapItem(f32, u32);
impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1 == other.1
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

impl TopK {
    pub fn new(k: usize) -> Self {
        Self { k, heap: std::collections::BinaryHeap::with_capacity(k + 1) }
    }

    #[inline]
    pub fn push(&mut self, id: u32, dist: f32) {
        if self.heap.len() < self.k {
            self.heap.push(HeapItem(dist, id));
        } else if let Some(worst) = self.heap.peek()
            && dist < worst.0
        {
            self.heap.pop();
            self.heap.push(HeapItem(dist, id));
        }
    }

    /// (id, distance) ascending by distance.
    pub fn into_sorted(self) -> Vec<(u32, f32)> {
        let mut v: Vec<(u32, f32)> =
            self.heap.into_iter().map(|HeapItem(d, i)| (i, d)).collect();
        v.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        v
    }
}
