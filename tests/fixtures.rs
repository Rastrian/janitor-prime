//! Golden fixture corpus (PLAN §3 F4.9, refactree's `op.json` spirit,
//! A.4): every `tests/fixtures/*.json` pairs a rule with a source file and
//! the expected match set — count, exact span texts, or capture texts —
//! and this loader asserts each one through the real `Session::scan`
//! single-file path. The corpus is the DSL's anti-regression fence: a
//! semantics change that alters any match set fails here by name.

use std::path::Path;

use janitor_prime::orchestrator::Session;
use janitor_prime::scan::{DEFAULT_MAX_FILE_SIZE, ScanParams};
use serde_json::Value;

/// One fixture outcome, reported by name on failure.
fn run_fixture(path: &Path) {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("<unnamed>");
    let raw = std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{name}: {err}"));
    let fixture: Value =
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("{name}: invalid JSON: {err}"));

    let rule = fixture["rule"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: rule"));
    let file = fixture["file"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: file"));
    let trivia_strict = fixture["trivia"].as_str() == Some("strict");
    let source_path = path.with_file_name(file);

    let mut params = ScanParams::new(rule, &source_path);
    params.strict_trivia = trivia_strict;
    params.max_file_size = DEFAULT_MAX_FILE_SIZE;

    let session = Session::new();
    let report = session
        .scan(&params)
        .unwrap_or_else(|err| panic!("{name}: scan failed: {err}"));
    assert_eq!(report.trivia, fixture["trivia"].as_str().unwrap_or("skip"));

    let source = std::fs::read(&source_path).unwrap_or_else(|err| panic!("{name}: {err}"));

    // Exact span texts, when given (byte-strict ground truth).
    if let Some(texts) = fixture["texts"].as_array() {
        let got: Vec<String> = report
            .matches
            .iter()
            .map(|m| String::from_utf8_lossy(&source[m.start_byte..m.end_byte]).into_owned())
            .collect();
        let want: Vec<String> = texts
            .iter()
            .map(|t| t.as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(got, want, "{name}: exact span texts");
        assert_eq!(report.matches.len(), texts.len(), "{name}: count");
    }

    // Match count, when given.
    if let Some(count) = fixture["count"].as_u64() {
        assert_eq!(report.matches.len() as u64, count, "{name}: match count");
    }

    // Capture texts in order, when given (capture priority is exercised by
    // ordering: first binding wins).
    if let Some(caps) = fixture["captures"].as_array() {
        assert_eq!(
            report.matches.len(),
            caps.len(),
            "{name}: one capture expectation per match"
        );
        for (match_idx, want) in caps.iter().enumerate() {
            let want = want.as_str().unwrap_or_default();
            let captures = &report.matches[match_idx].captures;
            assert_eq!(captures.len(), 1, "{name}: match {match_idx}");
            let cap = &captures[0];
            assert!(cap.symbol.starts_with('@'), "{name}: symbols render with @");
            let got = String::from_utf8_lossy(&source[cap.start_byte..cap.end_byte]).into_owned();
            assert_eq!(got, want, "{name}: match {match_idx} capture");
        }
    }
}

#[test]
fn golden_fixture_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut fixtures: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("fixtures dir: {err}"))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    fixtures.sort();
    assert!(
        fixtures.len() >= 10,
        "the corpus must carry its ground truth (found {})",
        fixtures.len()
    );
    for fixture in &fixtures {
        run_fixture(fixture);
    }
}
