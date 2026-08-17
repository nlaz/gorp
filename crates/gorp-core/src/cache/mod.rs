//! The index as a cache (RESEARCH.md §8).
//!
//! A query path resolves to an index by: (1) `.gorp/` at the path itself,
//! (2) `.gorp/` at an ancestor, walking up the way git finds `.git` and
//! stopping at the repo boundary, `$HOME`, or the filesystem root, then (3) the
//! deepest central-cache entry whose root contains the path.
//!
//! Entries live under `$GORP_CACHE_DIR` (default `~/.cache/gorp`), keyed
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
    CacheEntryInfo, cache_clear, cache_max_bytes, cache_status, clear_local, enforce_budget,
    enforce_budget_protecting, enforce_budget_with_cap, local_indexes,
};
pub use compat::{cache_base, cache_entries, cache_generation, compat_key, gc_old_generations};

use crate::ChunkParams;
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
    /// a miss). False for a repo-local `.gorp`, an explicit artifact whose
    /// failures the user should see.
    pub from_cache: bool,
}

/// How many times anything has resolved an index in this process.
///
/// Counted here rather than at the call sites because the call sites are the
/// problem: one exact-mode miss reaches three — the CLI's "is this the first
/// search" check, the engine's own resolution, and the CLI's check again before
/// offering a ranked suggestion — and each one canonicalizes the path and scans
/// the generation directory. A per-search counter cannot see the two outside
/// the engine, which are exactly the two nobody remembers are there.
static DISCOVER_CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn discover_calls() -> u32 {
    DISCOVER_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

fn rel_prefix(root: &Path, scope: &Path) -> String {
    scope.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default()
}

/// Resolve the index that should serve a query over `query_root`, if any.
/// Resolve the index that should serve a query over `query_root`, if any.
///
/// `params` is the chunking the caller would use if it had to build. A cache
/// entry only answers when it was built with the same parameters: chunk spans are
/// what a ranked result *is*, so an entry built with a different window answers a
/// different question. A repo-local `.gorp/` is exempt — the user built it
/// deliberately, and its parameters are the ones they chose.
pub fn discover(query_root: &Path, params: &ChunkParams) -> Option<Discovered> {
    DISCOVER_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let canon = std::fs::canonicalize(query_root).ok()?;
    if !canon.is_dir() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // (1)+(2): in-tree .gorp at the scope or an ancestor.
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
        .filter(|(dir, root)| {
            canon.starts_with(root)
                && dir.join("meta.json").is_file()
                && entry_params(dir).as_ref() == Some(params)
        })
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
fn cache_entry_dir(root: &Path, params: &ChunkParams) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let base = cache_generation();
    // Name entries after the directory they cache, not just a hash, so that
    // `ls ~/.cache/gorp/*` and `du -sh *` are readable — you can see which
    // repos are costing you space without opening root.txt.
    // Last two components, so `.../gorp-core/src` reads as
    // "gorp-core-src" rather than an uninformative "src".
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
    // The params tag is in the name as well as in params.txt, so `ls` shows why
    // one repo has two entries.
    let stem = format!("{label}-{:08x}-{}", h.finish() as u32, params_tag(params));
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
) -> Result<(PathBuf, store::BuildStats)> {
    let dir = cache_entry_dir(root, &opts.params);
    // The build assembles itself in a staging sibling and is swapped in by
    // rename, so no reader can ever see a partly written entry and no mapped
    // file is ever rewritten underneath one (`store::build_staged`).
    let staging = store::staging_path(&dir);
    std::fs::create_dir_all(&staging)?;
    // root.txt first, before a single byte of index. It is what makes an entry
    // *enumerable*, and only enumerable entries can be counted or reclaimed —
    // written afterwards, a build interrupted partway (Ctrl-C during the first
    // search of a large repo, which is exactly when people interrupt) left a
    // directory that `cache --status` could not see and `cache --prune` could
    // not free. Writing it into the staging directory keeps that property: the
    // staging directory lives inside the generation directory, so `cache_status`
    // still counts it, while `cache_entries` skips it so it can never be
    // discovered or promoted over.
    std::fs::write(staging.join("root.txt"), root.to_string_lossy().as_bytes())?;
    // Alongside root.txt, and for the same reason: discovery has to know what
    // this entry is *for* without parsing meta.json, whose file table can run to
    // megabytes on a large corpus.
    std::fs::write(staging.join("params.txt"), params_tag(&opts.params))?;
    let stats = store::build_staged(&staging, &dir, root, opts, progress)?;

    // Reclaim only after the entry is complete and registered, so the budget
    // enforcer actually sees what was just built. Running it before the write
    // meant a corpus larger than the whole budget evicted every *other* entry
    // and then sat over the cap until some later write noticed.
    //
    // Protecting the new entry is the other half. Without it the enforcer saw
    // the entry it had just been given and evicted it — the query then missed on
    // re-discovery and streamed anyway, paying for a complete build *and* a
    // complete cold search and keeping neither (5 of 8 queries under budget
    // pressure).
    gc_old_generations();
    enforce_budget_protecting(&dir);

    // Scope promotion: a wider entry serves every descendant through the
    // prefix filter, so narrower ones are now dead weight.
    for (edir, eroot) in cache_entries() {
        // Same parameters only: a narrower entry built with a different window
        // is not superseded by this one, because it answers a different question.
        let same_params = entry_params(&edir).as_ref() == Some(&opts.params);
        if same_params && eroot != root && eroot.starts_with(root) {
            let _ = std::fs::remove_dir_all(edir);
        }
    }
    Ok((dir, stats))
}

/// The chunking an entry was built with, as it appears in its name and in
/// `params.txt`. Short and legible on purpose: `ls` on the cache should show why
/// one repo has two entries.
///
/// `max_file_bytes` is not in the tag because it is not reachable from the CLI.
/// If it ever becomes tunable it has to join this, or entries built with
/// different size caps will collide.
///
/// A character budget gets a `c` tag rather than a `w` one. It has to: a
/// budgeted entry and a line-windowed entry cut the same tree into different
/// chunks, so sharing a directory name would let one answer a query posed
/// against the other — which is exactly the `--window` sweep's cache
/// poisoning, one parameter later.
/// The budget tag carries `window` as well, because in budget mode the overlap
/// is `budget * overlap / window` — all three decide where the cuts land, and a
/// tag that omits one makes an entry undiscoverable by the options that built
/// it (the second search re-streams forever, silently).
/// Function mode gets an `f` tag for the same reason budget gets a `c` one,
/// and it carries `window`/`overlap` because gap regions between definitions
/// are still window-cut. Emitted unconditionally — even a grammarless build
/// must *name* function params deterministically — but only parsed back
/// behind the feature, so a build without grammars reclaims `f` entries
/// instead of serving them with the wrong cutter.
fn params_tag(params: &ChunkParams) -> String {
    match (params.function, params.budget) {
        (Some(f), _) => format!("f{}w{}o{}", f, params.window, params.overlap),
        (None, Some(b)) => format!("c{}w{}o{}", b, params.window, params.overlap),
        (None, None) => format!("w{}o{}", params.window, params.overlap),
    }
}

/// Read back an entry's chunk parameters. `None` for an entry from a build that
/// predates `params.txt`, which then matches nothing and is reclaimed as
/// unreadable — the same degradation any other unusable entry gets. A binary
/// without budget support reads a `c` tag the same way, which is the correct
/// degradation rather than a mismatched hit.
fn entry_params(dir: &Path) -> Option<ChunkParams> {
    let tag = std::fs::read_to_string(dir.join("params.txt")).ok()?;
    let tag = tag.trim();
    #[cfg(feature = "func-chunk")]
    if let Some(rest) = tag.strip_prefix('f') {
        let (f, rest) = rest.split_once('w')?;
        let (window, overlap) = rest.split_once('o')?;
        return Some(ChunkParams {
            window: window.parse().ok()?,
            overlap: overlap.parse().ok()?,
            function: Some(f.parse().ok()?),
            ..Default::default()
        });
    }
    let (budget, rest) = match tag.strip_prefix('c') {
        Some(rest) => {
            let (b, rest) = rest.split_once('w')?;
            (Some(b.parse().ok()?), rest)
        }
        None => (None, tag.strip_prefix('w')?),
    };
    let (window, overlap) = rest.split_once('o')?;
    Some(ChunkParams {
        window: window.parse().ok()?,
        overlap: overlap.parse().ok()?,
        budget,
        ..Default::default()
    })
}

#[cfg(test)]
mod params_tag_tests {
    use super::*;

    /// Round-trip, because the tag is the only record of an entry's chunking
    /// and a tag that does not parse back is an entry that answers nothing.
    #[test]
    fn every_tag_parses_back_to_the_params_that_wrote_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut cases = vec![
            ChunkParams::default(),
            ChunkParams { window: 8, overlap: 2, ..Default::default() },
            ChunkParams { budget: Some(800), ..Default::default() },
            ChunkParams { window: 8, overlap: 2, budget: Some(200), ..Default::default() },
        ];
        #[cfg(feature = "func-chunk")]
        cases.extend([
            ChunkParams { function: Some(crate::FUNC_CAP_DEFAULT), ..Default::default() },
            ChunkParams { window: 8, overlap: 2, function: Some(48), ..Default::default() },
        ]);
        for params in cases {
            std::fs::write(dir.path().join("params.txt"), params_tag(&params)).unwrap();
            assert_eq!(entry_params(dir.path()), Some(params), "{}", params_tag(&params));
        }
    }

    #[test]
    fn a_budget_and_a_line_window_never_share_a_tag() {
        let lines = ChunkParams { window: 8, overlap: 2, ..Default::default() };
        let budgeted = ChunkParams { budget: Some(8), ..lines };
        assert_ne!(params_tag(&lines), params_tag(&budgeted));
    }

    /// The three modes are three tag prefixes; none can serve another's query.
    /// Function-over-budget precedence is part of the identity: params carrying
    /// both must tag as function, matching what the cutter does with them.
    #[test]
    fn a_function_entry_never_shares_a_tag_with_either_other_mode() {
        let lines = ChunkParams { window: 8, overlap: 2, ..Default::default() };
        let f = ChunkParams { function: Some(96), ..lines };
        assert_ne!(params_tag(&lines), params_tag(&f));
        assert_ne!(params_tag(&ChunkParams { budget: Some(96), ..lines }), params_tag(&f));
        let both = ChunkParams { budget: Some(800), function: Some(96), ..lines };
        assert!(params_tag(&both).starts_with('f'), "function wins over budget in the tag");
    }
}
