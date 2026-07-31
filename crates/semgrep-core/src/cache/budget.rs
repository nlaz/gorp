//! Space reclamation: what the cache is allowed to keep, and what goes first.

use super::compat::{cache_generation, gc_old_generations};
use std::path::{Path, PathBuf};

/// Total cache budget in bytes. `SEMGREP_CACHE_MAX_BYTES` overrides.
///
/// Default 2 GiB: the median real repo indexes to ~5 MB, so this holds
/// hundreds of ordinary projects, while one kernel-scale corpus (946 MB) can
/// still fit alongside a few others. Without a cap the cache only grows —
/// which was the honest caveat in the README, and is the thing that turns a
/// cache into a slow disk leak.
pub fn cache_max_bytes() -> u64 {
    std::env::var("SEMGREP_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024)
}

/// How long an entry may sit without a `meta.json` before it is presumed
/// abandoned rather than mid-build. Generous: a kernel-scale build takes ~45 s.
const ABANDONED_AFTER_SECS: u64 = 600;

#[derive(Debug, Clone)]
pub struct CacheEntryInfo {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub bytes: u64,
    /// Seconds since this entry was last read or written.
    pub age_secs: u64,
    /// False once the indexed directory no longer exists — a dead entry that
    /// can never be useful again.
    pub root_exists: bool,
    /// Registered but never published: `root.txt` without `meta.json`. Either a
    /// build in flight right now, or one that was interrupted and left this
    /// behind. Age tells the two apart.
    pub incomplete: bool,
}

/// Bytes an entry occupies. Recursive: entries happen to be flat today, which
/// made the non-recursive version accidentally correct, but "accidentally
/// correct" is how a budget silently stops counting half the cache.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter_map(|e| Some((e.path(), e.metadata().ok()?)))
        .map(|(path, m)| if m.is_dir() { dir_bytes(&path) } else { m.len() })
        .sum()
}

/// Every entry in this generation, with size and recency. Powers both the
/// budget enforcer and `semgrep cache --status`.
pub fn cache_status() -> Vec<CacheEntryInfo> {
    let now = std::time::SystemTime::now();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(cache_generation()) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        let Ok(root) = std::fs::read_to_string(dir.join("root.txt")) else { continue };
        let root = PathBuf::from(root.trim());
        // `last_check` is touched by read-repair, `meta.json` by a build, so
        // the newer of the two is when this entry was last actually used.
        // `root.txt` is the fallback: an entry mid-build has only that.
        let age = ["last_check", "meta.json", "root.txt"]
            .iter()
            .filter_map(|f| std::fs::metadata(dir.join(f)).ok()?.modified().ok())
            .filter_map(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .min()
            .unwrap_or(u64::MAX);
        out.push(CacheEntryInfo {
            bytes: dir_bytes(&dir),
            root_exists: root.is_dir(),
            // A `.building-`/`.trash-` directory is counted — it occupies real
            // space and must be reclaimable if the build that made it died —
            // but never treated as a usable entry, whatever it contains. A
            // finished-but-unswapped staging dir has a complete meta.json and
            // would otherwise read as healthy and sit there forever.
            incomplete: crate::store::is_transient(&dir) || !dir.join("meta.json").is_file(),
            dir,
            root,
            age_secs: age,
        });
    }
    out.sort_by_key(|e| e.age_secs);
    out
}

/// What one reclamation pass did.
///
/// `stuck` is the reason this is a struct rather than the pair it used to be:
/// a delete that fails is not a detail the caller can be left to not know
/// about, and `semgrep-core` does not print — every word the user sees is
/// written by the CLI's `out` module, which is what keeps "stdout is data,
/// stderr is commentary" checkable in one place.
#[derive(Debug, Default, Clone)]
pub struct Reclaimed {
    pub removed: usize,
    pub freed: u64,
    /// Entries the enforcer chose but could not delete. Non-empty means the
    /// cache is still over budget and no further eviction was attempted.
    pub stuck: Vec<PathBuf>,
}

/// Drop dead entries, then evict least-recently-used until under budget.
/// Called after a write, so the cost lands on the path that already pays for a
/// full corpus pass.
pub fn enforce_budget() -> Reclaimed {
    enforce_budget_with_cap(cache_max_bytes(), ABANDONED_AFTER_SECS)
}

/// [`enforce_budget`], sparing one entry from LRU eviction.
///
/// For the entry a write just produced. Reclamation runs after registration so
/// the enforcer can see what triggered it (FIXES.md #5) — but seeing it, it
/// evicted it, and the query that had just paid for a full index build missed on
/// re-discovery and streamed the corpus as well. Protecting the new entry makes
/// that "pay once, keep it"; if it alone exceeds the cap it survives this call
/// and is evicted by the next write like anything else.
pub fn enforce_budget_protecting(keep: &Path) -> Reclaimed {
    enforce_budget_inner(cache_max_bytes(), ABANDONED_AFTER_SECS, Some(keep))
}

/// [`enforce_budget`] with explicit thresholds. Separated so a caller — a test,
/// or a future `--max-bytes` flag — can exercise reclamation without mutating
/// the process environment that `cache_max_bytes` reads.
pub fn enforce_budget_with_cap(cap: u64, abandoned_after_secs: u64) -> Reclaimed {
    enforce_budget_inner(cap, abandoned_after_secs, None)
}

fn enforce_budget_inner(cap: u64, abandoned_after_secs: u64, keep: Option<&Path>) -> Reclaimed {
    let mut entries = cache_status();
    let mut out = Reclaimed::default();
    let (mut n, mut freed) = (0usize, 0u64);

    // 1. Entries that can never serve a query, in either of the two ways:
    //    the repo is gone (a moved or deleted checkout would otherwise hold
    //    its index forever), or the build that registered them never published
    //    a meta.json and is long past finishing. A young incomplete entry is
    //    left alone — that is a build happening right now.
    entries.retain(|e| {
        let dead = !e.root_exists || (e.incomplete && e.age_secs >= abandoned_after_secs);
        if !dead {
            return true;
        }
        if std::fs::remove_dir_all(&e.dir).is_ok() {
            n += 1;
            freed += e.bytes;
        }
        false
    });

    // 2. LRU until under the cap. Oldest first; `cache_status` sorts by
    //    recency ascending, so walk from the back.
    //
    //    A failed delete used to be silent and non-terminal: the victim was
    //    popped whether or not it went, and `total` fell only on success, so one
    //    undeletable directory made the loop chew through every healthy entry
    //    behind it — four entries in, one out, and the survivor was the
    //    undeletable one, at exit 0 with no warning (SIMULATION.md §1.7). It
    //    stops now. An entry that will not delete is a permissions anomaly, not
    //    ordinary pressure, and pressing on converts it into the loss of the
    //    whole cache while freeing nothing.
    let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
    while total > cap {
        let Some(victim) = entries.pop() else { break };
        // The entry the caller just built is not a candidate — but skipping it
        // is not a reason to stop, since something older may still be
        // evictable. Its bytes stay in `total`, so a cache over cap solely
        // because of it simply runs the list out and exits.
        if keep.is_some_and(|k| k == victim.dir) {
            continue;
        }
        if std::fs::remove_dir_all(&victim.dir).is_ok() {
            total = total.saturating_sub(victim.bytes);
            n += 1;
            freed += victim.bytes;
        } else {
            out.stuck.push(victim.dir);
            break;
        }
    }
    out.removed = n;
    out.freed = freed;
    out
}

/// Delete every entry in every generation. `semgrep cache --clear`.
pub fn cache_clear() -> (usize, u64) {
    let mut n = 0;
    let mut freed = 0;
    for e in cache_status() {
        if std::fs::remove_dir_all(&e.dir).is_ok() {
            n += 1;
            freed += e.bytes;
        }
    }
    gc_old_generations();
    (n, freed)
}
