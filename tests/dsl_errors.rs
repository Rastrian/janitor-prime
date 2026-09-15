//! DSL error quality through the real binary (PLAN §3 F4.8, ADR-0003):
//! every typed rule-authoring error surfaces as a janitor.v1 envelope with
//! its stable code, byte position, expected set, and — where a near-miss
//! exists — a did-you-mean suggestion. These are the product surface an
//! agent retries from (docs/DSL.md §7 documents the contract this asserts).

use std::process::Command;

/// Run `janitor scan` with `rule` over an empty dir; return
/// (exit code, envelope JSON).
fn scan_rule(rule: &str) -> (Option<i32>, serde_json::Value) {
    let bin = env!("CARGO_BIN_EXE_janitor");
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(bin)
        .args(["scan", "--rule", rule])
        .arg(tmp.path())
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (
        out.status.code(),
        serde_json::from_str(&stdout)
            .unwrap_or_else(|err| panic!("stdout not JSON ({err}): {stdout}")),
    )
}

fn error_body(envelope: &serde_json::Value) -> &serde_json::Value {
    envelope
        .get("error")
        .unwrap_or_else(|| panic!("no error body: {envelope}"))
}

#[test]
fn lex_error_carries_position_and_expected_set() {
    let (code, envelope) = scan_rule("(under block (seq a #))");
    assert_eq!(code, Some(1), "operational error exit");
    assert_eq!(envelope["schema"], "janitor.v1");
    assert_eq!(envelope["ok"], false);
    let body = error_body(&envelope);
    assert_eq!(body["code"], "E_DSL_LEX");
    assert_eq!(body["details"]["position"], 20, "byte offset of the '#'");
    let expected = body["details"]["expected"].as_array().cloned();
    assert!(
        expected.is_some_and(|set| set.iter().any(|t| t == "identifier")),
        "the expected-token set names what was wanted: {body}"
    );
}

#[test]
fn parse_error_suggests_the_operator() {
    let (code, envelope) = scan_rule("(under block (sq a b))");
    assert_eq!(code, Some(1));
    let body = error_body(&envelope);
    assert_eq!(body["code"], "E_DSL_PARSE");
    assert_eq!(body["details"]["position"], 14, "points at the head");
    assert_eq!(
        body["details"]["suggestion"], "did you mean: (seq …)?",
        "the agent-retry surface"
    );
}

#[test]
fn two_body_atoms_suggest_seq_wrapping() {
    let (_, envelope) = scan_rule("(under block (kind a) (kind b))");
    let body = error_body(&envelope);
    assert_eq!(body["code"], "E_DSL_PARSE");
    assert!(
        body["details"]["suggestion"]
            .as_str()
            .is_some_and(|s| s.contains("wrap multiple atoms in (seq")),
        "got {body}"
    );
}

#[test]
fn unknown_kind_is_loud_with_a_nearest_kind_suggestion() {
    let (_, envelope) = scan_rule("(under function_itm _)");
    let body = error_body(&envelope);
    assert_eq!(body["code"], "E_DSL_UNKNOWN_KIND");
    assert_eq!(body["details"]["kind"], "function_itm");
    // `lang` names the whole enabled set (the registry-wide gate's scope),
    // so it is "rust" under the default features and the full grammar list
    // under --all-features — what must hold either way is that rust, the
    // grammar owning the suggestion, is named.
    assert!(
        body["details"]["lang"]
            .as_str()
            .is_some_and(|langs| langs.split(", ").any(|l| l == "rust")),
        "got {body}"
    );
    assert_eq!(
        body["details"]["suggestion"], "function_item",
        "nearest known kind within edit distance 2"
    );
}

#[test]
fn duplicate_capture_symbol_is_a_compile_error() {
    let (_, envelope) = scan_rule("(under block (seq (capture @x _) (capture @x _)))");
    let body = error_body(&envelope);
    assert_eq!(body["code"], "E_DSL_COMPILE");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("@x") && m.contains("twice")),
        "the message names the symbol: {body}"
    );
}

#[test]
fn unify_with_an_uncaptured_symbol_is_a_compile_error() {
    // F8 lifted the E_UNSUPPORTED_VERSION refusal: unify now executes. The
    // loud failure that replaces it is the constraint naming a symbol the
    // rule never captures — a filter that could never pass would be a
    // silent no-op (docs/DSL.md §7).
    let (_, envelope) = scan_rule(
        "(under function_item (seq (capture @fn identifier) block) \
         (constraints (unify @fn @ghost)))",
    );
    let body = error_body(&envelope);
    assert_eq!(body["code"], "E_DSL_COMPILE");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("@ghost")),
        "the message names the uncaptured symbol: {body}"
    );
}

#[test]
fn unknown_lang_flag_is_typed_not_silent() {
    let bin = env!("CARGO_BIN_EXE_janitor");
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(bin)
        .args(["scan", "--rule", "_", "--lang", "ruby"])
        .arg(tmp.path())
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(1));
    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("stdout is JSON");
    assert_eq!(envelope["error"]["code"], "E_PARSE_NO_GRAMMAR_FOR_LANG");
}

#[test]
fn missing_rule_flag_is_a_usage_error_envelope() {
    let bin = env!("CARGO_BIN_EXE_janitor");
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(bin)
        .arg("scan")
        .arg(tmp.path())
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(2), "usage exit");
    let envelope: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("usage mistakes stay machine-parseable");
    assert_eq!(envelope["error"]["code"], "E_USAGE");
}

#[test]
fn a_good_rule_reports_success_envelope_over_a_real_file() {
    let bin = env!("CARGO_BIN_EXE_janitor");
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("ok.rs"),
        "fn f() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n",
    )
    .expect("write");
    let out = Command::new(bin)
        .args([
            "scan",
            "--rule",
            "(under block (seq (kind let_declaration) (kind let_declaration)))",
        ])
        .arg(tmp.path())
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(0));
    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("stdout is JSON");
    assert_eq!(envelope["schema"], "janitor.v1");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["stats"]["matches"], 1);
    assert_eq!(envelope["data"]["matches"][0]["kind"], "block");
    assert_eq!(envelope["data"]["matches"][0]["path"], "ok.rs");
    // The report echoes the run parameters (docs/DSL.md §8).
    assert_eq!(envelope["data"]["trivia"], "skip");
}
