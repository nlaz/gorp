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

/// The document a `§` citation points at.
///
/// A bare `§9.1` in a comment means RESEARCH.md: it is the only numbered
/// document in `docs/`, and the convention the codebase was written in. A bare
/// filename doing double duty — it is joined to [`DOCS_DIR`] to read the file,
/// so a comment cites `RESEARCH.md §9.1` however deep the docs are filed, and
/// moving the directory is one constant rather than 155 edits.
const DOC: &str = "RESEARCH.md";

/// Where that document lives, relative to the repo root.
const DOCS_DIR: &str = "docs";

/// Every `§N` cited from Rust source, as (section, file).
///
/// This used to attribute each citation to one of four documents by reading
/// backwards from the `§` for a filename, and before that it checked all four
/// against RESEARCH.md — so a citation naming another document passed only
/// because RESEARCH.md happened to have a section of the same number, and a
/// correct citation of a section RESEARCH.md lacked would have failed. Both
/// directions were wrong: the guard was passing for the wrong reason. The other
/// three documents were folded away in 2026-08 and the pointers to them removed
/// from the comments that carried them, so one namespace is left and there is
/// nothing to attribute.
fn cited_sections(root: &Path) -> BTreeSet<(String, String)> {
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
            cites.insert((number.to_string(), rel.clone()));
        }
    }
    cites
}

/// Every section a comment points at must exist, or the pointer is worse than no
/// pointer: it costs a reader a search that ends nowhere.
#[test]
fn research_citations_resolve() {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join(DOCS_DIR).join(DOC))
        .unwrap_or_else(|_| panic!("{DOC} should exist in {DOCS_DIR}/"));
    let defined = defined_sections(&text);
    assert!(!defined.is_empty(), "parsed no section headings out of {DOC}");

    let mut dangling: Vec<String> = cited_sections(&root)
        .into_iter()
        .filter(|(number, _)| !defined.contains(number))
        .map(|(number, file)| format!("{DOC} §{number} (cited in {file})"))
        .collect();
    dangling.sort();
    dangling.dedup();

    assert!(
        dangling.is_empty(),
        "source comments cite sections that do not exist:\n  {}\n\
         Either the section was renumbered or the citation was a guess.\n\
         (Every § in a comment is read as {DOC}.)",
        dangling.join("\n  ")
    );
}

/// The files CLAUDE.md tells an agent to look at have to be there. It is the
/// first thing read in a session and the last thing anyone thinks to update.
///
/// Only repo-rooted paths are checked, and only paths in *this* repo: modules
/// named relative to a layer (`build/embed`) and paths in the sibling
/// gorp-bench checkout (`../gorp-bench/...`, `bench/...`) are unambiguous in
/// context but not resolvable from this root without guessing which prefix to
/// try — and a checker that guesses is one that fails for the wrong reason.
#[test]
fn claude_md_paths_exist() {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("CLAUDE.md")).expect("CLAUDE.md");

    const ROOTED: [&str; 6] = ["crates/", "eval/", "tools/", "tests/", ".github/", "docs/"];

    // Gitignored trees cannot be required to exist: `eval/data/` is scratch
    // and multi-gigabyte, present on a developer's disk and absent in CI.
    // Read the patterns rather than hardcoding them, so this stays true when
    // .gitignore changes.
    let ignored: Vec<String> = std::fs::read_to_string(root.join(".gitignore"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().trim_start_matches('/').trim_end_matches('/').to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.contains('*'))
        .collect();

    let mut missing = Vec::new();
    for token in text.split('`').skip(1).step_by(2) {
        let candidate = token.trim().trim_end_matches('/');
        if !ROOTED.iter().any(|p| candidate.starts_with(p)) {
            continue;
        }
        if candidate.contains(' ') || candidate.contains('*') || candidate.contains("::") {
            continue;
        }
        if ignored.iter().any(|i| candidate == i || candidate.starts_with(&format!("{i}/"))) {
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
