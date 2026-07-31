//! Everything that writes to stdout or stderr.
//!
//! One rule, and the CLI tests enforce it: **stdout is data, stderr is
//! commentary.** Results go to stdout in `path:line:text`, parseable by anything
//! that parses grep. Footers, warnings, stats, and suggestions go to stderr, so
//! a pipeline gets clean data and a human still gets told what happened.
//!
//! Printing lives here rather than beside the commands so the commands are about
//! deciding what to show, not how.

use semgrep_core::corpus;
use semgrep_core::rank::Mode;
use semgrep_core::search::{SearchHit, SearchResult};
use std::collections::HashSet;
use std::path::Path;

/// Exact mode prints at most this many matches unless --all is given; the footer
/// reports the true total, so enumeration is never silently lossy.
pub const EXACT_PRINT_CAP: usize = 250;

/// Bytes as a short human string. Decimal units, because that is what disk
/// tooling reports and the numbers are compared against `du`.
pub fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (bytes as f64, 0);
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

/// One result line per hit, plus optional context. The only writer of result
/// data, so the `path:line:text` contract has a single home.
pub fn hits(root: &Path, hits: &[SearchHit], shown: usize, json: bool, context_lines: usize) {
    for hit in &hits[..shown] {
        if json {
            // Unwrap: SearchHit is plain data with no map keys, so it cannot fail
            // to serialize.
            println!("{}", serde_json::to_string(hit).expect("SearchHit serializes"));
        } else {
            println!("{}:{}:{}", hit.path, hit.line, hit.text);
            if context_lines > 0 {
                context(root, hit, context_lines);
            }
        }
    }
}

pub fn footer(query: &str, mode: Mode, result: &SearchResult, shown: usize, suggested: bool) {
    let n = result.hits.len();
    if mode == Mode::Keyword {
        let n_files = result.hits.iter().map(|h| h.path.as_str()).collect::<HashSet<_>>().len();
        let files = if n_files == 1 { "file" } else { "files" };
        if n == 0 {
            if !suggested {
                // Both recovery moves, not just one: a 0-hit -e can mean the
                // path was wrong just as often as the vocabulary was.
                eprintln!(
                    "semgrep: 0 matches · wrong path? broaden it · searching for a \
                     concept? drop -e and ask in plain language"
                );
            }
        } else if shown < n {
            eprintln!(
                "semgrep: showing {shown} of {n} matches in {n_files} {files} \
                 (--all for every match) · locating a concept? drop -e and ask in plain language"
            );
        } else {
            eprintln!("semgrep: {n} matches in {n_files} {files}");
        }
    } else if n == 0 {
        eprintln!(
            "semgrep: no results · try broader phrasing or a nearby concept; \
             -e '{query}' checks for the exact string"
        );
    } else {
        eprintln!(
            "semgrep: ranked top {n} of {} candidates · not it? rephrase the query, \
             or -e '<pattern>' for every exact match",
            result.report.n_chunks_considered.max(n)
        );
    }
}

pub fn stats(mode: Mode, result: &SearchResult) {
    let r = &result.report;
    eprintln!(
        "semgrep: mode={:?} hits={} index={} hnsw={} chunks={} walk/load={:.1}ms rank={:.1}ms total={:.1}ms{}",
        mode,
        result.hits.len(),
        r.used_index,
        r.used_hnsw,
        r.n_chunks_considered,
        r.walk_ms() + r.load_ms(),
        r.rank_ms(),
        r.total_ms,
        peak_rss_mb().map(|m| format!(" peak_rss={m:.0}MB")).unwrap_or_default(),
    );
    // Zero-valued stages are filtered here and only here: the report carries the
    // whole schedule so machine consumers see a fixed shape, while a human
    // reading a line does not want fifteen `=0.0ms` entries to scan past.
    let line = r
        .stages
        .iter()
        .filter(|s| s.ms > 0.0)
        .map(|s| format!("{}={:.1}ms", s.stage.name(), s.ms))
        .collect::<Vec<_>>()
        .join(" ");
    if !line.is_empty() {
        eprintln!("semgrep: provenance: {line}");
        // The residual. Printed always, including when it is small, because a
        // number that only appears when it is bad teaches nobody what normal
        // looks like.
        eprintln!(
            "semgrep: accounted={:.1}ms unattributed={:.1}ms ({:.0}%)",
            r.accounted_ms(),
            r.unattributed_ms(),
            100.0 * r.unattributed_ms() / r.total_ms.max(f64::EPSILON),
        );
    }
    if r.stale_files > 0 {
        eprintln!(
            "semgrep: warning: {} files changed since indexing (run `semgrep index`)",
            r.stale_files
        );
    }
}

/// Frame a hit with `n` lines either side. Context lines use `path-line-text`
/// so a consumer can tell a match from its surroundings — grep's convention.
pub fn context(root: &Path, hit: &SearchHit, n: usize) {
    let Some(text) = corpus::read_text(&root.join(&hit.path)) else { return };
    let lines: Vec<&str> = text.lines().collect();
    let center = hit.line as usize;
    let lo = center.saturating_sub(n).max(1);
    let hi = (center + n).min(lines.len());
    for i in lo..=hi {
        if i != center {
            println!("{}-{}-{}", hit.path, i, lines[i - 1]);
        }
    }
    println!("--");
}

/// One JSON object on stderr. Stderr, not stdout: `--json` owns stdout and the
/// two must compose without corrupting the hit stream.
pub fn stats_json(line: &str) {
    eprintln!("{line}");
}

pub fn peak_rss_mb() -> Option<f64> {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) } == 0;
    if !ok {
        return None;
    }
    let ru = unsafe { ru.assume_init() };
    // macOS reports bytes; Linux reports KiB.
    let bytes = if cfg!(target_os = "macos") {
        ru.ru_maxrss as f64
    } else {
        ru.ru_maxrss as f64 * 1024.0
    };
    Some(bytes / 1e6)
}
