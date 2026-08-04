//! semgrep — a semantic grep for agents.
//!
//! ```text
//! semgrep "where is the retry backoff computed" src/   # ranked (default)
//! semgrep -e 'fn \w+_config' .                         # exact regex, grep semantics
//! semgrep index .                                      # build .semgrep/
//! ```
//!
//! The ranked path serves the *locate* contract (the best few places, bounded
//! output); `-e` serves *enumerate* and *verify* (every match, grep exit codes).
//!
//! This file is only the entry point: `cli` defines the surface, `cmd` implements
//! the verbs, `out` does every write to stdout and stderr. Keeping printing in one
//! place is what lets the command modules be about deciding rather than
//! formatting.

mod cli;
mod cmd;
mod out;
mod telemetry;

use clap::Parser;

/// Parse argv, adding one line of recovery advice to the failure clap cannot
/// explain on its own.
///
/// A query that begins with a dash — `semgrep "-v --variable option deploy"` —
/// is read as flags, and clap says `unexpected argument '-v' found`, which
/// tells a caller nothing about how to proceed. It happened twice in 143 real
/// agent invocations (RESEARCH.md §18 tier 1). `--` is the answer and nobody
/// types it unprompted.
///
/// Only the advice is ours: clap still owns the error text, the exit code, and
/// `--help`/`--version`, which it reports through the same error type.
fn parse_args() -> cli::Cli {
    match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            if matches!(e.kind(), clap::error::ErrorKind::UnknownArgument) {
                eprintln!(
                    "semgrep: if that was your query and not a flag, put it after `--`:\n\
                     semgrep -- \"-v your query here\" [path]"
                );
            }
            std::process::exit(if e.use_stderr() { EXIT_ERROR } else { 0 });
        }
    }
}

fn main() {
    let code = match cmd::dispatch(parse_args()) {
        Ok(code) => code,
        Err(e) => {
            // `{:#}` so the anyhow context chain shows, not just the outermost
            // message — "no .semgrep index here" alone would not say where.
            eprintln!("semgrep: {e:#}");
            // An error should say what to do next (RESEARCH.md §6). The common
            // way to reach an invalid pattern is typing a call — `-e 'foo('` —
            // where the paren is regex syntax and the caller meant a literal.
            // The parse error alone is a wall of regex internals with no exit;
            // one of the ten argv vectors that still failed after the grep-compat
            // work was exactly this, and `-F` answers it.
            let msg = format!("{e:#}");
            if msg.contains("invalid pattern") {
                eprintln!(
                    "semgrep: searching for a literal? -F takes the pattern as \
                     plain text · or drop -e and ask in plain language"
                );
            }
            EXIT_ERROR
        }
    };
    std::process::exit(code);
}

/// Grep's convention, which agents and shell scripts both rely on: 0 found,
/// 1 nothing found, 2 something went wrong.
pub const EXIT_FOUND: i32 = 0;
pub const EXIT_NONE: i32 = 1;
pub const EXIT_ERROR: i32 = 2;
