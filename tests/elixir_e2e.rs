//! Elixir end-to-end acceptance (F9d task 4, experimental tier): the
//! ruby template reduced per the brief — `init → scan → apply → refused
//! diff → note → re-anchor`, through the real binary, over the bundled
//! elixir fixture repo (`tests/fixtures/elixir/`, copied to a tempdir so
//! `janitor init` never writes into the repository):
//!
//! 1. `scan` finds a real structural population (every do-block def
//!    clause, def/defp keyword captured — elixir def clauses are `call`
//!    nodes, so the rule is shape-based: identifier target + a head
//!    `call` in `arguments` + a `do_block` child);
//! 2. a multi-file elixir refactor (rename `normalize_value` →
//!    `normalize_units`: the @spec mention, both clause heads and the
//!    internal call in `math.ex`, plus both call sites in `report.ex`)
//!    applied **atomically** in one diff, validated by whole-file
//!    re-parse under the *elixir* grammar;
//! 3. a syntax-breaking diff **refused with coordinates**
//!    (`E_PARSE_INVALID` carrying the offending node's byte/point
//!    range) with the whole diff rolled back — the untouched bytes
//!    prove it;
//! 4. a pointer note that **re-anchors** after an edit above moves the
//!    def (content-hash match, exact drift). The anchor is `totals` —
//!    a name unique in the corpus, because the renamed funnel name now
//!    has four content-identical occurrences and the content-hash
//!    re-anchor is deliberately first-nearest (measured: drift 28, the
//!    @spec occurrence, not the head).
//!
//! EXPERIMENTAL caveat this acceptance cannot fence (recorded in
//! docs/SCHEMA.md and the norm table header): FUNCTION = `call` means
//! every invocation is also a FUNCTION match; a def discriminator is
//! post-v0.2 pattern work.

#![cfg(feature = "lang-elixir")]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_janitor")
}

/// One `janitor` invocation in `dir` (env-isolated so this machine's XDG
/// config cannot leak into the global-layer assertions). Every payload on
/// stdout is a `janitor.v1` envelope — asserted once, for every command.
fn janitor(dir: &Path, args: &[&str]) -> (Option<i32>, Value) {
    let out = Command::new(bin())
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg-absent"))
        .args(args)
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let envelope: Value = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    assert_eq!(
        envelope["schema"], "janitor.v1",
        "every stdout payload is a versioned envelope: {stdout}"
    );
    (out.status.code(), envelope)
}

/// Copy the bundled elixir fixture repo into a fresh tempdir.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elixir");
    copy_tree(&src, tmp.path()).expect("fixture repo copies");
    tmp
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        if from.is_dir() {
            copy_tree(&from, &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(&from, dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Byte offsets of every occurrence of `needle` in `file` (ascending).
fn find_all(file: &Path, needle: &[u8]) -> Vec<usize> {
    let bytes = std::fs::read(file).expect("read file");
    bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(at, _)| at)
        .collect()
}

/// The bytes at a capture's span, stamped from live scan output.
fn capture_bytes(dir: &Path, path: &str, cap: &Value) -> Vec<u8> {
    let start = cap["start_byte"].as_u64().expect("start") as usize;
    let end = cap["end_byte"].as_u64().expect("end") as usize;
    std::fs::read(dir.join(path)).expect("read file")[start..end].to_vec()
}

/// The capture report row for `symbol`, from one match.
fn capture(match_value: &Value, symbol: &str) -> Value {
    match_value["captures"]
        .as_array()
        .expect("captures")
        .iter()
        .find(|c| c["symbol"] == symbol)
        .cloned()
        .unwrap_or(Value::Null)
}

/// Every do-block def clause, def/defp keyword captured: a `call` whose
/// children are [identifier (the `def`/`defp` keyword), arguments
/// carrying the head `call`, do_block]. Shape-measured: `defmodule`
/// heads take an `alias` in arguments (not a call) and keyword-form
/// clauses (`do:`) carry no do_block — both correctly stay out.
const SCAN_RULE: &str =
    "(under call (seq (capture @def identifier) (under arguments call) do_block))";

/// Every call head's `arguments` span (starts at the name identifier —
/// measured): the post-rename scan that stamps the note anchor.
const HEAD_RULE: &str = "(under call (seq (capture @head arguments)))";

#[test]
fn elixir_agent_loop_scan_apply_refuse_note_reanchor() {
    let tmp = fixture_repo();
    let dir = tmp.path();

    // --- init -------------------------------------------------------------
    let (code, init) = janitor(dir, &["init"]);
    assert_eq!(code, Some(0), "{init}");
    assert_eq!(init["ok"], true);
    assert!(dir.join(".janitor/LOG.txt").exists());

    // --- scan: every do-block def clause, keyword captured -----------------
    // Hand-counted from the fixture trees: catalog.ex 3 (tagged_count,
    // label, render), math.ex 2 (scale, totals — the guarded
    // normalize_value/2 head is a binary_operator in arguments, so the
    // head-call shape honestly excludes it), report.ex 2 (show, summary).
    let (code, scan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert_eq!(
        matches.len(),
        7,
        "the corpus carries a real def-clause population: {scan}"
    );
    for m in matches {
        assert_eq!(m["kind"], "call", "pivot kind is elixir's def-clause call");
        let cap = capture(m, "@def");
        assert_eq!(cap["kind"], "identifier");
        let text = String::from_utf8_lossy(&capture_bytes(dir, m["path"].as_str().unwrap(), &cap))
            .to_string();
        assert!(
            text == "def" || text == "defp",
            "the captured keyword is def or defp, measured: {text}"
        );
    }
    // matches are file-grouped, paths sorted (canonical order).
    let mut paths: Vec<&str> = matches
        .iter()
        .map(|m| m["path"].as_str().expect("path"))
        .collect();
    paths.dedup();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);

    // --- apply: the atomic two-file rename ----------------------------------
    // normalize_value lives in math.ex (the @spec mention, two clause
    // heads, scale's internal call) and report.ex (show's and summary's
    // calls): six edits over two files — one diff, one flush, re-parsed
    // as elixir. The @spec mention rides along: a rename that left the
    // spec pointing at the dead name would be dishonest even though the
    // syntax gate cannot see it.
    const OLD: &str = "normalize_value";
    const NEW: &str = "normalize_units";
    let mut operations = Vec::new();
    for (name, expect) in [("math.ex", 4), ("report.ex", 2)] {
        let hits = find_all(&dir.join(name), OLD.as_bytes());
        assert_eq!(
            hits.len(),
            expect,
            "{name} carries the spec+heads+internal call (math) or the two calls (report)"
        );
        let edits: Vec<String> = hits
            .iter()
            .map(|&at| {
                format!(
                    r#"{{"start_byte":{at},"old_end_byte":{},"new_text":"{NEW}","old_text":"{OLD}"}}"#,
                    at + OLD.len()
                )
            })
            .collect();
        operations.push(format!(
            r#"{{"path":"{name}","edits":[{}]}}"#,
            edits.join(",")
        ));
    }
    let payload = format!("{{\"operations\":[{}]}}", operations.join(","));
    let (code, apply) = janitor(dir, &["apply", "--diff", &payload]);
    assert_eq!(code, Some(0), "{apply}");
    assert_eq!(apply["ok"], true);
    assert_eq!(apply["data"]["stats"]["files"], 2, "one diff, two files");
    assert_eq!(
        apply["data"]["stats"]["edits"], 6,
        "six rename edits: spec, two heads, one internal call, two controller calls"
    );
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    for file in files {
        assert_eq!(file["status"], "flushed", "atomic: every file flushed");
        assert_eq!(
            file["errors_after"], 0,
            "the re-parse gate ran under the elixir grammar"
        );
    }
    let math = std::fs::read_to_string(dir.join("math.ex")).expect("math.ex");
    assert!(
        math.contains("@spec normalize_units(number())"),
        "disk truth: the spec mentions the new name"
    );
    assert!(!math.contains(OLD), "old name gone from math.ex");
    let report = std::fs::read_to_string(dir.join("report.ex")).expect("report.ex");
    assert!(!report.contains(OLD), "old name gone from report.ex");

    // --- apply refused: syntax-breaking elixir diff, with coordinates -------
    // Break math.ex by dropping the closing quote of the @doc string
    // (elixir strings run across newlines, so the damage swallows the
    // rest of the module into one ERROR node — measured), while
    // report.ex's edit in the same diff is perfectly valid: the whole
    // diff must roll back — nothing flushed, nothing mutated.
    let math_path = dir.join("math.ex");
    let math_bytes = std::fs::read(&math_path).expect("math.ex");
    let string_open = math_bytes
        .windows(b"\"Normalizes one value".len())
        .position(|w| w == b"\"Normalizes one value")
        .expect("the @doc string opens");
    let break_at = math_bytes[string_open + 1..]
        .iter()
        .position(|&b| b == b'"')
        .map(|at| string_open + 1 + at)
        .expect("the @doc string closes");
    let before_math = std::fs::read(&math_path).expect("math.ex");
    let before_report = std::fs::read(dir.join("report.ex")).expect("report.ex");
    // r##-delimited: the payload text itself contains `"#`, which would
    // close an r#"…"# string (the ruby e2e hit the same thing).
    let break_payload = format!(
        r##"{{"operations":[
            {{"path":"math.ex","edits":[{{"start_byte":{break_at},"old_end_byte":{},"new_text":"","old_text":"\""}}]}},
            {{"path":"report.ex","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"# staged with the refused diff\n"}}]}}
        ]}}"##,
        break_at + 1,
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["ok"], false);
    assert_eq!(
        refused["error"]["code"], "E_PARSE_INVALID",
        "the elixir syntax gate refuses the broken diff"
    );
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(!nodes.is_empty(), "coordinates ride along: {refused}");
    for node in nodes {
        assert_eq!(node["path"], "math.ex".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["is_missing"].is_boolean());
    }
    assert_eq!(
        std::fs::read(&math_path).expect("math.ex"),
        before_math,
        "rollback: the broken file is untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("report.ex")).expect("report.ex"),
        before_report,
        "rollback: the OTHER file's valid edit is not flushed either"
    );

    // --- note: anchor to a def's name, stamped from a fresh scan -----------
    // totals is the deliberately unique anchor: the renamed funnel name
    // now has four content-identical occurrences and the content-hash
    // re-anchor is first-nearest — a collision would make drift
    // assertions measure the wrong occurrence.
    let (code, head_scan) = janitor(dir, &["scan", "--rule", HEAD_RULE]);
    assert_eq!(code, Some(0), "{head_scan}");
    let anchored_head = head_scan["data"]["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .find(|m| {
            m["path"] == "math.ex" && {
                let cap = capture(m, "@head");
                capture_bytes(dir, "math.ex", &cap).starts_with(b"totals(")
            }
        })
        .expect("head scan finds totals' head")
        .clone();
    let cap = capture(&anchored_head, "@head");
    let t_start = cap["start_byte"].as_u64().expect("start");
    // The head span starts at the name identifier (measured); the name is
    // its first "totals".len() bytes.
    let t_end = t_start + "totals".len() as u64;
    let notation = format!("math.ex:{t_start}-{t_end}:identifier");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "totals folds the scale results; span stamped from scan output",
        ],
    );
    assert_eq!(code, Some(0), "{anchored}");
    assert_eq!(anchored["data"]["id"], 0);
    assert_eq!(
        anchored["data"]["ptr"]["span"],
        serde_json::json!([t_start, t_end])
    );
    assert_eq!(anchored["data"]["ptr"]["kind_name"], "identifier");

    // --- edit above the anchor (outside janitor, the way a teammate would) --
    let math = std::fs::read_to_string(&math_path).expect("math.ex");
    let def_at = math
        .find("def totals(")
        .map(|at| math[..at].rfind('\n').map(|nl| nl + 1).unwrap_or(0))
        .expect("totals' def line");
    let inserted = "# measured after the rename: this funnel feeds every report\n";
    let mut moved = String::with_capacity(math.len() + inserted.len());
    moved.push_str(&math[..def_at]);
    moved.push_str(inserted);
    moved.push_str(&math[def_at..]);
    std::fs::write(&math_path, &moved).expect("write moved");

    // --- note again: the stored pointer re-anchors with exact drift ---------
    let stored = anchored["data"]["ptr"].to_string();
    let (code, reanchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &stored,
            "the anchor followed the comment inserted above totals' def; \
             re-anchored by content hash, drift equals the inserted bytes",
        ],
    );
    assert_eq!(code, Some(0), "{reanchored}");
    assert_eq!(reanchored["data"]["id"], 1);
    assert_eq!(reanchored["data"]["reanchored"], true);
    assert_eq!(
        reanchored["data"]["drift"].as_u64(),
        Some(inserted.len() as u64)
    );
    assert_eq!(
        reanchored["data"]["ptr"]["span"],
        serde_json::json!([
            t_start + inserted.len() as u64,
            t_end + inserted.len() as u64
        ])
    );
}
