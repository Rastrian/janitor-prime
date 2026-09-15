//! Go end-to-end acceptance (F9b task 4): the v0.1 definition-of-done
//! loop (`tests/v01_acceptance.rs`) re-run over a **go** fixture module
//! through the real binary — `init → wake (empty) → scan → apply → note
//! → re-anchor → wake (budget) → nap settle → final wake` — plus the two
//! go-specific proofs the brief demands:
//!
//! 1. a multi-file go refactor (rename `NormalizeCode` →
//!    `NormalizeSource`: the def plus its doc-comment mention in
//!    `shapes/scale.go`, the two call sites in `shapes/fold.go`, and the
//!    package-qualified call site in `main.go`) applied **atomically**
//!    in one diff, validated by whole-file re-parse under the *go*
//!    grammar. Note the honest go shape: import lines carry package
//!    *paths*, not symbol names, so a symbol rename's cross-file edits
//!    are def + call sites — unlike python, no import-statement edits
//!    exist to make (a package *move* is what rewrites a go import
//!    line, and that is a path change janitor does not synthesize);
//! 2. a syntax-breaking diff **refused with coordinates** (`E_PARSE_INVALID`
//!    carrying the offending node's byte/point range) with the whole diff
//!    rolled back — the untouched bytes prove it;
//! 3. a pointer note that **re-anchors** after an edit above moves the
//!    function (content-hash match, exact drift).
//!
//! The fixture repo is the bundled corpus `tests/fixtures/go/` (the
//! same module the KP-6-go measurement runs over: package `shapes` with
//! two files plus its `main` consumer), copied to a tempdir so
//! `janitor init` never writes into the repository.

#![cfg(feature = "lang-go")]

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

/// Copy the bundled go fixture module into a fresh tempdir.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go");
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
    b"// typed late in review: this funnel is load-bearing for callers too\n";

const SCAN_RULE: &str =
    "(under function_declaration (seq (capture @fn identifier) parameter_list))";

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
fn go_agent_loop_is_the_definition_of_done() {
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

    // --- scan: every go func, name captured --------------------------------
    let (code, scan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert!(
        matches.len() >= 7,
        "the module carries a real function population: {scan}"
    );
    for m in matches {
        assert_eq!(m["kind"], "function_declaration", "pivot kind is go's");
        assert_eq!(m["captures"][0]["kind"], "identifier");
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

    // The rename target: shapes/scale.go's NormalizeCode def.
    let target = matches
        .iter()
        .find(|m| {
            m["path"] == "shapes/scale.go" && {
                let cap = capture(m, "@fn");
                let (start, end) = (
                    cap["start_byte"].as_u64().expect("start") as usize,
                    cap["end_byte"].as_u64().expect("end") as usize,
                );
                let text = std::fs::read(dir.join("shapes/scale.go")).expect("scale");
                &text[start..end] == b"NormalizeCode"
            }
        })
        .expect("scan finds NormalizeCode's def")
        .clone();
    let cap = capture(&target, "@fn");
    let (start, end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    assert_eq!(end - start, "NormalizeCode".len() as u64);

    // --- apply: the atomic multi-file rename --------------------------------
    // NormalizeCode lives in shapes/scale.go (the def, its doc-comment
    // mention, and the Version.Apply call) and is called in
    // shapes/fold.go (twice) and main.go (once, package-qualified): six
    // edits over three files — one diff, one flush, re-parsed as go.
    const OLD: &str = "NormalizeCode";
    const NEW: &str = "NormalizeSource";
    let mut operations = Vec::new();
    for name in ["shapes/scale.go", "shapes/fold.go", "main.go"] {
        let hits = find_all(&dir.join(name), OLD.as_bytes());
        assert!(
            name != "shapes/fold.go" || hits.len() == 2,
            "fold.go carries exactly the two call sites"
        );
        assert!(!hits.is_empty(), "{name} carries at least one site");
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
        apply["data"]["stats"]["edits"], 6,
        "six rename edits: the def, the doc-comment mention, and four call sites"
    );
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 3);
    for file in files {
        assert_eq!(file["status"], "flushed", "atomic: every file flushed");
        assert_eq!(
            file["errors_after"], 0,
            "the re-parse gate ran under the go grammar"
        );
    }
    let scale = std::fs::read_to_string(dir.join("shapes/scale.go")).expect("scale");
    assert!(
        scale.contains("func NormalizeSource("),
        "disk truth: renamed"
    );
    assert!(!scale.contains(OLD), "old name gone from shapes/scale.go");

    // --- apply refused: syntax-breaking go diff, with coordinates -----------
    // Break shapes/scale.go with an unterminated string literal (drop
    // the closing quote of squashSuffix's "-x"), while main.go's edit in
    // the same diff is perfectly valid: the whole diff must roll back —
    // nothing flushed, nothing mutated.
    let break_at = scale.find("\"-x\"").expect("break target");
    let before_scale = std::fs::read(dir.join("shapes/scale.go")).expect("scale");
    let before_main = std::fs::read(dir.join("main.go")).expect("main");
    let break_payload = format!(
        r#"{{"operations":[
            {{"path":"shapes/scale.go","edits":[{{"start_byte":{break_at},"old_end_byte":{},"new_text":"\"-x","old_text":"\"-x\""}}]}},
            {{"path":"main.go","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"// staged with the refused diff\n"}}]}}
        ]}}"#,
        break_at + 4,
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["ok"], false);
    assert_eq!(
        refused["error"]["code"], "E_PARSE_INVALID",
        "the go syntax gate refuses the broken diff"
    );
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(!nodes.is_empty(), "coordinates ride along: {refused}");
    for node in nodes {
        assert_eq!(node["path"], "shapes/scale.go".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["is_missing"].is_boolean());
    }
    assert_eq!(
        std::fs::read(dir.join("shapes/scale.go")).expect("scale"),
        before_scale,
        "rollback: the broken file is untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("main.go")).expect("main"),
        before_main,
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
            m["path"] == "shapes/scale.go" && {
                let cap = capture(m, "@fn");
                let (s, e) = (
                    cap["start_byte"].as_u64().unwrap_or(u64::MAX) as usize,
                    cap["end_byte"].as_u64().unwrap_or(0) as usize,
                );
                let text = std::fs::read(dir.join("shapes/scale.go")).expect("scale");
                &text[s..e] == b"NormalizeSource"
            }
        })
        .expect("rescan finds NormalizeSource")
        .clone();
    let cap = capture(&renamed, "@fn");
    let (r_start, r_end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    let notation = format!("shapes/scale.go:{r_start}-{r_end}:identifier");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "NormalizeSource (was NormalizeCode) is the one funnel every \
             later phase passes through; span stamped from scan output",
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
    let scale_path = dir.join("shapes/scale.go");
    let old = std::fs::read(&scale_path).expect("scale");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&scale_path, &moved).expect("write moved");

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
            "the go rename flushed clean across three files in one diff",
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
