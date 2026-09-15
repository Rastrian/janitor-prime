//! Ruby end-to-end acceptance (F9c task 4, experimental tier): the
//! python template reduced per the brief — `init → scan → apply →
//! refused diff → note → re-anchor` (the nap leg is already covered by
//! the language-agnostic `tests/v01_acceptance.rs` and is skipped
//! here). Through the real binary, over the bundled ruby fixture repo
//! (`tests/fixtures/ruby/`, copied to a tempdir so `janitor init`
//! never writes into the repository):
//!
//! 1. `scan` finds a real structural population (every parameterized
//!    method, name captured);
//! 2. a multi-file ruby refactor (rename `normalize_code` →
//!    `normalize_source`: the def plus its internal caller in
//!    `user.rb`, and the call sites in `orders.rb`) applied
//!    **atomically** in one diff, validated by whole-file re-parse
//!    under the *ruby* grammar;
//! 3. a syntax-breaking diff **refused with coordinates**
//!    (`E_PARSE_INVALID` carrying the offending node's byte/point
//!    range) with the whole diff rolled back — the untouched bytes
//!    prove it;
//! 4. a pointer note that **re-anchors** after an edit above moves the
//!    method (content-hash match, exact drift).

#![cfg(feature = "lang-ruby")]

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

/// Copy the bundled ruby fixture repo into a fresh tempdir.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ruby");
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

/// Bytes inserted above the anchored method between the two notes; the
/// re-anchor drift must equal exactly this.
const INSERTED_ABOVE: &[u8] =
    b"# typed late in review: this funnel is load-bearing for orders too\n";

/// Every parameterized method, name captured (a method's named children
/// are name, method_parameters, body_statement — parameters directly
/// follow the name).
const SCAN_RULE: &str = "(under method (seq (capture @fn identifier) method_parameters))";

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
fn ruby_agent_loop_scan_apply_refuse_note_reanchor() {
    let tmp = fixture_repo();
    let dir = tmp.path();

    // --- init -------------------------------------------------------------
    let (code, init) = janitor(dir, &["init"]);
    assert_eq!(code, Some(0), "{init}");
    assert_eq!(init["ok"], true);
    assert!(dir.join(".janitor/LOG.txt").exists());

    // --- scan: every parameterized ruby method, name captured --------------
    let (code, scan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert_eq!(
        matches.len(),
        10,
        "the corpus carries a real method population: {scan}"
    );
    for m in matches {
        assert_eq!(m["kind"], "method", "pivot kind is ruby's");
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

    // The rename target: user.rb's normalize_code def (stamped from live
    // output, never hand-counted — the anti-hallucination contract).
    let target = matches
        .iter()
        .find(|m| {
            m["path"] == "user.rb" && {
                let cap = capture(m, "@fn");
                let (start, end) = (
                    cap["start_byte"].as_u64().expect("start") as usize,
                    cap["end_byte"].as_u64().expect("end") as usize,
                );
                let text = std::fs::read(dir.join("user.rb")).expect("user.rb");
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

    // --- apply: the atomic two-file rename ----------------------------------
    // normalize_code lives in user.rb (the def plus display_name's call)
    // and orders.rb (summary's and labels' calls): four edits over two
    // files — one diff, one flush, re-parsed as ruby.
    const OLD: &str = "normalize_code";
    const NEW: &str = "normalize_source";
    let mut operations = Vec::new();
    for name in ["user.rb", "orders.rb"] {
        let hits = find_all(&dir.join(name), OLD.as_bytes());
        assert_eq!(
            hits.len(),
            2,
            "{name} carries exactly the def/call pair (user) or two calls (orders)"
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
        apply["data"]["stats"]["edits"], 4,
        "four rename edits: the def, one internal call, two orders calls"
    );
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    for file in files {
        assert_eq!(file["status"], "flushed", "atomic: every file flushed");
        assert_eq!(
            file["errors_after"], 0,
            "the re-parse gate ran under the ruby grammar"
        );
    }
    let user = std::fs::read_to_string(dir.join("user.rb")).expect("user.rb");
    assert!(
        user.contains("def normalize_source("),
        "disk truth: renamed"
    );
    assert!(!user.contains(OLD), "old name gone from user.rb");

    // --- apply refused: syntax-breaking ruby diff, with coordinates ---------
    // Break user.rb with an unclosed string (drop the closing quote of
    // display_name's interpolated literal), while orders.rb's edit in the
    // same diff is perfectly valid: the whole diff must roll back —
    // nothing flushed, nothing mutated.
    let break_at = user.rfind("}>\"").map(|at| at + 2).expect("break target");
    let before_user = std::fs::read(dir.join("user.rb")).expect("user.rb");
    let before_orders = std::fs::read(dir.join("orders.rb")).expect("orders.rb");
    // r##-delimited: the payload text itself contains `"#`, which would
    // close an r#"…"# string (the python e2e hit the same thing).
    let break_payload = format!(
        r##"{{"operations":[
            {{"path":"user.rb","edits":[{{"start_byte":{break_at},"old_end_byte":{},"new_text":"","old_text":"\""}}]}},
            {{"path":"orders.rb","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"# staged with the refused diff\n"}}]}}
        ]}}"##,
        break_at + 1,
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["ok"], false);
    assert_eq!(
        refused["error"]["code"], "E_PARSE_INVALID",
        "the ruby syntax gate refuses the broken diff"
    );
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(!nodes.is_empty(), "coordinates ride along: {refused}");
    for node in nodes {
        assert_eq!(node["path"], "user.rb".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["is_missing"].is_boolean());
    }
    assert_eq!(
        std::fs::read(dir.join("user.rb")).expect("user.rb"),
        before_user,
        "rollback: the broken file is untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("orders.rb")).expect("orders.rb"),
        before_orders,
        "rollback: the OTHER file's valid edit is not flushed either"
    );

    // --- note: anchor to the renamed method's name --------------------------
    // Re-scan post-rename so the span is stamped from live output.
    let (code, rescan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{rescan}");
    let renamed = rescan["data"]["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .find(|m| {
            m["path"] == "user.rb" && {
                let cap = capture(m, "@fn");
                let (s, e) = (
                    cap["start_byte"].as_u64().unwrap_or(u64::MAX) as usize,
                    cap["end_byte"].as_u64().unwrap_or(0) as usize,
                );
                let text = std::fs::read(dir.join("user.rb")).expect("user.rb");
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
    let notation = format!("user.rb:{r_start}-{r_end}:identifier");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "normalize_source (was normalize_code) is the one funnel every \
             user field passes through; span stamped from scan output",
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
    let user_path = dir.join("user.rb");
    let old = std::fs::read(&user_path).expect("user.rb");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&user_path, &moved).expect("write moved");

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
}
