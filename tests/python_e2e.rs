//! Python end-to-end acceptance (F9a task 4): the v0.1 definition-of-done
//! loop (`tests/v01_acceptance.rs`) re-run over a **python** fixture repo
//! through the real binary — `init → wake (empty) → scan → apply → note →
//! re-anchor → wake (budget) → nap settle → final wake` — plus the two
//! python-specific proofs the brief demands:
//!
//! 1. a multi-file python refactor (rename `normalize_code` →
//!    `normalize_source`: the def in `utils.py` plus its call sites in
//!    `models.py` and `service.py`) applied **atomically** in one diff,
//!    validated by whole-file re-parse under the *python* grammar;
//! 2. a syntax-breaking diff **refused with coordinates** (`E_PARSE_INVALID`
//!    carrying the offending node's byte/point range) with the whole diff
//!    rolled back — the untouched bytes prove it;
//! 3. a pointer note that **re-anchors** after an edit above moves the
//!    function (content-hash match, exact drift).
//!
//! The fixture repo is the bundled corpus `tests/fixtures/python/` (the
//! same six files the KP-6-python measurement runs over), copied to a
//! tempdir so `janitor init` never writes into the repository.

#![cfg(feature = "lang-python")]

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

/// Copy the bundled python fixture repo into a fresh tempdir.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python");
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

/// The budget the budgeted wake runs under (v0.1 loop shape: the raw
/// three-fact log must exceed it, the settled layout must fit inside).
const BUDGET: u64 = 48;

/// Bytes inserted above the anchored function between the two notes; the
/// re-anchor drift must equal exactly this.
const INSERTED_ABOVE: &[u8] =
    b"# typed late in review: this helper is load-bearing for imports too\n";

const SCAN_RULE: &str = "(under function_definition (seq (capture @fn identifier) parameters))";

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
fn python_agent_loop_is_the_definition_of_done() {
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

    // --- scan: every python def, name captured ------------------------------
    let (code, scan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert!(
        matches.len() >= 20,
        "the corpus carries a real function population: {scan}"
    );
    for m in matches {
        assert_eq!(m["kind"], "function_definition", "pivot kind is python's");
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

    // The rename target: utils.py's normalize_code def.
    let target = matches
        .iter()
        .find(|m| {
            m["path"] == "utils.py" && {
                let cap = capture(m, "@fn");
                let (start, end) = (
                    cap["start_byte"].as_u64().expect("start") as usize,
                    cap["end_byte"].as_u64().expect("end") as usize,
                );
                let text = std::fs::read(dir.join("utils.py")).expect("utils");
                &text[start..end] == b"normalize_code"
            }
        })
        .expect("scan finds normalize_code's def")
        .clone();
    let cap = capture(&target, "@fn");
    let (start, end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    assert_eq!(end - start, "normalize_code".len() as u64);

    // --- apply: the atomic multi-file rename --------------------------------
    // normalize_code lives in utils.py (the def) and is named by imports
    // and calls in models.py and service.py: six edits over three files —
    // the def, two imports, three call sites — one diff, one flush,
    // re-parsed as python.
    const OLD: &str = "normalize_code";
    const NEW: &str = "normalize_source";
    let mut operations = Vec::new();
    for name in ["utils.py", "models.py", "service.py"] {
        let hits = find_all(&dir.join(name), OLD.as_bytes());
        assert!(
            name != "utils.py" || hits.len() == 1,
            "utils.py carries exactly the def"
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
        "six rename edits: the def, two imports, three call sites"
    );
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 3);
    for file in files {
        assert_eq!(file["status"], "flushed", "atomic: every file flushed");
        assert_eq!(
            file["errors_after"], 0,
            "the re-parse gate ran under the python grammar"
        );
    }
    let utils = std::fs::read_to_string(dir.join("utils.py")).expect("utils");
    assert!(
        utils.contains("def normalize_source("),
        "disk truth: renamed"
    );
    assert!(!utils.contains(OLD), "old name gone from utils.py");

    // --- apply refused: syntax-breaking python diff, with coordinates -------
    // Break utils.py with an unclosed f-string (drop the closing `")`),
    // while models.py's edit in the same diff is perfectly valid: the
    // whole diff must roll back — nothing flushed, nothing mutated.
    let break_at = utils.find("not a money amount").expect("break target");
    let close = utils[break_at..]
        .find("\")")
        .map(|at| break_at + at)
        .expect("closing quote pair");
    let before_utils = std::fs::read(dir.join("utils.py")).expect("utils");
    let before_models = std::fs::read(dir.join("models.py")).expect("models");
    // r##-delimited: the payload text itself contains `"#`, which would
    // close an r#"…"# string.
    let break_payload = format!(
        r##"{{"operations":[
            {{"path":"utils.py","edits":[{{"start_byte":{close},"old_end_byte":{},"new_text":"\"","old_text":"\")"}}]}},
            {{"path":"models.py","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"# staged with the refused diff\n"}}]}}
        ]}}"##,
        close + 2,
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["ok"], false);
    assert_eq!(
        refused["error"]["code"], "E_PARSE_INVALID",
        "the python syntax gate refuses the broken diff"
    );
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(!nodes.is_empty(), "coordinates ride along: {refused}");
    for node in nodes {
        assert_eq!(node["path"], "utils.py".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["is_missing"].is_boolean());
    }
    assert_eq!(
        std::fs::read(dir.join("utils.py")).expect("utils"),
        before_utils,
        "rollback: the broken file is untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("models.py")).expect("models"),
        before_models,
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
            m["path"] == "utils.py" && {
                let cap = capture(m, "@fn");
                let (s, e) = (
                    cap["start_byte"].as_u64().unwrap_or(u64::MAX) as usize,
                    cap["end_byte"].as_u64().unwrap_or(0) as usize,
                );
                let text = std::fs::read(dir.join("utils.py")).expect("utils");
                &text[s..e] == b"normalize_source"
            }
        })
        .expect("rescan finds normalize_source")
        .clone();
    let cap = capture(&renamed, "@fn");
    let (r_start, r_end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    let notation = format!("utils.py:{r_start}-{r_end}:identifier");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "normalize_source (was normalize_code) is the one funnel every \
             order code passes through; span stamped from scan output",
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
    let utils_path = dir.join("utils.py");
    let old = std::fs::read(&utils_path).expect("utils");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&utils_path, &moved).expect("write moved");

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
            "the python rename flushed clean across three files in one diff",
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
