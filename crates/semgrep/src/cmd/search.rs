//! The default verb: ranked search, or exact search with `-e`.

use crate::cli::Cli;
use crate::out;
use crate::telemetry::{self, Phase};
use anyhow::Result;
use semgrep_core::ChunkParams;
use semgrep_core::cache;
use semgrep_core::keyword::KeywordOptions;
use semgrep_core::rank::Mode;
use semgrep_core::search::{SearchOptions, search};
use std::path::{Path, PathBuf};

pub fn run(cli: &Cli, query: &str) -> Result<i32> {
    let root = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));
    // Before anything else, and before the "caching this scope" notice in
    // particular, which used to announce that it was indexing a directory that
    // did not exist. A missing path is an error (exit 2), not an empty result
    // (exit 1): an agent reads "no results" as *the code is not there*, when in
    // fact the path was simply wrong. `exists`, not `is_dir`, because a single
    // file is a legitimate scope that the streaming path handles.
    if !root.exists() {
        anyhow::bail!("{}: no such file or directory", root.display());
    }
    let (mode, mode_reason) = resolve_mode(cli)?;
    let opts = options(cli, mode);

    let result = search(&root, query, &opts)?;
    let exit = if result.hits.is_empty() { crate::EXIT_NONE } else { crate::EXIT_FOUND };

    // Emitted here, not at the end of the function: an exact miss runs a second
    // full search below, and an envelope should describe its own invocation.
    // Emitting last would order the records suggestion-first and fold the
    // suggestion's memory into the primary's RSS high-water mark.
    telemetry::emit(
        &telemetry::search_envelope(
            Phase::Primary,
            mode,
            mode_reason,
            &root,
            query,
            &opts,
            &result,
            exit,
        ),
        cli.stats_json,
    );

    // Exact mode caps its output but never its count: the footer reports the
    // true total, so `-e` stays a trustworthy answer to "how many".
    let shown = if mode == Mode::Keyword && !cli.all && !cli.json {
        result.hits.len().min(out::EXACT_PRINT_CAP)
    } else {
        result.hits.len()
    };
    out::hits(&root, &result.hits, shown, cli.json, cli.context);

    // A miss should be a gradient, not a dead end. When `-e` finds nothing and an
    // index makes it cheap, show what ranked search finds for the same terms — on
    // stderr, so stdout stays empty and the exit code still means "no match".
    let suggested = mode == Mode::Keyword
        && result.hits.is_empty()
        && suggest_ranked_alternatives(cli, &root, query, &opts);

    out::footer(query, mode, &result, shown, suggested);
    if cli.stats {
        out::stats(mode, &result);
    }
    Ok(exit)
}

/// `-e` wins over `--mode`: the explicit contract beats the harness knob.
/// Returns why, because "hybrid" in a trace is ambiguous between asked-for and
/// defaulted-to, and the two are different experiments.
fn resolve_mode(cli: &Cli) -> Result<(Mode, &'static str)> {
    if cli.exact {
        return Ok((Mode::Keyword, "exact-flag"));
    }
    match cli.tuning.mode.as_deref() {
        None => Ok((Mode::Hybrid, "default")),
        Some("hybrid") => Ok((Mode::Hybrid, "mode-flag")),
        Some("keyword") => Ok((Mode::Keyword, "mode-flag")),
        Some("bm25") => Ok((Mode::Bm25, "mode-flag")),
        Some("semantic") => Ok((Mode::Semantic, "mode-flag")),
        Some(other) => anyhow::bail!("unknown mode {other:?} (hybrid|keyword|bm25|semantic)"),
    }
}

fn options(cli: &Cli, mode: Mode) -> SearchOptions {
    let t = &cli.tuning;
    SearchOptions {
        mode,
        k: cli.top,
        no_index: t.no_index,
        use_hnsw: !t.brute_force,
        check_stale: cli.check_stale,
        sem_weight: t.sem_weight,
        diversify: !t.no_diversify,
        mmr_lambda: t.mmr_lambda,
        prf_terms: t.prf,
        // MaxSim reranks the semantic candidate list before fusion, so it can
        // only pay off where that list decides the answer. In `--mode semantic`
        // it does, and the rerank is worth +0.080 R@5 (etcd, CI [+0.010,+0.155],
        // p=0.040); in hybrid, BM25 carries the fused result and 97% of queries
        // come back unchanged (RESEARCH.md §13.10). So: on for semantic,
        // opt-in elsewhere, and `--no-maxsim` to turn it off.
        rerank_maxsim: (t.maxsim || mode == Mode::Semantic) && !t.no_maxsim,
        maxsim_pool: t.maxsim_pool,
        maxsim_blend: t.maxsim_blend,
        maxsim_post: t.maxsim_post,
        params: ChunkParams { window: t.window, overlap: t.overlap, ..Default::default() },
        repair_max_drift: t.repair_max_drift,
        on_first_search: Some(announce_first_search),
        keyword: KeywordOptions {
            case_insensitive: cli.ignore_case,
            fixed_string: cli.fixed_string,
            max_hits: 0,
        },
    }
}

/// A cold ranked search is also a cache build, and on a large scope that is
/// visibly slow. Say so before it happens rather than looking hung.
///
/// Handed to the engine rather than decided here. The CLI used to answer "is
/// this the first search?" by calling `cache::discover` itself — a full path
/// canonicalization and generation-directory scan, on *every* ranked query, to
/// decide whether to print one line — and then the engine resolved the same
/// scope again. The engine already knows; it just had no way to say so before
/// the fact (SIMULATION.md §4).
fn announce_first_search() {
    eprintln!("semgrep: first ranked search of this scope — caching it (later searches are fast)");
}

/// On an exact-mode miss, print the top ranked hits for the same terms to stderr.
/// Returns whether anything printed.
///
/// Only when an index already covers this scope — via discovery, so subdirectory
/// scopes and ancestor or cache entries count. The fallback must never turn a
/// fast miss into a corpus pass or a surprise cache build.
fn suggest_ranked_alternatives(
    cli: &Cli,
    root: &Path,
    query: &str,
    opts: &SearchOptions,
) -> bool {
    if opts.no_index || cache::discover(root, &opts.params).is_none() {
        return false;
    }
    // No cold-start notice: this path already refused to run unless an index
    // exists, so it can never be the search that builds one.
    let ranked_opts =
        SearchOptions { mode: Mode::Hybrid, k: 3, on_first_search: None, ..opts.clone() };
    let Ok(ranked) = search(root, query, &ranked_opts) else { return false };
    // A whole second engine invocation, and until now it appeared in no report
    // at all: `--stats` describes the primary search only, so an exact miss
    // looked like a keyword scan and cost a keyword scan plus a hybrid query.
    telemetry::emit(
        &telemetry::search_envelope(
            Phase::Suggest,
            Mode::Hybrid,
            "exact-miss-suggestion",
            root,
            query,
            &ranked_opts,
            &ranked,
            crate::EXIT_NONE,
        ),
        cli.stats_json,
    );
    if ranked.hits.is_empty() {
        return false;
    }
    eprintln!("semgrep: -e found 0 exact matches · ranked search for the same terms finds:");
    for hit in &ranked.hits {
        eprintln!(
            "semgrep:   {}:{}:{}",
            out::quote_path(&hit.path),
            hit.line,
            truncate(hit.text.trim(), 100)
        );
    }
    eprintln!(
        "semgrep: (wrong path? broaden it · drop -e to run this ranked search on stdout)"
    );
    true
}

/// Cut to `max` characters on a character boundary, so a suggestion line stays
/// short without panicking on multi-byte text.
fn truncate(text: &str, max: usize) -> &str {
    text.char_indices().nth(max).map_or(text, |(i, _)| &text[..i])
}
