//! janitor-prime: deterministic refactoring engine, transactional VFS and
//! persistent memory for autonomous agents (PLAN §0).
//!
//! F0 scaffold (PLAN §3 F0): crate layout, versioned JSON envelope, error
//! taxonomy, and `--version` as the only command. The domain modules
//! (`vfs`, `ast`, `memory`, `orchestrator`) are typed skeletons; their
//! phases (F2+) fill them in.
//!
//! Hard rules in force from this commit (PLAN §1.1): no `unwrap`/`expect`/
//! `panic` in production code (clippy-denied), stdout is 100% JSON through
//! the `janitor.v1` envelope, diagnostics go to stderr via `tracing`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod ast;
mod cli;
mod envelope;
mod error;
mod memory;
mod orchestrator;
#[cfg(feature = "spikes")]
mod spikes;
mod vfs;

use clap::Parser;
use std::process::ExitCode;

use crate::cli::VersionData;
use crate::envelope::Envelope;

fn main() -> ExitCode {
    init_diagnostics();

    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return cli::render_clap_error(&err),
    };

    if cli.version {
        let data = VersionData {
            version: env!("CARGO_PKG_VERSION"),
            grammars: cli::ENABLED_GRAMMARS,
        };
        return cli::emit(&Envelope::ok(data), cli.pretty, envelope::EXIT_SUCCESS);
    }

    // F0: --version is the only command. Anything else is a usage error —
    // the command set (init|wake|note|nap|scan|apply) arrives with F2+.
    let usage = Envelope::error(
        "E_USAGE",
        "no command given; this F0 scaffold provides only --version (see --help)",
        None,
    );
    cli::emit(&usage, cli.pretty, envelope::EXIT_USAGE)
}

/// stderr-only diagnostics, env-gated via `RUST_LOG` (default: `warn`).
/// Never a stdout side effect (PLAN §1.1, §10).
fn init_diagnostics() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
