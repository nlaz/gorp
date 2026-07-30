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

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_dir() { 0 } else { m.len() })
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
            incomplete: !dir.join("meta.json").is_file(),
            dir,
            root,
            age_secs: age,
        });
    }
    out.sort_by_key(|e| e.age_secs);
    out
}

/// Drop dead entries, then evict least-recently-used until under budget.
/// Returns (entries removed, bytes reclaimed). Called after a write, so the
/// cost lands on the path that already pays for a full corpus pass.
pub fn enforce_budget() -> (usize, u64) {
    enforce_budget_with_cap(cache_max_bytes(), ABANDONED_AFTER_SECS)
}

/// [`enforce_budget`] with explicit thresholds. Separated so a caller — a test,
/// or a future `--max-bytes` flag — can exercise reclamation without mutating
/// the process environment that `cache_max_bytes` reads.
pub fn enforce_budget_with_cap(cap: u64, abandoned_after_secs: u64) -> (usize, u64) {
    let mut entries = cache_status();
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
    let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
    while total > cap {
        let Some(victim) = entries.pop() else { break };
        if std::fs::remove_dir_all(&victim.dir).is_ok() {
            total = total.saturating_sub(victim.bytes);
            n += 1;
            freed += victim.bytes;
        }
    }
    (n, freed)
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
