//! Compatibility generations: which cache entries this binary may read.
//!
//! Entries are namespaced by a key covering the index format, the embedding
//! width, and a fingerprint of the compiled-in table. An entry written by an
//! incompatible binary sorts into a sibling directory and is simply not found —
//! the failure mode is "not there", not "error".

use super::store::FORMAT_VERSION;
use crate::EMBED_DIM;
use std::path::PathBuf;

/// Fingerprint of the compiled-in embedding stack — table, tokenizer, and
/// pooling all at once, since it is just "what does this binary encode this
/// probe to". Two binaries agree iff their vectors are interchangeable.
///
/// `dims` alone is not enough: swapping to a *different* table of the same
/// width (e.g. a code-distilled 256-dim model) would pass a dims check and
/// then silently score yesterday's vectors against today's queries.
fn table_fingerprint() -> u64 {
    static FP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FP.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        // Deliberately mixed: prose, code punctuation, and an OOV-ish token,
        // so tokenizer changes move the fingerprint too.
        //
        // **This literal is hashed into `compat_key`.** Changing it orphans
        // every cache generation on every machine, for everyone, with no
        // other symptom — so it is not a string to sweep up in a rename. It
        // was last changed in the semgrep→gorp rename, deliberately, because
        // that rename moved `cache_base()` and invalidated the caches anyway.
        let v = ese::encode_single("gorp::probe scalar_None retry_backoff 42");
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for x in v.iter() {
            // Quantize before hashing: identical tables must fingerprint
            // identically across build flags that only perturb the last bits.
            ((x * 4096.0).round() as i64).hash(&mut h);
        }
        h.finish()
    })
}

/// Directory name for this binary's cache generation. Entries from an
/// incompatible binary sort into a sibling directory and are ignored, then
/// reclaimed by [`gc_old_generations`].
pub fn compat_key() -> String {
    // 16 bits. This never has to resist collisions globally — only to tell
    // apart the tables one machine has actually used. It is, though, the only
    // guard against the *silent* failure: an entry with matching dims but a
    // different table loads cleanly and scores yesterday's vectors against
    // today's queries. 8 bits would collide once per ~256 table swaps for two
    // characters of path; 16 makes it once per ~65k.
    format!("v{FORMAT_VERSION}-d{EMBED_DIM}-{:04x}", table_fingerprint() as u16)
}

/// Base directory for cache entries. `GORP_CACHE_DIR` overrides (tests,
/// hygiene); default is `$XDG_CACHE_HOME`/gorp or `~/.cache/gorp`.
/// Resolved once per process so concurrent env reads never race.
pub fn cache_base() -> PathBuf {
    static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        if let Some(d) = std::env::var_os("GORP_CACHE_DIR") {
            return PathBuf::from(d);
        }
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
            })
            .join("gorp")
    })
    .clone()
}

/// All cache entries as (entry_dir, canonical_root). Entries whose recorded
/// root no longer exists are skipped (GC candidates, not errors).
pub fn cache_entries() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(cache_generation()) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        // A build in flight, or the entry a finished build is about to delete.
        // Both carry a root.txt and the second even carries a complete
        // meta.json, so without this they would be discoverable — and a query
        // answered from a `.trash-` directory is a query answered from an index
        // that is about to be removed underneath it.
        if crate::store::is_transient(&dir) {
            continue;
        }
        let Ok(root) = std::fs::read_to_string(dir.join("root.txt")) else { continue };
        let root = PathBuf::from(root.trim());
        if root.is_dir() {
            out.push((dir, root));
        }
    }
    out
}

/// This binary's generation directory: `<cache base>/<compat key>/`.
pub fn cache_generation() -> PathBuf {
    cache_base().join(compat_key())
}

/// Remove cache generations this binary cannot use, plus pre-generation
/// (flat) entries left by older builds. Called after a successful write,
/// never on a read path — reclaiming space must not be a side effect of
/// answering a query.
///
/// Only two shapes are ever removed, so an unrelated directory someone
/// pointed `GORP_CACHE_DIR` at is left alone: a sibling generation
/// (`v<fmt>-d<dims>-<fp>`), and a legacy entry (holds `meta.json` directly —
/// generations hold entry *directories*).
pub fn gc_old_generations() {
    let (base, keep) = (cache_base(), compat_key());
    let Ok(rd) = std::fs::read_dir(&base) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() || p.file_name().is_some_and(|n| n == keep.as_str()) {
            continue;
        }
        let is_generation = p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('v'))
            && !p.join(&keep).exists()
            && !p.join("meta.json").exists();
        let is_legacy_entry = p.join("meta.json").is_file() && p.join("root.txt").is_file();
        if is_generation || is_legacy_entry {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}
