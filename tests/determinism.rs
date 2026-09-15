//! ADR-0016 (3) determinism fence: the same scan under
//! `RAYON_NUM_THREADS=1` and `=8` must emit **byte-identical JSON** — the
//! canonical `(start_byte, end_byte, rule_id)` order plus path-sorted
//! files make the report a function of the input, not the thread count.
//! `stats.wall_ms` is the single documented exception (wall-clock by
//! definition); it is stripped before comparison and asserted to exist.

use std::process::Command;

fn corpus() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Enough files that both 1 and 8 threads have real work to partition,
    // with overlapping match shapes (adjacent starts) exercising the
    // overlap flag's order-independence.
    for i in 0..24_u32 {
        let pkg = i % 3;
        let nested = tmp.path().join(format!("pkg{pkg}")).join("inner");
        std::fs::create_dir_all(&nested).expect("dirs");
        std::fs::write(
            nested.join(format!("mod{i}.rs")),
            format!(
                "fn unit_{i}(v: u32) -> u32 {{\n    let a = v + {i};\n    let b = a * 2;\n    // note\n    let c = b;\n    c\n}}\n"
            ),
        )
        .expect("write");
    }
    tmp
}

fn scan_with_threads(root: &std::path::Path, threads: u32) -> String {
    let bin = env!("CARGO_BIN_EXE_janitor");
    let out = Command::new(bin)
        .env("RAYON_NUM_THREADS", threads.to_string())
        .args([
            "scan",
            "--rule",
            "(under block (seq (kind let_declaration) (kind let_declaration)))",
        ])
        .arg(root)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(0), "scan under {threads} threads");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn scan_json_is_byte_identical_across_rayon_thread_counts() {
    let tmp = corpus();

    let one = scan_with_threads(tmp.path(), 1);
    let eight = scan_with_threads(tmp.path(), 8);

    // wall_ms exists and is stripped: the only field allowed to vary.
    let strip_wall = |raw: &str| -> String {
        let mut envelope: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|err| panic!("not JSON: {err}"));
        assert!(
            envelope["data"]["stats"]["wall_ms"].as_u64().is_some(),
            "wall_ms must exist (and be stripped for comparison)"
        );
        envelope["data"]["stats"]["wall_ms"].take();
        serde_json::to_string(&envelope).expect("re-serialize")
    };

    let one_stripped = strip_wall(&one);
    let eight_stripped = strip_wall(&eight);
    assert_eq!(
        one_stripped, eight_stripped,
        "ADR-0016 (3): the report is a function of the input, not the thread count"
    );

    // The comparison bites: 24 files × 3 lets each = 48 matches under the
    // default policy (two adjacent pairs per file), overlap flags included.
    let envelope: serde_json::Value = serde_json::from_str(&one).expect("re-parse");
    assert_eq!(envelope["data"]["stats"]["matches"], 48);
    assert_eq!(envelope["data"]["stats"]["files"], 24);
}

/// v0.3 B1: the same fence over a FILTERED rule — the leaf-text pass runs
/// per file inside the workers before canonical assembly, so a filtered
/// report is as thread-count-independent as an unfiltered one. The class
/// glob keeps the `a` and `b` bindings (48 of the 72 lets), the same
/// filter corpus-wide.
#[test]
fn filtered_scan_json_is_byte_identical_across_rayon_thread_counts() {
    let tmp = corpus();
    let run = |threads: u32| -> String {
        let bin = env!("CARGO_BIN_EXE_janitor");
        let out = Command::new(bin)
            .env("RAYON_NUM_THREADS", threads.to_string())
            .args([
                "scan",
                "--rule",
                "(under block (seq (capture @l let_declaration)) \
                 (filters (glob @l \"[ab]\")))",
            ])
            .arg(tmp.path())
            .output()
            .expect("binary runs");
        assert_eq!(out.status.code(), Some(0), "scan under {threads} threads");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let one = run(1);
    let eight = run(8);
    let strip_wall = |raw: &str| -> String {
        let mut envelope: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|err| panic!("not JSON: {err}"));
        envelope["data"]["stats"]["wall_ms"].take();
        serde_json::to_string(&envelope).expect("re-serialize")
    };
    assert_eq!(
        strip_wall(&one),
        strip_wall(&eight),
        "the filtered report is a function of the input, not the thread count"
    );
    let envelope: serde_json::Value = serde_json::from_str(&one).expect("re-parse");
    assert_eq!(envelope["data"]["stats"]["matches"], 48, "the a and b lets");
}

/// v0.3 B3: the same fence over `--duplicates --summary` — the summary's
/// per-kind rows are a pure function of the canonically ordered groups,
/// so the whole report (aggregate included) is byte-identical under
/// `RAYON_NUM_THREADS=1` vs `=8`, through the real binary.
#[test]
fn duplicates_summary_json_is_byte_identical_across_rayon_thread_counts() {
    let tmp = corpus();

    let run = |threads: u32| -> String {
        let bin = env!("CARGO_BIN_EXE_janitor");
        let out = Command::new(bin)
            .env("RAYON_NUM_THREADS", threads.to_string())
            .args(["scan", "--duplicates", "--summary", "--min-members", "3"])
            .arg(tmp.path())
            .output()
            .expect("binary runs");
        assert_eq!(
            out.status.code(),
            Some(0),
            "duplicates under {threads} threads"
        );
        let mut envelope: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|err| panic!("not JSON under {threads} threads ({err})"));
        assert!(
            envelope["data"]["summary"].is_object(),
            "the summary block rides the envelope"
        );
        envelope["data"]["stats"]["wall_ms"].take();
        serde_json::to_string(&envelope).expect("re-serialize")
    };

    assert_eq!(
        run(1),
        run(8),
        "v0.3 B3: the summary is a function of the input, not the thread count"
    );
}

/// F9a: the same fence over **python** — the second language must inherit
/// ADR-0016 (3) byte-identical reports under `RAYON_NUM_THREADS=1` vs
/// `=8`, over the bundled python fixture repo (comment-heavy, decorated
/// defs, elif/else chains — the shapes whose comment-skipping could in
/// principle depend on interleaving if assembly were order-sensitive).
/// Ground truth mirrors the KP-6-python measurement: method-adjacency
/// counts 7 in skip mode and 5 under `--strict-trivia`.
#[test]
#[cfg(feature = "lang-python")]
fn python_scan_json_is_byte_identical_across_rayon_thread_counts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python")
        .canonicalize()
        .expect("fixture repo");
    let rule = "(under block (seq (kind function_definition) (kind function_definition)))";

    let scan = |threads: u32, strict: bool| {
        let bin = env!("CARGO_BIN_EXE_janitor");
        let mut cmd = Command::new(bin);
        cmd.env("RAYON_NUM_THREADS", threads.to_string())
            .args(["scan", "--rule", rule]);
        if strict {
            cmd.arg("--strict-trivia");
        }
        let out = cmd.arg(&root).output().expect("binary runs");
        assert_eq!(out.status.code(), Some(0), "scan under {threads} threads");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let strip_wall = |raw: &str| -> String {
        let mut envelope: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|err| panic!("not JSON: {err}"));
        assert!(envelope["data"]["stats"]["wall_ms"].as_u64().is_some());
        envelope["data"]["stats"]["wall_ms"].take();
        serde_json::to_string(&envelope).expect("re-serialize")
    };

    for strict in [true, false] {
        let one = strip_wall(&scan(1, strict));
        let eight = strip_wall(&scan(8, strict));
        assert_eq!(
            one, eight,
            "python report (strict-trivia={strict}) is thread-count-independent"
        );
    }

    // The comparison bites: the KP-6-python hand-computed totals.
    let skip: serde_json::Value = serde_json::from_str(&scan(8, false)).expect("re-parse");
    let strict: serde_json::Value = serde_json::from_str(&scan(8, true)).expect("re-parse");
    assert_eq!(skip["data"]["stats"]["matches"], 7);
    assert_eq!(strict["data"]["stats"]["matches"], 5);
}

/// F9b: the same fence over **go** — third language, same guarantee:
/// byte-identical reports under `RAYON_NUM_THREADS=1` vs `=8`, over the
/// bundled go fixture module (doc comments between functions, a grouped
/// import block, struct tags — the shapes whose comment-skipping could
/// in principle depend on interleaving if assembly were
/// order-sensitive). Ground truth mirrors the KP-6-go measurement:
/// function-adjacency counts 4 in skip mode and 2 under
/// `--strict-trivia`.
#[test]
#[cfg(feature = "lang-go")]
fn go_scan_json_is_byte_identical_across_rayon_thread_counts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/go")
        .canonicalize()
        .expect("fixture module");
    let rule = "(under source_file (seq (kind function_declaration) (kind function_declaration)))";

    let scan = |threads: u32, strict: bool| {
        let bin = env!("CARGO_BIN_EXE_janitor");
        let mut cmd = Command::new(bin);
        cmd.env("RAYON_NUM_THREADS", threads.to_string())
            .args(["scan", "--rule", rule]);
        if strict {
            cmd.arg("--strict-trivia");
        }
        let out = cmd.arg(&root).output().expect("binary runs");
        assert_eq!(out.status.code(), Some(0), "scan under {threads} threads");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let strip_wall = |raw: &str| -> String {
        let mut envelope: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|err| panic!("not JSON: {err}"));
        assert!(envelope["data"]["stats"]["wall_ms"].as_u64().is_some());
        envelope["data"]["stats"]["wall_ms"].take();
        serde_json::to_string(&envelope).expect("re-serialize")
    };

    for strict in [true, false] {
        let one = strip_wall(&scan(1, strict));
        let eight = strip_wall(&scan(8, strict));
        assert_eq!(
            one, eight,
            "go report (strict-trivia={strict}) is thread-count-independent"
        );
    }

    // The comparison bites: the KP-6-go hand-computed totals.
    let skip: serde_json::Value = serde_json::from_str(&scan(8, false)).expect("re-parse");
    let strict: serde_json::Value = serde_json::from_str(&scan(8, true)).expect("re-parse");
    assert_eq!(skip["data"]["stats"]["matches"], 4);
    assert_eq!(strict["data"]["stats"]["matches"], 2);
}

/// F9b: the same fence over **typescript and tsx** — one scan whose file
/// set spans both grammars of the crate (`.ts` under typescript,
/// `.tsx` under tsx) must be byte-identical under
/// `RAYON_NUM_THREADS=1` vs `=8`, both trivia modes. Ground truth
/// mirrors the KP-6-typescript measurement: exported-declaration
/// adjacency counts 4 in skip mode and 2 under `--strict-trivia`.
#[test]
#[cfg(feature = "lang-typescript")]
fn typescript_scan_json_is_byte_identical_across_rayon_thread_counts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/typescript")
        .canonicalize()
        .expect("fixture module");
    let rule = "(under program (seq (kind export_statement) (kind export_statement)))";

    let scan = |threads: u32, strict: bool| {
        let bin = env!("CARGO_BIN_EXE_janitor");
        let mut cmd = Command::new(bin);
        cmd.env("RAYON_NUM_THREADS", threads.to_string())
            .args(["scan", "--rule", rule]);
        if strict {
            cmd.arg("--strict-trivia");
        }
        let out = cmd.arg(&root).output().expect("binary runs");
        assert_eq!(out.status.code(), Some(0), "scan under {threads} threads");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let strip_wall = |raw: &str| -> String {
        let mut envelope: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|err| panic!("not JSON: {err}"));
        assert!(envelope["data"]["stats"]["wall_ms"].as_u64().is_some());
        envelope["data"]["stats"]["wall_ms"].take();
        serde_json::to_string(&envelope).expect("re-serialize")
    };

    for strict in [true, false] {
        let one = strip_wall(&scan(1, strict));
        let eight = strip_wall(&scan(8, strict));
        assert_eq!(
            one, eight,
            "ts/tsx report (strict-trivia={strict}) is thread-count-independent"
        );
    }

    // The comparison bites: the KP-6-typescript hand-computed totals,
    // and both grammars really took part in the scan.
    let skip: serde_json::Value = serde_json::from_str(&scan(8, false)).expect("re-parse");
    let strict: serde_json::Value = serde_json::from_str(&scan(8, true)).expect("re-parse");
    assert_eq!(skip["data"]["stats"]["matches"], 4);
    assert_eq!(strict["data"]["stats"]["matches"], 2);
    assert_eq!(
        skip["data"]["stats"]["langs"],
        serde_json::json!({ "typescript": 2, "tsx": 1 })
    );
}
