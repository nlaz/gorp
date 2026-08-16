//! Comparing an index's file table against the tree as it is now.

use crate::FileMeta;

/// What changed between an index's file table and the tree as it is now.
///
/// Paths are index-root-relative (the form the file table stores), so both
/// callers can look files up and read them without further translation.
/// Split out of the two places that used to compute it inline — whole-index
/// staleness counting and scoped read-repair — which had drifted into two
/// implementations of one predicate, neither testable without a filesystem.
#[derive(Debug, Default, PartialEq)]
pub struct Diff {
    /// Live files the index has never seen.
    pub added: Vec<String>,
    /// (file_id, path) for files the index has, whose size or mtime moved.
    pub modified: Vec<(u32, String)>,
    /// file_ids the index has that are no longer on disk.
    pub deleted: Vec<u32>,
}

impl Diff {
    /// Files that need reading to bring an index up to date.
    pub fn stale_paths(&self) -> impl Iterator<Item = &String> {
        self.added.iter().chain(self.modified.iter().map(|(_, p)| p))
    }

    /// Base file_ids whose indexed content must not be served.
    pub fn tombstones(&self) -> impl Iterator<Item = u32> + '_ {
        self.modified.iter().map(|&(id, _)| id).chain(self.deleted.iter().copied())
    }

    /// Number of drifted files, each counted once.
    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Diff `live` (walked under `prefix`, so its paths are prefix-relative)
/// against `indexed` (the whole file table, index-root-relative).
///
/// `prefix` scopes the comparison: only indexed files under it participate, so
/// a query against a subdirectory does not see the rest of the corpus as
/// deleted. `""` compares the whole tree.
///
/// A file counts as modified when size or mtime differs. That is cheap and
/// wrong only in the direction that matters least: a rewrite preserving both is
/// missed, which no content-free check can catch.
pub fn diff(indexed: &[FileMeta], live: &[FileMeta], prefix: &str) -> Diff {
    let under_prefix = |path: &str| {
        prefix.is_empty() || path.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
    };
    let mut known: std::collections::HashMap<&str, (u32, u64, u64)> = indexed
        .iter()
        .enumerate()
        .filter(|(_, f)| under_prefix(&f.path))
        .map(|(id, f)| (f.path.as_str(), (id as u32, f.size, f.mtime)))
        .collect();

    let mut out = Diff::default();
    for f in live {
        let path =
            if prefix.is_empty() { f.path.clone() } else { format!("{}/{}", prefix, f.path) };
        match known.remove(path.as_str()) {
            Some((_, size, mtime)) if size == f.size && mtime == f.mtime => {}
            Some((id, _, _)) => out.modified.push((id, path)),
            None => out.added.push(path),
        }
    }
    // Whatever the index still claims under this scope is gone.
    out.deleted.extend(known.into_values().map(|(id, _, _)| id));

    // Deterministic order: `known` is a HashMap, and callers embed these ids in
    // chunk ids and rank order.
    out.added.sort_unstable();
    out.modified.sort_unstable();
    out.deleted.sort_unstable();
    out
}
