//! OCaml end-to-end acceptance (F9c task 4, experimental tier): the
//! python template reduced per the brief — `init → scan → apply →
//! refused diff → note → re-anchor` (nap leg skipped; see the ruby e2e)
//! — with the ocaml-specific twist the two-LangId mapping exists for:
//! the atomic rename spans a `.ml` AND its `.mli` in one diff, so BOTH
//! grammars' re-parse gates run on the same transaction. Through the
//! real binary, over the bundled fixture repo (`tests/fixtures/ocaml/`,
//! copied to a tempdir):
//!
//! 1. `scan` finds a real structural population (six parameterized
//!    bindings) and reports both languages — `ocaml: 2`,
//!    `ocaml_interface: 1` — the mapping's CLI-level proof;
//! 2. the rename `normalize_code` → `normalize_source` (the def and two
//!    internal calls in `store.ml`, plus the `val` specification in
//!    `store.mli`) applied atomically, four edits, two files, two
//!    grammars;
//! 3. a syntax-breaking diff (an unclosed comment swallowing the rest
//!    of `store.ml`) refused with `E_PARSE_INVALID` + coordinates, the
//!    whole diff rolled back;
//! 4. a pointer note that re-anchors after an edit above moves the
//!    definition (content-hash match, exact drift).

#![cfg(feature = "lang-ocaml")]

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

/// Copy the bundled ocaml fixture repo into a fresh tempdir.
fn fixture_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ocaml");
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

/// Bytes inserted above the anchored definition between the two notes;
/// the re-anchor drift must equal exactly this.
const INSERTED_ABOVE: &[u8] =
    b"(* typed late in review: this funnel is load-bearing for report too *)\n";

/// Every parameterized binding, name captured (a let_binding's children
/// are the value_name, then parameters, then the body).
const SCAN_RULE: &str = "(under let_binding (seq (capture @fn value_name) parameter))";

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
fn ocaml_agent_loop_scan_apply_refuse_note_reanchor() {
    let tmp = fixture_repo();
    let dir = tmp.path();

    // --- init -------------------------------------------------------------
    let (code, init) = janitor(dir, &["init"]);
    assert_eq!(code, Some(0), "{init}");
    assert_eq!(init["ok"], true);
    assert!(dir.join(".janitor/LOG.txt").exists());

    // --- scan: six parameterized bindings, both languages reported --------
    let (code, scan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{scan}");
    assert_eq!(scan["ok"], true);
    let matches = scan["data"]["matches"].as_array().expect("matches");
    assert_eq!(
        matches.len(),
        6,
        "the corpus carries a real binding population: {scan}"
    );
    for m in matches {
        assert_eq!(m["kind"], "let_binding", "pivot kind is ocaml's");
        assert_eq!(m["captures"][0]["kind"], "value_name");
    }
    // The two-LangId mapping, at the CLI level: two .ml files scanned as
    // ocaml, one .mli as ocaml_interface — one scan, two grammars.
    assert_eq!(
        scan["data"]["stats"]["langs"],
        serde_json::json!({ "ocaml": 2, "ocaml_interface": 1 }),
        "both ocaml LangIds report: {scan}"
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

    // The rename target: store.ml's normalize_code def (stamped from
    // live output, never hand-counted).
    let target = matches
        .iter()
        .find(|m| {
            m["path"] == "store.ml" && {
                let cap = capture(m, "@fn");
                let (start, end) = (
                    cap["start_byte"].as_u64().expect("start") as usize,
                    cap["end_byte"].as_u64().expect("end") as usize,
                );
                let text = std::fs::read(dir.join("store.ml")).expect("store.ml");
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

    // --- apply: the atomic .ml+.mli rename ---------------------------------
    // normalize_code lives in store.ml (the def, entry_key's call and
    // render's call) and in store.mli (the val specification): four edits
    // over two files UNDER TWO GRAMMARS — one diff, one flush, each file
    // re-parsed by its own grammar.
    const OLD: &str = "normalize_code";
    const NEW: &str = "normalize_source";
    let mut operations = Vec::new();
    for name in ["store.ml", "store.mli"] {
        let hits = find_all(&dir.join(name), OLD.as_bytes());
        assert_eq!(
            hits.len(),
            if name == "store.ml" { 3 } else { 1 },
            "{name}: the def + two calls (ml), or the val spec (mli)"
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
    assert_eq!(
        apply["data"]["stats"]["files"], 2,
        "one diff, two files, two grammars"
    );
    assert_eq!(
        apply["data"]["stats"]["edits"], 4,
        "four rename edits: the def, two internal calls, one val spec"
    );
    let files = apply["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    for file in files {
        assert_eq!(file["status"], "flushed", "atomic: every file flushed");
        assert_eq!(
            file["errors_after"], 0,
            "the re-parse gate ran under each file's own grammar"
        );
    }
    let store = std::fs::read_to_string(dir.join("store.ml")).expect("store.ml");
    assert!(
        store.contains("let normalize_source input"),
        "disk truth: renamed"
    );
    assert!(!store.contains(OLD), "old name gone from store.ml");
    let iface = std::fs::read_to_string(dir.join("store.mli")).expect("store.mli");
    assert!(
        iface.contains("val normalize_source :"),
        "the interface renamed with the implementation"
    );

    // --- apply refused: syntax-breaking ocaml diff, with coordinates ------
    // Break store.ml by deleting the FIRST `in` of its let..in chain (the
    // mandatory keyword token between a local binding and its body — a
    // MISSING node, the honest break; an unclosed comment, by contrast,
    // recovers silently in this grammar — errors_after 0, measured),
    // while report.ml's edit in the same diff is perfectly valid: the
    // whole diff must roll back.
    let in_at = store.find(" in\n").expect("break target") + 1;
    let before_store = std::fs::read(dir.join("store.ml")).expect("store.ml");
    let before_report = std::fs::read(dir.join("report.ml")).expect("report.ml");
    let break_payload = format!(
        r#"{{"operations":[
            {{"path":"store.ml","edits":[{{"start_byte":{in_at},"old_end_byte":{},"new_text":"","old_text":"in"}}]}},
            {{"path":"report.ml","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"(* staged with the refused diff *)\n"}}]}}
        ]}}"#,
        in_at + 2,
    );
    let (code, refused) = janitor(dir, &["apply", "--diff", &break_payload]);
    assert_eq!(code, Some(1), "operational refusal: {refused}");
    assert_eq!(refused["ok"], false);
    assert_eq!(
        refused["error"]["code"], "E_PARSE_INVALID",
        "the ocaml syntax gate refuses the broken diff"
    );
    let nodes = refused["error"]["details"]["nodes"]
        .as_array()
        .expect("nodes");
    assert!(!nodes.is_empty(), "coordinates ride along: {refused}");
    for node in nodes {
        assert_eq!(node["path"], "store.ml".to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["is_missing"].is_boolean());
    }
    assert_eq!(
        std::fs::read(dir.join("store.ml")).expect("store.ml"),
        before_store,
        "rollback: the broken file is untouched"
    );
    assert_eq!(
        std::fs::read(dir.join("report.ml")).expect("report.ml"),
        before_report,
        "rollback: the OTHER file's valid edit is not flushed either"
    );

    // --- note: anchor to the renamed definition's name ---------------------
    // Re-scan post-rename so the span is stamped from live output.
    let (code, rescan) = janitor(dir, &["scan", "--rule", SCAN_RULE]);
    assert_eq!(code, Some(0), "{rescan}");
    let renamed = rescan["data"]["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .find(|m| {
            m["path"] == "store.ml" && {
                let cap = capture(m, "@fn");
                let (s, e) = (
                    cap["start_byte"].as_u64().unwrap_or(u64::MAX) as usize,
                    cap["end_byte"].as_u64().unwrap_or(0) as usize,
                );
                let text = std::fs::read(dir.join("store.ml")).expect("store.ml");
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
    let notation = format!("store.ml:{r_start}-{r_end}:value_name");
    let (code, anchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            "normalize_source (was normalize_code) is the one funnel every \
             store field passes through; span stamped from scan output",
        ],
    );
    assert_eq!(code, Some(0), "{anchored}");
    assert_eq!(anchored["data"]["id"], 0);
    assert_eq!(
        anchored["data"]["ptr"]["span"],
        serde_json::json!([r_start, r_end])
    );
    assert_eq!(anchored["data"]["ptr"]["kind_name"], "value_name");

    // --- edit above the anchor (outside janitor, the way a teammate would) -
    let store_path = dir.join("store.ml");
    let old = std::fs::read(&store_path).expect("store.ml");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&store_path, &moved).expect("write moved");

    // --- note again: the stored pointer re-anchors with exact drift --------
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
