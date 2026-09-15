//! v0.1 definition-of-done acceptance test (PLAN §7, eval-2 amendment 5 /
//! E11(b); F7 brief task 5): ONE scripted agent session through the real
//! binary — `init → wake (empty) → scan (finds target) → apply (good diff,
//! flushed) → note (anchor) → edit above anchor → note (re-anchor) → wake
//! with budget → nap loop settle → final wake fits` — asserting every
//! payload's schema, ok, and key fields along the way.
//!
//! This, not the release checklist, is v0.1's cut-to-zero test: with only
//! the "in" list of PLAN §7, an agent can discover → stage → validate →
//! flush → remember a refactor and recall it under a budget with
//! anti-hallucinated anchors. If this test cannot pass, the release does
//! not ship — it is the gate.
//!
//! One deliberate extension of the brief's skeleton, documented here: the
//! session appends a **third** note (the agent's free-form record of the
//! refactor it just landed) before the budgeted wake. The nap step of the
//! loop is unreachable with two facts — a size-2 block at the end of the
//! log has age 0 and the biased tiling always demotes it to raw
//! (`size ≤ AGE_ALPHA × age`, §1.5.2) — so the completeness gate needs a
//! third, newer fact to make the oldest pair compressible. Real agents
//! write that note; the loop reads the same either way.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_janitor")
}

/// One `janitor` invocation in `dir` (env-isolated so this machine's XDG
/// config cannot leak into the global-layer assertions). Every payload on
/// stdout is a `janitor.v1` envelope — that much is asserted here, once,
/// for every single command of the session.
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

/// The budget the budgeted wake runs under. Sized for this session's fact
/// texts: their raw total sits well above it (the gate must fire), and the
/// settled layout — one summary over the oldest pair plus the newest raw
/// fact — fits inside it (the final wake must fit, untruncated).
const BUDGET: u64 = 48;

/// The bytes inserted above the anchored node between the two notes: the
/// re-anchor drift must equal exactly this.
const INSERTED_ABOVE: &[u8] = b"fn zeta() -> u8 {\n    1\n}\n\n";

#[test]
fn v01_agent_loop_is_the_definition_of_done() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();

    // The fixture repo: one rust function whose name the scan will find.
    // Plain files — this loop needs no git (Commit pointers are a separate
    // anchor flavor, covered by the F6 suites).
    std::fs::write(
        dir.join("lib.rs"),
        "fn alpha(x: u32) -> u32 {\n    let base = x + 1;\n    base * 2\n}\n",
    )
    .expect("write fixture");

    // --- init -------------------------------------------------------------
    let (code, init) = janitor(dir, &["init"]);
    assert_eq!(code, Some(0), "{init}");
    assert_eq!(init["ok"], true);
    assert_eq!(init["data"]["initialized"], true);
    assert!(dir.join(".janitor/LOG.txt").exists());
    assert!(dir.join(".janitor/TREE").is_dir());
    assert!(dir.join(".janitor/STATE.json").exists());

    // --- wake (empty): everything trivially fits, nothing is compressed --
    let (code, empty) = janitor(dir, &["wake"]);
    assert_eq!(code, Some(0), "{empty}");
    assert_eq!(empty["ok"], true);
    assert_eq!(empty["data"]["awake"], true);
    assert_eq!(
        empty["data"]["items"].as_array().map(Vec::len),
        Some(0),
        "an empty log serves an empty payload: {empty}"
    );
    assert_eq!(empty["data"]["global"]["source"], "embedded");

    // --- scan: discover the target, with a capture whose span drives apply
    let (code, scan) = janitor(
        dir,
        &[
            "scan",
            "--rule",
            "(under function_item (seq (capture @fn (kind identifier)) _))",
        ],
    );
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    assert_eq!(
        scan["data"]["rule"],
        "(under function_item (seq (capture @fn (kind identifier)) _))"
    );
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 1, "exactly the one function: {scan}");
    assert_eq!(matches[0]["path"], "lib.rs");
    assert_eq!(matches[0]["kind"], "function_item");
    let captures = matches[0]["captures"].as_array().expect("captures");
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0]["symbol"], "@fn");
    let (start, end) = (
        captures[0]["start_byte"].as_u64().expect("capture start"),
        captures[0]["end_byte"].as_u64().expect("capture end"),
    );
    assert_eq!(end - start, "alpha".len() as u64);
    assert_eq!(
        &std::fs::read(dir.join("lib.rs")).expect("lib")[start as usize..end as usize],
        b"alpha",
        "scan coordinates index the same bytes apply will edit"
    );

    // --- apply: the good diff, staged, validated by re-parse, flushed -----
    // A same-length rename keeps the scan-stamped span valid after the
    // flush, which is exactly what the anchored note below consumes.
    let payload = format!(
        r#"{{"operations":[{{"path":"lib.rs","edits":[
            {{"start_byte":{start},"old_end_byte":{end},"new_text":"delta","old_text":"alpha"}}]}}]}}"#
    );
    let (code, apply) = janitor(dir, &["apply", "--diff", &payload]);
    assert_eq!(code, Some(0), "{apply}");
    assert_eq!(apply["ok"], true);
    assert_eq!(apply["data"]["dry_run"], false);
    assert_eq!(apply["data"]["stats"]["files"], 1);
    assert_eq!(apply["data"]["stats"]["edits"], 1);
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "lib.rs");
    assert_eq!(files[0]["status"], "flushed");
    assert_eq!(
        files[0]["errors_after"], 0,
        "the gate passed: clean re-parse"
    );
    assert!(
        files[0]["sha_after"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the report stamps the post-flush content hash"
    );
    assert!(
        std::fs::read_to_string(dir.join("lib.rs")).is_ok_and(|text| text.starts_with("fn delta(")),
        "disk truth: the edit landed"
    );

    // --- note: anchor the fact to the node the scan captured --------------
    let notation = format!("lib.rs:{start}-{end}:identifier");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "delta (was alpha) doubles its incremented input; span stamped from scan capture \
             output and validated against the live tree",
        ],
    );
    assert_eq!(code, Some(0), "{anchored}");
    assert_eq!(anchored["ok"], true);
    assert_eq!(anchored["data"]["id"], 0);
    assert_eq!(
        anchored["data"]["ptr"]["span"],
        serde_json::json!([start, end])
    );
    assert_eq!(anchored["data"]["ptr"]["kind_name"], "identifier");
    assert!(
        anchored["data"]["ptr"]["content_sha"]
            .as_str()
            .is_some_and(|sha| sha.len() == 16),
        "the anchor carries the anti-hallucination content hash"
    );

    // --- edit above the anchor (outside janitor, the way a teammate would)
    let lib = dir.join("lib.rs");
    let old = std::fs::read(&lib).expect("lib");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&lib, &moved).expect("write moved");

    // --- note again: the stored pointer re-anchors with exact drift -------
    let stored = anchored["data"]["ptr"].to_string();
    let (code, reanchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &stored,
            "the anchor followed the insert above it; re-anchored by content hash, drift equals \
             the inserted bytes",
        ],
    );
    assert_eq!(code, Some(0), "{reanchored}");
    assert_eq!(reanchored["ok"], true);
    assert_eq!(reanchored["data"]["id"], 1);
    assert_eq!(reanchored["data"]["reanchored"], true);
    assert_eq!(
        reanchored["data"]["drift"].as_u64(),
        Some(INSERTED_ABOVE.len() as u64),
        "drift is exactly the bytes inserted above the old span"
    );
    assert_eq!(
        reanchored["data"]["ptr"]["span"],
        serde_json::json!([
            start + INSERTED_ABOVE.len() as u64,
            end + INSERTED_ABOVE.len() as u64
        ])
    );

    // The agent's own record of the refactor it just landed — the third
    // fact that makes the oldest pair compressible (see the module doc).
    let (code, record) = janitor(
        dir,
        &[
            "note",
            "the alpha to delta rename flushed clean through the re-parse gate",
        ],
    );
    assert_eq!(code, Some(0), "{record}");
    assert_eq!(record["data"]["id"], 2);

    // --- wake with budget: the completeness gate refuses with the request -
    let (code, gate) = janitor(dir, &["wake", "--budget", &BUDGET.to_string()]);
    assert_eq!(code, Some(1), "the gate is an operational refusal: {gate}");
    assert_eq!(gate["ok"], false);
    assert_eq!(gate["error"]["code"], "E_NAP_PENDING");
    assert!(
        gate["error"]["details"]["slot"].as_str().is_some(),
        "the refusal names the slot it needs"
    );

    // --- nap loop settle: act on each request until wake answers ----------
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

    // --- final wake fits: complete, inside the budget, coarsened + raw ----
    assert_eq!(settled.0, Some(0), "wake answers after settling");
    let final_wake = settled.1;
    assert_eq!(final_wake["ok"], true);
    assert_eq!(final_wake["data"]["awake"], true);
    assert!(
        final_wake["data"]["estimate"].as_u64().expect("estimate") <= BUDGET,
        "the payload fits the budget: {final_wake}"
    );
    assert_eq!(final_wake["data"]["truncated"], false);
    assert_eq!(final_wake["data"]["budget_tokens"], BUDGET);
    assert_eq!(final_wake["data"]["estimator"], "chars4");
    let items = final_wake["data"]["items"].as_array().expect("items");
    let summary = items
        .iter()
        .find(|item| item["kind"] == "summary")
        .expect("the oldest pair is coarsened into a summary");
    assert_eq!(summary["covers"], serde_json::json!([0, 2]));
    assert_eq!(summary["slot"], "0-2");
    assert!(
        items
            .iter()
            .any(|item| item["kind"] == "fact" && item["id"] == 2),
        "the newest fact stays raw"
    );
    assert_eq!(
        summary["text"], "0-2: the early anchored facts, compressed",
        "the summary standing in for the anchored pair is the agent's own line"
    );

    // Nothing left to compress: the protocol is closed.
    let (_, done) = janitor(dir, &["nap"]);
    assert_eq!(done["data"]["pending"], false);
    assert_eq!(done["data"]["message"], "Nothing left to compress.");
}
