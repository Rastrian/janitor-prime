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
        Some(Command::Mcp(args)) => run_mcp(args),
        Some(Command::Daemon(args)) => run_daemon(args, cli.pretty),
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
/// the report `data`; every typed failure maps through the envelope. F8
/// adds the second mode: `--duplicates` (fingerprint-grouped duplicate
/// detection, docs/SCHEMA.md) instead of `--rule`. F10a adds `--daemon`:
/// opt-in routing through the warm-cache daemon when its socket answers
/// `ping` for this workspace; the in-process fallback is silent for a
/// missing/mismatched daemon, but a daemon that was serving this workspace
/// yet delivered a torn frame warns on stderr first (issue #21 — the fall
/// must be visible, never a stdout side effect; ADR-0018 D7).
fn run_scan(args: cli::ScanArgs, pretty: bool) -> ExitCode {
    let root = args
        .path
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if args.daemon {
        match daemon_route(&args, &root) {
            janitor_prime::daemon::client::Routed::Served(response) => {
                let ok = response.get("ok").and_then(serde_json::Value::as_bool);
                let code = if ok == Some(true) {
                    EXIT_SUCCESS
                } else {
                    janitor_prime::envelope::EXIT_OPERATIONAL
                };
                return emit_json(&response, pretty, code);
            }
            // One line on stderr BEFORE the slow path serves — stdout stays
            // 100% JSON (ADR-0018 D7), and the warm-cache miss is visible.
            janitor_prime::daemon::client::Routed::Torn(reason) => {
                eprintln!(
                    "janitor: daemon response invalid/torn ({reason}) \
                     — falling back to in-process scan"
                );
            }
            janitor_prime::daemon::client::Routed::NoDaemon => {
                tracing::debug!("daemon unreachable; running in-process");
            }
        }
    }
    let session = Session::new();
    // clap's scan-mode group guarantees exactly one of --rule/--duplicates;
    // the total match keeps future callers honest instead of inventing a
    // precedence.
    match (args.duplicates, args.rule) {
        (true, _) => {
            let params = janitor_prime::scan::DuplicatesParams {
                root,
                lang_filter: args.lang,
                include_virtual: args.include_virtual,
                max_file_size: args.max_file_size,
                min_size: args.min_size,
                min_members: args.min_members,
                summary: args.summary,
                retain_cache: false,
            };
            match session.duplicates(&params) {
                Ok(mut report) => {
                    if args.daemon {
                        // v0.3 B4: the fallback is honest — the report says
                        // who served it and that nothing was warm.
                        report.served_by = Some("process");
                        report.stats.cache = Some(janitor_prime::scan::CacheTelemetry::default());
                    }
                    cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS)
                }
                Err(err) => emit_error(err, pretty),
            }
        }
        (false, Some(rule)) => {
            let params = ScanParams {
                rule,
                root,
                lang_filter: args.lang,
                include_virtual: args.include_virtual,
                strict_trivia: args.strict_trivia,
                max_file_size: args.max_file_size,
                retain_cache: false,
            };
            match session.scan(&params) {
                Ok(mut report) => {
                    if args.daemon {
                        report.served_by = Some("process");
                        report.stats.cache = Some(janitor_prime::scan::CacheTelemetry::default());
                    }
                    cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS)
                }
                Err(err) => {
                    let envelope: Envelope<serde_json::Value> = err.into();
                    cli::emit(&envelope, pretty, janitor_prime::envelope::EXIT_OPERATIONAL)
                }
            }
        }
        (false, None) => {
            let usage = Envelope::error(
                "E_USAGE",
                "scan needs exactly one mode: --rule \"<rule>\" (docs/DSL.md) or --duplicates \
                 (docs/SCHEMA.md)",
                None,
            );
            cli::emit(&usage, pretty, EXIT_USAGE)
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

/// The `--daemon` routing decision (ADR-0018 D7): build the socket request
/// from the CLI flags and hand it to the client's router — the `Routed`
/// verdict says whether the daemon served, was absent, or tore its frame
/// (the caller decides how loud each fall is).
fn daemon_route(
    args: &cli::ScanArgs,
    root: &std::path::Path,
) -> janitor_prime::daemon::client::Routed {
    let sock = janitor_prime::daemon::protocol::default_sock_path();
    let mut request = if args.duplicates {
        let mut request =
            janitor_prime::daemon::protocol::Request::duplicates(Some(root.display().to_string()));
        request.min_size = Some(args.min_size);
        // v0.3 B3: the socket carries the new duplicates knobs too — a
        // routed response must honor them, never silently drop them.
        request.min_members = args.min_members;
        request.summary = args.summary.then_some(true);
        request
    } else {
        janitor_prime::daemon::protocol::Request::scan(
            args.rule.clone().unwrap_or_default(),
            Some(root.display().to_string()),
        )
    };
    request.lang = args.lang.clone();
    request.include_virtual = args.include_virtual;
    request.max_file_size = Some(args.max_file_size);
    if args.strict_trivia {
        request.strict_trivia = true;
    }
    janitor_prime::daemon::client::route_request(
        &sock,
        root,
        &request,
        janitor_prime::daemon::client::PING_TIMEOUT,
    )
}

/// Re-emit a daemon response envelope verbatim (it already IS a janitor.v1
/// envelope; stdout stays 100% JSON either way).
fn emit_json(value: &serde_json::Value, pretty: bool, code: u8) -> ExitCode {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    let mut text = match text {
        Ok(text) => text,
        Err(err) => {
            tracing::error!(%err, "response re-serialization failed");
            return ExitCode::from(janitor_prime::envelope::EXIT_OPERATIONAL);
        }
    };
    text.push('\n');
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::from(code),
        Err(err) => {
            tracing::error!(%err, "stdout emission failed");
            ExitCode::from(janitor_prime::envelope::EXIT_OPERATIONAL)
        }
    }
}

/// `janitor daemon` (F10a, ADR-0018): run until shutdown or idle; exactly
/// two envelope lines on stdout (startup + exit), everything else on the
/// socket or stderr.
fn run_daemon(args: cli::DaemonArgs, pretty: bool) -> ExitCode {
    let config = janitor_prime::daemon::DaemonConfig {
        workspace: args
            .workspace
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
        sock: args
            .sock
            .unwrap_or_else(janitor_prime::daemon::protocol::default_sock_path),
        interval: std::time::Duration::from_secs(args.interval),
        max_cache_bytes: args.max_cache_mb * 1024 * 1024,
        idle: std::time::Duration::from_secs(args.idle_secs),
    };
    match janitor_prime::daemon::run(config, pretty) {
        Ok(()) => ExitCode::from(EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// `janitor init [--force]` (F6.7): one session, one skeleton.
fn run_init(args: cli::InitArgs, pretty: bool) -> ExitCode {
    let session = Session::new();
    match session.init(args.force) {
        Ok(report) => cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// `janitor wake [--budget N] [--task QUERY]` (F6.7; C1's ranked pass
/// when --task is given): the budgeted memory payload.
fn run_wake(args: cli::WakeArgs, pretty: bool) -> ExitCode {
    let session = Session::new();
    match session.wake(args.budget, args.task.as_deref()) {
        Ok(payload) => cli::emit(&Envelope::ok(payload), pretty, EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// `janitor note` (F6.7): resolve the anchor, append the fact. Anchor
/// failures are the anti-hallucination gate — nothing is appended.
/// v0.3 B5: `--stdin` takes the text from the pipe instead of the
/// argument (quoting sanity for evidence-carrying notes); the validation
/// is identical either way.
fn run_note(args: cli::NoteArgs, pretty: bool) -> ExitCode {
    let text = if args.stdin {
        match read_note_stdin() {
            Ok(text) => text,
            Err(err) => return emit_error(err, pretty),
        }
    } else if let Some(text) = args.text {
        text
    } else {
        // Unreachable past clap's note_source group; kept total for
        // future callers.
        let usage = Envelope::error(
            "E_USAGE",
            "note needs its text: pass it as the argument or pipe it with --stdin",
            None,
        );
        return cli::emit(&usage, pretty, EXIT_USAGE);
    };
    let target = if let Some(notation) = args.ptr.as_deref() {
        Some(NoteTarget::Ptr(notation.to_owned()))
    } else {
        args.commit
            .as_deref()
            .map(|sha| NoteTarget::Commit(sha.to_owned()))
    };
    let params = NoteParams {
        target,
        text,
        lock: args.lock,
    };
    let session = Session::new();
    match session.note(&params) {
        Ok(report) => cli::emit(&Envelope::ok(report), pretty, EXIT_SUCCESS),
        Err(err) => emit_error(err, pretty),
    }
}

/// Read a `note --stdin` fact: stdin to EOF, then strip ONE trailing
/// newline (a piped line carries it; a `\r\n` pair loses both). Interior
/// newlines stay — the single-line discipline is the same typed
/// validation, never a silent truncation. Non-UTF-8 is the typed
/// `E_VFS_INVALID_UTF8`, same as apply's stdin payload.
fn read_note_stdin() -> Result<String, JanitorError> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|err| VfsError::Io {
            path: "<stdin>".to_owned(),
            message: err.to_string(),
        })?;
    let mut text = String::from_utf8(buf).map_err(|_| VfsError::InvalidUtf8 {
        path: "<stdin>".to_owned(),
    })?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
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

/// `janitor mcp [--workspace DIR]` (F10b, ADR-0019): resolve the
/// workspace (refusing loudly on stderr when it is not a directory — MCP
/// mode owns stdout, so it must stay frame-pure even on startup
/// failure), make it the process root so every `Session` the server
/// builds has exactly the CLI's path semantics, and serve JSON-RPC until
/// stdin closes.
fn run_mcp(args: cli::McpArgs) -> ExitCode {
    let workspace = args
        .workspace
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let canonical = match workspace.canonicalize() {
        Ok(path) if path.is_dir() => path,
        Ok(path) => {
            tracing::error!("--workspace is not a directory: {}", path.display());
            return ExitCode::from(janitor_prime::envelope::EXIT_OPERATIONAL);
        }
        Err(err) => {
            tracing::error!(error = %err, "--workspace {:?} cannot be resolved", workspace);
            return ExitCode::from(janitor_prime::envelope::EXIT_OPERATIONAL);
        }
    };
    if let Err(err) = std::env::set_current_dir(&canonical) {
        tracing::error!(error = %err, "cannot enter workspace {:?}", canonical);
        return ExitCode::from(janitor_prime::envelope::EXIT_OPERATIONAL);
    }
    match janitor_prime::mcp::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &canonical,
    ) {
        Ok(()) => ExitCode::from(EXIT_SUCCESS),
        Err(err) => {
            let err = JanitorError::from(err);
            tracing::error!(code = err.code(), "MCP session failed: {err}");
            ExitCode::from(janitor_prime::envelope::EXIT_OPERATIONAL)
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
