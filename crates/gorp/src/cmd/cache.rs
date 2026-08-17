//! `gorp cache` — inspect or reclaim the search cache.

use crate::out::PROG;
use crate::out::human;
use anyhow::Result;
use gorp_core::cache;

pub fn run(prune: bool, clear: bool) -> Result<i32> {
    if clear {
        let central = cache::cache_clear();
        // In-tree indexes are swept from the working directory down. There is
        // no directory to enumerate them — each lives wherever its corpus does
        // — so where the caller stands is the only statement of scope on offer,
        // and `--clear` from `~` means every index under `~`.
        let here = std::env::current_dir()?;
        let local = cache::clear_local(&here);
        println!("cleared {} entries, reclaimed {}", central.removed, human(central.freed));
        // Silent at zero: the line above already says nothing was there, and a
        // second line saying it again is noise on the common case.
        if local.removed > 0 {
            println!(
                "cleared {} in-tree {} under {}, reclaimed {}",
                local.removed,
                if local.removed == 1 { "index" } else { "indexes" },
                here.display(),
                human(local.freed)
            );
        }
        for dir in central.stuck.iter().chain(&local.stuck) {
            eprintln!("{PROG}: warning: cannot remove {}", dir.display());
        }
        return Ok(crate::EXIT_FOUND);
    }
    if prune {
        // An explicit prune reclaims everything reclaimable, not just the
        // current generation: automatic GC only runs on a cold write, so a
        // user who only queries warm scopes would never reclaim anything.
        cache::gc_old_generations();
        let r = cache::enforce_budget();
        println!("pruned {} entries, reclaimed {}", r.removed, human(r.freed));
        // Stderr, because it is commentary on a stdout report — and it has to
        // be said at all: an entry the enforcer cannot delete stops reclamation
        // and leaves the cache over budget, which used to happen at exit 0 with
        // nothing printed anywhere.
        for dir in &r.stuck {
            eprintln!(
                "{PROG}: warning: cannot remove {} — cache is still over budget",
                dir.display()
            );
        }
    }
    let entries = cache::cache_status();
    let total: u64 = entries.iter().map(|e| e.bytes).sum();
    let cap = cache::cache_max_bytes();
    println!(
        "{}  ({} entries, {} of {} budget)",
        cache::cache_base().display(),
        entries.len(),
        human(total),
        human(cap)
    );
    println!("generation {}", cache::compat_key());
    for e in &entries {
        let age = if e.age_secs > 86_400 {
            format!("{}d", e.age_secs / 86_400)
        } else if e.age_secs > 3_600 {
            format!("{}h", e.age_secs / 3_600)
        } else {
            format!("{}m", e.age_secs / 60)
        };
        println!(
            "  {:>9}  {:>5} ago  {}{}",
            human(e.bytes),
            age,
            e.root.display(),
            if e.root_exists { "" } else { "   (gone — prunable)" }
        );
    }
    if !entries.is_empty() {
        println!(
            "\ngorp cache --prune to reclaim, --clear to remove all; \
                  GORP_CACHE_MAX_BYTES sets the budget"
        );
    }
    Ok(crate::EXIT_FOUND)
}
