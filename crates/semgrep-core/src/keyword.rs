//! Keyword (regex/literal) mode: parallel scan built on ripgrep's engine
//! crates (`grep-regex`, `grep-searcher`, `ignore`). Same semantics agents
//! already expect from grep/rg; no index involved.

use anyhow::{Context, Result};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct KeywordOptions {
    pub case_insensitive: bool,
    /// Treat the pattern as a literal string, not a regex.
    pub fixed_string: bool,
    /// Stop after this many total hits (0 = unlimited).
    pub max_hits: usize,
}

impl Default for KeywordOptions {
    fn default() -> Self {
        Self { case_insensitive: false, fixed_string: false, max_hits: 0 }
    }
}

#[derive(Debug, Clone)]
pub struct KeywordHit {
    /// Path relative to the search root.
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// Scan `root` for `pattern`. Results are sorted (path, line) for
/// deterministic output.
pub fn scan(root: &Path, pattern: &str, opts: &KeywordOptions) -> Result<Vec<KeywordHit>> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(opts.case_insensitive);
    if opts.fixed_string {
        builder.fixed_strings(true);
    }
    let matcher =
        builder.build(pattern).with_context(|| format!("invalid pattern: {pattern}"))?;

    let hits = Mutex::new(Vec::new());
    let done = std::sync::atomic::AtomicBool::new(false);

    ignore::WalkBuilder::new(root).build_parallel().run(|| {
        let matcher = matcher.clone();
        let hits = &hits;
        let done = &done;
        let max_hits = opts.max_hits;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        Box::new(move |entry| {
            use ignore::WalkState;
            if done.load(std::sync::atomic::Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else { return WalkState::Continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            let rel =
                path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
            if rel.starts_with(".semgrep/") {
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
                let mut all = hits.lock().unwrap();
                all.extend(local);
                if max_hits > 0 && all.len() >= max_hits {
                    done.store(true, std::sync::atomic::Ordering::Relaxed);
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });

    let mut all = hits.into_inner().unwrap();
    all.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    if opts.max_hits > 0 {
        all.truncate(opts.max_hits);
    }
    Ok(all)
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

        let hits = scan(dir.path(), "needle", &KeywordOptions::default()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "a.txt");
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[1].path, "b.txt");
    }

    #[test]
    fn case_insensitive_and_regex() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn FooBar() {}\nfn baz() {}\n").unwrap();
        let opts = KeywordOptions { case_insensitive: true, ..Default::default() };
        assert_eq!(scan(dir.path(), "foobar", &opts).unwrap().len(), 1);
        assert_eq!(
            scan(dir.path(), r"fn \w+\(\)", &KeywordOptions::default()).unwrap().len(),
            2
        );
    }
}
