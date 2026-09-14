//! The `janitor` binary: CLI entry point over the `janitor_prime` library
//! (PLAN §0, §1.6). The library target landed at F3.6 for the integration
//! tests; this file is intentionally thin — parse arguments, emit the
//! versioned JSON envelope, exit.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::Parser;
use std::process::ExitCode;

use janitor_prime::cli::{self, VersionData};
use janitor_prime::envelope::{EXIT_SUCCESS, EXIT_USAGE, Envelope};

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
        return cli::emit(&Envelope::ok(data), cli.pretty, EXIT_SUCCESS);
    }

    // F0: --version is the only command. Anything else is a usage error —
    // the command set (init|wake|note|nap|scan|apply) arrives with F2+.
    let usage = Envelope::error(
        "E_USAGE",
        "no command given; this F0 scaffold provides only --version (see --help)",
        None,
    );
    cli::emit(&usage, cli.pretty, EXIT_USAGE)
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
