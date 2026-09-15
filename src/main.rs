//! The `janitor` binary: CLI entry point over the `janitor_prime` library
//! (PLAN §0, §1.6). The library target landed at F3.6 for the integration
//! tests; this file is intentionally thin — parse arguments, dispatch the
//! command, emit the versioned JSON envelope, exit.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::Parser;
use std::io::Read as _;
use std::process::ExitCode;

use janitor_prime::apply::{ApplyParams, DiffPayload};
use janitor_prime::cli::{self, Command, VersionData};
use janitor_prime::envelope::{EXIT_SUCCESS, EXIT_USAGE, Envelope};
use janitor_prime::error::{JanitorError, VfsError};
use janitor_prime::orchestrator::{NoteParams, NoteTarget, Session};
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
        Some(Command::Apply(args)) => run_apply(args, cli.pretty),
        Some(Command::Init(args)) => run_init(args, cli.pretty),
        Some(Command::Wake(args)) => run_wake(args, cli.pretty),
        Some(Command::Note(args)) => run_note(args, cli.pretty),
        Some(Command::Nap(args)) => run_nap(args, cli.pretty),
        None => {
            let usage = Envelope::error(
                "E_USAGE",
                "no command given; `janitor scan --rule \"<rule>\" [PATH]` (docs/DSL.md), \
                 `janitor apply --diff <json|@file> | --diff-file <path|->` (docs/SCHEMA.md), \
                 and the memory set `init | wake | note | nap` (docs/SCHEMA.md) are the \
                 available commands",
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

/// `janitor apply`: read the payload (inline, `@file`, file, or stdin),
/// parse it typedly, run the transaction, emit the report envelope.
fn run_apply(args: cli::ApplyArgs, pretty: bool) -> ExitCode {
    let payload_text = match read_payload(&args) {
        Ok(text) => text,
        Err(err) => return emit_error(err, pretty),
    };
    let payload = match DiffPayload::from_json_str(&payload_text) {
        Ok(payload) => payload,
        Err(err) => return emit_error(err, pretty),
    };
    let params = ApplyParams {
        payload,
        dry_run: args.dry_run,
    };
    let session = Session::new();
    match session.apply(&params) {
        Ok(report) => cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// Materialize the payload text from whichever source the flags chose:
/// `--diff` inline (or `@file`), or `--diff-file <path|->` (stdin under
/// `-`). Read/decode failures are the typed VFS errors (`E_VFS_IO`,
/// `E_VFS_INVALID_UTF8`) — the payload is UTF-8 JSON or a typed refusal
/// (E6), never a lossy read.
fn read_payload(args: &cli::ApplyArgs) -> Result<String, JanitorError> {
    let (label, bytes) = if let Some(diff) = args.diff.as_deref() {
        match diff.strip_prefix('@') {
            Some(path) => (path.to_owned(), read_file_bytes(path)?),
            None => return Ok(diff.to_owned()),
        }
    } else {
        match args.diff_file.as_deref() {
            Some("-") => {
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .map_err(|err| VfsError::Io {
                        path: "<stdin>".to_owned(),
                        message: err.to_string(),
                    })?;
                ("<stdin>".to_owned(), buf)
            }
            Some(path) => (path.to_owned(), read_file_bytes(path)?),
            // The clap arg group makes this unreachable through the CLI;
            // kept total for future callers.
            None => {
                return Err(JanitorError::Edit(
                    janitor_prime::error::EditError::Payload {
                        detail: "no payload source".to_owned(),
                    },
                ));
            }
        }
    };
    String::from_utf8(bytes).map_err(|_| VfsError::InvalidUtf8 { path: label }.into())
}

fn read_file_bytes(path: &str) -> Result<Vec<u8>, JanitorError> {
    std::fs::read(path).map_err(|err| {
        VfsError::Io {
            path: path.to_owned(),
            message: err.to_string(),
        }
        .into()
    })
}

/// A typed operational failure as an error envelope, exit code 1.
fn emit_error(err: JanitorError, pretty: bool) -> ExitCode {
    let envelope: Envelope<serde_json::Value> = err.into();
    cli::emit(&envelope, pretty, janitor_prime::envelope::EXIT_OPERATIONAL)
}

/// `janitor init [--force]` (F6.7): one session, one skeleton.
fn run_init(args: cli::InitArgs, pretty: bool) -> ExitCode {
    let session = Session::new();
    match session.init(args.force) {
        Ok(report) => cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// `janitor wake [--budget N]` (F6.7): the budgeted memory payload.
fn run_wake(args: cli::WakeArgs, pretty: bool) -> ExitCode {
    let session = Session::new();
    match session.wake(args.budget) {
        Ok(payload) => cli::emit(&Envelope::ok(payload), pretty, EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// `janitor note` (F6.7): resolve the anchor, append the fact. Anchor
/// failures are the anti-hallucination gate — nothing is appended.
fn run_note(args: cli::NoteArgs, pretty: bool) -> ExitCode {
    let target = if let Some(notation) = args.ptr.as_deref() {
        Some(NoteTarget::Ptr(notation.to_owned()))
    } else {
        args.commit
            .as_deref()
            .map(|sha| NoteTarget::Commit(sha.to_owned()))
    };
    let params = NoteParams {
        target,
        text: args.text,
        lock: args.lock,
    };
    let session = Session::new();
    match session.note(&params) {
        Ok(report) => cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// `janitor nap` (F6.7): bare prints the next pending request; `--slot`
/// plus `--text` stores the agent-written line.
fn run_nap(args: cli::NapArgs, pretty: bool) -> ExitCode {
    let session = Session::new();
    match (&args.slot, &args.text) {
        (None, None) => match session.nap_next() {
            Ok(plan) => cli::emit(&Envelope::ok(plan), pretty, EXIT_SUCCESS),
            Err(err) => emit_error(err, pretty),
        },
        (Some(slot), Some(text)) => match session.nap_settle(slot, text) {
            Ok(outcome) => cli::emit(&Envelope::ok(outcome), pretty, EXIT_SUCCESS),
            Err(err) => emit_error(err, pretty),
        },
        // clap's `requires` wiring makes this unreachable through the CLI;
        // kept total for future callers.
        (slot, text) => {
            let usage = Envelope::error(
                "E_USAGE",
                "--slot and --text travel together or not at all",
                Some(serde_json::json!({ "slot": slot, "text": text })),
            );
            cli::emit(&usage, pretty, EXIT_USAGE)
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
