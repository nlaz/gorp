//! Stamp the build's identity into the binary.
//!
//! `bench/run.py` reconstructs this from outside — shelling out to `git` next to
//! the binary and hoping the tree it reads is the tree that built it. It usually
//! is. When it is not, the provenance on a published number is silently wrong,
//! and that is the failure mode RESEARCH.md §13.7 is about. A binary that
//! answers "which commit are you" itself cannot be wrong about it.

use std::process::Command;

fn main() {
    // An explicit GORP_GIT_SHA in the build environment wins over shelling out
    // to git: a source tarball has no .git to ask, and a CI release build
    // should stamp the exact sha it checked out rather than trust the cwd.
    let sha =
        std::env::var("GORP_GIT_SHA").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        });

    // A dirty tree means the sha does not describe what is running. Recorded
    // rather than hidden, so a simulation run can refuse to publish from one.
    // Same override rule as the sha, for builds outside a git checkout.
    let dirty = match std::env::var("GORP_GIT_DIRTY").ok().filter(|s| !s.is_empty()) {
        Some(v) => v == "true" || v == "1",
        None => Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .is_some_and(|o| !o.stdout.is_empty()),
    };

    println!("cargo:rustc-env=GORP_GIT_SHA={sha}");
    println!("cargo:rustc-env=GORP_GIT_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=GORP_BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
    // Rerun when HEAD moves, so the stamp does not go stale across commits.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-env-changed=GORP_GIT_SHA");
    println!("cargo:rerun-if-env-changed=GORP_GIT_DIRTY");
}
