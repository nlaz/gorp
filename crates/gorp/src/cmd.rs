//! The verbs. One module each, and a dispatch that only routes.

mod cache;
mod index;
mod search;

use crate::cli::{Cli, Cmd};
use anyhow::Result;

pub fn dispatch(cli: Cli) -> Result<i32> {
    match &cli.cmd {
        Some(Cmd::Cache { prune, clear }) => cache::run(*prune, *clear),
        Some(Cmd::Index {
            path,
            hnsw,
            sif,
            sif_a,
            sif_center,
            sif_idf,
            embed_preproc,
            chunk_path,
            chunk_budget,
            status,
            window,
            overlap,
            chunking,
            chunk_cap,
        }) => index::run(index::Args {
            path: path.clone(),
            hnsw: *hnsw,
            sif: *sif,
            sif_a: *sif_a,
            sif_center: *sif_center,
            sif_idf: *sif_idf,
            embed_preproc: embed_preproc.clone(),
            chunk_path: chunk_path.clone(),
            chunk_budget: *chunk_budget,
            chunking: chunking.clone(),
            chunk_cap: *chunk_cap,
            status: *status,
            window: *window,
            overlap: *overlap,
            stats_json: cli.stats_json,
        }),
        None => {
            let Some(query) = cli.query.clone() else {
                anyhow::bail!("usage: gorp <QUERY> [PATH]  (see --help)");
            };
            search::run(&cli, &query)
        }
    }
}
