//! The docs cite measurements; this checks the citations still resolve.
//!
//! Source comments earn their length by pointing at where a number came from —
//! "§9.4", "§10.7". That only works while the section exists under that number,
//! and nothing stopped a renumber from silently orphaning forty comments. The
//! rationale is the most valuable thing in this codebase and also the easiest to
//! rot, because nothing executes it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Section numbers a document defines, from headings like `## 9.4 Title`.
fn defined_sections(markdown: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in markdown.lines() {
        let Some(rest) = line.trim_start().strip_prefix('#') else { continue };
        let rest = rest.trim_start_matches('#').trim();
        let number: String =
            rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let number = number.trim_end_matches('.');
        if number.is_empty() || !number.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        // A citation of §9 is answered by the §9 heading, and §9.4 by its own —
        // so record exactly what is written and nothing implied.
        out.insert(number.to_string());
    }
    out
}

/// Documents a `§` citation may point at, and the default when none is named.
///
/// A bare `§9.1` means RESEARCH.md — that is the convention the codebase was
/// written in and most citations still use it. Anything else has to say so.
const DEFAULT_DOC: &str = "RESEARCH.md";
const KNOWN_DOCS: [&str; 5] =
    ["RESEARCH.md", "SIMULATION.md", "FIXES.md", "AUDIT.md", "FOLD.md"];

/// How far back of a `§` to look for the document it belongs to. Long enough to
/// cross a comment-line wrap (`SIMULATION.md\n/// §1.5`), short enough that an
/// unrelated filename in the previous sentence cannot claim the citation.
const DOC_LOOKBEHIND: usize = 64;

/// Every `<doc> §N` cited from Rust source, as (document, section, file).
///
/// The document matters, and used to be assumed. Every citation was checked
/// against RESEARCH.md whatever it named, so `SIMULATION.md §1.1` passed only
/// because RESEARCH.md happens to have a §1.1 of its own, and a correct
/// citation of a section RESEARCH.md lacks would have failed. Both directions
/// were wrong — the guard was passing for the wrong reason, which is the
/// failure mode SIMULATION.md §5 is about.
fn cited_sections(root: &Path) -> BTreeSet<(String, String, String)> {
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);

    let mut cites = BTreeSet::new();
    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        for (i, _) in text.match_indices('§') {
            let after = &text[i + '§'.len_utf8()..];
            let number: String =
                after.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            let number = number.trim_end_matches('.');
            if number.is_empty() {
                continue;
            }
            // The nearest document named just before the §, if any: the last
            // one to start within the lookbehind window wins, so
            // "RESEARCH.md §8 ... SIMULATION.md §1.3" attributes each correctly.
            // The window start snaps down to a char boundary — a fixed byte
            // offset can land inside a multi-byte character (an em-dash 64
            // bytes before a § did exactly that) and slicing there panics.
            let mut lo = i.saturating_sub(DOC_LOOKBEHIND);
            while !text.is_char_boundary(lo) {
                lo -= 1;
            }
            let window = &text[lo..i];
            let doc = KNOWN_DOCS
                .iter()
                .filter_map(|d| window.rfind(d).map(|at| (at, *d)))
                .max_by_key(|&(at, _)| at)
                .map(|(_, d)| d)
                .unwrap_or(DEFAULT_DOC);
            cites.insert((doc.to_string(), number.to_string(), rel.clone()));
        }
    }
    cites
}

/// Every section a comment points at must exist, or the pointer is worse than no
/// pointer: it costs a reader a search that ends nowhere.
#[test]
fn research_citations_resolve() {
    let root = repo_root();
    let defined: std::collections::BTreeMap<&str, BTreeSet<String>> = KNOWN_DOCS
        .iter()
        .map(|doc| {
            let text = std::fs::read_to_string(root.join(doc))
                .unwrap_or_else(|_| panic!("{doc} should exist at the repo root"));
            let sections = defined_sections(&text);
            assert!(!sections.is_empty(), "parsed no section headings out of {doc}");
            (*doc, sections)
        })
        .collect();

    let mut dangling: Vec<String> = cited_sections(&root)
        .into_iter()
        .filter(|(doc, number, _)| !defined[doc.as_str()].contains(number))
        .map(|(doc, number, file)| format!("{doc} §{number} (cited in {file})"))
        .collect();
    dangling.sort();
    dangling.dedup();

    assert!(
        dangling.is_empty(),
        "source comments cite sections that do not exist:\n  {}\n\
         Either the section was renumbered or the citation was a guess.\n\
         (A § with no document named within {DOC_LOOKBEHIND} bytes before it is \
         read as {DEFAULT_DOC}.)",
        dangling.join("\n  ")
    );
}

/// The files CLAUDE.md tells an agent to look at have to be there. It is the
/// first thing read in a session and the last thing anyone thinks to update.
///
/// Only repo-rooted paths are checked. CLAUDE.md also names modules relative to
/// a layer (`build/embed`, `locbench/replay.py`), which are unambiguous in
/// context but not resolvable from the root without guessing which prefix to
/// try — and a checker that guesses is one that fails for the wrong reason.
#[test]
fn claude_md_paths_exist() {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("CLAUDE.md")).expect("CLAUDE.md");

    const ROOTED: [&str; 6] = ["crates/", "eval/", "bench/", "tools/", "tests/", ".github/"];

    let mut missing = Vec::new();
    for token in text.split('`').skip(1).step_by(2) {
        let candidate = token.trim().trim_end_matches('/');
        if !ROOTED.iter().any(|p| candidate.starts_with(p)) {
            continue;
        }
        if candidate.contains(' ') || candidate.contains('*') || candidate.contains("::") {
            continue;
        }
        if !root.join(candidate).exists() {
            missing.push(candidate.to_string());
        }
    }
    missing.sort();
    missing.dedup();
    assert!(missing.is_empty(), "CLAUDE.md points at paths that do not exist: {missing:?}");
}
