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
