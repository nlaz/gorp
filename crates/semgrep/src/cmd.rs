//! The verbs. One module each, and a dispatch that only routes.

mod cache;
mod index;
mod search;

use crate::cli::{Cli, Cmd};
use anyhow::Result;

pub fn dispatch(cli: Cli) -> Result<i32> {
    match &cli.cmd {
        Some(Cmd::Cache { prune, clear }) => cache::run(*prune, *clear),
        Some(Cmd::Index { path, hnsw, sif, sif_a, sif_center, status, window, overlap }) => {
            index::run(index::Args {
                path: path.clone(),
                hnsw: *hnsw,
                sif: *sif,
                sif_a: *sif_a,
                sif_center: *sif_center,
                status: *status,
                window: *window,
                overlap: *overlap,
            })
        }
        None => {
            let Some(query) = cli.query.clone() else {
                anyhow::bail!("usage: semgrep <QUERY> [PATH]  (see --help)");
            };
            search::run(&cli, &query)
        }
    }
}
