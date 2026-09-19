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

pub use generated::{ENABLED_GRAMMARS, EXPERIMENTAL_GRAMMARS, STABLE_GRAMMARS};

/// The tier split of a build's grammar set (B9): which enabled grammars
/// are stable (the lang-all fence set) and which are experimental (not
/// yet through docs/PROMOTION.md's checklist). Append-only sibling of the
/// flat `grammars` list — that list is untouched.
#[derive(Debug, Serialize)]
pub struct VersionTiers {
    pub stable: &'static [&'static str],
    /// Absent when the build carries no experimental grammar (an empty
    /// array would read as "a tier that happens to be empty"; absence is
    /// the honest "this build has no experimental grammars").
    #[serde(skip_serializing_if = "<[&str]>::is_empty")]
    pub experimental: &'static [&'static str],
}

/// Payload of the `--version` envelope (PLAN §3 F0.2; `tiers` appended at
/// B9 — `version` and `grammars` are byte-stable, key order included).
#[derive(Debug, Serialize)]
pub struct VersionData {
    pub version: &'static str,
    pub grammars: &'static [&'static str],
    pub tiers: VersionTiers,
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
    /// oldest-first, plus the standing global layer and state (F6);
    /// --task ranks the recall against a query below the 350-token
    /// ceiling, degrades honestly to positional at or above it (C1/C1-R).
    Wake(WakeArgs),
    /// Append one validated fact (one line, ≤ 280 bytes — as an argument
    /// or piped via --stdin); --ptr anchors it with the notation
    /// `path:byte_start-byte_end:kind_name` (spans from janitor's own
    /// reports), --commit to a commit.
    Note(NoteArgs),
    /// The compression protocol: no args prints the next pending request;
    /// --slot + --text stores the agent-written summary line (F6).
    Nap(NapArgs),
    /// Serve the MCP stdio protocol (F10b, ADR-0019): janitor as native
    /// tools for MCP clients (docs/MCP.md). Stdout carries MCP frames
    /// only; run it from the client's config, not a terminal.
    Mcp(McpArgs),
    /// Run the warm-cache daemon (F10a, ADR-0018): watch the workspace and
    /// serve scan/duplicates/ping/stats over a local Unix socket
    /// (docs/SCHEMA.md § daemon for the protocol).
    Daemon(DaemonArgs),
}

/// `janitor daemon` (F10a, ADR-0018): defaults mirror the ADR — 2 s poll,
/// 512 MiB cache bound, 30 min idle death.
#[derive(Debug, Parser)]
pub struct DaemonArgs {
    /// Workspace root the daemon serves and watches (default: the current
    /// directory; pinned and canonicalized at start — every request path
    /// must resolve inside it).
    #[arg(long)]
    pub workspace: Option<PathBuf>,

    /// Socket path (default: $JANITOR_SOCK or /tmp/janitor-$UID.sock;
    /// permissions 0600).
    #[arg(long)]
    pub sock: Option<PathBuf>,

    /// Watch poll interval in seconds (polling, not inotify — ADR-0018 D2).
    #[arg(long, default_value_t = crate::daemon::DEFAULT_INTERVAL_SECS)]
    pub interval: u64,

    /// Parse-cache bound in MiB; least-recently-used entries are evicted
    /// above it (ADR-0018 D4).
    #[arg(long, default_value_t = crate::daemon::DEFAULT_MAX_CACHE_MB)]
    pub max_cache_mb: u64,

    /// Exit after this many seconds without connections, so stale daemons
    /// die naturally (ADR-0018 D7).
    #[arg(long, default_value_t = crate::daemon::DEFAULT_IDLE_SECS)]
    pub idle_secs: u64,
}

/// `janitor init` (PLAN §1.6, F6.7).
#[derive(Debug, Parser)]
pub struct InitArgs {
    /// Reset an existing layer (destructive: the log and tree are deleted).
    #[arg(long)]
    pub force: bool,
}

/// `janitor wake` (PLAN §1.5.2, F6.7; C1 adds `--task`).
#[derive(Debug, Parser)]
pub struct WakeArgs {
    /// Token budget for the memory payload (chars4 estimator; the default
    /// mirrors OptMem's ~96 lines ≈ 8k tokens).
    #[arg(long, default_value_t = crate::memory::wake::DEFAULT_BUDGET)]
    pub budget: u64,

    /// Rank the recall against this query (store-local TF-IDF, scorer
    /// "tfidf-store"): the best-scoring facts are served raw while the
    /// budget allows, the rest fills with stored summaries. Runs only
    /// below the 350-token task ceiling (TASK_BUDGET_CEILING): at or
    /// above it the positional payload serves and the task block says
    /// why ("degraded": "budget_at_or_above_ceiling") — a big
    /// positional budget already covers everything via summaries.
    /// Absent: the position-only tiling, byte-identical to previous
    /// versions.
    #[arg(long)]
    pub task: Option<String>,
}

/// `janitor note --confidence` (C4): the agent-asserted provenance
/// level, spelled exactly as it is stored. One value ships (`low`); the
/// mapping onto the memory-layer enum lives here so the core stays
/// clap-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum ConfidenceArg {
    Low,
}

impl From<ConfidenceArg> for crate::memory::log::Confidence {
    fn from(value: ConfidenceArg) -> Self {
        match value {
            ConfidenceArg::Low => Self::Low,
        }
    }
}

/// `janitor note` (PLAN §1.5.3, F6.7): exactly one anchor flavor — the
/// compact pointer notation, a full stored-pointer JSON, a commit sha —
/// or none (a free fact). v0.3 B5 adds `--stdin` as the text source
/// (piped input, same validation); C4 adds `--confidence low`.
#[derive(Debug, Parser)]
#[command(
    group(clap::ArgGroup::new("anchor").args(["ptr", "commit"])),
    group(clap::ArgGroup::new("note_source").required(true).args(["text", "stdin"]))
)]
pub struct NoteArgs {
    /// Node anchor in the compact notation `path:byte_start-byte_end:kind_name`
    /// — e.g. `src/lib.rs:120-480:function_item` — where the byte span comes
    /// from janitor's own `scan` output; or a stored pointer's JSON
    /// (re-anchorable).
    #[arg(long, conflicts_with = "commit")]
    pub ptr: Option<String>,

    /// Commit anchor: a hex object name resolved via `git rev-parse`.
    #[arg(long = "commit")]
    pub commit: Option<String>,

    /// Read the fact text from stdin instead of the argument (piped
    /// evidence without quoting pain): reads to EOF, strips one trailing
    /// newline; the one-line ≤ 280-byte validation is identical.
    #[arg(long)]
    pub stdin: bool,

    /// Mark the fact architectural_lock: never compressed, never dropped.
    #[arg(long)]
    pub lock: bool,

    /// Tag the fact low confidence — YOUR assertion, never the binary's
    /// (no auto-detection): ranked wakes (`wake --task`) tie-break
    /// against it and drop it first under budget pressure; positional
    /// wakes seat it one split level coarser. It still serves whenever
    /// the budget allows — down-weighted, never filtered.
    #[arg(long, value_enum)]
    pub confidence: Option<ConfidenceArg>,

    /// The fact itself: one line, ≤ 280 bytes (or pipe it via --stdin).
    pub text: Option<String>,
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

/// `janitor mcp` (F10b, ADR-0019, docs/MCP.md).
#[derive(Debug, Parser)]
pub struct McpArgs {
    /// The workspace every tool path resolves within (default: the current
    /// directory). Paths that escape it are the typed E_VFS_INVALID_PATH
    /// refusal — same rule as the daemon lane.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
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

    /// Duplicates mode: only groups with at least N members survive
    /// (default 2 — a duplicate needs two; values below 2 behave as 2).
    #[arg(long, conflicts_with = "rule")]
    pub min_members: Option<u32>,

    /// Duplicates mode: prepend the per-kind aggregate block — group and
    /// member counts per (language, kind) plus totals, one envelope,
    /// no jq.
    #[arg(long, conflicts_with = "rule")]
    pub summary: bool,

    /// Route through the warm-cache daemon when its socket answers `ping`
    /// for this workspace; otherwise run in-process, silently (ADR-0018
    /// D7). The CLI never spawns the daemon itself — start it explicitly
    /// with `janitor daemon`.
    #[arg(long)]
    pub daemon: bool,
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
                    "tsx",
                    "java",
                    "c",
                    // Experimental tier (F9c+F9d) — same --version surface,
                    // fenced out of lang-all (ADR-0015 appended note).
                    "ruby",
                    "ocaml",
                    "ocaml_interface",
                    "elixir"
                ]
                .contains(grammar),
                "unknown grammar {grammar:?} embedded by build.rs"
            );
        }
    }

    /// B9: the `tiers` key is the append-only tier split of `grammars` —
    /// stable holds exactly the enabled stable-set grammars, experimental
    /// exactly the enabled experimental ones, and the two lists partition
    /// `grammars` in registry order. This FAILS if any experimental
    /// grammar is listed as stable (or vice versa), in any feature combo:
    /// the expected tier of every grammar name is pinned here against the
    /// build's actual split.
    #[test]
    fn version_tiers_split_grammars_by_promotion_state() {
        for (name, stable_expected) in [
            ("rust", true),
            ("python", true),
            ("go", true),
            ("javascript", true),
            ("typescript", true),
            ("tsx", true),
            ("java", true),
            ("c", true),
            ("ruby", false),
            ("ocaml", false),
            ("ocaml_interface", false),
            ("elixir", false),
        ] {
            let enabled = ENABLED_GRAMMARS.contains(&name);
            assert_eq!(
                STABLE_GRAMMARS.contains(&name),
                enabled && stable_expected,
                "{name}: stable-tier membership must follow the promotion state"
            );
            assert_eq!(
                EXPERIMENTAL_GRAMMARS.contains(&name),
                enabled && !stable_expected,
                "{name}: experimental-tier membership must follow the promotion state"
            );
        }
        // Partition, registry order, both directions: no grammar vanishes
        // from the split and none is counted twice.
        let mut joined = STABLE_GRAMMARS.to_vec();
        joined.extend_from_slice(EXPERIMENTAL_GRAMMARS);
        assert_eq!(joined, ENABLED_GRAMMARS);
        assert!(
            STABLE_GRAMMARS
                .iter()
                .all(|g| !EXPERIMENTAL_GRAMMARS.contains(g))
        );
    }

    /// B9: the serialized `--version` payload — `tiers` is a NEW key after
    /// the byte-stable `version`/`grammars` pair; `experimental` appears
    /// iff the build carries an experimental grammar; the bytes are
    /// deterministic run to run (the pin-check fence relies on it).
    #[test]
    fn version_data_serializes_tiers_append_only_and_deterministically() {
        let data = VersionData {
            version: env!("CARGO_PKG_VERSION"),
            grammars: ENABLED_GRAMMARS,
            tiers: VersionTiers {
                stable: STABLE_GRAMMARS,
                experimental: EXPERIMENTAL_GRAMMARS,
            },
        };
        let value = serde_json::to_value(&data).expect("serializes");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["grammars"], serde_json::json!(ENABLED_GRAMMARS));

        let tiers = &value["tiers"];
        assert_eq!(tiers["stable"], serde_json::json!(STABLE_GRAMMARS));
        // `experimental` appears iff the build carries an experimental
        // grammar — absence, not an empty array. Matched against a local
        // copy of the const: the default-feature view would const-fold an
        // `is_empty` branch (clippy 1.90's const_is_empty; the MSRV leg).
        let expected_experimental: Vec<&str> = EXPERIMENTAL_GRAMMARS.to_vec();
        match tiers.get("experimental") {
            None => assert!(
                expected_experimental.is_empty(),
                "experimental grammars {expected_experimental:?} present but the key is absent — {value}"
            ),
            Some(list) => assert_eq!(
                list,
                &serde_json::json!(expected_experimental),
                "the experimental tier must serialize exactly its grammar list"
            ),
        }

        // Byte-determinism and append-only key order: the exact string is
        // stable across serializations, `tiers` trailing.
        let a = serde_json::to_string(&data).expect("serializes");
        let b = serde_json::to_string(&data).expect("serializes");
        assert_eq!(a, b);
        assert!(
            a.find("\"grammars\":")
                .is_some_and(|g| a.find("\"tiers\":").is_some_and(|t| g < t)),
            "tiers is the new trailing key: {a}"
        );
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
        // v0.3 B3: the new knobs default off/None (back-compat)…
        assert!(!args.summary);
        assert_eq!(args.min_members, None);

        // …and parse under --duplicates.
        let cli = Cli::try_parse_from([
            "janitor",
            "scan",
            "--duplicates",
            "src",
            "--summary",
            "--min-members",
            "3",
        ])
        .expect("duplicates with the v0.3 flags");
        let Some(Command::Scan(args)) = cli.command else {
            unreachable!("scan subcommand");
        };
        assert!(args.summary);
        assert_eq!(args.min_members, Some(3));

        for argv in [
            vec!["janitor", "scan", "--duplicates", "--rule", "_"],
            vec!["janitor", "scan", "--duplicates", "--strict-trivia"],
            vec!["janitor", "scan", "--rule", "_", "--min-size", "4"],
            vec!["janitor", "scan", "--rule", "_", "--summary"],
            vec!["janitor", "scan", "--rule", "_", "--min-members", "3"],
        ] {
            let err = Cli::try_parse_from(argv).expect_err("mode flags exclude each other");
            let (_envelope, code) = clap_error_envelope(&err);
            assert_eq!(code, envelope::EXIT_USAGE);
        }
    }

    /// F10b: `mcp` parses with and without --workspace; the flag is for
    /// the client config, not the terminal.
    #[test]
    fn mcp_subcommand_parses() {
        let cli = Cli::try_parse_from(["janitor", "mcp"]).expect("mcp");
        let Some(Command::Mcp(args)) = cli.command else {
            unreachable!("mcp subcommand");
        };
        assert_eq!(args.workspace, None);

        let cli =
            Cli::try_parse_from(["janitor", "mcp", "--workspace", "/tmp/repo"]).expect("workspace");
        let Some(Command::Mcp(args)) = cli.command else {
            unreachable!("mcp subcommand");
        };
        assert_eq!(
            args.workspace.as_deref(),
            Some(std::path::Path::new("/tmp/repo"))
        );
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

        // wake: default budget, override, and the C1 ranked mode
        let cli = Cli::try_parse_from(["janitor", "wake"]).expect("wake");
        let Some(Command::Wake(args)) = cli.command else {
            unreachable!("wake subcommand");
        };
        assert_eq!(args.budget, crate::memory::wake::DEFAULT_BUDGET);
        assert_eq!(args.task, None, "the positional pass is the default");
        let cli = Cli::try_parse_from(["janitor", "wake", "--budget", "128"]).expect("wake budget");
        let Some(Command::Wake(args)) = cli.command else {
            unreachable!("wake subcommand");
        };
        assert_eq!(args.budget, 128);
        let cli = Cli::try_parse_from([
            "janitor",
            "wake",
            "--task",
            "daemon socket errors",
            "--budget",
            "1500",
        ])
        .expect("ranked wake");
        let Some(Command::Wake(args)) = cli.command else {
            unreachable!("wake subcommand");
        };
        assert_eq!(args.task.as_deref(), Some("daemon socket errors"));
        assert_eq!(args.budget, 1500);

        // note: free fact, anchored fact, locked fact
        let cli =
            Cli::try_parse_from(["janitor", "note", "the overlay invariants hold"]).expect("note");
        let Some(Command::Note(args)) = cli.command else {
            unreachable!("note subcommand");
        };
        assert_eq!(args.text.as_deref(), Some("the overlay invariants hold"));
        assert!(args.ptr.is_none() && args.commit.is_none() && !args.lock);
        assert_eq!(args.confidence, None, "untagged is the default");
        assert!(!args.stdin, "the argument is the default source");

        // C4: --confidence low parses (and composes with --lock); any
        // other level is a usage error — the one asserted value ships.
        let cli = Cli::try_parse_from([
            "janitor",
            "note",
            "--confidence",
            "low",
            "--lock",
            "migrated import, truncated mid-thought",
        ])
        .expect("low-confidence lock");
        let Some(Command::Note(args)) = cli.command else {
            unreachable!("note subcommand");
        };
        assert_eq!(args.confidence, Some(ConfidenceArg::Low));
        assert!(args.lock);
        let err = Cli::try_parse_from(["janitor", "note", "--confidence", "high", "t"])
            .expect_err("one asserted level ships");
        let (_envelope, code) = clap_error_envelope(&err);
        assert_eq!(code, envelope::EXIT_USAGE);

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

        // v0.3 B5: --stdin is the alternative text source — parses with
        // every anchor flavor, and exactly one source is taken (both at
        // once, or neither, is a usage error).
        let cli =
            Cli::try_parse_from(["janitor", "note", "--stdin", "--lock"]).expect("stdin note");
        let Some(Command::Note(args)) = cli.command else {
            unreachable!("note subcommand");
        };
        assert!(args.stdin);
        assert_eq!(args.text, None);
        assert!(args.lock);
        let cli = Cli::try_parse_from([
            "janitor",
            "note",
            "--ptr",
            "src/lib.rs:120-480:function_item",
            "--stdin",
        ])
        .expect("anchored stdin note");
        let Some(Command::Note(args)) = cli.command else {
            unreachable!("note subcommand");
        };
        assert!(args.stdin && args.ptr.is_some() && args.text.is_none());

        for argv in [
            vec!["janitor", "note", "--stdin", "and an argument"],
            vec!["janitor", "note", "--lock"],
        ] {
            let err = Cli::try_parse_from(argv).expect_err("exactly one note source");
            let (_envelope, code) = clap_error_envelope(&err);
            assert_eq!(code, envelope::EXIT_USAGE);
        }

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
