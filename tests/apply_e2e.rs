//! F5 integration suites (PLAN §3 F5.4, F5.7): the transactional apply
//! loop exercised end to end — golden flows, the typed refusals, the
//! whole-file gate with coordinates, the honest partial flush, the
//! exclusivity guard through the real cache-take path, and the
//! ADR-0016-style determinism fence (byte-identical result JSON under
//! `RAYON_NUM_THREADS` 1 vs 8; `stats.wall_ms` the only stripped field).

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use janitor_prime::apply::{ApplyParams, DiffPayload};
use janitor_prime::ast::cache::ParseEntry;
use janitor_prime::ast::parse::parse_snapshot;
use janitor_prime::error::{JanitorError, VfsError};
use janitor_prime::orchestrator::Session;
use janitor_prime::scan::ScanParams;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_janitor")
}

/// One `janitor apply` invocation over an inline payload, under a pinned
/// rayon thread count; returns (exit code, stdout).
fn apply_bin(threads: u32, payload: &str, extra_args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(bin())
        .env("RAYON_NUM_THREADS", threads.to_string())
        .arg("apply")
        .arg("--diff")
        .arg(payload)
        .args(extra_args)
        .output()
        .expect("binary runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> u64 {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or(0) as u64
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read")
}

/// Three-file corpus: a shared identifier to rename, plus per-file shapes
/// for multi-edit flows (comments between items exercise nothing special
/// here — apply is byte-level — but keep the corpus honest).
fn corpus(root: &Path) {
    std::fs::create_dir_all(root.join("src")).expect("dirs");
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha(x: u32) -> u32 {\n    let a = x + 1;\n    a\n}\n",
    )
    .expect("write lib");
    std::fs::write(
        root.join("src/main.rs"),
        "mod lib;\nfn main() {\n    let v = lib::alpha(2);\n    let w = lib::alpha(v);\n    println!(\"{w}\");\n}\n",
    )
    .expect("write main");
    std::fs::write(
        root.join("src/third.rs"),
        "// leading comment\nfn alpha() -> u32 {\n    7\n}\n",
    )
    .expect("write third");
}

/// Golden flows through the real binary: a single edit, a multi-file
/// rename, and multi-edit-per-file — including an insertion that grows
/// the file before later edits' original offsets (the descending-order
/// correctness case, end to end). Envelope shape, disk truth, sorted
/// per-file results.
#[test]
fn golden_apply_flows_through_the_binary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    corpus(tmp.path());
    let lib = tmp.path().join("src/lib.rs");
    let lib_s = lib.display().to_string();
    let main = tmp.path().join("src/main.rs");
    let main_s = main.display().to_string();
    let third = tmp.path().join("src/third.rs");
    let third_s = third.display().to_string();

    let lib_bytes = read(&lib);
    let main_bytes = read(&main);
    let third_bytes = read(&third);

    let lib_alpha = find(&lib_bytes, b"alpha");
    let main_alpha = find(&main_bytes, b"alpha");
    let main_second = find(&main_bytes[main_alpha as usize + 5..], b"alpha") + main_alpha + 5;
    let third_alpha = find(&third_bytes, b"alpha");
    let third_let = find(&third_bytes, b"7\n");

    let payload = format!(
        r#"{{"operations":[
            {{"path":"{lib_s}","edits":[
                {{"start_byte":{lib_alpha},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}},
            {{"path":"{main_s}","edits":[
                {{"start_byte":{main_alpha},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}},
                {{"start_byte":{main_second},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}},
            {{"path":"{third_s}","edits":[
                {{"start_byte":{third_alpha},"old_end_byte":{},"new_text":"gamma","old_text":"alpha"}},
                {{"start_byte":{third_let},"old_end_byte":{},"new_text":"let SEVEN: u32 = 7;\n    SEVEN"}}]}}
        ]}}"#,
        lib_alpha + 5,
        main_alpha + 5,
        main_second + 5,
        third_alpha + 5,
        third_let + 1,
    );

    let (code, stdout) = apply_bin(4, &payload, &[]);
    assert_eq!(code, Some(0), "golden apply exits 0: {stdout}");
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("JSON envelope");
    assert_eq!(envelope["schema"], "janitor.v1");
    assert_eq!(envelope["ok"], true);
    let data = &envelope["data"];
    assert_eq!(data["dry_run"], false);
    assert_eq!(data["stats"]["files"], 3);
    assert_eq!(data["stats"]["edits"], 5);
    // Results sorted by path.
    let paths: Vec<&str> = data["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|f| f["path"].as_str().expect("path string"))
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    assert!(
        data["files"]
            .as_array()
            .expect("files")
            .iter()
            .all(|f| f["status"] == "flushed" && f["errors_after"] == 0)
    );

    // Disk truth: every edit landed, including the multi-edit file whose
    // second edit sat at original offsets the first edit shifted past.
    assert_eq!(
        read(&lib),
        b"pub fn beta(x: u32) -> u32 {\n    let a = x + 1;\n    a\n}\n".to_vec()
    );
    let main_text = String::from_utf8(read(&main)).expect("utf-8");
    assert!(!main_text.contains("alpha"), "both sites renamed");
    assert!(main_text.contains("lib::beta(2)") && main_text.contains("lib::beta(v)"));
    assert_eq!(
        read(&third),
        b"// leading comment\nfn gamma() -> u32 {\n    let SEVEN: u32 = 7;\n    SEVEN\n}\n"
            .to_vec()
    );
}

/// The three typed payload mismatches (ADR-0014): stale view, overlap,
/// old-bytes mismatch — each its own envelope code, exit 1, disk
/// untouched.
#[test]
fn typed_mismatches_refuse_with_their_envelopes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    corpus(tmp.path());
    let lib = tmp.path().join("src/lib.rs");
    let lib_s = lib.display().to_string();
    let before = read(&lib);
    let lib_alpha = find(&before, b"alpha");

    let cases: [(&str, &str, &str); 3] = [
        (
            "stale",
            "E_STALE_VIEW",
            &format!(
                r#"{{"operations":[{{"path":"{lib_s}","expect_sha":"ffffffffffffffff","edits":[{{"start_byte":{lib_alpha},"old_end_byte":{},"new_text":"beta"}}]}}]}}"#,
                lib_alpha + 5
            ),
        ),
        (
            "overlap",
            "E_EDIT_OVERLAP",
            &format!(
                r#"{{"operations":[{{"path":"{lib_s}","edits":[
                    {{"start_byte":{lib_alpha},"old_end_byte":{},"new_text":"xxxxxxx"}},
                    {{"start_byte":{},"old_end_byte":{},"new_text":"yy"}}]}}]}}"#,
                lib_alpha + 3,
                lib_alpha + 2,
                lib_alpha + 6
            ),
        ),
        (
            "mismatch",
            "E_EDIT_MISMATCH",
            &format!(
                r#"{{"operations":[{{"path":"{lib_s}","edits":[{{"start_byte":{lib_alpha},"old_end_byte":{},"new_text":"beta","old_text":"omega"}}]}}]}}"#,
                lib_alpha + 5
            ),
        ),
    ];

    for (name, code, payload) in cases {
        let (exit, stdout) = apply_bin(2, payload, &[]);
        assert_eq!(exit, Some(1), "{name}: operational exit 1");
        let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("JSON envelope");
        assert_eq!(envelope["ok"], false, "{name}");
        assert_eq!(envelope["error"]["code"], code, "{name}: {stdout}");
        assert_eq!(
            envelope["error"]["details"]["path"], lib_s,
            "{name}: the error names the offending file"
        );
        assert_eq!(read(&lib), before, "{name}: disk untouched");
    }
}

/// The syntax gate (KP-4, whole-file): an edit that breaks the re-parse
/// yields `E_PARSE_INVALID` with the offending node's coordinates (kind,
/// byte range, line/col, in_edited_range), and the whole diff rolls back
/// — the other file's valid edit is NOT flushed either (the rollback
/// proof: disk bytes compared pre/post failed apply).
#[test]
fn syntax_gate_rejection_carries_coordinates_and_rolls_back_everything() {
    let tmp = tempfile::tempdir().expect("tempdir");
    corpus(tmp.path());
    let lib = tmp.path().join("src/lib.rs");
    let lib_s = lib.display().to_string();
    let third = tmp.path().join("src/third.rs");
    let third_s = third.display().to_string();
    let before_lib = read(&lib);
    let before_third = read(&third);

    let third_bytes = read(&third);
    let at = find(&third_bytes, b"alpha");
    let payload = format!(
        r#"{{"operations":[
            {{"path":"{lib_s}","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"// fine\n"}}]}},
            {{"path":"{third_s}","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"alpha break"}}]}}
        ]}}"#,
        at + 5,
    );

    let (exit, stdout) = apply_bin(2, &payload, &[]);
    assert_eq!(exit, Some(1));
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("JSON envelope");
    assert_eq!(envelope["error"]["code"], "E_PARSE_INVALID");
    let nodes = envelope["error"]["details"]["nodes"]
        .as_array()
        .expect("node coordinates in the payload");
    assert!(!nodes.is_empty());
    for node in nodes {
        assert_eq!(node["path"], third.display().to_string());
        assert!(node["start_byte"].is_u64() && node["end_byte"].is_u64());
        assert!(node["start_point"]["row"].is_u64());
        assert!(node["start_point"]["column"].is_u64());
        assert!(node["is_missing"].is_boolean());
        assert!(node["in_edited_range"].is_boolean());
    }

    // The rollback proof: BOTH files' disk bytes are byte-identical.
    assert_eq!(
        read(&lib),
        before_lib,
        "the valid file is not flushed either"
    );
    assert_eq!(read(&third), before_third);
}

/// `E_FLUSH_PARTIAL` injection through the session: a read-only directory
/// rejects one file's tmp+rename mid-commit. The report is honest — the
/// writable file persisted, the read-only one rolled back (disk keeps its
/// original bytes; the staged entry is gone).
#[test]
fn flush_partial_injection_reports_per_file_outcomes() {
    // Root ignores directory read-only bits; probe and skip honestly.
    let probe = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o555))
        .expect("chmod probe");
    let writable_as_root = std::fs::File::create(probe.path().join("probe")).is_ok();
    std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o755))
        .expect("restore probe dir");
    if writable_as_root {
        eprintln!("running as root: read-only injection unavailable, skipping");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let ok_dir = tmp.path().join("a_ok");
    let ro_dir = tmp.path().join("z_ro");
    std::fs::create_dir(&ok_dir).expect("dirs");
    std::fs::create_dir(&ro_dir).expect("dirs");
    let keep = ok_dir.join("keep.rs");
    let keep_s = keep.display().to_string();
    let fail = ro_dir.join("fail.rs");
    let fail_s = fail.display().to_string();
    std::fs::write(&keep, "fn one() -> u32 { 1 }\n").expect("write");
    std::fs::write(&fail, "fn two() -> u32 { 2 }\n").expect("write");
    let fail_before = read(&fail);
    std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o555)).expect("chmod ro");

    let payload = DiffPayload::from_json_str(&format!(
        r#"{{"operations":[
            {{"path":"{keep_s}","edits":[{{"start_byte":{},"old_end_byte":{},"new_text":"uno"}}]}},
            {{"path":"{fail_s}","edits":[{{"start_byte":{},"old_end_byte":{},"new_text":"dos"}}]}}
        ]}}"#,
        find(&read(&keep), b"one"),
        find(&read(&keep), b"one") + 3,
        find(&fail_before, b"two"),
        find(&fail_before, b"two") + 3,
    ))
    .expect("payload");

    let session = Session::new();
    let err = session
        .apply(&ApplyParams {
            payload,
            dry_run: false,
        })
        .expect_err("the read-only file fails the flush");
    // Restore permissions for the cleanup and the asserts that follow.
    std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o755)).expect("restore");

    let JanitorError::Vfs(VfsError::FlushPartial { report }) = err else {
        unreachable!("the flush is the failing leg");
    };
    assert_eq!(report.persisted, vec![keep.display().to_string()]);
    assert_eq!(report.rolled_back, vec![fail.display().to_string()]);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].path, fail.display().to_string());

    // Disk truth: the writable file persisted, the read-only one kept its
    // original bytes (no silent partial content).
    assert_eq!(read(&keep), b"fn uno() -> u32 { 1 }\n".to_vec());
    assert_eq!(read(&fail), fail_before);
}

/// The exclusivity path end to end (ADR-0007 / §1.2.1 step 5): the agent
/// loop `scan → apply` on the same path, with a warm cache entry. The
/// positive leg proves `take()` handed the choreography an exclusively
/// owned tree (a stray handle would have tripped `E_PARSE_SHARED_TREE`);
/// the negative leg pins that refusal with zero mutation.
#[test]
fn exclusivity_guard_end_to_end_scan_then_apply() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("lib.rs");
    let path_s = path.display().to_string();
    std::fs::write(
        &path,
        "fn alpha(x: u32) -> u32 {\n    let inside = x + 1;\n    inside\n}\n",
    )
    .expect("write");

    let session = Session::new();
    // 1. Scan the path — matches exist, and the streaming discipline
    //    leaves the cache empty (the warm leg below re-inserts).
    let report = session
        .scan(&ScanParams::new(
            "(under function_item (seq (capture @fn (kind identifier)) _))",
            &path,
        ))
        .expect("scan");
    assert_eq!(report.matches.len(), 1, "the function's name matches");
    let capture = &report.matches[0].captures[0];
    assert_eq!(capture.symbol, "@fn");
    assert_eq!(
        &read(&path)[capture.start_byte..capture.end_byte],
        b"alpha",
        "scan coordinates index the same bytes apply will edit"
    );

    // 2. Warm the cache for the same path — the shape a multi-command
    //    warm process would hold between scan and apply.
    let key = janitor_prime::vfs::VfsPath::new(&path).expect("key");
    let bytes = session.overlay().read_file(&path).expect("bytes");
    let generation = session.overlay().current_gen(&path).expect("gen");
    let lang = session.registry().for_extension("rs").expect("rust");
    let mut parser = session.registry().parser(lang).expect("parser");
    let tree = parse_snapshot(&mut parser, &bytes).expect("parses");
    let entry = ParseEntry::new(generation, Arc::clone(&bytes), tree);
    session.cache().insert(key.clone(), entry.clone());
    // The cache and this local handle are the only two; dropping ours
    // leaves the cache's as the single handle the guard demands.
    assert_eq!(Arc::strong_count(&entry.tree), 2);
    drop(entry);
    assert!(!session.cache().is_empty(), "the warm entry is live");

    // 3. Apply the edit the scan coordinates described: `take` consumes
    //    the warm entry; success IS the exclusivity proof.
    let payload = DiffPayload::from_json_str(&format!(
        r#"{{"operations":[{{"path":"{path_s}","edits":[{{"start_byte":{},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}}]}}"#,
        capture.start_byte,
        capture.end_byte
    ))
    .expect("payload");
    session
        .apply(&ApplyParams {
            payload,
            dry_run: false,
        })
        .expect("exclusively owned entry edits cleanly");
    assert!(
        read(&path).starts_with(b"fn beta("),
        "the scan-derived edit flushed"
    );
    // A fresh entry replaced the taken one (post-flush bytes, clean).
    let new_bytes = session.overlay().read_file(&path).expect("post bytes");
    let fresh = session
        .cache()
        .get(
            &key,
            session.overlay().current_gen(&path).expect("gen"),
            &new_bytes,
        )
        .expect("fresh entry live after apply");
    assert_eq!(fresh.error_count, 0);

    // 4. Negative leg: a warm entry with a stray handle — the guard
    //    refuses with E_PARSE_SHARED_TREE and nothing mutates.
    std::fs::write(
        &path,
        "fn gamma(x: u32) -> u32 {\n    let inside = x + 1;\n    inside\n}\n",
    )
    .expect("reset file");
    let bytes = session.overlay().read_file(&path).expect("bytes");
    let generation = session.overlay().current_gen(&path).expect("gen");
    let tree = parse_snapshot(&mut parser, &bytes).expect("parses");
    let entry = ParseEntry::new(generation, Arc::clone(&bytes), tree);
    session.cache().insert(key.clone(), entry.clone());
    let reader = Arc::clone(&entry.tree); // the stray handle
    drop(entry);

    let payload = DiffPayload::from_json_str(&format!(
        r#"{{"operations":[{{"path":"{path_s}","edits":[{{"start_byte":{},"old_end_byte":{},"new_text":"delta","old_text":"gamma"}}]}}]}}"#,
        find(&bytes, b"gamma"),
        find(&bytes, b"gamma") + 5
    ))
    .expect("payload");
    let err = session
        .apply(&ApplyParams {
            payload,
            dry_run: false,
        })
        .expect_err("the guard trips on the stray handle");
    assert!(
        matches!(
            err,
            JanitorError::Parse(janitor_prime::error::ParseError::SharedTree { count: 2 })
        ),
        "got {err:?}"
    );
    assert_eq!(
        Arc::strong_count(&reader),
        1,
        "the taken entry dropped; the stray reader is the only handle left - no hidden ts_tree_copy"
    );
    assert!(
        read(&path).starts_with(b"fn gamma("),
        "disk untouched by the refused apply"
    );
}

/// The determinism fence (ADR-0016 (3) applied to apply): the same
/// multi-file, multi-edit payload against identical corpora under
/// `RAYON_NUM_THREADS=1` and `=8` emits byte-identical JSON — the report
/// is a function of the input, not the thread count (`stats.wall_ms`
/// stripped, and asserted to exist).
#[test]
fn apply_json_is_byte_identical_across_rayon_thread_counts() {
    let one = tempfile::tempdir().expect("tempdir");
    let eight = tempfile::tempdir().expect("tempdir");
    corpus(one.path());
    corpus(eight.path());

    let lib = one.path().join("src/lib.rs");
    let lib_s = lib.display().to_string();
    let main = one.path().join("src/main.rs");
    let main_s = main.display().to_string();
    let third = one.path().join("src/third.rs");
    let third_s = third.display().to_string();
    let lib_bytes = read(&lib);
    let main_bytes = read(&main);
    let third_bytes = read(&third);
    let payload = format!(
        r#"{{"operations":[
            {{"path":"{lib_s}","edits":[
                {{"start_byte":{},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}},
                {{"start_byte":{},"old_end_byte":{},"new_text":"let aa = x + 1;\n    aa\n","old_text":"let a = x + 1;\n    a\n"}}]}},
            {{"path":"{main_s}","edits":[{{"start_byte":{},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}},
            {{"path":"{third_s}","edits":[{{"start_byte":{},"old_end_byte":{},"new_text":"gamma","old_text":"alpha"}}]}}
        ]}}"#,
        find(&lib_bytes, b"alpha"),
        find(&lib_bytes, b"alpha") + 5,
        find(&lib_bytes, b"let a"),
        find(&lib_bytes, b"let a") + "let a = x + 1;\n    a\n".len() as u64,
        find(&main_bytes, b"alpha"),
        find(&main_bytes, b"alpha") + 5,
        find(&third_bytes, b"alpha"),
        find(&third_bytes, b"alpha") + 5,
    );
    // The same payload, path-for-path, over the second corpus.
    let payload_eight = payload.replace(
        one.path().display().to_string().as_str(),
        &eight.path().display().to_string(),
    );

    let strip_wall = |raw: &str| -> String {
        let mut envelope: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|err| panic!("not JSON: {err}"));
        assert!(
            envelope["data"]["stats"]["wall_ms"].as_u64().is_some(),
            "wall_ms must exist (and be stripped for comparison)"
        );
        envelope["data"]["stats"]["wall_ms"].take();
        serde_json::to_string(&envelope).expect("re-serialize")
    };

    let (code_one, out_one) = apply_bin(1, &payload, &[]);
    let (code_eight, out_eight) = apply_bin(8, &payload_eight, &[]);
    assert_eq!(code_one, Some(0), "{}", out_one);
    assert_eq!(code_eight, Some(0), "{}", out_eight);

    let one_stripped = strip_wall(&out_one);
    let eight_stripped = strip_wall(&out_eight);
    // Path-rewrite the second corpus's output back before comparing.
    let eight_normalized = eight_stripped.replace(
        eight.path().display().to_string().as_str(),
        one.path().display().to_string().as_str(),
    );
    assert_eq!(
        one_stripped, eight_normalized,
        "the apply report is a function of the input, not the thread count"
    );

    // Both corpora landed identically on disk.
    for rel in ["src/lib.rs", "src/main.rs", "src/third.rs"] {
        assert_eq!(
            read(&one.path().join(rel)),
            read(&eight.path().join(rel)),
            "{rel}"
        );
    }
}

/// `--dry-run` through the binary: validated and gated (the report
/// carries real per-file facts), then rolled back — disk untouched — and
/// the report's `sha_before` chains into the real apply's `expect_sha`
/// (the documented agent loop from docs/SCHEMA.md).
#[test]
fn dry_run_validates_reports_and_chains_into_a_real_apply() {
    let tmp = tempfile::tempdir().expect("tempdir");
    corpus(tmp.path());
    let lib = tmp.path().join("src/lib.rs");
    let lib_s = lib.display().to_string();
    let before = read(&lib);
    let at = find(&before, b"alpha");

    let payload = format!(
        r#"{{"operations":[{{"path":"{lib_s}","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}}]}}"#,
        at + 5
    );

    let (code, stdout) = apply_bin(2, &payload, &["--dry-run"]);
    assert_eq!(code, Some(0), "dry-run is a success outcome: {stdout}");
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("JSON envelope");
    let data = &envelope["data"];
    assert_eq!(data["dry_run"], true);
    assert_eq!(data["files"][0]["status"], "rolled_back");
    assert_eq!(data["files"][0]["edits"], 1);
    assert_eq!(read(&lib), before, "disk untouched by the dry-run");

    // Chain: the dry-run's sha_before is the real apply's expect_sha.
    let sha_before = data["files"][0]["sha_before"]
        .as_str()
        .expect("sha reported")
        .to_owned();
    let chained = format!(
        r#"{{"operations":[{{"path":"{lib_s}","expect_sha":"{sha_before}","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}}]}}"#,
        at + 5
    );
    let (code, stdout) = apply_bin(2, &chained, &[]);
    assert_eq!(code, Some(0), "chained apply flushes: {stdout}");
    assert!(
        read(&lib).starts_with(b"pub fn beta("),
        "the real apply landed"
    );
}

/// The agent-friendly stdin pipe: `--diff-file -` reads the payload from
/// stdin (and `--diff @file` reads it from a file) — both flush the same
/// edit.
#[test]
fn payload_sources_file_and_stdin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    corpus(tmp.path());
    let third = tmp.path().join("src/third.rs");
    let third_s = third.display().to_string();
    let before = read(&third);
    let at = find(&before, b"alpha");

    let payload = format!(
        r#"{{"operations":[{{"path":"{third_s}","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}}]}}"#,
        at + 5
    );
    let payload_file = tmp.path().join("diff.json");
    std::fs::write(&payload_file, &payload).expect("write payload");
    let mut child = Command::new(bin())
        .env("RAYON_NUM_THREADS", "2")
        .args(["apply", "--diff-file", "-", "--dry-run"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(payload.as_bytes())
        .expect("pipe payload");
    let out = child.wait_with_output().expect("collect");
    assert_eq!(out.status.code(), Some(0));
    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("JSON envelope");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["files"][0]["status"], "rolled_back");

    // @file indirection for --diff.
    let (code, stdout) = apply_bin(2, &format!("@{}", payload_file.display()), &[]);
    assert_eq!(code, Some(0), "at-file flushes: {stdout}");
    assert!(read(&third).starts_with(b"// leading comment\nfn beta("));
}

/// A malformed payload is the typed `E_EDIT_PAYLOAD` envelope, not a
/// panic and not a partial run.
#[test]
fn malformed_payload_is_a_typed_error_envelope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    corpus(tmp.path());
    let (code, stdout) = apply_bin(2, "this is not json", &[]);
    assert_eq!(code, Some(1));
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("JSON envelope");
    assert_eq!(envelope["error"]["code"], "E_EDIT_PAYLOAD");

    // Duplicate path spellings that canonicalize together are refused
    // too, never interleaved.
    let lib: PathBuf = tmp.path().join("src/lib.rs");
    let lib_s = lib.display().to_string();
    let bytes = read(&lib);
    let at = find(&bytes, b"alpha");
    let dup = format!(
        r#"{{"operations":[
            {{"path":"{lib_s}","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"b1"}}]}},
            {{"path":"{}/./src/lib.rs","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"b2"}}]}}
        ]}}"#,
        at + 5,
        tmp.path().display(),
        at + 5
    );
    let (code, stdout) = apply_bin(2, &dup, &[]);
    assert_eq!(code, Some(1));
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("JSON envelope");
    assert_eq!(envelope["error"]["code"], "E_EDIT_PAYLOAD");
    assert_eq!(read(&lib), bytes, "nothing applied");
}
