//! The row space a warm query ranks over: the index, plus the repair overlay,
//! addressed as one list.
//!
//! Base rows are chunks of the index; delta rows are chunks of files the index
//! does not know correctly, indexed in memory by `cache::repair`. Ranking sees
//! one id space — `[0, n_base)` is base, `[n_base, len)` is delta — and every
//! comparison that depends on that split lives here.
//!
//! It used to live as `id < n_base` and `id - n_base` written out at six places
//! inside one 290-line function, alongside two closures that captured the
//! overlay. That arithmetic is the most delicate invariant in the warm path: get
//! it wrong and a query returns another file's text with a straight face. Here
//! it is stated once and can be tested directly.

use crate::cache::repair::Repair;
use crate::rank::bm25::Bm25Index;
use crate::store::LoadedIndex;
use crate::{Chunk, EMBED_DIM};

/// Is `path` inside `prefix`? The same test `search::indexed::candidates` and
/// `corpus::diff` apply, which is why a scope of `src` never matches `srcgen/`.
fn under(path: &str, prefix: &str) -> bool {
    prefix.is_empty() || path.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
}

pub struct Rows<'a> {
    idx: &'a LoadedIndex,
    repair: Option<Repair>,
    /// Where delta rows begin. The only place this boundary is named.
    n_base: u32,
    /// Rows inside the query's subtree, or `None` when the query covers the
    /// whole corpus and every row qualifies.
    in_scope: Option<Vec<bool>>,
}

impl<'a> Rows<'a> {
    /// `prefix` is the query scope relative to the index root; `""` is the whole
    /// corpus.
    ///
    /// The scope is resolved here, once, and applied *before* ranking truncates.
    /// It used to be applied after: the fused list was cut to 256 rows and only
    /// then filtered to the subtree, so a scope holding none of the corpus-wide
    /// top 256 got nothing at all. On tokio, `.github` and `docs` each returned
    /// **zero** hits out of 8,042 indexed chunks (SIMULATION.md §1.7). Filtering
    /// first also makes a narrow scope cheaper, since the vector scan skips
    /// every row it can never return.
    pub fn new(idx: &'a LoadedIndex, repair: Option<Repair>, prefix: &str) -> Self {
        let n_base = idx.chunks.len() as u32;
        let mut rows = Self { idx, repair, n_base, in_scope: None };
        if !prefix.is_empty() {
            let mask =
                (0..rows.len() as u32).map(|id| under(rows.path_of(id), prefix)).collect();
            rows.in_scope = Some(mask);
        }
        rows
    }

    /// A row's path, borrowed. `chunk` clones it; building the scope mask over
    /// half a million rows must not.
    fn path_of(&self, id: u32) -> &str {
        match self.delta_index(id) {
            None => &self.idx.file(&self.idx.chunks[id as usize]).path,
            Some(j) => {
                &self.repair.as_ref().expect("a delta id implies an overlay").delta.paths[j]
            }
        }
    }

    /// True when the query covers the whole index, so nothing is filtered.
    ///
    /// The semantic path asks because the HNSW graph returns its own bounded
    /// list and cannot be told to skip rows: under a scope it would reintroduce
    /// exactly the starvation the mask exists to prevent, so a scoped query
    /// brute-forces instead. That is not a loss — the mask makes the scan skip
    /// most of the matrix anyway.
    pub fn whole_corpus(&self) -> bool {
        self.in_scope.is_none()
    }

    /// Whether a row may be returned at all: its file still exists as indexed,
    /// **and** it lies inside the query's scope. One predicate, because the two
    /// reasons a row cannot be served should never be applied at different
    /// points in the pipeline again.
    pub fn serves(&self, id: u32) -> bool {
        self.live(id) && self.in_scope.as_ref().is_none_or(|m| m[id as usize])
    }

    /// Total rows a query may rank over.
    pub fn len(&self) -> usize {
        self.idx.chunks.len() + self.n_delta()
    }

    pub fn n_delta(&self) -> usize {
        self.repair.as_ref().map_or(0, |r| r.delta.chunks.len())
    }

    /// Drifted files this query had to work around, for the report.
    pub fn n_dirty(&self) -> usize {
        self.repair.as_ref().map_or(0, |r| r.n_dirty)
    }

    /// The chunk a row points at, and its index-root-relative path.
    pub fn chunk(&self, id: u32) -> (Chunk, String) {
        match self.delta_index(id) {
            None => {
                let c = self.idx.chunks[id as usize];
                (c, self.idx.file(&c).path.clone())
            }
            Some(j) => {
                let r = self.repair.as_ref().expect("a delta id implies an overlay");
                (r.delta.chunks[j], r.delta.paths[j].clone())
            }
        }
    }

    /// False when a base row's file has changed or vanished, so its indexed text
    /// no longer describes anything on disk and must not be served. Delta rows
    /// are always live — they were just read.
    pub fn live(&self, id: u32) -> bool {
        match (self.delta_index(id), &self.repair) {
            (Some(_), _) | (_, None) => true,
            (None, Some(r)) => !r.tombstones.contains(&self.idx.chunks[id as usize].file_id),
        }
    }

    /// A row's embedding. Base rows are dequantized from the mmap'd matrix on
    /// the fly, which is free at candidate scale; delta rows are already f32.
    pub fn vector(&self, id: u32) -> Option<Vec<f32>> {
        match self.delta_index(id) {
            Some(j) => Some(crate::rank::dequantize_i8(&self.repair.as_ref()?.delta.vecs[j])),
            None => {
                let row = id as usize;
                let m = self.idx.emb_matrix_i8();
                Some(crate::rank::dequantize_i8(&m[row * EMBED_DIM..(row + 1) * EMBED_DIM]))
            }
        }
    }

    /// The overlay's own lexical index, when there is one.
    pub fn delta_bm25(&self) -> Option<&Bm25Index> {
        self.repair.as_ref().map(|r| &r.delta.bm25)
    }

    /// The overlay's quantized embeddings, in delta-row order.
    pub fn delta_vectors(&self) -> &[Vec<i8>] {
        self.repair.as_ref().map_or(&[], |r| &r.delta.vecs)
    }

    /// Fold the overlay into a ranked base list: drop rows whose files drifted,
    /// append the overlay's own ranking with its ids translated into this space,
    /// then re-rank and truncate.
    ///
    /// `higher_is_better` because BM25 scores rise with relevance while vector
    /// distances fall; nothing else about the merge differs. The overlay's BM25
    /// idf comes from the delta alone rather than the whole corpus, which is an
    /// approximation that holds while the overlay stays small.
    pub fn merge(
        &self,
        base: Vec<(u32, f32)>,
        delta: Vec<(u32, f32)>,
        pool: usize,
        higher_is_better: bool,
    ) -> Vec<(u32, f32)> {
        let mut out: Vec<(u32, f32)> =
            base.into_iter().filter(|&(id, _)| self.serves(id)).collect();
        if self.repair.is_some() {
            out.extend(
                delta
                    .into_iter()
                    .map(|(j, score)| (self.n_base + j, score))
                    .filter(|&(id, _)| self.serves(id)),
            );
            if higher_is_better {
                out.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            } else {
                out.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            }
            out.truncate(pool);
        }
        out
    }

    /// `Some(delta row)` for an overlay id, `None` for a base id.
    fn delta_index(&self, id: u32) -> Option<usize> {
        (id >= self.n_base).then(|| (id - self.n_base) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::repair::{Delta, Repair};
    use crate::store::{BuildOptions, LoadNeeds, LoadedIndex, build};
    use crate::{Chunk, ChunkParams};
    use std::collections::HashSet;

    /// A two-file corpus whose chunks are easy to tell apart by content.
    fn indexed_fixture() -> (tempfile::TempDir, LoadedIndex) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn gamma() {}\nfn delta() {}\n").unwrap();
        let params = ChunkParams { window: 8, overlap: 2, ..Default::default() };
        build(dir.path(), &BuildOptions { params, ..Default::default() }, |_, _| {}).unwrap();
        let idx = LoadedIndex::load(dir.path(), LoadNeeds { bm25: true, hnsw: false }).unwrap();
        (dir, idx)
    }

    fn overlay(n_chunks: usize, tombstones: HashSet<u32>) -> Repair {
        Repair {
            tombstones,
            delta: Delta {
                chunks: (0..n_chunks)
                    .map(|i| Chunk {
                        file_id: 0,
                        start_line: i as u32 + 1,
                        end_line: i as u32 + 1,
                    })
                    .collect(),
                paths: (0..n_chunks).map(|i| format!("delta{i}.rs")).collect(),
                // Quantized, as the overlay now stores them: 64 is 0.5 in i8 scale.
                vecs: (0..n_chunks).map(|_| vec![64i8; crate::EMBED_DIM]).collect(),
                bm25: crate::rank::bm25::Bm25Index::new(),
            },
            n_dirty: n_chunks,
        }
    }

    #[test]
    fn without_an_overlay_every_row_is_a_live_base_row() {
        let (_dir, idx) = indexed_fixture();
        let n = idx.chunks.len();
        let rows = Rows::new(&idx, None, "");
        assert_eq!(rows.len(), n);
        assert_eq!(rows.n_delta(), 0);
        assert_eq!(rows.n_dirty(), 0);
        for id in 0..n as u32 {
            assert!(rows.live(id), "row {id} should be live with no repair");
            assert!(rows.vector(id).is_some(), "row {id} should have a vector");
        }
    }

    /// The boundary itself: ids below `n_base` resolve through the index, ids at
    /// or above it resolve through the overlay. Getting this off by one serves
    /// another file's text under the right-looking path.
    #[test]
    fn the_base_delta_boundary_routes_each_id_to_the_right_source() {
        let (_dir, idx) = indexed_fixture();
        let n_base = idx.chunks.len() as u32;
        let rows = Rows::new(&idx, Some(overlay(3, HashSet::new())), "");

        assert_eq!(rows.len() as u32, n_base + 3);
        assert_eq!(rows.n_delta(), 3);

        // Last base row comes from the index...
        let (_, path) = rows.chunk(n_base - 1);
        assert!(path.ends_with(".rs") && !path.starts_with("delta"), "got {path}");
        // ...and the first delta row comes from the overlay.
        assert_eq!(rows.chunk(n_base).1, "delta0.rs");
        assert_eq!(rows.chunk(n_base + 2).1, "delta2.rs");
    }

    #[test]
    fn tombstones_kill_base_rows_and_never_delta_rows() {
        let (_dir, idx) = indexed_fixture();
        let n_base = idx.chunks.len() as u32;
        // file_id 0 is a.rs; tombstone it.
        let rows = Rows::new(&idx, Some(overlay(2, HashSet::from([0]))), "");

        let dead: Vec<u32> = (0..n_base).filter(|&id| !rows.live(id)).collect();
        assert!(!dead.is_empty(), "tombstoning file 0 should kill its chunks");
        for id in &dead {
            assert_eq!(idx.chunks[*id as usize].file_id, 0);
        }
        // Delta rows were just read off disk, so they are live by construction.
        for j in 0..2 {
            assert!(rows.live(n_base + j), "delta row {j} must be live");
        }
    }

    #[test]
    fn vectors_come_from_the_matrix_or_the_overlay() {
        let (_dir, idx) = indexed_fixture();
        let n_base = idx.chunks.len() as u32;
        let rows = Rows::new(&idx, Some(overlay(1, HashSet::new())), "");

        let base = rows.vector(0).expect("base row has a vector");
        assert_eq!(base.len(), crate::EMBED_DIM);
        // Dequantized from i8, so within one quantization step of unit range.
        assert!(base.iter().all(|v| (-1.01..=1.01).contains(v)), "base vector out of range");

        // Dequantized the same way base rows are, so MMR compares like with like.
        let delta = rows.vector(n_base).expect("delta row has a vector");
        assert_eq!(delta.len(), crate::EMBED_DIM);
        assert!(
            delta.iter().all(|v| (*v - 64.0 / 127.0).abs() < 1e-6),
            "delta vectors must dequantize through the same path as base rows"
        );
    }

    /// `merge` is what folds the overlay into a ranked base list, and it has to
    /// respect both the sort direction and the tombstones.
    #[test]
    fn merge_drops_tombstoned_rows_and_translates_delta_ids() {
        let (_dir, idx) = indexed_fixture();
        let n_base = idx.chunks.len() as u32;
        let rows = Rows::new(&idx, Some(overlay(2, HashSet::from([0]))), "");

        // Every base row, best-first, plus two overlay rows.
        let base: Vec<(u32, f32)> = (0..n_base).map(|id| (id, 10.0 - id as f32)).collect();
        let delta = vec![(0u32, 99.0), (1, 1.0)];
        let merged = rows.merge(base, delta, 100, true);

        // Tombstoned base rows are gone.
        assert!(
            merged.iter().all(|&(id, _)| id >= n_base || rows.live(id)),
            "a tombstoned row survived the merge"
        );
        // Delta ids were translated into the shared space.
        assert!(merged.iter().any(|&(id, s)| id == n_base && s == 99.0));
        assert!(merged.iter().any(|&(id, s)| id == n_base + 1 && s == 1.0));
        // Higher is better, so the overlay's 99.0 leads.
        assert_eq!(merged[0], (n_base, 99.0));
        assert!(merged.windows(2).all(|w| w[0].1 >= w[1].1), "not descending: {merged:?}");
    }

    #[test]
    fn merge_can_sort_ascending_for_distances() {
        let (_dir, idx) = indexed_fixture();
        let n_base = idx.chunks.len() as u32;
        let rows = Rows::new(&idx, Some(overlay(1, HashSet::new())), "");
        let base: Vec<(u32, f32)> = (0..n_base).map(|id| (id, 0.5 + id as f32)).collect();
        let merged = rows.merge(base, vec![(0, 0.01)], 100, false);
        assert_eq!(merged[0], (n_base, 0.01), "the nearest row must lead");
        assert!(merged.windows(2).all(|w| w[0].1 <= w[1].1), "not ascending: {merged:?}");
    }

    #[test]
    fn merge_truncates_to_the_pool() {
        let (_dir, idx) = indexed_fixture();
        let n_base = idx.chunks.len() as u32;
        let rows = Rows::new(&idx, Some(overlay(4, HashSet::new())), "");
        let base: Vec<(u32, f32)> = (0..n_base).map(|id| (id, id as f32)).collect();
        let delta: Vec<(u32, f32)> = (0..4).map(|j| (j, 100.0 + j as f32)).collect();
        assert_eq!(rows.merge(base, delta, 3, true).len(), 3);
    }

    /// With no overlay there is nothing to merge into, so the base list must
    /// pass through untouched — including its order.
    #[test]
    fn merge_without_an_overlay_passes_the_base_list_through() {
        let (_dir, idx) = indexed_fixture();
        let rows = Rows::new(&idx, None, "");
        let base = vec![(2u32, 5.0f32), (0, 9.0), (1, 1.0)];
        assert_eq!(rows.merge(base.clone(), Vec::new(), 100, true), base);
    }
}
