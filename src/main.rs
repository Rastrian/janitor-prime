//! The `janitor` binary: CLI entry point over the `janitor_prime` library
//! (PLAN §0, §1.6). The library target landed at F3.6 for the integration
//! tests; this file is intentionally thin — parse arguments, dispatch the
//! command, emit the versioned JSON envelope, exit.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::Parser;
use std::process::ExitCode;

use janitor_prime::cli::{self, Command, VersionData};
use janitor_prime::envelope::{EXIT_SUCCESS, EXIT_USAGE, Envelope};
use janitor_prime::orchestrator::Session;
use janitor_prime::scan::ScanParams;

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

    match cli.command {
        Some(Command::Scan(args)) => run_scan(args, cli.pretty),
        None => {
            let usage = Envelope::error(
                "E_USAGE",
                "no command given; `janitor scan --rule \"<rule>\" [PATH]` is the F4 command \
                 (see --help and docs/DSL.md)",
                None,
            );
            cli::emit(&usage, cli.pretty, EXIT_USAGE)
        }
    }
}

/// `janitor scan`: one session, one scan, one envelope — success carries
/// the report `data`; every typed failure maps through the envelope.
fn run_scan(args: cli::ScanArgs, pretty: bool) -> ExitCode {
    let root = args.path.unwrap_or_else(|| std::path::PathBuf::from("."));
    let params = ScanParams {
        rule: args.rule,
        root,
        lang_filter: args.lang,
        include_virtual: args.include_virtual,
        strict_trivia: args.strict_trivia,
        max_file_size: args.max_file_size,
    };
    let session = Session::new();
    match session.scan(&params) {
        Ok(report) => cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS),
        Err(err) => {
            let envelope: Envelope<serde_json::Value> = err.into();
            cli::emit(&envelope, pretty, janitor_prime::envelope::EXIT_OPERATIONAL)
        }
    }
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
