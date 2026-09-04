//! The daemon warm path, end to end: spawn-or-connect, freshness sync,
//! fallback. One test, in phases, because the phases share a slow-to-warm
//! daemon and must happen in order — the same reason the sim harness runs
//! sessions rather than assertions.
#![cfg(all(feature = "daemon", unix))]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_gorp");

/// Kills the daemon on drop so a failing assertion can't leak a process that
/// holds the store lock into the next `cargo test`.
struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// Run a search; returns (exit, stdout, stderr).
fn search(cache: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .env("GORP_CACHE_DIR", cache)
        .env_remove("GORP_DAEMON")
        .env_remove("GORP_AUTO_INDEX")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The daemon is "serving this query" exactly when --stats says so: a cold
/// stream reports index=false, a flat warm index reports index=true without
/// the marker, and the client refuses the daemon until its manifest matches
/// the tree. The explicit marker is the provenance a benchmark condition
/// checks, so the test checks the same thing.
fn went_warm(stderr: &str) -> bool {
    stderr.contains("daemon=true")
}

#[test]
fn daemon_serves_syncs_and_never_blocks_a_query() {
    let cache = tempfile::tempdir().unwrap();
    let corpus = tempfile::tempdir().unwrap();
    let corpus = corpus.path().canonicalize().unwrap();
    write(
        &corpus,
        "src/retry.py",
        "def compute_backoff(attempt):\n    \"\"\"Exponential retry backoff.\"\"\"\n    return min(30.0, 0.5 * 2 ** attempt)\n",
    );
    write(
        &corpus,
        "src/parser.py",
        "def parse_config(path):\n    \"\"\"Read the config file into a dict.\"\"\"\n    return dict(l.split('=') for l in open(path))\n",
    );

    let daemon = DaemonGuard(
        Command::new(BIN)
            .arg("daemon")
            .arg(&corpus)
            .env("GORP_CACHE_DIR", cache.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    // Phase 1: searches answer from the very first moment — locally while the
    // daemon fills, warm once it converges. Nothing here may error or hang.
    let corpus_s = corpus.to_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let warm = loop {
        let (exit, stdout, stderr) =
            search(cache.path(), &["--stats", "retry backoff exponential", corpus_s]);
        assert_eq!(exit, 0, "search failed while daemon warms: {stderr}");
        assert!(stdout.contains("retry.py"), "wrong answer during warmup: {stdout}");
        if went_warm(&stderr) {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(warm, "daemon never became the answering path");

    // Phase 2: edit-then-search sees the edit (workplan acceptance 2). The
    // sync runs inside this invocation, so no sleep is needed or allowed.
    write(
        &corpus,
        "src/widgets.py",
        "def frobnicate_widgets(ws):\n    \"\"\"Frobnicate every widget.\"\"\"\n    return [w.frob() for w in ws]\n",
    );
    let (exit, stdout, stderr) =
        search(cache.path(), &["--stats", "frobnicate widgets", corpus_s]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(stdout.contains("widgets.py"), "edit not visible: {stdout}");
    assert!(went_warm(&stderr), "edit-then-search should stay on the warm path: {stderr}");

    // Phase 3: GORP_DAEMON=0 bypasses the daemon entirely.
    let out = Command::new(BIN)
        .args(["--stats", "retry backoff exponential", corpus_s])
        .env("GORP_CACHE_DIR", cache.path())
        .env("GORP_DAEMON", "0")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("index=false"), "GORP_DAEMON=0 must stream: {stderr}");

    // Phase 4: kill -9. The next search must answer correctly (locally —
    // dead pid in the sidecar reads as absent) rather than hang or error.
    let pid = daemon.0.id() as i32;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(200));
    let (exit, stdout, stderr) = search(cache.path(), &["retry backoff exponential", corpus_s]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(stdout.contains("retry.py"), "post-kill search broken: {stdout}");
}

#[test]
fn spawn_race_leaves_exactly_one_daemon() {
    let cache = tempfile::tempdir().unwrap();
    let corpus = tempfile::tempdir().unwrap();
    write(corpus.path(), "a.txt", "the quick brown fox\n");

    let spawn = || {
        Command::new(BIN)
            .arg("daemon")
            .arg(corpus.path())
            .env("GORP_CACHE_DIR", cache.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut a = spawn();
    let mut b = spawn();

    // One exits 0 with the lock message promptly; the other keeps serving.
    // (Both exiting means nobody serves; neither exiting means two do.)
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut done: Vec<bool> = vec![false, false];
    while Instant::now() < deadline && done.iter().filter(|d| **d).count() < 1 {
        for (i, child) in [&mut a, &mut b].into_iter().enumerate() {
            if let Ok(Some(status)) = child.try_wait() {
                assert!(status.success(), "race loser must exit 0");
                done[i] = true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(done.iter().filter(|d| **d).count(), 1, "exactly one racer should have exited");
    let winner = if done[0] { &mut b } else { &mut a };
    assert!(winner.try_wait().unwrap().is_none(), "the winner must still be serving");
    let _ = winner.kill();
    let _ = winner.wait();
}
