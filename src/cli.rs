//! CLI surface (PLAN §1.6).
//!
//! F0 scope: `--version` is the only command, per the F0 execution brief —
//! the command set (`init|wake|note|nap|scan|apply`) lands with its phases
//! (F2+). What is frozen here is the *shape* of every exit: clap errors are
//! intercepted and re-emitted as JSON envelopes so even usage mistakes stay
//! machine-parseable on stdout (PLAN §1.6).

use clap::Parser;
use clap::error::ErrorKind;
use serde::Serialize;
use serde_json::Value;
use std::process::ExitCode;

use crate::envelope::{self, Envelope};

/// Grammar list embedded by build.rs from the enabled cargo features
/// (canonical registry order). Inner doc comments are not permitted in
/// `include!` expansions, so the generated file uses plain comments.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/grammars.rs"));
}

pub use generated::ENABLED_GRAMMARS;

/// Payload of the `--version` envelope (PLAN §3 F0.2).
#[derive(Debug, Serialize)]
pub struct VersionData {
    pub version: &'static str,
    pub grammars: &'static [&'static str],
}

#[derive(Debug, Parser)]
#[command(
    name = "janitor",
    about = "Deterministic refactoring engine, transactional VFS and persistent memory for autonomous agents",
    // clap's built-in --version prints plain text; ours must be the janitor.v1
    // envelope, so we own the flag.
    disable_version_flag = true,
    // Rendered clap text ends up inside JSON envelopes — no ANSI escapes.
    color = clap::ColorChoice::Never
)]
pub struct Cli {
    /// Print version + enabled grammars as a janitor.v1 JSON envelope and exit.
    #[arg(long)]
    pub version: bool,

    /// Pretty-print the JSON envelope (stdout stays JSON either way).
    #[arg(long)]
    pub pretty: bool,
}

/// Map a clap error onto (envelope, exit code) without emitting it.
///
/// `--help` (and `--version`, if clap ever synthesizes it) is deliberate
/// human use: rendered help is wrapped in an ok envelope so stdout stays 100%
/// JSON. Everything else is a usage error: `E_USAGE`, exit code 2.
pub fn clap_error_envelope(err: &clap::Error) -> (Envelope<Value>, u8) {
    let rendered = err.render().to_string();
    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => (
            Envelope::ok(serde_json::json!({ "help": rendered })),
            envelope::EXIT_SUCCESS,
        ),
        _ => (
            Envelope::error(
                "E_USAGE",
                rendered,
                Some(serde_json::json!({ "kind": format!("{:?}", err.kind()) })),
            ),
            envelope::EXIT_USAGE,
        ),
    }
}

/// Render a clap error as a JSON envelope on stdout and return its exit code.
pub fn render_clap_error(err: &clap::Error) -> ExitCode {
    let (envelope, code) = clap_error_envelope(err);
    // The offending command line already failed to parse, so --pretty (a
    // valid flag alongside an invalid one) is unknowable here; errors emit
    // compact JSON.
    emit(&envelope, false, code)
}

/// Emit an envelope to stdout, mapping emission failures to the operational
/// exit code (diagnostics on stderr via tracing, never plain stdout).
pub fn emit<T: Serialize>(envelope: &Envelope<T>, pretty: bool, code: u8) -> ExitCode {
    match envelope::emit_stdout(envelope, pretty) {
        Ok(()) => ExitCode::from(code),
        Err(err) => {
            let err = crate::error::JanitorError::from(err);
            tracing::error!(code = err.code(), "stdout envelope emission failed: {err}");
            ExitCode::from(envelope::EXIT_OPERATIONAL)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flag_parses() {
        let cli = Cli::try_parse_from(["janitor", "--version"]).expect("valid invocation");
        assert!(cli.version);
        assert!(!cli.pretty);
    }

    #[test]
    fn pretty_composes_with_version() {
        let cli =
            Cli::try_parse_from(["janitor", "--pretty", "--version"]).expect("valid invocation");
        assert!(cli.version);
        assert!(cli.pretty);
    }

    #[test]
    fn enabled_grammars_reflect_features() {
        #[cfg(feature = "lang-rust")]
        assert!(ENABLED_GRAMMARS.contains(&"rust"));
        for grammar in ENABLED_GRAMMARS {
            assert!(
                [
                    "rust",
                    "python",
                    "go",
                    "javascript",
                    "typescript",
                    "java",
                    "c"
                ]
                .contains(grammar),
                "unknown grammar {grammar:?} embedded by build.rs"
            );
        }
    }

    #[test]
    fn unknown_flag_is_a_usage_error_envelope() {
        let err = Cli::try_parse_from(["janitor", "--nope"]).expect_err("unknown flag");
        let (envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);
        assert!(!envelope.ok);
        let body = envelope
            .error
            .expect("usage envelope carries an error body");
        assert_eq!(body.code, "E_USAGE");
        assert_eq!(
            body.details.expect("usage details carry the clap kind")["kind"],
            "UnknownArgument"
        );
    }

    #[test]
    fn help_is_wrapped_in_an_ok_envelope() {
        let err = Cli::try_parse_from(["janitor", "--help"]).expect_err("--help short-circuits");
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
        let (envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_SUCCESS);
        assert!(envelope.ok);
        let data = envelope.data.expect("help envelope carries data");
        assert!(
            data["help"]
                .as_str()
                .unwrap_or_default()
                .contains("--version")
        );
    }
}
