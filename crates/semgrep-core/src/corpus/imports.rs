//! Import extraction for the file graph (RESEARCH.md §35.3): each language's
//! import statements, tree-sitter parsed, emitted as normalized specifier
//! strings and resolved against the corpus file table.
//!
//! Two deliberate departures from `funcchunk`:
//!
//! - **ERROR nodes do not bail.** `cut` must abandon a file whose parse is
//!   wrong because chunk boundaries have to be exact; an import is a local
//!   node, so whatever subtrees did parse still yield true edges. A file that
//!   fails to parse at all simply contributes none.
//! - **Extraction and resolution are separate.** `extract` is a pure function
//!   of one file's text; `Resolver` owns the corpus-wide suffix index. An
//!   import that resolves to nothing — a stdlib module, a vendored package,
//!   an ambiguous suffix — drops silently, the same posture `bridge` takes
//!   toward unreadable files.
//!
//! Specifiers are normalized to `/`-separated segments with no extension:
//! `from a.b import c` → `a/b`, `use crate::foo::bar` → `foo/bar`,
//! `#include <linux/mm.h>` → `linux/mm`, `import "./util"` → `./util`.

use super::funcchunk::lang_for_path;
use tree_sitter::{Node, Parser};

/// Import specs of one file, best-effort. Empty on unsupported extensions or
/// a text tree-sitter cannot parse at all.
pub(crate) fn extract(rel_path: &str, text: &str) -> Vec<String> {
    let Some(lang) = lang_for_path(rel_path) else { return Vec::new() };
    let ext = rel_path.rsplit('.').next().unwrap_or_default();
    let mut parser = Parser::new();
    if parser.set_language(&(lang.ts)()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else { return Vec::new() };
    let mut out = Vec::new();
    walk(tree.root_node(), text.as_bytes(), ext, &mut out);
    out.sort_unstable();
    out.dedup();
    out
}

fn walk(node: Node, src: &[u8], ext: &str, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(spec) = spec_of(child, src, ext) {
            if !spec.is_empty() {
                out.push(spec);
            }
            continue; // an import node's children are already consumed
        }
        walk(child, src, ext, out);
    }
}

/// The specifier of `node` when it IS an import statement for `ext`'s
/// language, else None. Node-kind tables per grammar; the string forms are
/// what each grammar names them, checked against the pinned grammar crates.
fn spec_of(node: Node, src: &[u8], ext: &str) -> Option<String> {
    let kind = node.kind();
    let take = |n: Node| n.utf8_text(src).ok().map(str::to_string);
    match ext {
        "py" => match kind {
            // `import a.b, c` / `from .x import y` — the module names are the
            // dotted_name / relative_import children.
            "import_statement" | "import_from_statement" => {
                let mut cursor = node.walk();
                let specs: Vec<String> = node
                    .children(&mut cursor)
                    .filter(|c| matches!(c.kind(), "dotted_name" | "relative_import" | "aliased_import"))
                    .filter_map(|c| match c.kind() {
                        // `import a.b as x` — the name is the first child.
                        "aliased_import" => c.child(0).and_then(take),
                        _ => take(c),
                    })
                    .map(|s| normalize_dotted(&s))
                    .collect();
                // `from a import b`: only the FROM module names a file; the
                // imported names may be symbols. Keep the module list only.
                Some(specs.into_iter().take(1).collect::<Vec<_>>().join(""))
            }
            _ => None,
        },
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" => match kind {
            // `import x from "spec"` / `export ... from "spec"`.
            "import_statement" | "export_statement" => {
                string_child(node, src).map(|s| trim_ext(&s))
            }
            // `require("spec")` / `import("spec")`.
            "call_expression" => {
                let callee = node.child(0)?.utf8_text(src).ok()?;
                if callee == "require" || callee == "import" {
                    string_child(node, src).map(|s| trim_ext(&s))
                } else {
                    None
                }
            }
            _ => None,
        },
        "rs" => (kind == "use_declaration").then(|| {
            let arg = node
                .child_by_field_name("argument")
                .and_then(|a| a.utf8_text(src).ok())
                .unwrap_or_default();
            normalize_rust_use(arg)
        }),
        "go" => match kind {
            "import_spec" => string_child(node, src),
            _ => None,
        },
        "c" | "h" => (kind == "preproc_include").then(|| {
            let path = node
                .child_by_field_name("path")
                .and_then(|p| p.utf8_text(src).ok())
                .unwrap_or_default();
            trim_ext(path.trim_matches(|c| matches!(c, '"' | '<' | '>')))
        }),
        "java" => (kind == "import_declaration").then(|| {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|c| c.kind() == "scoped_identifier" || c.kind() == "identifier")
                .and_then(|c| c.utf8_text(src).ok())
                .map(|s| normalize_dotted(s))
                .unwrap_or_default()
        }),
        "rb" => (kind == "call").then(|| {
            let callee = node
                .child_by_field_name("method")
                .and_then(|m| m.utf8_text(src).ok())
                .unwrap_or_default();
            if callee == "require" || callee == "require_relative" {
                string_child(node, src).map(|s| trim_ext(&s)).unwrap_or_default()
            } else {
                String::new()
            }
        }),
        "php" => (kind == "namespace_use_declaration").then(|| {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|c| c.kind() == "namespace_use_clause")
                .and_then(|c| c.utf8_text(src).ok())
                .map(|s| s.replace('\\', "/"))
                .unwrap_or_default()
        }),
        _ => None,
    }
}

/// First string literal anywhere under `node`, unquoted.
fn string_child(node: Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string" || child.kind() == "string_literal" || child.kind() == "interpreted_string_literal" {
            let s = child.utf8_text(src).ok()?;
            return Some(s.trim_matches(|c| matches!(c, '"' | '\'' | '`')).to_string());
        }
        if let Some(s) = string_child(child, src) {
            return Some(s);
        }
    }
    None
}

fn normalize_dotted(s: &str) -> String {
    // Leading dots (Python relative imports) survive as `../`-ish markers:
    // one dot is the current package, each further dot one level up.
    let dots = s.chars().take_while(|&c| c == '.').count();
    let rest = s[dots..].replace('.', "/");
    match dots {
        0 => rest,
        1 => format!("./{rest}"),
        n => format!("{}{rest}", "../".repeat(n - 1)),
    }
}

fn normalize_rust_use(arg: &str) -> String {
    // `crate::a::b::{c, d}` → `a/b`. Group/glob tails and generic params are
    // beyond a suffix resolver; keep the stem path.
    let stem: String = arg
        .split("::")
        .take_while(|seg| !seg.starts_with('{') && *seg != "*")
        .filter(|seg| !matches!(*seg, "crate" | "self" | "super"))
        .collect::<Vec<_>>()
        .join("/");
    stem.split(['{', '<']).next().unwrap_or_default().trim_end_matches('/').to_string()
}

fn trim_ext(s: &str) -> String {
    match s.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 4 && !ext.contains('/') => {
            stem.to_string()
        }
        _ => s.to_string(),
    }
}

/// Resolves normalized specs to corpus file ids by path suffix.
///
/// Keys are each file's extension-less path plus every tail of up to three
/// segments; a spec resolves through its own longest tail. Ambiguity is a
/// deny, not a guess: a spec matching more than [`AMBIGUITY_CAP`] files names
/// a package, not a file, and wiring a seed to a dozen "utils" would be the
/// locale-pack failure §33 already met.
pub(crate) struct Resolver {
    by_suffix: std::collections::HashMap<String, Vec<u32>>,
}

const AMBIGUITY_CAP: usize = 4;
const MAX_TAIL_SEGMENTS: usize = 3;

impl Resolver {
    pub(crate) fn new(paths: impl Iterator<Item = (u32, String)>) -> Self {
        let mut by_suffix: std::collections::HashMap<String, Vec<u32>> =
            std::collections::HashMap::new();
        for (id, path) in paths {
            let stem = trim_ext(&path);
            // `a/b/__init__` answers to `a/b` — Python package imports.
            let stem = stem.strip_suffix("/__init__").unwrap_or(&stem);
            let segs: Vec<&str> = stem.split('/').collect();
            for k in 1..=MAX_TAIL_SEGMENTS.min(segs.len()) {
                let key = segs[segs.len() - k..].join("/");
                by_suffix.entry(key).or_default().push(id);
            }
        }
        Resolver { by_suffix }
    }

    /// File ids `spec` (as `extract` emits it) resolves to, from `from_dir`
    /// (the importing file's directory, for `./`-relative specs). Empty when
    /// unknown or ambiguous.
    pub(crate) fn resolve(&self, from_dir: &str, spec: &str) -> Vec<u32> {
        let spec = if let Some(rel) = spec.strip_prefix("./") {
            if from_dir.is_empty() { rel.to_string() } else { format!("{from_dir}/{rel}") }
        } else if spec.starts_with("../") {
            let mut dir: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
            let mut rest = spec;
            while let Some(r) = rest.strip_prefix("../") {
                dir.pop();
                rest = r;
            }
            let mut parts = dir;
            parts.extend(rest.split('/'));
            parts.join("/")
        } else {
            spec.to_string()
        };
        let segs: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return Vec::new();
        }
        // Longest tail that resolves wins; shorter tails only consulted when
        // longer ones name nothing at all.
        for k in (1..=MAX_TAIL_SEGMENTS.min(segs.len())).rev() {
            let key = segs[segs.len() - k..].join("/");
            if let Some(ids) = self.by_suffix.get(&key) {
                if ids.len() <= AMBIGUITY_CAP {
                    return ids.clone();
                }
                return Vec::new(); // ambiguous: a package name, not a file
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_resolve_to_corpus_files_in_each_supported_language() {
        let cases: &[(&str, &str, &str)] = &[
            ("a.py", "import os\nfrom pkg.util import helper\n", "pkg/util"),
            ("a.py", "from .sibling import thing\n", "./sibling"),
            ("a.js", "import { x } from './util.js';\n", "./util"),
            ("a.ts", "const y = require('lib/parse');\n", "lib/parse"),
            ("a.rs", "use crate::cache::compat;\n", "cache/compat"),
            ("a.go", "import (\n\t\"example.com/proj/store\"\n)\n", "example.com/proj/store"),
            ("a.c", "#include <linux/mm.h>\n#include \"local.h\"\n", "linux/mm"),
            ("A.java", "import com.example.util.Strings;\n", "com/example/util/Strings"),
            ("a.rb", "require 'json'\nrequire_relative 'helper'\n", "helper"),
            ("a.php", "<?php\nuse App\\Service\\Mailer;\n", "App/Service/Mailer"),
        ];
        for (path, text, want) in cases {
            let specs = extract(path, text);
            assert!(
                specs.iter().any(|s| s == want),
                "{path}: expected {want:?} among {specs:?}"
            );
        }
    }

    #[test]
    fn a_broken_file_still_yields_the_imports_that_parsed() {
        let text = "import os\nfrom pkg.util import helper\ndef broken(:\n";
        let specs = extract("a.py", text);
        assert!(specs.iter().any(|s| s == "pkg/util"), "got {specs:?}");
    }

    #[test]
    fn the_resolver_joins_relatives_and_denies_ambiguity() {
        let paths = vec![
            (0u32, "src/pkg/util.py".to_string()),
            (1, "src/pkg/sibling.py".to_string()),
            (2, "src/other/util.py".to_string()),
            (3, "src/pkg/__init__.py".to_string()),
        ];
        let r = Resolver::new(paths.into_iter());
        assert_eq!(r.resolve("src/pkg", "./sibling"), vec![1]);
        assert_eq!(r.resolve("", "pkg/util"), vec![0]);
        // Bare `util` names two files — both come back, under the cap.
        let mut both = r.resolve("", "util");
        both.sort_unstable();
        assert_eq!(both, vec![0, 2]);
        // A Python package resolves through its __init__.
        assert!(r.resolve("", "src/pkg").contains(&3));
        assert_eq!(r.resolve("", "no/such/thing"), Vec::<u32>::new());
    }
}
