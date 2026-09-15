//! F10b MCP e2e (ADR-0019): the v0.1 agent loop and every tool, driven
//! through the REAL binary speaking the REAL protocol — one child
//! `janitor mcp --workspace <fixture>` per session, requests written as
//! newline-delimited JSON-RPC, responses read back frame by frame.
//!
//! What this suite pins, per the F10b brief:
//!
//! - **Handshake**: `initialize` (version negotiation, serverInfo,
//!   tools capability) → `notifications/initialized` (no response) →
//!   `tools/list` (exactly the six `janitor_*` tools, JSON Schema each) →
//!   `ping`.
//! - **Round-trips**: `tools/call` for every tool against a fixture
//!   workspace; each result carries the `janitor.v1` envelope as
//!   structured content (and the same JSON as text).
//! - **Error paths**: a bad rule is the typed `E_DSL_*` refusal in
//!   structured content with `isError: true`; a dangling pointer is
//!   `E_POINTER_DANGLING`; traversal and absolute paths are the typed
//!   workspace refusal `E_VFS_INVALID_PATH`.
//! - **Stdout purity**: with `RUST_LOG=debug` (adversarial — anything
//!   janitor logs is requested), every byte the server ever writes to
//!   stdout is a JSON frame, and nothing else remains after exit.
//! - **The v0.1 acceptance loop through MCP**: the
//!   tests/v01_acceptance.rs session re-run via `tools/call` (`init`
//!   stays CLI-side per the brief): wake → scan → apply → note → re-note
//!   → nap gate → settle → wake fits.
//! - **Both framings**: one session speaks LSP-style `Content-Length`
//!   frames end to end (the brief's framing), mirrored by the server.

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_janitor")
}

/// The fixture every session scans: one rust function whose name a
/// `(capture @fn …)` rule finds, exactly as in tests/v01_acceptance.rs.
const LIB: &str = "fn alpha(x: u32) -> u32 {\n    let base = x + 1;\n    base * 2\n}\n";

const RULE: &str = "(under function_item (seq (capture @fn (kind identifier)) _))";

const BUDGET: u64 = 48;
const INSERTED_ABOVE: &[u8] = b"fn zeta() -> u8 {\n    1\n}\n\n";

/// One live MCP session against the real binary. Every response line read
/// from stdout is asserted to be a JSON object — half of the purity proof;
/// the other half is [`Session::finish`] draining the pipe after exit.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Session {
    fn start(workspace: &Path) -> Self {
        let mut child = Command::new(bin())
            .args(["mcp", "--workspace"])
            .arg(workspace)
            .env("XDG_CONFIG_HOME", workspace.join("xdg-absent"))
            // Adversarial on purpose: ask janitor to log everything, then
            // prove none of it can reach stdout.
            .env("RUST_LOG", "debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("janitor mcp spawns");
        let stdin = child.stdin.take().expect("stdin pipe");
        let stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    /// One request → one response, matched by id.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write_frame(&frame.to_string());
        let response = self.read_frame();
        assert_eq!(
            response["id"], id,
            "responses stay matched to requests: {response}"
        );
        response
    }

    /// A notification: sent, never answered (the next response must still
    /// match its own id — the desync fence).
    fn notify(&mut self, method: &str) {
        let frame = json!({ "jsonrpc": "2.0", "method": method });
        self.write_frame(&frame.to_string());
    }

    /// The full handshake, returning the initialize result.
    fn handshake(&mut self) -> Value {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "0.0" },
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "janitor");
        assert_eq!(
            init["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            init["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        self.notify("notifications/initialized");
        init["result"].clone()
    }

    /// One `tools/call`, returning the MCP result object (content,
    /// structuredContent, isError).
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        assert!(
            response.get("error").is_none(),
            "tool call is not a protocol error: {response}"
        );
        response["result"].clone()
    }

    /// The janitor.v1 envelope inside a tool result.
    fn envelope(result: &Value) -> &Value {
        result
            .get("structuredContent")
            .expect("structured content carries the envelope")
    }

    fn write_frame(&mut self, line: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.write_all(b"\n"))
            .and_then(|()| self.stdin.flush())
            .expect("write request");
    }

    fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read response");
        assert!(read > 0, "server closed stdout mid-session");
        serde_json::from_str(line.trim_end())
            .unwrap_or_else(|err| panic!("stdout purity: frame is not JSON ({err}): {line:?}"))
    }

    /// Close stdin (the stdio shutdown), wait for exit, and prove the
    /// purity remainder: nothing but already-read frames was ever written.
    fn finish(mut self) -> i32 {
        drop(self.stdin);
        let mut leftover = String::new();
        self.stdout
            .read_to_string(&mut leftover)
            .expect("drain stdout");
        assert!(
            leftover.chars().all(char::is_whitespace),
            "stdout purity: non-frame bytes after the last response: {leftover:?}"
        );
        let status = self.child.wait().expect("server exits");
        let _ = self.child.stderr.take();
        status.code().expect("clean exit code")
    }
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().canonicalize().expect("canonical workspace");
    std::fs::write(ws.join("lib.rs"), LIB).expect("fixture repo");
    (tmp, ws)
}

/// The CLI half of the loop (`init` is CLI-side per the brief).
fn janitor_cli(ws: &Path, args: &[&str]) -> Value {
    let out = Command::new(bin())
        .current_dir(ws)
        .env("XDG_CONFIG_HOME", ws.join("xdg-absent"))
        .args(args)
        .output()
        .expect("cli runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("cli envelope")
}

/// Handshake → tools/list: exactly the six tools, each with a JSON Schema
/// and honest annotations; ping round-trips with an empty result.
#[test]
fn handshake_lists_six_tools_and_pings() {
    let (_tmp, ws) = fixture();
    let mut session = Session::start(&ws);
    session.handshake();

    let pong = session.request("ping", json!({}));
    assert_eq!(pong["result"], json!({}));

    let listed = session.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        [
            "janitor_wake",
            "janitor_scan",
            "janitor_apply",
            "janitor_note",
            "janitor_nap",
            "janitor_status",
        ]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|text| text.contains("SCHEMA.md") || text.contains("DSL.md")),
            "descriptions cite the contract docs"
        );
    }
    let apply = &tools[2];
    assert_eq!(apply["annotations"]["readOnlyHint"], false);
    assert_eq!(apply["annotations"]["destructiveHint"], false);

    assert_eq!(session.finish(), 0);
}

/// Round-trips for the read-only tools over the fixture: wake (empty),
/// scan (one match), scan duplicates, status — every envelope ok, every
/// one also present as text.
#[test]
fn readonly_tools_round_trip() {
    let (_tmp, ws) = fixture();
    janitor_cli(&ws, &["init"]);
    let mut session = Session::start(&ws);
    session.handshake();

    // wake (empty log): awake, nothing to serve.
    let wake = session.call("janitor_wake", json!({}));
    assert_eq!(wake["isError"], false);
    let envelope = Session::envelope(&wake);
    assert_eq!(envelope["schema"], "janitor.v1");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["awake"], true);
    assert_eq!(envelope["data"]["items"].as_array().map(Vec::len), Some(0));
    let text = wake["content"][0]["text"].as_str().expect("text");
    assert_eq!(
        serde_json::from_str::<Value>(text).expect("text is JSON"),
        *envelope
    );

    // scan: the fixture's one function, coordinates that index the bytes.
    let scan = session.call("janitor_scan", json!({ "rule": RULE }));
    assert_eq!(scan["isError"], false);
    let matches = scan["structuredContent"]["data"]["matches"]
        .as_array()
        .expect("matches");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["path"], "lib.rs");
    assert_eq!(matches[0]["kind"], "function_item");

    // duplicates: the tiny fixture has no groups at the default floor,
    // but the mode answers with the canonical report shape.
    let duplicates = session.call("janitor_scan", json!({ "duplicates": true }));
    assert_eq!(duplicates["isError"], false);
    assert_eq!(
        duplicates["structuredContent"]["data"]["kind"],
        "duplicates"
    );

    // status: initialized layer, one workspace, version + grammars.
    let status = session.call("janitor_status", json!({}));
    assert_eq!(status["isError"], false);
    let data = &status["structuredContent"]["data"];
    assert_eq!(data["workspace"], ws.display().to_string());
    assert_eq!(data["memory"]["initialized"], true);
    assert!(
        data["grammars"]
            .as_array()
            .is_some_and(|langs| !langs.is_empty())
    );

    assert_eq!(session.finish(), 0);
}

/// The v0.1 acceptance loop (tests/v01_acceptance.rs) re-run through
/// tools/call — init stays CLI-side. The nap gate arrives as isError with
/// the pending request inside the structured error envelope.
#[test]
fn v01_agent_loop_through_the_mcp_layer() {
    let (_tmp, ws) = fixture();
    janitor_cli(&ws, &["init"]);

    let mut session = Session::start(&ws);
    session.handshake();

    // scan: capture the span that drives apply and the anchored note.
    let scan = session.call("janitor_scan", json!({ "rule": RULE }));
    let captures = scan["structuredContent"]["data"]["matches"][0]["captures"]
        .as_array()
        .expect("captures");
    let (start, end) = (
        captures[0]["start_byte"].as_u64().expect("start"),
        captures[0]["end_byte"].as_u64().expect("end"),
    );
    assert_eq!((end - start) as usize, "alpha".len());

    // apply: same-length rename, flushed through the re-parse gate.
    let apply = session.call(
        "janitor_apply",
        json!({ "diff": { "operations": [ { "path": "lib.rs", "edits": [
            { "start_byte": start, "old_end_byte": end,
              "new_text": "delta", "old_text": "alpha" } ] } ] } }),
    );
    assert_eq!(apply["isError"], false);
    let files = apply["structuredContent"]["data"]["files"]
        .as_array()
        .expect("files");
    assert_eq!(files[0]["status"], "flushed");
    assert_eq!(files[0]["errors_after"], 0);
    assert!(
        std::fs::read_to_string(ws.join("lib.rs")).is_ok_and(|text| text.starts_with("fn delta(")),
        "disk truth through the MCP layer"
    );

    // note: anchor to the scan-stamped span.
    let notation = format!("lib.rs:{start}-{end}:identifier");
    let anchored = session.call(
        "janitor_note",
        json!({ "ptr": notation, "text":
            "delta (was alpha) doubles its incremented input; span stamped from a scan capture" }),
    );
    assert_eq!(anchored["isError"], false);
    let anchored_data = &anchored["structuredContent"]["data"];
    assert_eq!(anchored_data["id"], 0);
    assert_eq!(anchored_data["ptr"]["kind_name"], "identifier");

    // edit above the anchor (outside janitor), then re-anchor by content.
    let lib = ws.join("lib.rs");
    let old = std::fs::read(&lib).expect("lib");
    let mut moved = Vec::with_capacity(old.len() + INSERTED_ABOVE.len());
    moved.extend_from_slice(INSERTED_ABOVE);
    moved.extend_from_slice(&old);
    std::fs::write(&lib, &moved).expect("teammate edit");
    let stored = anchored_data["ptr"].to_string();
    let reanchored = session.call(
        "janitor_note",
        json!({ "ptr": stored, "text":
            "the anchor followed the insert above it; re-anchored by content hash" }),
    );
    assert_eq!(reanchored["isError"], false);
    let reanchored_data = &reanchored["structuredContent"]["data"];
    assert_eq!(reanchored_data["id"], 1);
    assert_eq!(reanchored_data["reanchored"], true);
    assert_eq!(
        reanchored_data["drift"].as_u64(),
        Some(INSERTED_ABOVE.len() as u64)
    );

    // the third fact that makes the oldest pair compressible.
    let record = session.call(
        "janitor_note",
        json!({ "text": "the alpha to delta rename flushed clean through the re-parse gate" }),
    );
    assert_eq!(record["structuredContent"]["data"]["id"], 2);

    // the completeness gate, through MCP: isError + E_NAP_PENDING with the
    // actionable request inside structured content.
    let gate = session.call("janitor_wake", json!({ "budget": BUDGET }));
    assert_eq!(gate["isError"], true);
    let error = &gate["structuredContent"]["error"];
    assert_eq!(error["code"], "E_NAP_PENDING");
    assert!(error["details"]["slot"].as_str().is_some());
    assert!(error["details"]["request"]["slot"].as_str().is_some());

    // the nap loop: request → settle → wake, until wake answers.
    let mut guard = 0;
    let final_wake = loop {
        guard += 1;
        assert!(guard < 32, "the nap loop must converge");
        let nap = session.call("janitor_nap", json!({}));
        assert_eq!(nap["isError"], false);
        if nap["structuredContent"]["data"]["pending"] == false {
            break session.call("janitor_wake", json!({ "budget": BUDGET }));
        }
        let slot = nap["structuredContent"]["data"]["request"]["slot"]
            .as_str()
            .expect("actionable slot")
            .to_owned();
        let settled = session.call(
            "janitor_nap",
            json!({ "slot": slot, "text": format!("{slot}: the early anchored facts, compressed") }),
        );
        assert_eq!(settled["isError"], false, "settle accepted");
        let woke = session.call("janitor_wake", json!({ "budget": BUDGET }));
        if woke["isError"] == false {
            break woke;
        }
    };
    let data = &final_wake["structuredContent"]["data"];
    assert_eq!(data["awake"], true);
    assert_eq!(data["truncated"], false);
    assert!(data["estimate"].as_u64().expect("estimate") <= BUDGET);
    let items = data["items"].as_array().expect("items");
    let summary = items
        .iter()
        .find(|item| item["kind"] == "summary")
        .expect("the oldest pair is compressed");
    assert_eq!(summary["slot"], "0-2");
    assert!(
        items
            .iter()
            .any(|item| item["kind"] == "fact" && item["id"] == 2),
        "the newest fact stays raw"
    );

    let done = session.call("janitor_nap", json!({}));
    assert_eq!(done["structuredContent"]["data"]["pending"], false);

    assert_eq!(session.finish(), 0);
}

/// The typed error paths: a hallucinated span is E_POINTER_DANGLING, a
/// bad rule is E_DSL_*, traversal and absolute paths are the workspace
/// refusal, and a dry-run really leaves disk untouched.
#[test]
fn typed_errors_and_dry_run() {
    let (_tmp, ws) = fixture();
    janitor_cli(&ws, &["init"]);
    let mut session = Session::start(&ws);
    session.handshake();

    // dangling: a span that is not a node boundary is refused, not repaired.
    let dangling = session.call(
        "janitor_note",
        json!({ "ptr": "lib.rs:4-8:identifier", "text": "hallucinated span" }),
    );
    assert_eq!(dangling["isError"], true);
    assert_eq!(
        dangling["structuredContent"]["error"]["code"],
        "E_POINTER_DANGLING"
    );

    // bad rule: the loud E5 gate, as structured content.
    let bad_rule = session.call("janitor_scan", json!({ "rule": "(kind not_a_real_kind)" }));
    assert_eq!(bad_rule["isError"], true);
    let code = bad_rule["structuredContent"]["error"]["code"]
        .as_str()
        .expect("code");
    assert!(code.starts_with("E_DSL_"), "got {code}");

    // traversal and absolute paths: the typed workspace refusal.
    for path in ["../outside", "sub/../../outside", "/etc/passwd"] {
        let escape = session.call("janitor_scan", json!({ "rule": "_", "path": path }));
        assert_eq!(escape["isError"], true, "path {path}");
        assert_eq!(
            escape["structuredContent"]["error"]["code"], "E_VFS_INVALID_PATH",
            "path {path}"
        );
    }

    // an escaping apply payload is refused before anything is staged.
    let escape_apply = session.call(
        "janitor_apply",
        json!({ "diff": { "operations": [ { "path": "../outside.rs", "edits": [
            { "start_byte": 0, "old_end_byte": 0, "new_text": "x" } ] } ] } }),
    );
    assert_eq!(escape_apply["isError"], true);
    assert_eq!(
        escape_apply["structuredContent"]["error"]["code"],
        "E_VFS_INVALID_PATH"
    );

    // dry-run: gated, reported, rolled back — disk untouched.
    let before = std::fs::read(ws.join("lib.rs")).expect("lib");
    let dry = session.call(
        "janitor_apply",
        json!({ "dry_run": true, "diff": { "operations": [ { "path": "lib.rs", "edits": [
            { "start_byte": 3, "old_end_byte": 8, "new_text": "delta", "old_text": "alpha" } ] } ] } }),
    );
    assert_eq!(dry["isError"], false);
    let files = dry["structuredContent"]["data"]["files"]
        .as_array()
        .expect("files");
    assert_eq!(files[0]["status"], "rolled_back");
    assert_eq!(std::fs::read(ws.join("lib.rs")).expect("lib"), before);

    assert_eq!(session.finish(), 0);
}

/// Stdout purity under adversarial logging: with RUST_LOG=debug, every
/// stdout byte across a busy session (handshake, all six tools, errors)
/// is a JSON frame — and the pipe is empty after exit. Stdout discipline
/// is the whole stdio contract.
#[test]
fn stdout_carries_nothing_but_frames_even_with_debug_logging() {
    let (_tmp, ws) = fixture();
    janitor_cli(&ws, &["init"]);
    let mut session = Session::start(&ws);
    session.handshake(); // Session::start already set RUST_LOG=debug
    session.call("janitor_status", json!({}));
    session.call("janitor_wake", json!({}));
    session.call("janitor_scan", json!({ "rule": RULE }));
    session.call("janitor_scan", json!({ "rule": "(" })); // parse error frame
    session.call("janitor_nap", json!({}));
    session.notify("notifications/cancelled");
    session.request("ping", json!({}));
    assert_eq!(session.finish(), 0);
}

/// LSP-style Content-Length framing end to end: the same handshake and a
/// tool call, written and read as framed bodies, mirrored by the server.
#[test]
fn content_length_framed_session_is_mirrored() {
    let (_tmp, ws) = fixture();
    let mut child = Command::new(bin())
        .args(["mcp", "--workspace"])
        .arg(&ws)
        .env("XDG_CONFIG_HOME", ws.join("xdg-absent"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    let write_framed = |stdin: &mut ChildStdin, value: &Value| {
        let body = value.to_string();
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n{}", body.len(), body).as_bytes())
            .and_then(|()| stdin.flush())
            .expect("write framed request");
    };
    let read_framed = |stdout: &mut ChildStdout| -> Value {
        let mut reader = BufReader::new(stdout);
        let mut header = String::new();
        let mut length = 0usize;
        loop {
            header.clear();
            reader.read_line(&mut header).expect("header line");
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((key, value)) = header.split_once(':')
                && key.trim().eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().expect("length");
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("body");
        serde_json::from_slice(&body).expect("framed response is JSON")
    };

    write_framed(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": "initialize",
                 "params": { "protocolVersion": "2025-06-18" } }),
    );
    let init = read_framed(&mut stdout);
    assert_eq!(init["result"]["serverInfo"]["name"], "janitor");

    write_framed(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call",
                 "params": { "name": "janitor_scan",
                             "arguments": { "rule": RULE } } }),
    );
    let scan = read_framed(&mut stdout);
    assert_eq!(scan["result"]["isError"], false);
    assert_eq!(
        scan["result"]["structuredContent"]["data"]["matches"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    drop(stdin);
    let status = child.wait().expect("exit").code().expect("code");
    assert_eq!(status, 0);
}

/// A nonexistent workspace is refused before the session starts: stdout
/// stays empty (no envelope leaks), the failure is the exit code.
#[test]
fn bad_workspace_refuses_without_touching_stdout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(bin())
        .args(["mcp", "--workspace"])
        .arg(tmp.path().join("not-a-dir"))
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "stdout stays frame-pure: no session started"
    );
}

/// B8 (PLAN-v0.3 item 8): tools/list carries an `outputSchema` on every tool,
/// `prompts/list` exposes the guided loops, and `prompts/get` renders one —
/// the MCP surfaces that make janitor discoverable by clients that never read
/// the docs.
#[test]
fn b8_output_schema_and_prompts_round_trip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("lib.rs"), LIB).expect("fixture");
    let mut session = Session::start(tmp.path());
    session.handshake();
    session.notify("notifications/initialized");

    let tools_list = session.request("tools/list", json!({}));
    let tools = tools_list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .clone();
    assert_eq!(tools.len(), 6, "the six janitor tools");
    assert!(
        tools.iter().all(|t| t.get("outputSchema").is_some()),
        "every tool declares its outputSchema: {tools:?}"
    );

    let prompts_list = session.request("prompts/list", json!({}));
    let prompts = prompts_list["result"]["prompts"]
        .as_array()
        .expect("prompts array");
    let names: Vec<&str> = prompts.iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(names.contains(&"janitor_refactor_audit"), "{names:?}");
    assert!(names.contains(&"janitor_memory_recall"), "{names:?}");

    let get = session.request(
        "prompts/get",
        json!({
            "name": "janitor_memory_recall",
            "arguments": { "budget": "2000" }
        }),
    );
    let messages = get["result"]["messages"]
        .as_array()
        .expect("prompt messages");
    assert!(
        !messages.is_empty(),
        "a prompt renders at least one message"
    );
    assert_eq!(messages[0]["role"], "user");

    // Unknown prompt: a typed error, never a panic or a stdout leak.
    let unknown = session.request("prompts/get", json!({ "name": "nope" }));
    assert!(
        unknown.get("error").is_some(),
        "unknown prompt is a typed error"
    );

    session.finish();
}
