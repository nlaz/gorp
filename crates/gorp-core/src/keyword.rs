//! Keyword (regex/literal) mode: parallel scan built on ripgrep's engine
//! crates (`grep-regex`, `grep-searcher`, `ignore`). Same semantics agents
//! already expect from grep/rg; no index involved.

use anyhow::{Context, Result};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct KeywordOptions {
    pub case_insensitive: bool,
    /// Treat the pattern as a literal string, not a regex.
    pub fixed_string: bool,
    /// Stop after this many total hits (0 = unlimited).
    pub max_hits: usize,
    /// Keep at most this many hits in memory (0 = unlimited), while still
    /// visiting and *counting* everything. Unlike `max_hits` this never changes
    /// the reported totals — it exists because a broad pattern over a large
    /// corpus can match millions of lines whose text the caller will never
    /// print. The retained hits are the smallest by (path, line), so bounded
    /// output is byte-identical to the head of the unbounded sort.
    pub retain: usize,
}

/// Ordered by (path, line, text) so a `BinaryHeap` of hits is a max-heap on
/// output order — the retention bound pops the *last* hit, keeping the head.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeywordHit {
    /// Path relative to the search root.
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// What a scan found: the (possibly retention-bounded) hits, plus the true
/// totals, which stay honest even when `retain` dropped hit text.
#[derive(Debug, Default)]
pub struct KeywordScan {
    /// Sorted (path, line); at most `retain` of them when `retain > 0`.
    pub hits: Vec<KeywordHit>,
    /// Every match found, retained or not.
    pub total: usize,
    /// Files with at least one match.
    pub files: usize,
    /// Directory entries the walk could not read (permissions, races).
    pub walk_errors: usize,
}

/// Scan `root` for `pattern`. Results are sorted (path, line) for
/// deterministic output.
pub fn scan(root: &Path, pattern: &str, opts: &KeywordOptions) -> Result<KeywordScan> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(opts.case_insensitive);
    if opts.fixed_string {
        builder.fixed_strings(true);
    }
    let matcher =
        builder.build(pattern).with_context(|| format!("invalid pattern: {pattern}"))?;

    struct Shared {
        /// Max-heap on (path, line): popping evicts the hit that sorts last.
        hits: std::collections::BinaryHeap<KeywordHit>,
        total: usize,
        files: usize,
    }
    let shared =
        Mutex::new(Shared { hits: std::collections::BinaryHeap::new(), total: 0, files: 0 });
    let walk_errors = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicBool::new(false);

    ignore::WalkBuilder::new(root).build_parallel().run(|| {
        let matcher = matcher.clone();
        let shared = &shared;
        let walk_errors = &walk_errors;
        let done = &done;
        let max_hits = opts.max_hits;
        let retain = opts.retain;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        Box::new(move |entry| {
            use ignore::WalkState;
            if done.load(std::sync::atomic::Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                walk_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            let rel =
                // A root that IS a file strips to "", which printed hits as
                // `:9:text` with no filename (RESEARCH.md §16.11's sibling on
                // the keyword path).
                {
                    let stripped = path.strip_prefix(root).unwrap_or(path);
                    if stripped.as_os_str().is_empty() {
                        path.file_name().unwrap_or_default().to_string_lossy().into_owned()
                    } else {
                        stripped.to_string_lossy().replace('\\', "/")
                    }
                };
            if rel.starts_with(".gorp/") {
                return WalkState::Continue;
            }
            let mut local = Vec::new();
            let _ = searcher.search_path(
                &matcher,
                path,
                UTF8(|line, text| {
                    local.push(KeywordHit {
                        path: rel.clone(),
                        line,
                        text: text.trim_end_matches('\n').to_string(),
                    });
                    Ok(true)
                }),
            );
            if !local.is_empty() {
                let mut s = shared.lock().unwrap();
                s.files += 1;
                s.total += local.len();
                s.hits.extend(local);
                if retain > 0 {
                    while s.hits.len() > retain {
                        s.hits.pop();
                    }
                }
                if max_hits > 0 && s.total >= max_hits {
                    done.store(true, std::sync::atomic::Ordering::Relaxed);
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });

    let s = shared.into_inner().unwrap();
    // Ascending (path, line): `into_sorted_vec` on a max-heap is exactly the
    // sort the unbounded path always did.
    let mut hits = s.hits.into_sorted_vec();
    if opts.max_hits > 0 {
        hits.truncate(opts.max_hits);
    }
    Ok(KeywordScan {
        hits,
        total: s.total,
        files: s.files,
        walk_errors: walk_errors.into_inner(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_matches_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo needle\nthree\n").unwrap();
        fs::write(dir.path().join("b.txt"), "needle at start\n").unwrap();
        fs::write(dir.path().join("c.bin"), b"nee\0dle").unwrap();

        let scan = scan(dir.path(), "needle", &KeywordOptions::default()).unwrap();
        assert_eq!(scan.hits.len(), 2);
        assert_eq!(scan.total, 2);
        assert_eq!(scan.files, 2);
        assert_eq!(scan.hits[0].path, "a.txt");
        assert_eq!(scan.hits[0].line, 2);
        assert_eq!(scan.hits[1].path, "b.txt");
    }

    #[test]
    fn case_insensitive_and_regex() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn FooBar() {}\nfn baz() {}\n").unwrap();
        let opts = KeywordOptions { case_insensitive: true, ..Default::default() };
        assert_eq!(scan(dir.path(), "foobar", &opts).unwrap().hits.len(), 1);
        assert_eq!(
            scan(dir.path(), r"fn \w+\(\)", &KeywordOptions::default()).unwrap().hits.len(),
            2
        );
    }

    #[test]
    fn retention_bounds_memory_but_not_the_count() {
        let dir = tempfile::tempdir().unwrap();
        // 30 matches across 3 files; the parallel walk visits them in an
        // arbitrary order, which is exactly what retention must be immune to.
        for name in ["a.txt", "b.txt", "c.txt"] {
            let body: String = (1..=10).map(|i| format!("needle {i}\n")).collect();
            fs::write(dir.path().join(name), body).unwrap();
        }
        let opts = KeywordOptions { retain: 5, ..Default::default() };
        let scan = scan(dir.path(), "needle", &opts).unwrap();
        assert_eq!(scan.total, 30);
        assert_eq!(scan.files, 3);
        // The 5 retained hits are the 5 smallest by (path, line) — the head
        // of the full sorted output, so capped printing stays byte-identical.
        assert_eq!(scan.hits.len(), 5);
        for (i, hit) in scan.hits.iter().enumerate() {
            assert_eq!(hit.path, "a.txt");
            assert_eq!(hit.line, i as u64 + 1);
        }
    }
}
