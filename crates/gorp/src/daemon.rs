//! The warm path's client half of the per-repo daemon (`daemon` feature;
//! design: bog-kit's gorp-daemon-workplan.md, Phase C).
//!
//! Spawn-or-connect, from the searching process's point of view:
//!
//! 1. A live sidecar (`<store>.serve.json`, dead pid = absent) means a daemon
//!    holds this repo's fold store — sync the tree's drift into it, then ask
//!    it the query over the unix socket.
//! 2. No daemon, and either `GORP_DAEMON=1` or a second ranked search of this
//!    scope inside ten minutes: spawn `gorp daemon` detached and *don't wait*.
//! 3. Either way, when the daemon doesn't answer, the caller falls back to
//!    the ordinary tier-1/tier-0 path — the daemon is never on a query's
//!    critical path, and every failure below is a `None`, not an error.
//!
//! The freshness sync doubles as the readiness check: a daemon still filling
//! itself has a small manifest, the diff against the walked tree comes back
//! large, and this client answers locally until the daemon converges. Per
//! invocation it pushes at most [`SYNC_CAP`] changed files, so an
//! edit-then-search agent pays milliseconds while a branch switch degrades to
//! the local path instead of blocking on a re-embed.

use crate::out::PROG;
use anyhow::{Context, Result, bail};
use gorp_core::search::{
    Mode, SearchHit, SearchOptions, SearchReport, SearchResult, split_phrases,
};
use gorp_core::text::{EmbedPreproc, PathRender};
use gorp_core::{ChunkParams, FileMeta, cache, corpus};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Most changed files one invocation will push before declaring the daemon
/// not-ready and answering locally. Bounds the sync at roughly a second or
/// two of embedding; ordinary drift (an agent's edits between searches) is a
/// handful of files, and anything larger is the daemon's own fill loop's job.
pub const SYNC_CAP: usize = 256;

/// A prior ranked search inside this window makes the next one "repeated" —
/// the N>=2 spawn heuristic from workplan C3.
const REPEAT_WINDOW: Duration = Duration::from_secs(600);

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Generous: a full batch is up to [`SYNC_CAP`] files embedded inline.
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Wire types, shared with the server half (`cmd::daemon`). The hit mirrors
// the fields of `SearchHit` the daemon can fill, under the same JSON names,
// so the daemon's `--json` consumers and the CLI's parse one shape.
// ---------------------------------------------------------------------------

/// What a `PUT /docs/{path}` / batch upsert carries: the file's text plus the
/// size/mtime identity the manifest diff compares (same predicate as
/// `corpus::diff` — size or mtime moved means stale).
#[derive(Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct FileDoc {
    pub text: String,
    pub size: u64,
    pub mtime: u64,
}

/// One row of the daemon's `manifest` view: what it believes about a file.
#[derive(Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
}

// ---------------------------------------------------------------------------
// Layout: everything for one repo's daemon lives in one claimed directory
// under the cache root — never in the user's tree.
// ---------------------------------------------------------------------------

/// The claimed per-repo directory: fold store in `store/`, sidecar at
/// `store.serve.json` (bog-serve's sibling-suffix convention), stderr log at
/// `log`, spawn heuristic marker at `recent`.
///
/// The socket is the one piece that does NOT live here: `sockaddr_un` caps a
/// socket path at ~104 bytes on macOS, and the cache directory is routinely
/// deeper than that. It gets a short hashed name under the temp dir instead —
/// which is safe because nothing ever *derives* the socket path to connect:
/// clients read it out of the sidecar the daemon wrote after binding.
pub struct Layout {
    pub dir: PathBuf,
    pub store: PathBuf,
    pub sock: PathBuf,
    pub sidecar: PathBuf,
    pub log: PathBuf,
}

impl Layout {
    /// `root` must be canonical, same contract as cache discovery.
    pub fn resolve(root: &Path, params: &ChunkParams) -> Self {
        let dir = cache::daemon_dir(root, params);
        let store = dir.join("store");
        // Hash of the claimed dir, so one repo (and one params tag, and one
        // compat generation) is one socket; uid in the name because /tmp is
        // shared on Linux and a cross-user collision must fail as "not my
        // socket", not "address in use".
        let sock = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            dir.hash(&mut h);
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("gorp-{uid}-{:016x}.sock", h.finish()))
        };
        Self {
            sock,
            sidecar: bog_serve::sidecar_path(&store),
            log: dir.join("log"),
            store,
            dir,
        }
    }

    /// Create the directory and write the root.txt claim that makes
    /// `cache::daemon_dir` resolve here again (and lets a hash-bucket
    /// neighbour step past us). Idempotent.
    pub fn claim(&self, root: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let marker = self.dir.join("root.txt");
        if !marker.exists() {
            std::fs::write(marker, root.to_string_lossy().as_bytes())?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The entry point `cmd::search` calls.
// ---------------------------------------------------------------------------

/// Answer `query` from a running daemon, or return `None` — never an error —
/// after (possibly) arranging for one to exist next time. See module docs.
pub fn try_warm(root: &Path, query: &str, opts: &SearchOptions) -> Option<SearchResult> {
    if !eligible(query, opts) {
        return None;
    }
    let t0 = std::time::Instant::now();
    let canon = std::fs::canonicalize(root).ok()?;
    if !canon.is_dir() {
        return None;
    }
    let layout = Layout::resolve(&canon, &opts.params);

    match live_socket(&layout) {
        Some(sock) => {
            match serve_query(&sock, &canon, query, opts) {
                Ok(Some(mut result)) => {
                    result.report.total_ms = t0.elapsed().as_secs_f64() * 1e3;
                    Some(result)
                }
                // Not ready (still filling, or drift beyond the cap): the
                // local answer is the correct one.
                Ok(None) => None,
                Err(e) => {
                    debug_note(&format!("daemon query failed, answering locally: {e:#}"));
                    None
                }
            }
        }
        None => {
            if should_spawn(&layout) {
                spawn_detached(&canon, &layout);
            }
            None
        }
    }
}

/// The daemon serves one question: a single-phrase ranked query with stock
/// chunking and stock retrieval knobs. Anything else answers locally — the
/// hidden tuning surface must keep meaning what it means.
fn eligible(query: &str, opts: &SearchOptions) -> bool {
    if std::env::var("GORP_DAEMON").is_ok_and(|v| v == "0") {
        return false;
    }
    let d = SearchOptions::default();
    matches!(opts.mode, Mode::Bm25 | Mode::Semantic | Mode::Hybrid)
        && opts.k <= 100
        && opts.params == d.params
        && opts.prf_terms == d.prf_terms
        && opts.bridge_expand == d.bridge_expand
        && opts.min_score == d.min_score
        && matches!(opts.embed_preproc, EmbedPreproc::None)
        && matches!(opts.path_render, PathRender::Full)
        && !opts.no_index
        && !opts.check_stale
        && !opts.debug_features
        && split_phrases(query).len() == 1
}

/// The socket named by a sidecar whose pid is alive, else `None`. The sidecar
/// is advisory (a kill -9 leaves it behind); the fjall lock is what actually
/// guarantees at most one daemon.
fn live_socket(layout: &Layout) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(&layout.sidecar).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pid = v.get("pid")?.as_u64()?;
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return None;
    }
    Some(PathBuf::from(v.get("bind")?.get("unix")?.as_str()?))
}

/// Sync drift, then query. `Ok(None)` = daemon reachable but not ready.
fn serve_query(
    sock: &Path,
    root: &Path,
    query: &str,
    opts: &SearchOptions,
) -> Result<Option<SearchResult>> {
    let (ready, files_walked) = sync(sock, root, &opts.params, SYNC_CAP)?;
    if !ready {
        return Ok(None);
    }
    let mode = match opts.mode {
        Mode::Bm25 => "bm25",
        Mode::Semantic => "semantic",
        Mode::Hybrid => "hybrid",
        Mode::Keyword => unreachable!("eligible() excluded keyword"),
    };
    let target = format!(
        "/search?q={}&k={}&mode={}&sem_weight={}&bm25_pin={}",
        urlencode(query),
        opts.k,
        mode,
        opts.sem_weight,
        opts.bm25_pin,
    );
    // Real `SearchHit`s over the wire — the daemon runs the shared finalize
    // tail, so its JSON is the `--json` contract, unit rows and all.
    let data = get_json(sock, &target, SEARCH_TIMEOUT)?;
    let hits: Vec<SearchHit> = serde_json::from_value(data).context("bad /search payload")?;
    let n = hits.len();
    Ok(Some(SearchResult {
        hits,
        report: SearchReport {
            used_index: true,
            used_hnsw: true,
            daemon: true,
            files_walked,
            n_chunks_considered: n,
            n_phrases: 1,
            ..Default::default()
        },
    }))
}

/// Walk the tree, diff it against the daemon's manifest, and push up to
/// `cap` changed files in one batch transaction. Returns (in-sync-now,
/// files walked). Deletions are always pushed in full — they cost the daemon
/// retractions, not embeddings.
pub fn sync(
    sock: &Path,
    root: &Path,
    params: &ChunkParams,
    cap: usize,
) -> Result<(bool, usize)> {
    let walked = corpus::walk(root, params)?;
    let data = get_json(sock, "/manifest", SEARCH_TIMEOUT)?;
    let manifest: Vec<ManifestEntry> = serde_json::from_value(data).context("bad /manifest")?;
    let indexed: Vec<FileMeta> = manifest
        .into_iter()
        .map(|m| FileMeta { path: m.path, size: m.size, mtime: m.mtime })
        .collect();
    let diff = corpus::diff(&indexed, &walked, "");
    if diff.is_empty() {
        return Ok((true, walked.len()));
    }

    let by_path: std::collections::HashMap<&str, &FileMeta> =
        walked.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut ops: Vec<serde_json::Value> = Vec::new();
    for id in diff.deleted.iter() {
        ops.push(serde_json::json!({ "op": "remove", "key": indexed[*id as usize].path }));
    }
    // Two caps, and the byte cap is the load-bearing one: axum rejects
    // request bodies past ~2 MiB, and it does so by closing the connection
    // mid-upload — which the client experiences as a broken pipe, and which
    // the daemon's fill loop experienced as five retries and an abandoned
    // fill on every benchmark-scale repo. Half that limit leaves room for
    // JSON escaping. At least one stale file always ships, however large,
    // or a single file bigger than the cap would stall the sync forever.
    const BATCH_BYTES: usize = 1 << 20;
    let mut pushed = 0usize;
    let mut bytes = 0usize;
    let mut stale = 0usize;
    for path in diff.stale_paths() {
        stale += 1;
        if pushed >= cap || (pushed > 0 && bytes >= BATCH_BYTES) {
            continue; // keep counting, stop reading
        }
        pushed += 1;
        let Some(meta) = by_path.get(path.as_str()) else { continue };
        // Unreadable (binary, or raced away): upsert EMPTY text with the real
        // size/mtime, never a remove. Empty text chunks to nothing, so the
        // index stays clean, but the manifest records the identity — which is
        // what makes the diff converge. A remove here left the file eternally
        // "added": the walk admits binaries that `read_text` refuses, and on
        // the first real repo 560 of them wedged the fill's 256-file window
        // solid while the manifest sat frozen. Zero chunks against a recorded
        // identity is also exactly what the flat build does with these files.
        let text = corpus::read_text(&corpus::resolve(root, path)).unwrap_or_default();
        bytes += text.len();
        ops.push(serde_json::json!({
            "op": "upsert",
            "key": path,
            "data": FileDoc { text, size: meta.size, mtime: meta.mtime },
        }));
    }
    if !ops.is_empty() {
        let body = serde_json::Value::Array(ops).to_string();
        let (status, resp) = request(sock, "POST", "/batch", Some(&body), SYNC_TIMEOUT)?;
        if status != 200 {
            bail!(
                "batch of {pushed} files returned {status}: {}",
                resp.chars().take(300).collect::<String>()
            );
        }
    }
    Ok((stale <= pushed, walked.len()))
}

// ---------------------------------------------------------------------------
// Spawning.
// ---------------------------------------------------------------------------

/// `GORP_DAEMON=1` spawns on the first search; unset spawns on the second
/// ranked search of a scope inside [`REPEAT_WINDOW`] (the mtime marker is the
/// same idiom as read-repair's `last_check`, and lives under the cache for
/// the same reason: a search must never write into the user's tree).
fn should_spawn(layout: &Layout) -> bool {
    match std::env::var("GORP_DAEMON") {
        Ok(v) if v == "1" => true,
        Ok(_) => false,
        Err(_) => {
            let marker = layout.dir.join("recent");
            let fresh = std::fs::metadata(&marker)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < REPEAT_WINDOW);
            let _ = std::fs::create_dir_all(&layout.dir);
            let _ = std::fs::write(&marker, b"");
            fresh
        }
    }
}

/// Fire and forget: its own session so a Ctrl-C aimed at the search can't
/// take the daemon with it, stdio to the log so it can never pollute a
/// client's stdout. Losing a spawn race is fine — the loser sees the fjall
/// lock and exits 0.
fn spawn_detached(root: &Path, layout: &Layout) {
    let Ok(exe) = std::env::current_exe() else { return };
    if layout.claim(root).is_err() {
        return;
    }
    let Ok(log) = std::fs::OpenOptions::new().create(true).append(true).open(&layout.log)
    else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone().expect("dup log fd")))
        .stderr(std::process::Stdio::from(log));
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(_) => eprintln!(
            "{PROG}: starting a background daemon for {} — this search answers locally; \
             later ones go warm (log: {})",
            root.display(),
            layout.log.display()
        ),
        Err(e) => debug_note(&format!("could not spawn daemon: {e}")),
    }
}

/// Failures on the daemon path are quiet by design — the local answer is
/// always coming — except when the user explicitly opted in with
/// `GORP_DAEMON=1`, where silence would hide a daemon that never works.
fn debug_note(msg: &str) {
    if std::env::var("GORP_DAEMON").is_ok_and(|v| v == "1") {
        eprintln!("{PROG}: {msg}");
    }
}

// ---------------------------------------------------------------------------
// A minimal HTTP/1.1 client over a unix socket. `connection: close` on every
// request, so "read to EOF" bounds a response without keep-alive bookkeeping;
// one request per search and one per sync keeps that honest.
// ---------------------------------------------------------------------------

/// GET `target` and return the `data` field of bog-serve's `{seq, data}`
/// envelope.
pub fn get_json(sock: &Path, target: &str, timeout: Duration) -> Result<serde_json::Value> {
    let (status, body) = request(sock, "GET", target, None, timeout)?;
    if status != 200 {
        bail!("{target} returned {status}: {}", body.chars().take(200).collect::<String>());
    }
    let v: serde_json::Value = serde_json::from_str(&body).context("response is not JSON")?;
    v.get("data").cloned().ok_or_else(|| anyhow::anyhow!("response has no data field"))
}

pub fn request(
    sock: &Path,
    method: &str,
    target: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String)> {
    let mut stream = connect(sock)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {target} HTTP/1.1\r\nhost: gorp-daemon\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn connect(sock: &Path) -> Result<UnixStream> {
    // UnixStream has no connect_timeout; the socket either accepts promptly
    // (a healthy listener) or the file is stale and connect errors at once.
    // The read timeout is what bounds a hung daemon.
    let _ = CONNECT_TIMEOUT;
    UnixStream::connect(sock).with_context(|| format!("connecting {}", sock.display()))
}

fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no header terminator in response"))?;
    let head = std::str::from_utf8(&raw[..head_end]).context("non-utf8 response head")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("bad status line {status_line:?}"))?;
    let chunked = lines.filter_map(|l| l.split_once(':')).any(|(k, v)| {
        k.trim().eq_ignore_ascii_case("transfer-encoding")
            && v.trim().eq_ignore_ascii_case("chunked")
    });
    let body = &raw[head_end + 4..];
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len());
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow::anyhow!("truncated chunk size"))?;
        let size_str = std::str::from_utf8(&body[..line_end]).context("bad chunk size")?;
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or(""), 16)
            .context("bad chunk size")?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            bail!("truncated chunk body");
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

/// Percent-encode a query-string value (space included, so `+` never needs
/// interpreting on the other side).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_round_trips_through_serde_urlencoded_rules() {
        assert_eq!(urlencode("retry backoff"), "retry%20backoff");
        assert_eq!(urlencode("a+b&c=d"), "a%2Bb%26c%3Dd");
        assert_eq!(urlencode("plain-safe_chars.~"), "plain-safe_chars.~");
    }

    #[test]
    fn parse_response_handles_content_length_and_chunked() {
        let (status, body) =
            parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").unwrap();
        assert_eq!((status, body.as_str()), (200, "ok"));

        let raw = b"HTTP/1.1 404 Not Found\r\ntransfer-encoding: chunked\r\n\r\n\
                    4\r\nwha\n\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!((status, body.as_str()), (404, "wha\n"));
    }

    #[test]
    fn truncated_responses_are_errors_not_panics() {
        assert!(parse_response(b"HTTP/1.1 200").is_err());
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\nab")
                .is_err()
        );
    }
}
