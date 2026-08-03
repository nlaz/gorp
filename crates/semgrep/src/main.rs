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

fn main() {
    let code = match cmd::dispatch(cli::Cli::parse()) {
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
