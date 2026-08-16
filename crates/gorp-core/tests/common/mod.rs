//! Shared fixtures for the end-to-end test binaries.
//!
//! One corpus, one options builder, and the cache guard every mutating test
//! takes. Split across three binaries (`e2e_general`, `e2e_cache`,
//! `e2e_publish`) so the three concerns compile and run independently; each
//! binary is its own process and therefore gets its own cache directory,
//! which is what makes the guard below a per-binary serializer rather than a
//! global one.

#![allow(dead_code)]

use gorp_core::ChunkParams;
use gorp_core::search::{Mode, SearchOptions};
use std::fs;
use std::path::Path;

/// Take the cache for the duration of a test.
///
/// Every test in this binary shares one cache directory, and it cannot be
/// otherwise today: `cache::cache_base()` resolves `GORP_CACHE_DIR` through
/// a `OnceLock`, so the whole process gets one cache no matter what a test
/// sets. Cache state is therefore global mutable state, and these tests are all
/// mutators — a write-through search creates entries, while scope promotion and
/// budget enforcement delete entries belonging to whoever else is running.
///
/// So they are serialized. A finer read/write split was tried first and did not
/// hold: assertions about *which* entries exist, or about whether repair fired,
/// fail whenever a concurrent test's write-through prunes or promotes. Those
/// failures read as engine bugs, which is the expensive kind of flake.
/// Serializing costs nothing measurable — the whole binary runs in ~0.06 s.
///
/// The real fix is to make the cache root an explicit parameter instead of
/// process-global state, the same move `enforce_budget_with_cap` made for the
/// cap. That belongs with the `cache/` module split.
///
/// Poison is ignored deliberately: the lock orders tests, it guards no data.
/// Letting it poison turns one real failure into a dozen `PoisonError` panics
/// that bury the original.
pub fn isolate_cache() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("GORP_CACHE_DIR", dir.path());
            std::env::set_var("GORP_CACHE_TTL_SECS", "0");
        }
        // Leak: the cache dir must outlive every test in the process.
        std::mem::forget(dir);
        std::sync::Mutex::new(())
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// Small corpus with clearly separated topics so retrieval is unambiguous.
pub fn fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("docs")).unwrap();
    fs::write(
        dir.join("src/retry.rs"),
        r#"//! Retry logic with exponential backoff.

pub fn compute_backoff_delay(attempt: u32, base_ms: u64) -> u64 {
    let exp = base_ms.saturating_mul(2u64.saturating_pow(attempt));
    exp.min(30_000)
}

pub fn should_retry(status: u16, attempt: u32) -> bool {
    attempt < 5 && (status == 429 || status >= 500)
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/auth.rs"),
        r#"//! Session token validation.

pub fn validate_session_token(token: &str) -> bool {
    !token.is_empty() && token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn hash_password(password: &str, salt: &[u8]) -> Vec<u8> {
    // pretend this is argon2
    password.bytes().chain(salt.iter().copied()).collect()
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("docs/cooking.md"),
        "# Sourdough bread\n\nMix flour and water, let the starter ferment overnight.\nKnead the dough and bake at high temperature.\n",
    )
    .unwrap();
    fs::write(
        dir.join("docs/astronomy.md"),
        "# Telescopes\n\nA reflecting telescope uses mirrors to gather starlight.\nGalileo pioneered astronomical observation with refractors.\n",
    )
    .unwrap();
}

pub fn opts(mode: Mode) -> SearchOptions {
    SearchOptions {
        mode,
        k: 3,
        // small windows so each file yields at least one chunk quickly
        params: ChunkParams { window: 8, overlap: 2, ..Default::default() },
        ..Default::default()
    }
}

/// The pure streaming path (no index anywhere, none written).
pub fn stream_opts(mode: Mode) -> SearchOptions {
    SearchOptions { no_index: true, ..opts(mode) }
}

