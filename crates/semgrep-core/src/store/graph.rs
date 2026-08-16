//! The file graph (RESEARCH.md §35.3): undirected import edges between corpus
//! files, extracted at build time, serialized as `graph.bin`.
//!
//! CSR adjacency over file ids — `offsets[f]..offsets[f+1]` indexes `targets`.
//! Undirected on purpose: the vocabulary-gap census found gold one hop from a
//! good candidate in *either* direction (the seed imports the answer, or the
//! answer imports the seed), and a directed graph would halve the measured
//! 48–58% reach for no storage saved.
//!
//! Extraction (`build`) needs the tree-sitter grammars and lives behind
//! `func-chunk`; loading and querying do not — a grammarless binary still
//! serves graph expansion from an index that carries one.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
pub struct FileGraph {
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

impl FileGraph {
    pub fn neighbors(&self, file_id: u32) -> &[u32] {
        let f = file_id as usize;
        if f + 1 >= self.offsets.len() {
            return &[];
        }
        &self.targets[self.offsets[f] as usize..self.offsets[f + 1] as usize]
    }

    pub fn n_edges(&self) -> usize {
        self.targets.len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// From per-file neighbor lists (directed), symmetrized and deduped.
    pub(crate) fn from_adjacency(mut adj: Vec<Vec<u32>>) -> Self {
        let n = adj.len();
        // Symmetrize: an edge seen from either end exists from both.
        let directed: Vec<Vec<u32>> = adj.clone();
        for (from, tos) in directed.iter().enumerate() {
            for &to in tos {
                if (to as usize) < n {
                    adj[to as usize].push(from as u32);
                }
            }
        }
        let mut offsets = Vec::with_capacity(n + 1);
        let mut targets = Vec::new();
        offsets.push(0u32);
        for (f, mut tos) in adj.into_iter().enumerate() {
            tos.sort_unstable();
            tos.dedup();
            tos.retain(|&t| t as usize != f); // self-edges say nothing
            targets.extend_from_slice(&tos);
            offsets.push(targets.len() as u32);
        }
        FileGraph { offsets, targets }
    }

    /// Chunk ids belonging to the 1-hop neighbor files of the `seeds`' files,
    /// excluding the seed files' own chunks, capped at `cap`. Relies on the
    /// chunk table being grouped by `file_id` in walk order — the same
    /// invariant the chunk-id lockstep rests on.
    pub fn neighbor_chunks(
        &self,
        chunks: &[crate::Chunk],
        seeds: impl IntoIterator<Item = u32>,
        cap: usize,
    ) -> Vec<u32> {
        let mut seed_files = std::collections::HashSet::new();
        for id in seeds {
            if let Some(c) = chunks.get(id as usize) {
                seed_files.insert(c.file_id);
            }
        }
        let mut nbr_files: Vec<u32> = seed_files
            .iter()
            .flat_map(|&f| self.neighbors(f).iter().copied())
            .filter(|f| !seed_files.contains(f))
            .collect();
        nbr_files.sort_unstable();
        nbr_files.dedup();
        let mut out = Vec::new();
        for f in nbr_files {
            let lo = chunks.partition_point(|c| c.file_id < f);
            let hi = chunks.partition_point(|c| c.file_id <= f);
            for id in lo..hi {
                if out.len() >= cap {
                    return out;
                }
                out.push(id as u32);
            }
        }
        out
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("FileGraph serializes")
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

/// Extract the graph for `files` under `root`: one tree-sitter parse per
/// supported file, imports resolved against the file table, unresolved and
/// ambiguous specs dropped. Its own read pass rather than a hook inside
/// `corpus::pass` — that pass carries the chunk-id lockstep guarantee and
/// stays untouched; the second read is warm in the page cache.
#[cfg(feature = "func-chunk")]
pub(crate) fn build(root: &std::path::Path, files: &[crate::FileMeta]) -> FileGraph {
    use rayon::prelude::*;
    let resolver = crate::corpus::imports::Resolver::new(
        files.iter().enumerate().map(|(i, f)| (i as u32, f.path.clone())),
    );
    let adj: Vec<Vec<u32>> = files
        .par_iter()
        .map(|f| {
            let Ok(text) = std::fs::read_to_string(root.join(&f.path)) else {
                return Vec::new();
            };
            let from_dir = f.path.rsplit_once('/').map_or("", |(d, _)| d);
            let mut out: Vec<u32> = crate::corpus::imports::extract(&f.path, &text)
                .iter()
                .flat_map(|spec| resolver.resolve(from_dir, spec))
                .collect();
            out.sort_unstable();
            out.dedup();
            out
        })
        .collect();
    FileGraph::from_adjacency(adj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_undirected_and_self_edges_drop() {
        // 0 imports 1; 2 imports itself and 0.
        let g = FileGraph::from_adjacency(vec![vec![1], vec![], vec![2, 0]]);
        assert_eq!(g.neighbors(0), &[1, 2]);
        assert_eq!(g.neighbors(1), &[0]);
        assert_eq!(g.neighbors(2), &[0]);
        assert_eq!(g.neighbors(99), &[] as &[u32]);
    }

    #[test]
    fn the_bytes_round_trip() {
        let g = FileGraph::from_adjacency(vec![vec![1], vec![0]]);
        let back = FileGraph::from_bytes(&g.to_bytes()).unwrap();
        assert_eq!(back.neighbors(0), g.neighbors(0));
        assert_eq!(back.neighbors(1), g.neighbors(1));
    }
}
