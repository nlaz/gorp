//! `semgrep index` — build a repo-local index, or report its freshness.

use anyhow::Result;
use semgrep_core::ChunkParams;
use semgrep_core::store::{self, BuildOptions};
use std::path::{Path, PathBuf};

/// The subcommand's flags, so `dispatch` does not thread eight positionals.
pub struct Args {
    pub path: Option<PathBuf>,
    pub hnsw: bool,
    pub sif: bool,
    pub sif_a: f64,
    pub sif_center: bool,
    pub status: bool,
    pub window: u32,
    pub overlap: u32,
}

pub fn run(args: Args) -> Result<i32> {
    let root = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    if args.status {
        return status(&root);
    }
    let opts = BuildOptions {
        params: ChunkParams {
            window: args.window,
            overlap: args.overlap,
            ..Default::default()
        },
        hnsw: args.hnsw,
        sif: args.sif,
        sif_a: args.sif_a,
        sif_center: args.sif_center,
    };
    let start = std::time::Instant::now();
    let stats = store::build(&root, &opts, |done, total| {
        // Every 500 files: often enough to look alive on a big corpus, rare
        // enough not to spend the build writing to a terminal.
        if done % 500 == 0 || done == total {
            eprint!("\rindexing {done}/{total} files");
        }
    })?;
    eprintln!(
        "\rindexed {} files, {} chunks ({:.1} MB source) -> {} in {:.1}s ({:.1} MB)",
        stats.n_files,
        stats.n_chunks,
        stats.bytes_indexed as f64 / 1e6,
        store::index_dir(&root).display(),
        start.elapsed().as_secs_f64(),
        stats.index_bytes as f64 / 1e6,
    );
    Ok(crate::EXIT_FOUND)
}

/// Freshness, not a rebuild. Exits 1 when anything drifted, so a script can gate
/// on `semgrep index --status` the way it would on `git diff --quiet`.
fn status(root: &Path) -> Result<i32> {
    if !store::exists(root) {
        println!("no index (run `semgrep index {}`)", root.display());
        return Ok(crate::EXIT_NONE);
    }
    let idx = store::LoadedIndex::load(root, store::LoadNeeds { bm25: false, hnsw: false })?;
    let stale = idx.stale_files()?;
    println!(
        "index: {} files, {} chunks, hnsw={}, stale files: {stale}",
        idx.meta.files.len(),
        idx.meta.n_chunks,
        idx.meta.has_hnsw,
    );
    Ok(if stale == 0 { crate::EXIT_FOUND } else { crate::EXIT_NONE })
}
