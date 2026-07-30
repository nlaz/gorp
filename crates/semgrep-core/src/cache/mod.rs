//! The index as a cache (RESEARCH.md §8).
//!
//! A query path resolves to an index by: (1) `.semgrep/` at the path itself,
//! (2) `.semgrep/` at an ancestor, walking up the way git finds `.git` and
//! stopping at the repo boundary, `$HOME`, or the filesystem root, then (3) the
//! deepest central-cache entry whose root contains the path.
//!
//! Entries live under `$SEMGREP_CACHE_DIR` (default `~/.cache/semgrep`), keyed
//! by canonical corpus root, and are written as a side effect of cold ranked
//! searches. A cache entry is disposable: any failure to read one is a miss.
//!
//! Split by concern — resolution (here), compatibility generations (`gen`),
//! space reclamation (`budget`), and the read-repair overlay that keeps a warm
//! answer true of the current tree (`repair`).

mod budget;
mod compat;
pub mod repair;

pub use budget::{
    CacheEntryInfo, cache_clear, cache_max_bytes, cache_status, enforce_budget,
    enforce_budget_with_cap,
};
pub use compat::{cache_base, cache_entries, cache_generation, compat_key, gc_old_generations};

use crate::store::{self, BuildOptions};
use anyhow::Result;
use std::path::{Path, PathBuf};
use store::{exists, index_dir};

pub struct Discovered {
    /// Directory holding meta.json / chunks.bin / bm25.flat / emb.bin.
    pub index_dir: PathBuf,
    /// Canonical corpus root the index describes.
    pub root: PathBuf,
    /// Query scope relative to `root`, `/`-separated ("" = whole corpus).
    pub prefix: String,
    /// True for a central-cache entry (disposable: any failure to load it is
    /// a miss). False for a repo-local `.semgrep`, an explicit artifact whose
    /// failures the user should see.
    pub from_cache: bool,
}

fn rel_prefix(root: &Path, scope: &Path) -> String {
    scope.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default()
}

/// Resolve the index that should serve a query over `query_root`, if any.
pub fn discover(query_root: &Path) -> Option<Discovered> {
    let canon = std::fs::canonicalize(query_root).ok()?;
    if !canon.is_dir() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // (1)+(2): in-tree .semgrep at the scope or an ancestor.
    let mut cur = canon.clone();
    loop {
        if exists(&cur) {
            return Some(Discovered {
                index_dir: index_dir(&cur),
                prefix: rel_prefix(&cur, &canon),
                root: cur,
                from_cache: false,
            });
        }
        // Stop *after* checking: the repo root itself may hold the index,
        // but the walk must not escape into an enclosing repo or past $HOME.
        if cur.join(".git").exists() || home.as_deref() == Some(cur.as_path()) {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }

    // (3): deepest central-cache entry covering the scope. Entries live under
    // a *generation* directory keyed to this binary's format+table (see
    // `compat_key`), so an entry written by an incompatible binary is simply
    // not found here — no error to surface, nothing to evict on a read path.
    cache_entries()
        .into_iter()
        .filter(|(dir, root)| canon.starts_with(root) && dir.join("meta.json").is_file())
        .max_by_key(|(_, root)| root.components().count())
        .map(|(dir, root)| Discovered {
            index_dir: dir,
            prefix: rel_prefix(&root, &canon),
            root,
            from_cache: true,
        })
}

/// The entry directory for a canonical root, allocating a collision-free
/// name on first use (hash bucket + root.txt verification).
fn cache_entry_dir(root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let base = cache_generation();
    // Name entries after the directory they cache, not just a hash, so that
    // `ls ~/.cache/semgrep/*` and `du -sh *` are readable — you can see which
    // repos are costing you space without opening root.txt.
    // Last two components, so `.../semgrep-core/src` reads as
    // "semgrep-core-src" rather than an uninformative "src".
    let mut comps: Vec<String> = root
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    comps.reverse();
    let label: String = comps
        .join("-")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
        .chars()
        .rev()
        .take(28)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let stem = format!("{label}-{:08x}", h.finish() as u32);
    for i in 0..64 {
        let dir = if i == 0 { base.join(&stem) } else { base.join(format!("{stem}-{i}")) };
        match std::fs::read_to_string(dir.join("root.txt")) {
            Ok(r) if Path::new(r.trim()) != root => continue, // collision
            _ => return dir,
        }
    }
    base.join(stem) // unreachable in practice
}

/// Write-through: build a cache entry covering `root` (must be canonical),
/// then retire any entries for scopes strictly inside it (scope promotion —
/// the wider entry serves every descendant via the prefix filter).
pub fn write_cache_entry(
    root: &Path,
    opts: &BuildOptions,
    progress: impl FnMut(usize, usize),
) -> Result<PathBuf> {
    let dir = cache_entry_dir(root);
    std::fs::create_dir_all(&dir)?;
    // root.txt first, before a single byte of index. It is what makes an entry
    // *enumerable*, and only enumerable entries can be counted or reclaimed —
    // written afterwards, a build interrupted partway (Ctrl-C during the first
    // search of a large repo, which is exactly when people interrupt) left a
    // directory that `cache --status` could not see and `cache --prune` could
    // not free. It does not make the entry discoverable: that needs meta.json,
    // which `build_at` writes last.
    std::fs::write(dir.join("root.txt"), root.to_string_lossy().as_bytes())?;
    store::build_at(&dir, root, opts, progress)?;

    // Reclaim only after the entry is complete and registered, so the budget
    // enforcer actually sees what was just built. Running it before the write
    // meant a corpus larger than the whole budget evicted every *other* entry
    // and then sat over the cap until some later write noticed.
    gc_old_generations();
    enforce_budget();

    // Scope promotion: a wider entry serves every descendant through the
    // prefix filter, so narrower ones are now dead weight.
    for (edir, eroot) in cache_entries() {
        if eroot != root && eroot.starts_with(root) {
            let _ = std::fs::remove_dir_all(edir);
        }
    }
    Ok(dir)
}
