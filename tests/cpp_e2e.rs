//! C++ end-to-end acceptance (C3 of docs/PROMOTION.md): the v0.1
//! definition-of-done loop (`tests/v01_acceptance.rs` shape, the go_e2e
//! template) re-run over a **cpp** fixture module through the real
//! binary — `init → wake (empty) → scan → apply → note → re-anchor →
//! wake (budget) → nap settle → final wake` — plus the three apply-gate
//! honesty probes the promotion checklist pins **for this language**:
//!
//! 1. **Unclosed block comment** (the B9 class, generalized to C++
//!    delimiters): an edit that opens a `/*` and never closes it must
//!    be refused typedly. MEASURED, not assumed: tree-sitter-cpp
//!    0.23.4 follows the **rust class** — the grammar's own recovery
//!    emits ERROR nodes (`/*` mid-file parses with has_error=true; the
//!    ocaml-style silent-swallow does NOT happen) — so the refusal is
//!    `E_PARSE_INVALID` with coordinates, not
//!    `E_PARSE_UNCLOSED_COMMENT`; the lexical probe stays
//!    belt-and-braces behind the error-node gate. Either way: **zero
//!    silent flushes** — the same diff's valid file must not flush.
//! 2. **Deleted mandatory token** (MISSING class): deleting the `)`
//!    closing a parameter list is refused with coordinates — MEASURED:
//!    cpp recovers that deletion as a pure MISSING node (zero-width, no
//!    ERROR), while a deleted `;` recovers as a plain ERROR node; the
//!    probe pins the MISSING class that exists — and the whole diff
//!    rolls back.
//! 3. **Clean atomic multi-edit rename** through the real binary: the
//!    C++ rename shape is declaration-in-header, definition-in-source
//!    and call-in-consumer (`Process → FeedSample`: the `float
//!    Process(…);` prototype and its doc mention in `dsp/delay.h`, the
//!    `DelayLine::Process` definition in `dsp/delay.cpp`, the
//!    `line.Process(…)` call in `main.cpp`) applied atomically in one
//!    diff, validated by whole-file re-parse under the *cpp* grammar
//!    (the `.h` re-parses under cpp too — the routing decision riding
//!    the gate); the sabotage legs above roll the whole transaction.
//!
//! The fixture repo is the bundled corpus `tests/fixtures/cpp/` (the
//! same module the KP-6-cpp measurement and the determinism leg run
//! over), copied to a tempdir so `janitor init` never writes into the
//! repository.

#![cfg(feature = "lang-cpp")]

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

/// Copy the bundled cpp fixture module into a fresh tempdir.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cpp");
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

/// Bytes inserted above the anchored declarator between the two notes; the
/// re-anchor drift must equal exactly this.
const INSERTED_ABOVE: &[u8] =
    b"// typed late in review: the wet path is load-bearing for callers too\n";

/// Every function body in the module, declarator captured — the def
/// idiom (bodies are `function_definition`; prototypes are plain
/// `declaration`s and never match, the corpus's kind distinction).
const SCAN_RULE: &str = "(under function_definition (seq (capture @decl function_declarator) (kind compound_statement)))";

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
fn cpp_agent_loop_is_the_definition_of_done() {
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

    // --- scan: every function body, declarator captured ---------------------
    // Measured population: dsp/delay.cpp carries three bodies (Process,
    // Reset, MixTaps — NOT the ctor: its member-initializer list
    // (`field_initializer_list`) is a named child parked between the
    // declarator and the body, so the two-element adjacency seq skips
    // constructors, the C++-specific caveat this pins), dsp/delay.h one
    // (the Clamp template body — a `template_declaration >
    // function_definition`, visited at any depth), main.cpp three
    // (NormalizeGain, SweepGain, main).
    let (code, scan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert_eq!(
        matches.len(),
        7,
        "the module carries the measured function population: {scan}"
    );
    for m in matches {
        assert_eq!(m["kind"], "function_definition", "pivot kind is cpp's");
        assert_eq!(m["captures"][0]["kind"], "function_declarator");
    }
    // The .h rode the same scan under the cpp grammar (routing decision).
    assert_eq!(
        scan["data"]["stats"]["langs"],
        serde_json::json!({ "cpp": 3 })
    );
    // matches are file-grouped, paths sorted (canonical order).
    let mut paths: Vec<&str> = matches
        .iter()
        .map(|m| m["path"].as_str().expect("path"))
        .collect();
    paths.dedup();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);

    // The rename target: dsp/delay.cpp's DelayLine::Process definition.
    let target = matches
        .iter()
        .find(|m| {
            m["path"] == "dsp/delay.cpp" && {
                let cap = capture(m, "@decl");
                let (start, end) = (
                    cap["start_byte"].as_u64().expect("start") as usize,
                    cap["end_byte"].as_u64().expect("end") as usize,
                );
                let text = std::fs::read(dir.join("dsp/delay.cpp")).expect("delay");
                &text[start..end] == b"DelayLine::Process(float sample)"
            }
        })
        .expect("scan finds DelayLine::Process's declarator")
        .clone();
    let cap = capture(&target, "@decl");
    let (start, end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    assert_eq!(end - start, "DelayLine::Process(float sample)".len() as u64);

    // --- apply: the atomic multi-file rename --------------------------------
    // The honest C++ rename shape: `#include` lines never carry the
    // symbol, so the edits are the header prototype + its doc mention
    // (dsp/delay.h), the qualified definition (dsp/delay.cpp) and the
    // call site (main.cpp) — four edits over three files, one diff, one
    // flush, every file re-parsed under the cpp grammar.
    const OLD: &str = "Process";
    const NEW: &str = "FeedSample";
    let mut operations = Vec::new();
    for name in ["dsp/delay.h", "dsp/delay.cpp", "main.cpp"] {
        let hits = find_all(&dir.join(name), OLD.as_bytes());
        assert!(
            name != "dsp/delay.h" || hits.len() == 2,
            "the header carries the prototype and its doc mention"
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
        apply["data"]["stats"]["edits"], 4,
        "four rename edits: header prototype + doc, definition, call"
    );
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 3);
    for file in files {
        assert_eq!(file["status"], "flushed", "atomic: every file flushed");
        assert_eq!(
            file["errors_after"], 0,
            "the re-parse gate ran under the cpp grammar"
        );
    }
    let header = std::fs::read_to_string(dir.join("dsp/delay.h")).expect("header");
    assert!(
        header.contains("float FeedSample(float sample);"),
        "disk truth: the prototype renamed"
    );
    for name in ["dsp/delay.h", "dsp/delay.cpp", "main.cpp"] {
        let text = std::fs::read_to_string(dir.join(name)).expect("read");
        assert!(!text.contains(OLD), "old name gone from {name}");
    }

    // --- apply refused: the unclosed block comment, measured class ----------
    // Probe 1 (C3): swap `DelayLine::FeedSample`'s return-type-to-body
    // shape is left alone; instead the module's own doc comment above
    // MixTaps is turned into an OPENED `/*` that never closes — the B9
    // class generalized to C++ delimiters. MEASURED: tree-sitter-cpp
    // recovers this with ERROR nodes (the rust class), so the gate's
    // refusal is E_PARSE_INVALID with coordinates — this assert pins
    // WHICH class cpp follows; a grammar upgrade that starts
    // recovering silently flips the lexical probe on instead, and this
    // leg must be re-measured then. main.cpp's edit in the same diff is
    // perfectly valid: the whole diff must roll back — nothing flushed.
    let delay_text = std::fs::read_to_string(dir.join("dsp/delay.cpp")).expect("delay");
    let break_at = delay_text
        .find("// MixTaps is the dry/wet funnel")
        .expect("break target");
    let before_delay = std::fs::read(dir.join("dsp/delay.cpp")).expect("delay");
    let before_main = std::fs::read(dir.join("main.cpp")).expect("main");
    let break_payload = format!(
        r#"{{"operations":[
            {{"path":"dsp/delay.cpp","edits":[{{"start_byte":{break_at},"old_end_byte":{},"new_text":"/* never closed","old_text":"// MixTaps is the dry/wet funnel"}}]}},
            {{"path":"main.cpp","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"// staged with the refused diff\n"}}]}}
        ]}}"#,
        break_at + "// MixTaps is the dry/wet funnel".len(),
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["ok"], false);
    assert_eq!(
        refused["error"]["code"], "E_PARSE_INVALID",
        "the MEASURED cpp class: the grammar's own recovery errors on an \
         unclosed /* (zero silent flushes either way)"
    );
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(!nodes.is_empty(), "coordinates ride along: {refused}");
    for node in nodes {
        assert_eq!(node["path"], "dsp/delay.cpp".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["is_missing"].is_boolean());
    }
    assert_eq!(
        std::fs::read(dir.join("dsp/delay.cpp")).expect("delay"),
        before_delay,
        "rollback: the broken file is untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("main.cpp")).expect("main"),
        before_main,
        "rollback: the OTHER file's valid edit is not flushed either"
    );

    // --- apply refused AGAIN: the deleted mandatory token -------------------
    // Probe 2 (C3): delete the `)` closing FeedSample's parameter list.
    // MEASURED: tree-sitter-cpp recovers this deletion as a pure MISSING
    // node (zero-width, no ERROR siblings) — while a deleted `;`
    // recovers as a plain ERROR node (both classes measured in this
    // grammar; the MISSING leg pins the one that exists). The diff
    // again includes a valid main.cpp edit; the whole transaction rolls
    // back.
    let break_at = delay_text
        .find("FeedSample(float sample)")
        .expect("the parameter list's closer");
    let close_paren = break_at + "FeedSample(float sample".len();
    let break_payload = format!(
        r#"{{"operations":[
            {{"path":"dsp/delay.cpp","edits":[{{"start_byte":{close_paren},"old_end_byte":{},"new_text":"","old_text":")"}}]}},
            {{"path":"main.cpp","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"// staged with the refused diff\n"}}]}}
        ]}}"#,
        close_paren + 1,
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["error"]["code"], "E_PARSE_INVALID");
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(
        nodes.iter().any(|node| node["is_missing"] == true),
        "the MISSING token class, named: {refused}"
    );
    for node in nodes {
        assert_eq!(node["path"], "dsp/delay.cpp".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
    }
    assert_eq!(
        std::fs::read(dir.join("dsp/delay.cpp")).expect("delay"),
        before_delay,
        "rollback: nothing flushed from the second sabotage either"
    );
    assert_eq!(
        std::fs::read(dir.join("main.cpp")).expect("main"),
        before_main,
        "rollback: main.cpp is still untouched"
    );

    // --- note: anchor to the renamed method's declarator --------------------
    // Re-scan post-rename so the span is stamped from live output, never
    // hand-counted (the anti-hallucination contract).
    let (code, rescan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{rescan}");
    let renamed = rescan["data"]["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .find(|m| {
            m["path"] == "dsp/delay.cpp" && {
                let cap = capture(m, "@decl");
                let (s, e) = (
                    cap["start_byte"].as_u64().unwrap_or(u64::MAX) as usize,
                    cap["end_byte"].as_u64().unwrap_or(0) as usize,
                );
                let text = std::fs::read(dir.join("dsp/delay.cpp")).expect("delay");
                &text[s..e] == b"DelayLine::FeedSample(float sample)"
            }
        })
        .expect("rescan finds DelayLine::FeedSample")
        .clone();
    let cap = capture(&renamed, "@decl");
    let (r_start, r_end) = (
        cap["start_byte"].as_u64().expect("start"),
        cap["end_byte"].as_u64().expect("end"),
    );
    let notation = format!("dsp/delay.cpp:{r_start}-{r_end}:function_declarator");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "FeedSample (was Process) is the wet path every block passes \
             through; span stamped from scan output",
        ],
    );
    assert_eq!(code, Some(0), "{anchored}");
    assert_eq!(anchored["data"]["id"], 0);
    assert_eq!(
        anchored["data"]["ptr"]["span"],
        serde_json::json!([r_start, r_end])
    );
    assert_eq!(anchored["data"]["ptr"]["kind_name"], "function_declarator");

    // --- edit above the anchor (outside janitor, the way a teammate would) --
    let delay_path = dir.join("dsp/delay.cpp");
    let old = std::fs::read(&delay_path).expect("delay");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&delay_path, &moved).expect("write moved");

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
            "the cpp rename flushed clean across three files in one diff",
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
