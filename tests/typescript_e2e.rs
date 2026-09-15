//! TypeScript end-to-end acceptance (F9b task 4): the v0.1
//! definition-of-done loop (`tests/v01_acceptance.rs`) re-run over a
//! **typescript** fixture module through the real binary — `init → wake
//! (empty) → scan → apply → note → re-anchor → wake (budget) → nap
//! settle → final wake` — plus the typescript-specific proofs the brief
//! demands:
//!
//! 1. a multi-file typescript refactor (rename `normalizeCode` →
//!    `normalizeSource`: the def and its two template call sites in
//!    `src/format.ts`, the *import statement* and call site in
//!    `src/report.ts`, and the import statement and call site in
//!    `src/widget.tsx`) applied **atomically** in one diff, validated by
//!    whole-file re-parse — under the **typescript** grammar for the
//!    `.ts` files and the **tsx** grammar for the `.tsx` file, the
//!    multi-grammar lane exercised end to end (named imports carry the
//!    symbol, so this lane's rename really does include import-line
//!    edits, unlike go's);
//! 2. a syntax-breaking diff **refused with coordinates** (`E_PARSE_INVALID`
//!    carrying the offending node's byte/point range) with the whole diff
//!    rolled back — the untouched bytes prove it;
//! 3. a pointer note that **re-anchors** after an edit above moves the
//!    function (content-hash match, exact drift).
//!
//! The fixture repo is the bundled module `tests/fixtures/typescript/`
//! (the same corpus the KP-6-typescript measurement runs over), copied
//! to a tempdir so `janitor init` never writes into the repository.

#![cfg(feature = "lang-typescript")]

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

/// Copy the bundled typescript fixture module into a fresh tempdir.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typescript");
    copy_tree(&src, tmp.path()).expect("fixture module copies");
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

/// The budget the budgeted wake runs under (v0.1 loop shape: the raw
/// three-fact log must exceed it, the settled layout must fit inside).
const BUDGET: u64 = 48;

/// Bytes inserted above the anchored function between the two notes; the
/// re-anchor drift must equal exactly this.
const INSERTED_ABOVE: &[u8] =
    b"// typed late in review: this funnel is load-bearing for importers too\n";

const SCAN_RULE: &str =
    "(under function_declaration (seq (capture @fn identifier) formal_parameters))";

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

#[test]
fn typescript_agent_loop_is_the_definition_of_done() {
    let tmp = fixture_repo();
    let dir = tmp.path();

    // --- init -------------------------------------------------------------
    let (code, init) = janitor(dir, &["init"]);
    assert_eq!(code, Some(0), "{init}");
    assert_eq!(init["ok"], true);
    assert!(dir.join(".janitor/LOG.txt").exists());

    // --- wake (empty) ------------------------------------------------------
    let (code, empty) = janitor(dir, &["wake"]);
    assert_eq!(code, Some(0), "{empty}");
    assert_eq!(empty["data"]["awake"], true);
    assert_eq!(empty["data"]["items"].as_array().map(Vec::len), Some(0));

    // --- scan: every function, name captured, BOTH grammars -----------------
    // The rule resolves in typescript and tsx alike; src/widget.tsx is
    // compiled under the tsx grammar in the same scan.
    let (code, scan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert!(
        matches.len() >= 6,
        "the module carries a real function population: {scan}"
    );
    for m in matches {
        assert_eq!(m["kind"], "function_declaration", "pivot kind is ts's");
        assert_eq!(m["captures"][0]["kind"], "identifier");
    }
    // matches are file-grouped, paths sorted (canonical order).
    let mut paths: Vec<&str> = matches
        .iter()
        .map(|m| m["path"].as_str().expect("path"))
        .collect();
    paths.dedup();
    assert_eq!(
        paths,
        vec!["src/format.ts", "src/report.ts", "src/widget.tsx"],
        "all three files — .tsx included — scanned in one run"
    );
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);

    // The rename target: src/format.ts's normalizeCode def.
    let target = matches
        .iter()
        .find(|m| {
            m["path"] == "src/format.ts" && {
                let cap = capture(m, "@fn");
                let (start, end) = (
                    cap["start_byte"].as_u64().expect("start") as usize,
                    cap["end_byte"].as_u64().expect("end") as usize,
                );
                let text = std::fs::read(dir.join("src/format.ts")).expect("format");
                &text[start..end] == b"normalizeCode"
            }
        })
        .expect("scan finds normalizeCode's def")
        .clone();
    let cap = capture(&target, "@fn");
    let (start, end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    assert_eq!(end - start, "normalizeCode".len() as u64);

    // --- apply: the atomic multi-file rename --------------------------------
    // normalizeCode lives in src/format.ts (the def plus two template
    // call sites) and is named by an import statement plus a call in each
    // of src/report.ts and src/widget.tsx: seven edits over three files —
    // one diff, one flush, re-parsed as typescript (.ts) and tsx (.tsx).
    const OLD: &str = "normalizeCode";
    const NEW: &str = "normalizeSource";
    let mut operations = Vec::new();
    for name in ["src/format.ts", "src/report.ts", "src/widget.tsx"] {
        let hits = find_all(&dir.join(name), OLD.as_bytes());
        let expected = if name == "src/format.ts" { 3 } else { 2 };
        assert_eq!(
            hits.len(),
            expected,
            "{name}: the def plus two template call sites, or the import \
             line and one call site"
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
    assert_eq!(apply["data"]["stats"]["files"], 3, "one diff, three files");
    assert_eq!(
        apply["data"]["stats"]["edits"], 7,
        "seven rename edits: the def, two template call sites, two imports, two calls"
    );
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 3);
    for file in files {
        assert_eq!(file["status"], "flushed", "atomic: every file flushed");
        assert_eq!(
            file["errors_after"], 0,
            "the re-parse gate ran under the ts/tsx grammars"
        );
    }
    let report_src = std::fs::read_to_string(dir.join("src/report.ts")).expect("report");
    assert!(
        report_src.contains("import { normalizeSource } from \"./format.js\";"),
        "disk truth: the import line itself was updated"
    );
    assert!(!report_src.contains(OLD), "old name gone from report.ts");
    let widget = std::fs::read_to_string(dir.join("src/widget.tsx")).expect("widget");
    assert!(
        widget.contains("const canonical = normalizeSource(raw);"),
        "disk truth: the tsx call site was updated"
    );

    // --- apply refused: syntax-breaking ts diff, with coordinates ------------
    // Break src/format.ts by deleting the closing paren of the def's
    // `code.trim()` (an unbalanced call), while src/report.ts's edit in
    // the same diff is perfectly valid: the whole diff must roll back —
    // nothing flushed, nothing mutated.
    let format_src = std::fs::read_to_string(dir.join("src/format.ts")).expect("format");
    let trim_at = format_src.find("code.trim()").expect("break target");
    let close = trim_at + "code.trim()".len() - 1;
    let before_format = std::fs::read(dir.join("src/format.ts")).expect("format");
    let before_report = std::fs::read(dir.join("src/report.ts")).expect("report");
    let break_payload = format!(
        r#"{{"operations":[
            {{"path":"src/format.ts","edits":[{{"start_byte":{close},"old_end_byte":{},"new_text":"","old_text":")"}}]}},
            {{"path":"src/report.ts","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"// staged with the refused diff\n"}}]}}
        ]}}"#,
        close + 1,
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["ok"], false);
    assert_eq!(
        refused["error"]["code"], "E_PARSE_INVALID",
        "the typescript syntax gate refuses the broken diff"
    );
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(!nodes.is_empty(), "coordinates ride along: {refused}");
    for node in nodes {
        assert_eq!(node["path"], "src/format.ts".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["is_missing"].is_boolean());
    }
    assert_eq!(
        std::fs::read(dir.join("src/format.ts")).expect("format"),
        before_format,
        "rollback: the broken file is untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("src/report.ts")).expect("report"),
        before_report,
        "rollback: the OTHER file's valid edit is not flushed either"
    );

    // --- note: anchor to the renamed function's name ------------------------
    // Re-scan post-rename so the span is stamped from live output, never
    // hand-counted (the anti-hallucination contract).
    let (code, rescan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{rescan}");
    let renamed = rescan["data"]["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .find(|m| {
            m["path"] == "src/format.ts" && {
                let cap = capture(m, "@fn");
                let (s, e) = (
                    cap["start_byte"].as_u64().unwrap_or(u64::MAX) as usize,
                    cap["end_byte"].as_u64().unwrap_or(0) as usize,
                );
                let text = std::fs::read(dir.join("src/format.ts")).expect("format");
                &text[s..e] == b"normalizeSource"
            }
        })
        .expect("rescan finds normalizeSource")
        .clone();
    let cap = capture(&renamed, "@fn");
    let (r_start, r_end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    let notation = format!("src/format.ts:{r_start}-{r_end}:identifier");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "normalizeSource (was normalizeCode) is the one funnel every \
             importer passes through; span stamped from scan output",
        ],
    );
    assert_eq!(code, Some(0), "{anchored}");
    assert_eq!(anchored["data"]["id"], 0);
    assert_eq!(
        anchored["data"]["ptr"]["span"],
        serde_json::json!([r_start, r_end])
    );
    assert_eq!(anchored["data"]["ptr"]["kind_name"], "identifier");

    // --- edit above the anchor (outside janitor, the way a teammate would) --
    let format_path = dir.join("src/format.ts");
    let old = std::fs::read(&format_path).expect("format");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&format_path, &moved).expect("write moved");

    // --- note again: the stored pointer re-anchors with exact drift ---------
    let stored = anchored["data"]["ptr"].to_string();
    let (code, reanchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &stored,
            "the anchor followed the comment inserted above it; re-anchored \
             by content hash, drift equals the inserted bytes",
        ],
    );
    assert_eq!(code, Some(0), "{reanchored}");
    assert_eq!(reanchored["data"]["id"], 1);
    assert_eq!(reanchored["data"]["reanchored"], true);
    assert_eq!(
        reanchored["data"]["drift"].as_u64(),
        Some(INSERTED_ABOVE.len() as u64)
    );
    assert_eq!(
        reanchored["data"]["ptr"]["span"],
        serde_json::json!([
            r_start + INSERTED_ABOVE.len() as u64,
            r_end + INSERTED_ABOVE.len() as u64
        ])
    );

    // The agent's own record — the third fact that makes the oldest pair
    // compressible (v0.1 loop shape).
    let (code, record) = janitor(
        dir,
        &[
            "note",
            "the typescript rename flushed clean across three files (two \
             grammars) in one diff",
        ],
    );
    assert_eq!(code, Some(0), "{record}");
    assert_eq!(record["data"]["id"], 2);

    // --- wake with budget: the completeness gate refuses --------------------
    let (code, gate) = janitor(dir, &["wake", "--budget", &BUDGET.to_string()]);
    assert_eq!(code, Some(1), "the gate is an operational refusal: {gate}");
    assert_eq!(gate["error"]["code"], "E_NAP_PENDING");

    // --- nap loop settle ----------------------------------------------------
    let mut guard = 0;
    let settled = loop {
        guard += 1;
        assert!(guard < 32, "the nap loop must converge");
        let (_, plan) = janitor(dir, &["nap"]);
        if plan["data"]["pending"] == false {
            break janitor(dir, &["wake", "--budget", &BUDGET.to_string()]);
        }
        let slot = plan["data"]["request"]["slot"]
            .as_str()
            .expect("actionable slot")
            .to_owned();
        let text = format!("{slot}: the early anchored facts, compressed");
        let (code, done) = janitor(dir, &["nap", "--slot", &slot, "--text", &text]);
        assert_eq!(code, Some(0), "{done}");
        let (code, woke) = janitor(dir, &["wake", "--budget", &BUDGET.to_string()]);
        if code == Some(0) {
            break (Some(0), woke);
        }
    };

    // --- final wake fits ----------------------------------------------------
    assert_eq!(settled.0, Some(0), "wake answers after settling");
    let final_wake = settled.1;
    assert_eq!(final_wake["ok"], true);
    assert_eq!(final_wake["data"]["awake"], true);
    assert!(
        final_wake["data"]["estimate"].as_u64().expect("estimate") <= BUDGET,
        "the payload fits the budget: {final_wake}"
    );
    assert_eq!(final_wake["data"]["truncated"], false);
    let items = final_wake["data"]["items"].as_array().expect("items");
    assert!(
        items
            .iter()
            .any(|item| item["kind"] == "fact" && item["id"] == 2),
        "the newest fact stays raw"
    );

    // Nothing left to compress: the protocol is closed.
    let (_, done) = janitor(dir, &["nap"]);
    assert_eq!(done["data"]["pending"], false);
}
