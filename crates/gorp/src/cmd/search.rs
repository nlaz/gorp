//! The default verb: ranked search, or exact search with `-e`.

use crate::cli::Cli;
use crate::out;
use crate::out::PROG;
use crate::telemetry::{self, Phase};
use anyhow::Result;
use gorp_core::ChunkParams;
use gorp_core::cache;
use gorp_core::keyword::KeywordOptions;
use gorp_core::rank::Mode;
use gorp_core::search::{SearchOptions, search};
use std::path::{Path, PathBuf};

pub fn run(cli: &Cli, query: &str) -> Result<i32> {
    // Before anything else, and before the "caching this scope" notice in
    // particular, which used to announce that it was indexing a directory that
    // did not exist. A missing path is an error (exit 2), not an empty result
    // (exit 1): an agent reads "no results" as *the code is not there*, when in
    // fact the path was simply wrong. `exists`, not `is_dir`, because a single
    // file is a legitimate scope that the streaming path handles.
    // `--path` merges into the positionals so both spellings reach one code
    // path; see `Cli::path_flag` for why the alias exists at all.
    let given: Vec<PathBuf> = cli.paths.iter().chain(cli.path_flag.iter()).cloned().collect();
    let paths = paths_with_stdin(&given)?;
    for p in &paths {
        if !p.exists() {
            anyhow::bail!("{}: no such file or directory", p.display());
        }
    }
    let scope = Scope::resolve(&paths);
    let (mode, mode_reason) = resolve_mode(cli)?;
    let mut opts = options(cli, mode)?;
    let filter = Filter::new(cli, &scope)?;
    let root = scope.root.clone();

    // Exact mode caps what it *prints* at EXACT_PRINT_CAP; bound what it
    // *retains* to match, so a broad pattern over a large corpus counts every
    // match without holding every match's text. Only when nothing downstream
    // needs the full list: `--all` and `--json` print everything, and an
    // active filter selects from the full list before its own truncation.
    if mode == Mode::Keyword && !cli.all && !cli.json && !filter.active() {
        opts.keyword.retain = out::EXACT_PRINT_CAP;
    }

    // Over-fetch only when something will be filtered away, so the single-path
    // case — every snapshot case, and the overwhelming majority of real calls —
    // runs byte-for-byte as before.
    let search_opts = match filter.widen(opts.k) {
        Some(k) => SearchOptions { k, ..opts.clone() },
        None => opts.clone(),
    };
    let mut result = search(&root, query, &search_opts)?;
    let dropped = filter.apply(&mut result.hits, (mode != Mode::Keyword).then_some(opts.k));
    // The filter selects from the complete hit list (retention was off), so
    // the post-filter list IS the answer — restate the totals the footer
    // reads, or it would report matches the filter just dropped.
    if mode == Mode::Keyword && filter.active() {
        result.report.keyword_total = result.hits.len();
        result.report.keyword_files = result
            .hits
            .iter()
            .map(|h| h.path.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
    }
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
    // `-c` beats `-l`, grep's own precedence: counts are the more specific
    // request, and a caller passing both gets the one that says more.
    let emphasis = emphasis_for(cli, mode, query);
    if cli.count {
        out::counts(
            &per_file_counts(mode, &filter, &result),
            &print_opts(cli, &given, &emphasis, mode),
        );
    } else {
        out::hits(&root, &result.hits, shown, &print_opts(cli, &given, &emphasis, mode));
    }
    if dropped {
        // Name the filter that actually dropped them. Saying "the paths given"
        // when `--lines` did the cutting sends the caller to widen a scope that
        // was never the constraint.
        match filter.lines {
            Some((lo, hi)) => eprintln!(
                "{PROG}: results narrowed to lines {lo}-{} · matches outside that \
                 range were dropped",
                if hi == u32::MAX { "end".to_string() } else { hi.to_string() }
            ),
            // Ranked mode only: there the paths cut into a corpus-wide top-k
            // and the caller should hear that. In exact mode filtering to the
            // paths given is grep's ordinary contract — nothing was "narrowed".
            None if mode != Mode::Keyword => eprintln!(
                "{PROG}: results narrowed to the paths given · some ranked matches \
                 elsewhere in {} were dropped",
                root.display()
            ),
            None => {}
        }
    }

    // A miss should be a gradient, not a dead end. When `-e` finds nothing and an
    // index makes it cheap, show what ranked search finds for the same terms — on
    // stderr, so stdout stays empty and the exit code still means "no match".
    let suggested = mode == Mode::Keyword
        && result.hits.is_empty()
        && suggest_ranked_alternatives(cli, &root, query, &opts);

    // When `-c` answered, the exact-mode footer would restate the counts the
    // caller just read on stdout. The miss footer still prints: an empty
    // count list needs its "wrong path?" guidance as much as an empty match
    // list does, and the walk-errors note rides it.
    if !(cli.count && mode == Mode::Keyword && !result.hits.is_empty()) {
        out::footer(mode, &result, shown, suggested);
    }
    if cli.stats {
        out::stats(mode, &result);
    }
    Ok(exit)
}

/// Where to search, given zero or more paths.
///
/// grep takes a list; gorp's engine takes one root, because a root is what an
/// index and a cache entry are keyed by. So a list collapses to its common
/// ancestor and the extra paths become a filter (see [`Filter`]). One path stays
/// one root with no filter at all, which is what keeps the common case exact.
struct Scope {
    root: PathBuf,
    /// Root-relative prefixes to keep, empty when everything under `root` counts.
    keep: Vec<String>,
}

impl Scope {
    fn resolve(paths: &[PathBuf]) -> Self {
        match paths {
            [] => Self { root: PathBuf::from("."), keep: Vec::new() },
            [one] => Self { root: one.clone(), keep: Vec::new() },
            many => {
                let root = common_ancestor(many);
                let keep = many
                    .iter()
                    .filter_map(|p| {
                        let c = std::fs::canonicalize(p).ok()?;
                        let r = std::fs::canonicalize(&root).ok()?;
                        Some(c.strip_prefix(&r).ok()?.to_string_lossy().replace('\\', "/"))
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
                Self { root, keep }
            }
        }
    }
}

/// Expand a literal `-` into the newline-separated paths on stdin, so
/// `find . -name '*.py' | sg "query" -` works without `xargs`.
///
/// `-` is the Unix convention for "the other end of the pipe" and costs nothing
/// to honour: a caller who genuinely has a file named `-` can write `./-`. The
/// evidence for this one is thin — 3 `xargs` uses in 1,641 real invocations —
/// so it is here because it composes, not because it was asked for.
///
/// Blank lines are skipped; a `-` with nothing on stdin yields no paths, which
/// `Scope::resolve` then reads as "search the working directory", the same as
/// passing no path at all.
fn paths_with_stdin(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !paths.iter().any(|p| p.as_os_str() == "-") {
        return Ok(paths.to_vec());
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .map_err(|e| anyhow::anyhow!("reading paths from stdin: {e}"))?;
    let mut out = Vec::new();
    for p in paths {
        if p.as_os_str() == "-" {
            out.extend(buf.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from));
        } else {
            out.push(p.clone());
        }
    }
    Ok(out)
}

/// Deepest directory containing every path. Falls back to `.` if they share
/// nothing (different drives, or a canonicalize that failed).
fn common_ancestor(paths: &[PathBuf]) -> PathBuf {
    let mut it = paths.iter().map(|p| {
        let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        if c.is_dir() { c } else { c.parent().map(Path::to_path_buf).unwrap_or(c) }
    });
    let Some(mut acc) = it.next() else { return PathBuf::from(".") };
    for p in it {
        while !p.starts_with(&acc) {
            match acc.parent() {
                Some(up) => acc = up.to_path_buf(),
                None => return PathBuf::from("."),
            }
        }
    }
    acc
}

/// Keeps only the hits a caller asked for: under one of several paths (`Scope`),
/// matching a `-g` glob, or both.
///
/// Applied to *hits*, never to the corpus walk. Filtering the walk would change
/// what gets indexed, and the index is a shared cache keyed by root and chunk
/// params — one `-g '*.py'` search would poison every later search of that
/// scope, the shape of the `--window` sweep's cache poisoning. Filtering
/// results costs an over-fetch instead.
struct Filter {
    keep: Vec<String>,
    globs: Option<ignore::overrides::Override>,
    lines: Option<(u32, u32)>,
}

impl Filter {
    fn new(cli: &Cli, scope: &Scope) -> Result<Self> {
        let globs = if cli.glob.is_empty() {
            None
        } else {
            let mut b = ignore::overrides::OverrideBuilder::new(&scope.root);
            for g in &cli.glob {
                b.add(g).map_err(|e| anyhow::anyhow!("bad glob {g:?}: {e}"))?;
            }
            Some(b.build()?)
        };
        let lines = cli.lines.as_deref().map(parse_lines).transpose()?;
        Ok(Self { keep: scope.keep.clone(), globs, lines })
    }

    fn active(&self) -> bool {
        !self.keep.is_empty() || self.globs.is_some() || self.lines.is_some()
    }

    /// How many results to ask the engine for, or `None` to leave `k` alone.
    ///
    /// Filtering happens after ranking, so a narrow filter over a wide root can
    /// eat the whole head. Over-fetching trades a little work for a top-k that
    /// is usually still full; it is a heuristic, and the caller is told on
    /// stderr when it was not enough.
    fn widen(&self, k: usize) -> Option<usize> {
        self.active().then(|| k.saturating_mul(20).clamp(200, 2000))
    }

    fn matches(&self, path: &str) -> bool {
        let under = self.keep.is_empty()
            || self
                .keep
                .iter()
                .any(|p| path == p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/')));
        let globbed = self.globs.as_ref().is_none_or(|o| {
            if !o.matched(path, false).is_ignore() {
                return true;
            }
            // gitignore semantics rather than ripgrep's: a glob that matches an
            // ancestor directory includes everything under it, so `-g 'src/*'`
            // (or a bare `-g src`) scopes to the subtree instead of silently
            // matching nothing below the first level.
            std::path::Path::new(path)
                .ancestors()
                .skip(1)
                .take_while(|a| !a.as_os_str().is_empty())
                .any(|a| o.matched(a, true).is_whitelist())
        });
        under && globbed
    }

    /// Retain matching hits, then truncate to `truncate_to` when given.
    /// Returns whether anything was dropped *by the filter* — not by the
    /// truncation, which is ordinary top-k.
    ///
    /// Truncation keys off mode, not filter presence: ranked mode's contract
    /// is top-k, and `widen`'s over-fetch (up to 2000) makes cutting back to
    /// `k` mandatory there. Exact mode's contract is enumeration — a filter
    /// selects which matches count, never how many are reported — so its
    /// caller passes `None` and the print cap alone bounds the output.
    fn apply(
        &self,
        hits: &mut Vec<gorp_core::search::SearchHit>,
        truncate_to: Option<usize>,
    ) -> bool {
        if !self.active() {
            return false;
        }
        let before = hits.len();
        hits.retain(|h| {
            self.matches(&h.path)
                && self.lines.is_none_or(|(lo, hi)| h.line >= lo && h.line <= hi)
        });
        let dropped = hits.len() < before;
        if let Some(k) = truncate_to {
            hits.truncate(k);
        }
        dropped
    }
}

/// `A-B`, `A-` or `-B`, inclusive, 1-based — the shape `sed -n 'A,Bp'` uses.
///
/// This exists because agents piped the tool into `awk` and `grep` to do it:
/// `sg -k 20 "def " f.py | awk -F: '$2 < 2297'` and
/// `… | grep -E "8[0-9][0-9]|9[0-3][0-9]"` are both line-range narrowing spelled
/// as arithmetic on the second colon field. They were the only two of 32 real
/// gorp pipes wanting something `-k` could not give (RESEARCH.md §19.9), and
/// unlike the other 30 they need a second binary the harness may refuse.
fn parse_lines(spec: &str) -> Result<(u32, u32)> {
    let bad =
        || anyhow::anyhow!("bad --lines {spec:?}: expected A-B, A- or -B (inclusive, 1-based)");
    let (a, b) = spec.split_once('-').ok_or_else(bad)?;
    let lo = if a.is_empty() { 1 } else { a.trim().parse().map_err(|_| bad())? };
    let hi = if b.is_empty() { u32::MAX } else { b.trim().parse().map_err(|_| bad())? };
    if lo > hi {
        anyhow::bail!("bad --lines {spec:?}: {lo} is after {hi}");
    }
    Ok((lo, hi))
}

/// What `-c` prints: matches per file, in the order the hits would have
/// printed. Exact mode with no filter reads the scan's own counts, which
/// survive retention; exact mode with a filter counts the filtered hits,
/// which are complete because retention is off whenever a filter is active.
/// Ranked mode counts the k ranked hits in rank order — the counting analog
/// of `-l`.
fn per_file_counts(
    mode: Mode,
    filter: &Filter,
    result: &gorp_core::search::SearchResult,
) -> Vec<(String, usize)> {
    if mode == Mode::Keyword && !filter.active() {
        return result.report.keyword_per_file.clone();
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for h in &result.hits {
        match counts.iter_mut().find(|(p, _)| p == &h.path) {
            Some((_, n)) => *n += 1,
            None => counts.push((h.path.clone(), 1)),
        }
    }
    counts
}

/// The words to emphasise in the results, or nothing.
///
/// Ranked mode: the query is prose or a name guess, and its words are exactly
/// what a reader is looking for in the text. Exact mode: the pattern is
/// authoritative when it is a literal — `-F`, or a pattern with no regex
/// metacharacter in it — and unreadable as words when it is not. `fn \w+_token`
/// tokenizes to `fn`, `w`, `token`, and emphasising `token` in every line
/// would be claiming a match the engine never made, so a real regex gets no
/// emphasis at all until the matcher's own spans are plumbed through.
fn emphasis_for(cli: &Cli, mode: Mode, query: &str) -> out::Emphasis {
    if !out::color_enabled(&cli.color) || cli.json {
        return out::Emphasis::default();
    }
    if mode != Mode::Keyword {
        return out::Emphasis::of(query);
    }
    // A literal is its own emphasis; a regex is not readable as words.
    if cli.fixed_string || !query.contains(|c| "\\[](){}.*+?|^$".contains(c)) {
        return out::Emphasis::literal(query);
    }
    out::Emphasis::default()
}

fn print_opts<'a>(
    cli: &Cli,
    given: &[PathBuf],
    emphasis: &'a out::Emphasis,
    mode: Mode,
) -> out::Print<'a> {
    out::Print {
        emphasis,
        // Keyed off the resolved mode, not the `-e` flag, so `--mode keyword`
        // renders the same blocks.
        exact: mode == Mode::Keyword,
        json: cli.json,
        paths_only: cli.files_with_matches,
        before: cli.before_context.unwrap_or(cli.context),
        after: cli.after_context.unwrap_or(cli.context),
        max_columns: cli.max_columns,
        // grep's rule: one named file means the path is already known, so it is
        // noise on every line. `-H` asks for it back. `-l` and `--json` are
        // unaffected — both are path-carrying formats by definition.
        with_path: cli.compat.with_filename
            || cli.json
            || cli.files_with_matches
            || !(given.len() == 1 && given[0].is_file()),
        // Never under `--json`: an escape inside a JSON string field is not a
        // colour, it is corrupt data in a schema someone parses.
        color: !cli.json && out::color_enabled(&cli.color),
    }
}

/// `-e` wins over `--mode`: the explicit contract beats the harness knob.
/// Returns why, because "semantic" in a trace is ambiguous between asked-for
/// and defaulted-to, and the two are different experiments.
fn resolve_mode(cli: &Cli) -> Result<(Mode, &'static str)> {
    if cli.exact {
        return Ok((Mode::Keyword, "exact-flag"));
    }
    match cli.tuning.mode.as_deref() {
        // Semantic-first (RESEARCH.md §14): hybrid is off by default until the
        // semantic branch carries its own weight.
        None => Ok((Mode::Semantic, "default")),
        Some("hybrid") => Ok((Mode::Hybrid, "mode-flag")),
        Some("keyword") => Ok((Mode::Keyword, "mode-flag")),
        Some("bm25") => Ok((Mode::Bm25, "mode-flag")),
        Some("semantic") => Ok((Mode::Semantic, "mode-flag")),
        Some(other) => anyhow::bail!("unknown mode {other:?} (hybrid|keyword|bm25|semantic)"),
    }
}

fn options(cli: &Cli, mode: Mode) -> Result<SearchOptions> {
    let t = &cli.tuning;
    let passage = t.passage_shape();
    let embed_preproc = crate::cmd::index::parse_preproc(&t.embed_preproc)?;
    let path_render = crate::cmd::index::parse_path_render(&t.chunk_path)?;
    Ok(SearchOptions {
        mode,
        k: cli.top,
        no_index: t.no_index,
        use_hnsw: !t.brute_force,
        check_stale: cli.check_stale,
        sem_weight: t.sem_weight,
        // Env as well as a flag: opting a whole environment back into
        // automatic caching is a machine-setup decision, not a per-query one.
        write_through: t.index || std::env::var("GORP_AUTO_INDEX").is_ok_and(|v| v == "1"),
        diversify: !t.no_diversify,
        mmr_lambda: t.mmr_lambda,
        dedupe_overlap: t.dedupe_overlap,
        file_scope_window: t.file_scope_window,
        decl_boost: t.decl_boost,
        path_boost: t.path_boost,
        // Env rather than a flag: the only consumer is the §35.2 training
        // dump, which must not widen the CLI surface agents see.
        debug_features: std::env::var("GORP_DUMP_FEATURES").is_ok_and(|v| v == "1"),
        learned_blend: t.learned_blend,
        passage_lines: passage.lines,
        passage_chars: passage.chars,
        defines: t.headers,
        fine_rerank: !t.no_fine,
        fine_lines: t.fine_lines,
        fine_blend: t.fine_blend,
        min_score: t.min_score,
        keep_coarse_top: t.keep_coarse_top,
        bm25_pin: t.bm25_pin,
        bridge_expand: t.bridge_expand,
        bridge_weight: t.bridge_weight,
        // Any explicit passage request switches display back to a chunk cut;
        // the fine window keeps choosing the anchor and the order either way.
        passage_override: passage.explicit,
        unit_view: !t.no_unit,
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
        params: ChunkParams {
            window: t.window,
            overlap: t.overlap,
            budget: crate::cmd::index::budget(t.chunk_budget),
            function: crate::cmd::index::chunking(&t.chunking, t.chunk_cap)?,
            ..Default::default()
        },
        repair_max_drift: t.repair_max_drift,
        on_first_search: Some(announce_first_search),
        keyword: KeywordOptions {
            case_insensitive: cli.ignore_case,
            fixed_string: cli.fixed_string,
            word: cli.word,
            max_hits: 0,
            // Set by `run` once it knows whether a filter needs the full list.
            retain: 0,
        },
        embed_preproc,
        path_render,
    })
}

/// A cold ranked search is also a cache build, and on a large scope that is
/// visibly slow. Say so before it happens rather than looking hung.
///
/// Handed to the engine rather than decided here. The CLI used to answer "is
/// this the first search?" by calling `cache::discover` itself — a full path
/// canonicalization and generation-directory scan, on *every* ranked query, to
/// decide whether to print one line — and then the engine resolved the same
/// scope again. The engine already knows; it just had no way to say so before
/// the fact.
fn announce_first_search() {
    eprintln!(
        "{PROG}: first ranked search of this scope — caching it (later searches are fast)"
    );
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
        SearchOptions { mode: Mode::Semantic, k: 3, on_first_search: None, ..opts.clone() };
    let Ok(ranked) = search(root, query, &ranked_opts) else { return false };
    // A whole second engine invocation, and until now it appeared in no report
    // at all: `--stats` describes the primary search only, so an exact miss
    // looked like a keyword scan and cost a keyword scan plus a hybrid query.
    telemetry::emit(
        &telemetry::search_envelope(
            Phase::Suggest,
            Mode::Semantic,
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
    eprintln!("{PROG}: -e found 0 exact matches · ranked search for the same terms finds:");
    for hit in &ranked.hits {
        eprintln!(
            "{PROG}:   {}:{}:{}",
            out::quote_path(&hit.path),
            hit.line,
            truncate(hit.text.trim(), 100)
        );
    }
    eprintln!("{PROG}: (wrong path? broaden it · drop -e to run this ranked search on stdout)");
    true
}

/// Cut to `max` characters on a character boundary, so a suggestion line stays
/// short without panicking on multi-byte text.
fn truncate(text: &str, max: usize) -> &str {
    text.char_indices().nth(max).map_or(text, |(i, _)| &text[..i])
}
