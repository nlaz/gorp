//! Stamp the build's identity into the binary.
//!
//! `bench/run.py` reconstructs this from outside — shelling out to `git` next to
//! the binary and hoping the tree it reads is the tree that built it. It usually
//! is. When it is not, the provenance on a published number is silently wrong,
//! and that is the failure mode RESEARCH.md §13.7 is about. A binary that
//! answers "which commit are you" itself cannot be wrong about it.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // A dirty tree means the sha does not describe what is running. Recorded
    // rather than hidden, so a simulation run can refuse to publish from one.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());

    println!("cargo:rustc-env=GORP_GIT_SHA={sha}");
    println!("cargo:rustc-env=GORP_GIT_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=GORP_BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
    // Rerun when HEAD moves, so the stamp does not go stale across commits.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
