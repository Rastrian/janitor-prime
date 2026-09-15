//! Fuzz-style property targets (PLAN §3 F7.1, §8 R16): the four entry
//! points that face agent-authored bytes — the DSL rule text, the `apply`
//! diff payload, `LOG.txt`, and `TREE/<size>` records — must **never panic
//! and refuse only with typed errors**. The no-panic property IS the
//! product's contract (PLAN §1.1, ADR-0003); it must be tested, not
//! claimed. proptest executes each property body inside `catch_unwind`, so
//! a panic is a property failure with a shrinkable seed: "no panic" is
//! exactly "the property holds".
//!
//! Each target runs two legs: (a) **corpus replay** — every file under
//! `fuzz/corpus/<target>/` (enumerated at `CARGO_MANIFEST_DIR`, name-sorted,
//! asserted non-empty) is fed through the entry point as a fixed regression
//! seed, so a shrunk failure (or a field capture) is committed next to its
//! fix and re-run by the default `cargo test` gate forever; and (b) a
//! **proptest! block** over generated inputs (default 256 cases). The
//! decision to ship plain proptest targets instead of cargo-fuzz — nightly
//! is absent from this repo's toolchain pins and CI matrix — is recorded in
//! `fuzz/README.md`.

use std::path::PathBuf;

use janitor_prime::apply::diff::{DiffPayload, EditSpec, FileEdit, content_sha};
use janitor_prime::ast::dsl::parse_rule;
use janitor_prime::error::{DslError, EditError, JanitorError, MemoryError};
use janitor_prime::memory::LOCAL_DIR;
use janitor_prime::memory::log::{FactLog, LOG_FILE};
use janitor_prime::memory::tree_store::{BlockSlot, RECORD_WIDTH, TREE_DIR, TreeStore};
use proptest::prelude::*;
use proptest::test_runner::TestCaseResult;

/// The sources `FileEdit::validate` runs against: one pure-ASCII Rust
/// function and one multibyte-heavy string, so the UTF-8-boundary leg of
/// the offset discipline is exercised on real rune spans.
const RUST_SOURCE: &str = "fn alpha(x: u32) -> u32 {\n    let a = x + 1;\n    a\n}\n";
const MULTIBYTE_SOURCE: &str = "const S: &str = \"αβγδ\";\nfn ω(x: u32) -> u32 { x }\n";

/// Every seed of one corpus target, name-sorted (deterministic replay
/// order) and fenced non-empty: a corpus directory that silently vanished
/// from the package would otherwise turn replay into a vacuous pass.
macro_rules! corpus {
    ($target:literal) => {
        corpus_files(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fuzz/corpus/",
            $target
        ))
    };
}

fn corpus_files(dir: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("corpus dir {dir}: {err}"))
        .map(|entry| entry.expect("corpus entry").path())
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "corpus dir {dir} must hold at least one seed"
    );
    files
}

/// Read one seed as bytes (the log/tree corpora are deliberately non-UTF-8
/// in places).
fn read_seed(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// Read one seed as UTF-8 text (the dsl/diff corpora are authored text).
fn read_seed_utf8(path: &std::path::Path) -> String {
    String::from_utf8(read_seed(path))
        .unwrap_or_else(|err| panic!("{}: seed must be UTF-8: {err}", path.display()))
}

// ---------------------------------------------------------------------------
// Target 1: the DSL parser
// ---------------------------------------------------------------------------

/// DSL vocabulary pieces: the special characters, every operator name,
/// bare and hyphenated identifiers, comments, whitespace, a multibyte run,
/// and characters the lexer must refuse — concatenated in arbitrary order
/// and count. The regex-ish generator: every token the lexer knows, plus
/// the ones it must reject. The v0.3 B1 pieces (string literals with and
/// without terminators, the filter heads, class brackets) join both sides
/// of that split.
fn rule_text() -> impl Strategy<Value = String> {
    let pieces = vec![
        "(",
        ")",
        "@",
        "@sym",
        "_",
        "under",
        "seq",
        "capture",
        "kind",
        "any-of",
        "constraints",
        "unify",
        "filters",
        "text",
        "glob",
        "\"",
        "\"panic\"",
        "\"[a-z]*\"",
        "[",
        "]",
        "identifier",
        "block",
        "statement-block",
        ";",
        "; a comment\n",
        " ",
        "\n",
        "\t",
        "αβγ",
        "-",
        "#$%",
        "0",
    ];
    proptest::collection::vec(proptest::sample::select(pieces), 0..64)
        .prop_map(|parts| parts.concat())
}

/// The DSL contract over one input: `parse_rule` either succeeds or fails
/// with a typed `DslError` whose `Lex`/`Parse` position stays inside the
/// input — positions are byte offsets into the rule text, and a diagnostic
/// pointing past the input would be a lie worse than none (docs/DSL.md §7).
fn assert_dsl_outcome(src: &str, label: &str) -> TestCaseResult {
    match parse_rule(src) {
        Ok(_) => Ok(()),
        Err(DslError::Lex { pos, .. }) | Err(DslError::Parse { pos, .. }) => {
            prop_assert!(
                pos as usize <= src.len(),
                "{label}: position {pos} escapes the {}-byte input {src:?}",
                src.len()
            );
            Ok(())
        }
        // parse_rule only produces Lex/Parse; the remaining typed variants
        // belong to rule-compile, which this target does not reach.
        Err(DslError::UnknownKind { .. })
        | Err(DslError::Compile { .. })
        | Err(DslError::UnsupportedVersion { .. }) => Ok(()),
    }
}

#[test]
fn dsl_parser_no_panic_typed_errors_only() {
    // (a) Corpus replay: the KP-6 goldens, the full surface, and the
    // adversarial set (unbalanced parens, 500-deep nesting, `@` alone,
    // `#$%`, trailing tokens, operator-as-atom, empty, multibyte,
    // 4 KiB identifier) — plus the v0.3 B1 filter surface (text/glob
    // clauses and their strings/classes, alone and composed with
    // constraints, and their near-misses: unterminated string, bare
    // needle, empty block, unknown filter kind, unclosed class).
    for path in corpus!("dsl") {
        let label = path.display().to_string();
        assert_dsl_outcome(&read_seed_utf8(&path), &label)
            .unwrap_or_else(|err| panic!("{label}: {err}"));
    }

    // (b1) Vocabulary strategy: arbitrary concatenations of DSL tokens.
    proptest!(|(src in rule_text())| {
        assert_dsl_outcome(&src, "generated")?;
    });

    // (b2) Arbitrary unicode strings.
    proptest!(|(src in ".*")| {
        assert_dsl_outcome(&src, "arbitrary unicode")?;
    });

    // (b3) Arbitrary BYTES. The product takes --rule as a process argument,
    // and arguments are decoded to `String` before the parser is reached:
    // `str::from_utf8` at that boundary is the guard, so non-UTF-8 byte
    // sequences can never become a `parse_rule` input. The property still
    // feeds the lossy decoding through the parser — the U+FFFD
    // substitutions are just more bytes for the lexer to refuse, and even
    // that degenerate path must not panic.
    proptest!(|(raw in proptest::collection::vec(any::<u8>(), 0..512))| {
        let src = String::from_utf8_lossy(&raw);
        if std::str::from_utf8(&raw).is_err() {
            // The guard fired: this input is unreachable by the product's
            // --rule path, and the lossy simulation really did substitute.
            prop_assert!(src.contains('\u{FFFD}'), "lossy decode substitutes U+FFFD");
        }
        assert_dsl_outcome(&src, "lossy bytes")?;
    });
}

// ---------------------------------------------------------------------------
// Target 2: the diff payload parser and edit validator
// ---------------------------------------------------------------------------

/// A small recursive `serde_json::Value` generator (null / bool / number /
/// string / array / object, bounded depth): arbitrary JSON documents, so
/// `from_json_str`'s rejection surface is exercised on structurally
/// hostile input rather than just typos. Integers only — u64 keeps the
/// huge-offset cases (`u64::MAX`) in the generated population.
fn json_value(depth: u8) -> BoxedStrategy<serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<u64>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        ".*".prop_map(serde_json::Value::String),
    ];
    if depth == 0 {
        return leaf.boxed();
    }
    prop_oneof![
        leaf,
        proptest::collection::vec(json_value(depth - 1), 0..6).prop_map(serde_json::Value::Array),
        proptest::collection::hash_map(".*", json_value(depth - 1), 0..6)
            .prop_map(|map| serde_json::Value::Object(map.into_iter().collect())),
    ]
    .boxed()
}

#[test]
fn diff_payload_parser_no_panic_typed_errors_only() {
    // (a) Corpus replay: the documented shape plus malformed JSON, wrong
    // types, duplicate paths, u64::MAX offsets, inverted and rune-cutting
    // ranges, deep nesting.
    for path in corpus!("diff") {
        let label = path.display().to_string();
        let text = read_seed_utf8(&path);
        if let Err(err) = DiffPayload::from_json_str(&text) {
            assert!(
                matches!(err, JanitorError::Edit(EditError::Payload { .. })),
                "{label}: any parse refusal is the typed payload error, got {err:?}"
            );
        }
    }

    // (b1) Arbitrary JSON documents: Ok, or exactly the typed
    // E_EDIT_PAYLOAD — never a panic, never another code.
    proptest!(|(value in json_value(3))| {
        let text = serde_json::to_string(&value).expect("a serde_json::Value always serializes");
        if let Err(err) = DiffPayload::from_json_str(&text) {
            prop_assert!(
                matches!(err, JanitorError::Edit(EditError::Payload { .. })),
                "arbitrary JSON must refuse with the typed payload error, got {err:?}"
            );
        }
    });

    // (b2) FileEdit::validate over arbitrary offsets/texts against both
    // sources: never panics, and every refusal is a typed edit error —
    // StaleView/Mismatch/Overlap/OutOfRange, never Payload (that one is
    // from_json_str's; validate receives already-parsed values).
    proptest!(|(
        path in ".*",
        sha_mode in 0u8..3,
        edits in proptest::collection::vec(
            (any::<u64>(), any::<u64>(), ".*", proptest::option::of(".*")),
            0..8
        )
    )| {
        for (source, source_name) in
            [(RUST_SOURCE.as_bytes(), "rust"), (MULTIBYTE_SOURCE.as_bytes(), "multibyte")]
        {
            // sha_mode 0: no guard; 1: a plausible-but-wrong hash (the
            // StaleView leg); 2: the true hash of this source.
            let expect_sha = match sha_mode {
                0 => None,
                1 => Some("ffffffffffffffff".to_owned()),
                _ => Some(content_sha(source)),
            };
            let file_edit = FileEdit {
                path: path.clone(),
                expect_sha,
                edits: edits
                    .iter()
                    .map(|(start, old_end, new_text, old_text)| EditSpec {
                        start_byte: *start,
                        old_end_byte: *old_end,
                        new_text: new_text.clone(),
                        old_text: old_text.clone(),
                    })
                    .collect(),
            };
            if let Err(err) = file_edit.validate(source) {
                prop_assert!(
                    matches!(
                        err,
                        EditError::StaleView { .. }
                            | EditError::Mismatch { .. }
                            | EditError::Overlap { .. }
                            | EditError::OutOfRange { .. }
                    ),
                    "{source_name}: validate refused with {err:?} — the only legal refusals \
                     are StaleView/Mismatch/Overlap/OutOfRange"
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Target 3: the LOG.txt loader
// ---------------------------------------------------------------------------

/// The load-outcome discipline: `Ok` keeps position-is-identity
/// (`facts[i].id == i` — OptMem's rule, ADR-0006); `Err` is one of the
/// three typed codes this surface may legally produce.
fn assert_log_outcome(log: &FactLog) -> TestCaseResult {
    match log.load() {
        Ok(state) => {
            for (i, fact) in state.facts.iter().enumerate() {
                prop_assert_eq!(fact.id, i as u64, "position is identity: facts[i].id == i");
            }
            Ok(())
        }
        Err(err) => {
            prop_assert!(
                matches!(
                    err.code(),
                    "E_LOG_UNRECOVERABLE" | "E_NOT_INITIALIZED" | "E_VFS_IO"
                ),
                "untyped load refusal {}: {err:?}",
                err.code()
            );
            Ok(())
        }
    }
}

/// A fresh `.janitor` root inside its own tempdir, with `bytes` as the
/// LOG.txt content. One root per case, and the TempDir guard travels with
/// the log: load() may quarantine the torn tail and atomically rewrite the
/// log, so cases must not contaminate each other, and the scratch tree is
/// reclaimed when the guard drops.
fn log_with(bytes: &[u8]) -> (FactLog, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join(LOCAL_DIR);
    std::fs::create_dir_all(&root).expect("create the .janitor root");
    std::fs::write(root.join(LOG_FILE), bytes).expect("write the LOG.txt seed");
    (FactLog::new(&root), tmp)
}

#[test]
fn log_loader_no_panic_quarantine_never_panics() {
    // (a) Corpus replay: valid JSONL, a torn unterminated tail, non-UTF-8
    // bytes, garbage mid-file, empty, CRLF mix.
    for path in corpus!("log") {
        let label = path.display().to_string();
        let (log, _guard) = log_with(&read_seed(&path));
        assert_log_outcome(&log).unwrap_or_else(|err| panic!("{label}: {err}"));
    }

    // (b) Arbitrary bytes (capped at 4 KiB so the fsync-per-quarantine
    // discipline keeps the suite fast): load() — including its quarantine
    // writes and the atomic prefix rewrite — must never panic, must keep
    // ids positional on success, and must refuse only through the typed
    // codes.
    proptest!(|(bytes in proptest::collection::vec(any::<u8>(), 0..4096))| {
        let (log, _guard) = log_with(&bytes);
        assert_log_outcome(&log)?;
    });
}

// ---------------------------------------------------------------------------
// Target 4: the TREE record loader and slot notation parser
// ---------------------------------------------------------------------------

/// The load-outcome discipline for one level file: `Ok` records contain no
/// NUL (padding never leaks into the text) and the file on disk is a whole
/// multiple of the 256-byte record width after any torn-tail trim; `Err`
/// is the typed E_TREE_CORRUPT (or E_VFS_IO for real I/O failures).
fn assert_tree_outcome(store: &TreeStore, size: u64) -> TestCaseResult {
    match store.load(size) {
        Ok(Some((records, _warnings))) => {
            for record in &records {
                prop_assert!(
                    !record.contains('\0'),
                    "decoded records carry no padding NUL"
                );
            }
            let len = std::fs::metadata(store.dir().join(size.to_string()))
                .expect("the level file exists after load")
                .len();
            prop_assert_eq!(
                len % RECORD_WIDTH as u64,
                0,
                "a torn trailing record was trimmed to whole records"
            );
            Ok(())
        }
        // Unreachable in this target (the seed file is always written), but
        // the discipline allows it: a missing level reads as unset.
        Ok(None) => Ok(()),
        Err(err) => {
            prop_assert!(
                matches!(err.code(), "E_TREE_CORRUPT" | "E_VFS_IO"),
                "untyped load refusal {}: {err:?}",
                err.code()
            );
            Ok(())
        }
    }
}

/// A fresh `.janitor/TREE/<size>` level file with `bytes` (own tempdir per
/// case, same reasoning as the log target).
fn tree_with(bytes: &[u8], size: u64) -> (TreeStore, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join(LOCAL_DIR);
    let dir = root.join(TREE_DIR);
    std::fs::create_dir_all(&dir).expect("create the .janitor/TREE dir");
    std::fs::write(dir.join(size.to_string()), bytes).expect("write the level-file seed");
    (TreeStore::new(&root), tmp)
}

/// Slot-notation inputs: arbitrary strings, arbitrary `<u64>-<u64>` pairs
/// (inverted, huge, mis-punctuated), and aligned pairs that are frequently
/// legal — saturating so building the notation itself can never overflow.
fn slot_notation() -> impl Strategy<Value = String> {
    prop_oneof![
        ".*",
        (any::<u64>(), any::<u64>()).prop_map(|(lo, hi)| format!("{lo}-{hi}")),
        (1u32..17, any::<u64>()).prop_map(|(exp, raw)| {
            let size = 1u64 << exp;
            let lo = raw / size * size;
            format!("{lo}-{}", lo.saturating_add(size))
        }),
    ]
}

#[test]
fn tree_record_loader_no_panic_typed_errors_only() {
    // (a) Corpus replay: valid NUL-padded records, a torn partial record,
    // garbage after the NUL padding, a non-UTF-8 record, empty.
    for path in corpus!("tree") {
        let label = path.display().to_string();
        let (store, _guard) = tree_with(&read_seed(&path), 4);
        assert_tree_outcome(&store, 4).unwrap_or_else(|err| panic!("{label}: {err}"));
    }

    // (b1) Arbitrary level-file bytes (capped to keep the atomic-trim
    // rewrite cheap per case).
    proptest!(|(bytes in proptest::collection::vec(any::<u8>(), 0..2048))| {
        let (store, _guard) = tree_with(&bytes, 4);
        assert_tree_outcome(&store, 4)?;
    });

    // (b2) BlockSlot::parse: Ok means a power-of-two size >= 2 whose
    // notation round-trips (parse(notation()) == the same slot); Err is
    // exactly the typed MemoryError::SlotInvalid.
    proptest!(|(notation in slot_notation())| {
        match BlockSlot::parse(&notation) {
            Ok(slot) => {
                prop_assert!(slot.size >= 2 && slot.size.is_power_of_two());
                prop_assert!(
                    matches!(BlockSlot::parse(&slot.notation()), Ok(back) if back == slot),
                    "notation round-trips: {:?} -> {}",
                    slot,
                    slot.notation()
                );
            }
            Err(err) => {
                prop_assert!(
                    matches!(err, MemoryError::SlotInvalid { .. }),
                    "slot refusal is typed: {err:?}"
                );
            }
        }
    });
}
