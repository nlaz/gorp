//! The SIF pre-pass: corpus token frequencies, and the common component.
//!
//! SIF pooling weights each token by a/(a + p(w)), so it needs p(w) before any
//! chunk can be embedded — hence a pass over the corpus that only counts. It is
//! cheap next to the embedding pass that follows.

use crate::text::SifStats;
use crate::{EMBED_DIM, FileMeta, corpus};
use rayon::prelude::*;
use std::path::Path;

/// Files sampled to estimate the common component, and the cap on that sample.
const SAMPLE_LIMIT: usize = 512;

/// Count token frequencies over `files`, then estimate the common component if
/// `opts.sif_center` asked for it.
pub fn count(root: &Path, files: &[FileMeta], opts: &super::BuildOptions) -> SifStats {
    let mut stats = SifStats::default();
    for (_, batch) in corpus::pass_batches(files, 256, 16 << 20) {
        let partials: Vec<SifStats> = batch
            .par_iter()
            .map(|fm| {
                let mut s = SifStats::default();
                if let Some(text) = corpus::read_text(&corpus::abs_path(root, fm)) {
                    s.count(&text);
                }
                s
            })
            .collect();
        for p in partials {
            stats.merge_counts(p);
        }
    }
    // After the merge, never before: merge_counts deliberately ignores `a`, so
    // setting it earlier would be silently discarded.
    stats.a = opts.sif_a;

    if opts.sif_center {
        stats.mean = common_component(root, files, opts, &stats);
    }
    stats
}

/// The direction every pooled vector in this corpus shares.
///
/// SIF's second half: that shared direction carries no discriminative signal, so
/// subtracting it from every vector — chunks and queries alike, via `embed_sif`
/// — is what makes the weighting pay off. Estimated from a strided sample rather
/// than the whole corpus, because the mean converges long before the sample does.
fn common_component(
    root: &Path,
    files: &[FileMeta],
    opts: &super::BuildOptions,
    stats: &SifStats,
) -> Option<Vec<f32>> {
    if files.is_empty() {
        return None;
    }
    let stride = (files.len() / SAMPLE_LIMIT).max(1);
    let sampled: Vec<&FileMeta> = files.iter().step_by(stride).take(SAMPLE_LIMIT).collect();

    // One chunk per sampled file, pooled with the frequencies just counted.
    let vectors: Vec<[f32; EMBED_DIM]> = sampled
        .par_iter()
        .filter_map(|fm| {
            let text = corpus::read_text(&corpus::abs_path(root, fm))?;
            let (_, first) = corpus::chunk_lines(0, &text, &opts.params).into_iter().next()?;
            Some(crate::text::embed_sif(&corpus::doc_text(&fm.path, first), stats))
        })
        .collect();
    if vectors.is_empty() {
        return None;
    }

    let mut mean = vec![0.0f32; EMBED_DIM];
    for v in &vectors {
        for (m, x) in mean.iter_mut().zip(v.iter()) {
            *m += x;
        }
    }
    for m in mean.iter_mut() {
        *m /= vectors.len() as f32;
    }
    Some(mean)
}
