//! The default verb: ranked search, or exact search with `-e`.

use crate::cli::Cli;
use crate::out;
use anyhow::Result;
use semgrep_core::ChunkParams;
use semgrep_core::cache;
use semgrep_core::keyword::KeywordOptions;
use semgrep_core::rank::Mode;
use semgrep_core::search::{SearchOptions, search};
use std::path::{Path, PathBuf};

pub fn run(cli: &Cli, query: &str) -> Result<i32> {
    let root = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let mode = resolve_mode(cli)?;
    let opts = options(cli, mode);

    warn_if_first_search(&root, &opts);
    let result = search(&root, query, &opts)?;

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
        && suggest_ranked_alternatives(&root, query, &opts);

    out::footer(query, mode, &result, shown, suggested);
    if cli.stats {
        out::stats(mode, &result);
    }
    Ok(if result.hits.is_empty() { crate::EXIT_NONE } else { crate::EXIT_FOUND })
}

/// `-e` wins over `--mode`: the explicit contract beats the harness knob.
fn resolve_mode(cli: &Cli) -> Result<Mode> {
    if cli.exact {
        return Ok(Mode::Keyword);
    }
    match cli.tuning.mode.as_deref() {
        None | Some("hybrid") => Ok(Mode::Hybrid),
        Some("keyword") => Ok(Mode::Keyword),
        Some("bm25") => Ok(Mode::Bm25),
        Some("semantic") => Ok(Mode::Semantic),
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
        rerank_maxsim: t.maxsim,
        maxsim_pool: t.maxsim_pool,
        maxsim_blend: t.maxsim_blend,
        params: ChunkParams { window: t.window, overlap: t.overlap, ..Default::default() },
        keyword: KeywordOptions {
            case_insensitive: cli.ignore_case,
            fixed_string: cli.fixed_string,
            max_hits: 0,
        },
    }
}

/// A cold ranked search is also a cache build, and on a large scope that is
/// visibly slow. Say so before it happens rather than looking hung.
fn warn_if_first_search(root: &Path, opts: &SearchOptions) {
    if opts.mode != Mode::Keyword
        && !opts.no_index
        && cache::discover(root, &opts.params).is_none()
    {
        eprintln!(
            "semgrep: first ranked search of this scope — caching it (later searches are fast)"
        );
    }
}

/// On an exact-mode miss, print the top ranked hits for the same terms to stderr.
/// Returns whether anything printed.
///
/// Only when an index already covers this scope — via discovery, so subdirectory
/// scopes and ancestor or cache entries count. The fallback must never turn a
/// fast miss into a corpus pass or a surprise cache build.
fn suggest_ranked_alternatives(root: &Path, query: &str, opts: &SearchOptions) -> bool {
    if opts.no_index || cache::discover(root, &opts.params).is_none() {
        return false;
    }
    let ranked_opts = SearchOptions { mode: Mode::Hybrid, k: 3, ..opts.clone() };
    let Ok(ranked) = search(root, query, &ranked_opts) else { return false };
    if ranked.hits.is_empty() {
        return false;
    }
    eprintln!("semgrep: -e found 0 exact matches · ranked search for the same terms finds:");
    for hit in &ranked.hits {
        eprintln!("semgrep:   {}:{}:{}", hit.path, hit.line, truncate(hit.text.trim(), 100));
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
