//! CLI surface (PLAN §1.6).
//!
//! `scan` (F4), `apply` (F5), and the memory set `init|wake|note|nap`
//! (F6) are the wired commands. What is frozen from F0 is the *shape* of
//! every exit: clap errors are intercepted and re-emitted as JSON
//! envelopes so even usage mistakes stay machine-parseable on stdout
//! (PLAN §1.6).

use clap::Parser;
use clap::error::ErrorKind;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
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

    /// Pretty-print the JSON envelope (stdout stays JSON either way); parses
    /// before or after the subcommand (docs/DSL.md §8).
    /// global=true: clap accepts the root flag after the subcommand too.
    #[arg(long, global = true)]
    pub pretty: bool,

    /// The command to run (F4 wires `scan`; apply lands at F5, the memory
    /// commands at F6).
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The command set (PLAN §1.6) — growing by phase, superflat and
/// agent-friendly.
#[derive(Debug, Parser)]
pub enum Command {
    /// Parallel structural search: rule in, match coordinates out
    /// (see docs/DSL.md for the rule language and the report shape).
    #[command(verbatim_doc_comment)]
    Scan(ScanArgs),
    /// Transactional rewrite: diff payload in, whole-diff all-or-nothing
    /// stage → validate → flush out (see docs/SCHEMA.md for the payload
    /// and result shapes).
    Apply(ApplyArgs),
    /// Create the local memory layer (`$PWD/.janitor/`: LOG.txt, TREE/,
    /// STATE.json); refuses when it exists unless --force resets it.
    Init(InitArgs),
    /// Recall memory under a token budget: raw facts and summaries,
    /// oldest-first, plus the standing global layer and state (F6).
    Wake(WakeArgs),
    /// Append one validated fact (one line, ≤ 280 bytes); --ptr anchors it
    /// to a span taken from janitor's own reports, --commit to a commit.
    Note(NoteArgs),
    /// The compression protocol: no args prints the next pending request;
    /// --slot + --text stores the agent-written summary line (F6).
    Nap(NapArgs),
}

/// `janitor init` (PLAN §1.6, F6.7).
#[derive(Debug, Parser)]
pub struct InitArgs {
    /// Reset an existing layer (destructive: the log and tree are deleted).
    #[arg(long)]
    pub force: bool,
}

/// `janitor wake` (PLAN §1.5.2, F6.7).
#[derive(Debug, Parser)]
pub struct WakeArgs {
    /// Token budget for the memory payload (chars4 estimator; the default
    /// mirrors OptMem's ~96 lines ≈ 8k tokens).
    #[arg(long, default_value_t = crate::memory::wake::DEFAULT_BUDGET)]
    pub budget: u64,
}

/// `janitor note` (PLAN §1.5.3, F6.7): exactly one anchor flavor — the
/// compact pointer notation, a full stored-pointer JSON, a commit sha —
/// or none (a free fact).
#[derive(Debug, Parser)]
#[command(
    group(clap::ArgGroup::new("anchor").args(["ptr", "commit"]))
)]
pub struct NoteArgs {
    /// Node anchor: `path:start-end:kind` (byte offsets from janitor
    /// reports) or a stored pointer's JSON (re-anchorable).
    #[arg(long, conflicts_with = "commit")]
    pub ptr: Option<String>,

    /// Commit anchor: a hex object name resolved via `git rev-parse`.
    #[arg(long = "commit")]
    pub commit: Option<String>,

    /// Mark the fact architectural_lock: never compressed, never dropped.
    #[arg(long)]
    pub lock: bool,

    /// The fact itself: one line, ≤ 280 bytes.
    pub text: String,
}

/// `janitor nap` (PLAN §1.5.4, F6.7): `--slot` and `--text` travel
/// together or not at all.
#[derive(Debug, Parser)]
pub struct NapArgs {
    /// The slot to store (`<lo>-<hi>`, from a pending request).
    #[arg(long, requires = "text")]
    pub slot: Option<String>,

    /// The one-line summary (≤ 256 bytes), written by the agent.
    #[arg(long, requires = "slot")]
    pub text: Option<String>,
}

/// `janitor apply` (PLAN §3 F5.6, docs/SCHEMA.md): exactly one payload
/// source — `--diff` (inline JSON, or `@file` to read it from a file) or
/// `--diff-file` (a path, or `-` for stdin, the agent-friendly pipe).
#[derive(Debug, Parser)]
#[command(
    group(clap::ArgGroup::new("payload_source").required(true).args(["diff", "diff_file"]))
)]
pub struct ApplyArgs {
    /// The whole-diff JSON payload; prefix @ to read it from a file.
    #[arg(long, conflicts_with = "diff_file")]
    pub diff: Option<String>,

    /// File holding the JSON payload ("-" reads stdin).
    #[arg(long = "diff-file")]
    pub diff_file: Option<String>,

    /// Validate + gate + report, then roll back: nothing is flushed.
    #[arg(long)]
    pub dry_run: bool,
}

/// `janitor scan` (PLAN §3 F4.7, docs/DSL.md §8; F8 adds `--duplicates`).
/// Exactly one mode: `--rule` (structural search) or `--duplicates`
/// (fingerprint-grouped duplicate detection, no rule).
#[derive(Debug, Parser)]
#[command(
    group(clap::ArgGroup::new("scan_mode").required(true).args(["rule", "duplicates"]))
)]
pub struct ScanArgs {
    /// The DSL rule, s-expression form (docs/DSL.md). The scan mode unless
    /// --duplicates is given.
    #[arg(long)]
    pub rule: Option<String>,

    /// Duplicate-detection mode (F8): group all capturable subtrees at or
    /// above --min-size by Merkle fingerprint and report the groups
    /// (docs/SCHEMA.md). Mutually exclusive with --rule.
    #[arg(long)]
    pub duplicates: bool,

    /// Scan root (default: the current directory).
    pub path: Option<PathBuf>,

    /// Restrict to one canonical language name (e.g. rust); a grammar this
    /// build does not embed is a typed error.
    #[arg(long)]
    pub lang: Option<String>,

    /// Include overlay-staged virtual files (default OFF: scan reports
    /// disk reality, ADR-0016).
    #[arg(long)]
    pub include_virtual: bool,

    /// Byte-strict Seq adjacency (default: comment-skipping, ADR-0010).
    /// Rule mode only — duplicate equivalence is fixed by ADR-0010.
    #[arg(long, conflicts_with = "duplicates")]
    pub strict_trivia: bool,

    /// Skip files larger than this many bytes (default 4 MiB).
    #[arg(long, default_value_t = crate::scan::DEFAULT_MAX_FILE_SIZE)]
    pub max_file_size: u64,

    /// Duplicates mode: minimum subtree size in participating nodes
    /// (comment- and formatting-invariant) for grouping (default 8).
    #[arg(long, conflicts_with = "rule", default_value_t = crate::scan::DEFAULT_MIN_DUPLICATE_SIZE)]
    pub min_size: u32,
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

    /// docs/DSL.md §8 advertises `scan … [--pretty]`: the root-level flag
    /// must parse on either side of the subcommand (cargo's `--verbose`
    /// convention) and still reach the root `Cli` it modifies.
    #[test]
    fn pretty_parses_after_the_subcommand() {
        let cli = Cli::try_parse_from(["janitor", "scan", "--rule", "_", "--pretty"])
            .expect("scan with a trailing --pretty (docs/DSL.md §8)");
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

    /// F4/F8: `scan` parses in both modes with every flag and its defaults
    /// (docs/DSL.md §8); exactly one of --rule/--duplicates; a mode-less
    /// scan is a usage error.
    #[test]
    fn scan_subcommand_parses_with_defaults() {
        let cli = Cli::try_parse_from(["janitor", "scan", "--rule", "(under block (seq _ _))"])
            .expect("scan with only --rule");
        let Some(Command::Scan(args)) = cli.command else {
            unreachable!("scan subcommand");
        };
        assert_eq!(args.rule.as_deref(), Some("(under block (seq _ _))"));
        assert!(!args.duplicates);
        assert_eq!(args.path, None);
        assert_eq!(args.lang, None);
        assert!(!args.include_virtual);
        assert!(!args.strict_trivia);
        assert_eq!(args.max_file_size, crate::scan::DEFAULT_MAX_FILE_SIZE);
        assert_eq!(args.min_size, crate::scan::DEFAULT_MIN_DUPLICATE_SIZE);

        let cli = Cli::try_parse_from([
            "janitor",
            "scan",
            "--rule",
            "_",
            "src",
            "--lang",
            "rust",
            "--include-virtual",
            "--strict-trivia",
            "--max-file-size",
            "1024",
        ])
        .expect("scan with every flag");
        let Some(Command::Scan(args)) = cli.command else {
            unreachable!("scan subcommand");
        };
        assert_eq!(args.path.as_deref(), Some(std::path::Path::new("src")));
        assert_eq!(args.lang.as_deref(), Some("rust"));
        assert!(args.include_virtual && args.strict_trivia);
        assert_eq!(args.max_file_size, 1024);

        // A mode is required: usage error envelope, exit 2.
        let err = Cli::try_parse_from(["janitor", "scan"]).expect_err("a mode is required");
        let (envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);
        assert!(!envelope.ok);
    }

    /// F8: the duplicates mode parses with its defaults and tunables, and
    /// the mode flags keep each other honest — both modes at once, a
    /// rule-mode-only flag under --duplicates, and a duplicates-only flag
    /// under --rule are all usage errors.
    #[test]
    fn scan_duplicates_mode_parses_and_excludes_the_other_mode() {
        let cli =
            Cli::try_parse_from(["janitor", "scan", "--duplicates", "src", "--min-size", "12"])
                .expect("duplicates with a tunable");
        let Some(Command::Scan(args)) = cli.command else {
            unreachable!("scan subcommand");
        };
        assert!(args.duplicates);
        assert_eq!(args.rule, None);
        assert_eq!(args.path.as_deref(), Some(std::path::Path::new("src")));
        assert_eq!(args.min_size, 12);
        assert_eq!(args.max_file_size, crate::scan::DEFAULT_MAX_FILE_SIZE);

        let cli = Cli::try_parse_from(["janitor", "scan", "--duplicates", "--lang", "rust"])
            .expect("duplicates with the shared flags");
        let Some(Command::Scan(args)) = cli.command else {
            unreachable!("scan subcommand");
        };
        assert_eq!(args.min_size, crate::scan::DEFAULT_MIN_DUPLICATE_SIZE);

        for argv in [
            vec!["janitor", "scan", "--duplicates", "--rule", "_"],
            vec!["janitor", "scan", "--duplicates", "--strict-trivia"],
            vec!["janitor", "scan", "--rule", "_", "--min-size", "4"],
        ] {
            let err = Cli::try_parse_from(argv).expect_err("mode flags exclude each other");
            let (_envelope, code) = clap_error_envelope(&err);
            assert_eq!(code, envelope::EXIT_USAGE);
        }
    }

    /// F5: `apply` parses its flags (docs/SCHEMA.md); exactly one payload
    /// source is required and both at once is a usage error.
    #[test]
    fn apply_subcommand_parses_with_one_payload_source() {
        let cli = Cli::try_parse_from(["janitor", "apply", "--diff", r#"{"operations":[]}"#])
            .expect("inline payload");
        let Some(Command::Apply(args)) = cli.command else {
            unreachable!("apply subcommand");
        };
        assert_eq!(args.diff.as_deref(), Some(r#"{"operations":[]}"#));
        assert_eq!(args.diff_file, None);
        assert!(!args.dry_run);

        let cli = Cli::try_parse_from(["janitor", "apply", "--diff-file", "-", "--dry-run"])
            .expect("stdin payload with dry-run");
        let Some(Command::Apply(args)) = cli.command else {
            unreachable!("apply subcommand");
        };
        assert_eq!(args.diff_file.as_deref(), Some("-"));
        assert!(args.dry_run);

        // No source: usage error, exit 2 (the arg group is required).
        let err = Cli::try_parse_from(["janitor", "apply"]).expect_err("a source is required");
        let (_envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);

        // Both sources: usage error (conflicts_with).
        let err = Cli::try_parse_from(["janitor", "apply", "--diff", "{}", "--diff-file", "-"])
            .expect_err("one source only");
        let (_envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);
    }

    /// F6: the memory commands parse with their defaults and their
    /// together-or-not-at-all flags (docs/SCHEMA.md carries the payloads).
    #[test]
    fn memory_subcommands_parse_with_defaults() {
        // init
        let cli = Cli::try_parse_from(["janitor", "init"]).expect("init");
        let Some(Command::Init(args)) = cli.command else {
            unreachable!("init subcommand");
        };
        assert!(!args.force);

        // wake: default budget, override
        let cli = Cli::try_parse_from(["janitor", "wake"]).expect("wake");
        let Some(Command::Wake(args)) = cli.command else {
            unreachable!("wake subcommand");
        };
        assert_eq!(args.budget, crate::memory::wake::DEFAULT_BUDGET);
        let cli = Cli::try_parse_from(["janitor", "wake", "--budget", "128"]).expect("wake budget");
        let Some(Command::Wake(args)) = cli.command else {
            unreachable!("wake subcommand");
        };
        assert_eq!(args.budget, 128);

        // note: free fact, anchored fact, locked fact
        let cli =
            Cli::try_parse_from(["janitor", "note", "the overlay invariants hold"]).expect("note");
        let Some(Command::Note(args)) = cli.command else {
            unreachable!("note subcommand");
        };
        assert_eq!(args.text, "the overlay invariants hold");
        assert!(args.ptr.is_none() && args.commit.is_none() && !args.lock);

        let cli = Cli::try_parse_from([
            "janitor",
            "note",
            "--ptr",
            "src/lib.rs:120-480:function_item",
            "--lock",
            "read_dir never consults the buffer",
        ])
        .expect("anchored note");
        let Some(Command::Note(args)) = cli.command else {
            unreachable!("note subcommand");
        };
        assert_eq!(
            args.ptr.as_deref(),
            Some("src/lib.rs:120-480:function_item")
        );
        assert!(args.lock);

        let cli = Cli::try_parse_from([
            "janitor",
            "note",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "the refactor landed here",
        ])
        .expect("commit note");
        let Some(Command::Note(args)) = cli.command else {
            unreachable!("note subcommand");
        };
        assert_eq!(
            args.commit.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );

        // --ptr and --commit conflict; a missing text is a usage error.
        let err = Cli::try_parse_from([
            "janitor",
            "note",
            "--ptr",
            "a:0-4:identifier",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "t",
        ])
        .expect_err("one anchor flavor");
        let (_envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);
        let err = Cli::try_parse_from(["janitor", "note"]).expect_err("text is required");
        let (_envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);

        // nap: bare (next pending), and slot+text together
        let cli = Cli::try_parse_from(["janitor", "nap"]).expect("nap");
        let Some(Command::Nap(args)) = cli.command else {
            unreachable!("nap subcommand");
        };
        assert!(args.slot.is_none() && args.text.is_none());

        let cli = Cli::try_parse_from([
            "janitor",
            "nap",
            "--slot",
            "0-2",
            "--text",
            "facts 0-1: the overlay invariants",
        ])
        .expect("nap settle");
        let Some(Command::Nap(args)) = cli.command else {
            unreachable!("nap subcommand");
        };
        assert_eq!(args.slot.as_deref(), Some("0-2"));
        assert_eq!(
            args.text.as_deref(),
            Some("facts 0-1: the overlay invariants")
        );

        // Slot without text (and vice versa): usage error.
        let err = Cli::try_parse_from(["janitor", "nap", "--slot", "0-2"]).expect_err("slot alone");
        let (_envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);
        let err = Cli::try_parse_from(["janitor", "nap", "--text", "x"]).expect_err("text alone");
        let (_envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);
    }
}
