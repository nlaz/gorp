//! `gorp index` — build a repo-local index, or report its freshness.

use crate::telemetry;
use anyhow::Result;
use gorp_core::ChunkParams;
use gorp_core::store::{self, BuildOptions};
use std::path::{Path, PathBuf};

/// The subcommand's flags, so `dispatch` does not thread eight positionals.
pub struct Args {
    pub path: Option<PathBuf>,
    pub hnsw: bool,
    pub sif: bool,
    pub sif_a: f64,
    pub sif_center: bool,
    pub sif_idf: bool,
    pub embed_preproc: String,
    pub chunk_path: String,
    pub chunk_budget: u32,
    pub chunking: String,
    pub chunk_cap: u32,
    pub status: bool,
    pub window: u32,
    pub overlap: u32,
    pub stats_json: bool,
}

/// Parse `--embed-preproc`. Shared with `cmd::search` so the two verbs cannot
/// drift into accepting different spellings of the same experiment.
pub fn parse_preproc(s: &str) -> Result<gorp_core::text::EmbedPreproc> {
    gorp_core::text::EmbedPreproc::parse(s).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown --embed-preproc {s:?} ({})",
            gorp_core::text::EmbedPreproc::ALL.join("|")
        )
    })
}

/// Parse `--chunk-path`.
pub fn parse_path_render(s: &str) -> Result<gorp_core::text::PathRender> {
    use gorp_core::text::PathRender;
    match s {
        "full" => Ok(PathRender::Full),
        "dedupe" => Ok(PathRender::Dedupe),
        "tail" => Ok(PathRender::Tail),
        "scaled" => Ok(PathRender::Scaled),
        other => anyhow::bail!("unknown --chunk-path {other:?} (full|dedupe|tail|scaled)"),
    }
}

/// `--chunk-budget 0` means "use lines", which is what `None` is.
pub fn budget(n: u32) -> Option<u32> {
    (n > 0).then_some(n)
}

/// Parse `--chunking`. Shared with `cmd::search` for the same no-drift reason
/// as `parse_preproc`. A build without grammars rejects `function` loudly —
/// silently window-cutting under an `f`-tagged entry would poison the cache
/// for every build that does have them.
pub fn chunking(mode: &str, cap: u32) -> Result<Option<u32>> {
    match mode {
        "window" => Ok(None),
        "function" => {
            #[cfg(feature = "func-chunk")]
            {
                Ok(Some(cap.max(1)))
            }
            #[cfg(not(feature = "func-chunk"))]
            {
                let _ = cap;
                anyhow::bail!(
                    "--chunking function is not in this build (compiled without the \
                     func-chunk feature)"
                )
            }
        }
        other => anyhow::bail!("unknown --chunking {other:?} (window|function)"),
    }
}

pub fn run(args: Args) -> Result<i32> {
    let root = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    if args.status {
        return status(&root);
    }
    let embed_preproc = parse_preproc(&args.embed_preproc)?;
    let path_render = parse_path_render(&args.chunk_path)?;
    let opts = BuildOptions {
        params: ChunkParams {
            window: args.window,
            overlap: args.overlap,
            budget: budget(args.chunk_budget),
            function: chunking(&args.chunking, args.chunk_cap)?,
            ..Default::default()
        },
        hnsw: args.hnsw,
        sif: args.sif,
        sif_a: args.sif_a,
        sif_center: args.sif_center,
        sif_idf: args.sif_idf,
        embed_preproc,
        path_render,
    };
    // A build killed mid-swap leaves `.gorp.building-<pid>` / `.gorp.trash-<pid>`
    // siblings that nothing else ever revisits (the cache's sweeps only cover
    // ~/.cache/gorp). An explicit build is the natural moment to reclaim them.
    let swept = store::sweep_transients(&store::index_dir(&root));
    if swept > 0 {
        eprintln!(
            "{}: removed {swept} leftover build director{} from an interrupted index",
            crate::out::PROG,
            if swept == 1 { "y" } else { "ies" }
        );
    }
    // `\r`-rewriting progress is for terminals; under a captured stderr it is
    // hundreds of glued-together lines. The final summary always prints.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let stats = store::build(&root, &opts, |done, total| {
        // Every 500 files: often enough to look alive on a big corpus, rare
        // enough not to spend the build writing to a terminal.
        if tty && (done % 500 == 0 || done == total) {
            eprint!("\rindexing {done}/{total} files");
        }
    })?;
    eprintln!(
        "{}indexed {} files, {} chunks ({:.1} MB source) -> {} in {:.1}s ({:.1} MB)",
        if tty { "\r" } else { "" },
        stats.n_files,
        stats.n_chunks,
        stats.bytes_indexed as f64 / 1e6,
        store::index_dir(&root).display(),
        stats.total_ms / 1e3,
        stats.index_bytes as f64 / 1e6,
    );
    telemetry::emit(
        &telemetry::index_envelope(&root, &opts, &stats, crate::EXIT_FOUND),
        args.stats_json,
    );
    Ok(crate::EXIT_FOUND)
}

/// Freshness, not a rebuild. Exits 1 when anything drifted, so a script can gate
/// on `gorp index --status` the way it would on `git diff --quiet`.
fn status(root: &Path) -> Result<i32> {
    if !store::exists(root) {
        println!("no index (run `gorp index {}`)", root.display());
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
