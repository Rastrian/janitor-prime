//! MCP server over stdio (F10b, ADR-0019) — janitor as native tools for
//! MCP clients (Claude Code, Cursor, and the reference SDKs).
//!
//! **Thin by design**: the protocol surface is `initialize`,
//! `notifications/initialized`, `ping`, `tools/list`, `tools/call` — small
//! enough that the no-new-deps posture wins over an SDK (ADR-0019). The
//! framing ([`framing`]) is JSON-RPC 2.0 messages, newline-delimited per
//! the MCP stdio transport spec, with LSP-style `Content-Length` frames
//! accepted for clients that speak them (the session's first frame picks
//! the mode; responses mirror it).
//!
//! The six tools (`janitor_wake`, `janitor_scan`, `janitor_apply`,
//! `janitor_note`, `janitor_nap`, `janitor_status`) are **in-process calls
//! into the same `Session` code paths the CLI dispatches** — never a
//! subprocess of this binary — so the payloads are exactly the frozen
//! `janitor.v1` envelope contracts of docs/SCHEMA.md and docs/DSL.md, no
//! new semantics. Every tool result carries the envelope as MCP
//! structured content (`isError: true` + the `E_*` code inside on
//! failure); the `text` content block repeats the same JSON for
//! pre-2025-06-18 clients.
//!
//! **Workspace semantics**: one server per workspace (`janitor mcp
//! [--workspace DIR]`, the client's config starts it). The process
//! switches its working directory to the resolved workspace, every
//! client-supplied path is checked to resolve lexically inside it
//! (traversal and absolute paths are the typed `E_VFS_INVALID_PATH`
//! refusal), and the session is stateless — one fresh `Session` per
//! `tools/call`, exactly like one CLI invocation per command. No socket,
//! no daemon dependency (f10a integration is future work, out of this
//! lane). Stdout carries nothing but MCP frames: janitor's diagnostics
//! are stderr-`tracing` from F0, and the MCP loop never calls the CLI's
//! envelope emitter.
//!
//! Reminders baked into the surface (they are product decisions, not
//! transport details): the **agent** writes every nap summary (ADR-0013 —
//! `janitor_nap` returns the request, the client composes the line), and
//! the semantic layer stays with the project's own language servers
//! (ADR-0017 — janitor gates syntax only).

pub mod b8;
pub mod framing;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::apply::{ApplyParams, DiffPayload};
use crate::cli::ENABLED_GRAMMARS;
use crate::envelope::Envelope;
use crate::error::{JanitorError, OutputError};
use crate::memory::context::ContextManager;
use crate::memory::pointer::{self, CliPointer, SyntaxPointer};
use crate::memory::wake::DEFAULT_BUDGET;
use crate::orchestrator::{NoteParams, NoteTarget, Session};
use crate::scan::{
    DEFAULT_MAX_FILE_SIZE, DEFAULT_MIN_DUPLICATE_SIZE, DuplicatesParams, ScanParams,
};
use crate::vfs::VfsPath;

use framing::{Frame, FrameReader, FramedWriter, Framing};

/// Protocol versions this server speaks (spec revisions whose surface it
/// was verified against, 2026-09-15). The latest is echoed back when the
/// client asks for anything else.
pub const PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// The version answered to clients requesting an unknown/older dialect.
pub const LATEST_PROTOCOL: &str = "2025-06-18";

/// `structuredContent` exists from this revision on; older negotiated
/// dialects get the envelope in the `text` block only.
const STRUCTURED_CONTENT_SINCE: &str = "2025-06-18";

// JSON-RPC 2.0 error codes (spec) + the MCP "not initialized" convention.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const NOT_INITIALIZED: i32 = -32002;

/// The `initialize` result's standing instructions: the nap protocol and
/// the ADR-0017 semantic-layer rule, in the client agent's face from the
/// handshake on.
const INSTRUCTIONS: &str = "janitor refactors this workspace: janitor_scan finds structure, \
janitor_apply edits transactionally (whole-diff, re-parse-gated, rollback-safe — confirm with \
your user per your client's policy). janitor_wake/note/nap are the persistent memory protocol: \
YOU write every nap summary (on E_NAP_PENDING, settle the requested slot via janitor_nap, then \
wake again; nothing runs in the background). For type-level validation, run the project's own \
language server — janitor gates syntax only. All paths are workspace-relative (docs/MCP.md).";

/// Serve one MCP session: read framed JSON-RPC from `input` until EOF,
/// write framed responses to `output`. `workspace` must already be a
/// canonical directory (the CLI resolves and `chdir`s into it before
/// calling this). Read failures end the session as operational errors;
/// EOF is the clean shutdown the stdio spec prescribes.
pub fn serve<R: BufRead, W: Write>(
    input: R,
    output: W,
    workspace: &Path,
) -> Result<(), OutputError> {
    let mut reader = FrameReader::new(input);
    let mut writer = FramedWriter::new(output, Framing::Newline);
    let mut server = Server::new(workspace.to_path_buf());
    loop {
        let frame = match reader.next_frame() {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!(error = %err, "MCP stdin read failed");
                return Err(OutputError::Io(err.to_string()));
            }
        };
        let reply = match frame {
            Frame::Message(text) => server.handle_text(&text),
            // EOF is the client's shutdown signal; a structurally unusable
            // frame gets the JSON-RPC parse error and the session goes on.
            Frame::Eof => break,
            Frame::Invalid(reason) => Some(rpc_error(Value::Null, PARSE_ERROR, reason)),
        };
        if let Some(reply) = reply {
            writer.set_framing(reader.framing().unwrap_or(Framing::Newline));
            let json = serde_json::to_string(&reply)
                .map_err(|err| OutputError::Serialization(err.to_string()))?;
            writer
                .write_message(&json)
                .map_err(|err| OutputError::Io(err.to_string()))?;
        }
    }
    Ok(())
}

/// Per-session MCP state: the workspace every path resolves inside, the
/// lifecycle flag, and the negotiated protocol version.
struct Server {
    workspace: PathBuf,
    initialized: bool,
    negotiated: &'static str,
}

impl Server {
    fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            initialized: false,
            negotiated: LATEST_PROTOCOL,
        }
    }

    /// One raw frame in: parse, dispatch, maybe one response out.
    fn handle_text(&mut self, text: &str) -> Option<Value> {
        let value: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => {
                return Some(rpc_error(
                    Value::Null,
                    PARSE_ERROR,
                    "parse error: frame is not valid JSON",
                ));
            }
        };
        self.handle_value(value)
    }

    fn handle_value(&mut self, value: Value) -> Option<Value> {
        let object = match value {
            // Batching was removed from MCP in 2025-06-18; a single-object
            // stream is the whole contract.
            Value::Array(_) => {
                return Some(rpc_error(
                    Value::Null,
                    INVALID_REQUEST,
                    "invalid request: batched messages are not supported",
                ));
            }
            Value::Object(object) => object,
            _ => {
                return Some(rpc_error(
                    Value::Null,
                    INVALID_REQUEST,
                    "invalid request: expected a JSON-RPC message object",
                ));
            }
        };
        if let Some(version) = object.get("jsonrpc")
            && version != &json!("2.0")
        {
            return Some(rpc_error(
                id_of(&object).unwrap_or(Value::Null),
                INVALID_REQUEST,
                "invalid request: jsonrpc must be \"2.0\"",
            ));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // No method: a response to a request we never sent (this
            // server issues none) — ignore per JSON-RPC.
            return None;
        };
        let Some(id) = id_of(&object) else {
            // Notifications are never answered; the initialized
            // notification completes the handshake, the rest are ignored.
            return None;
        };
        let params = object.get("params");
        match method {
            "initialize" => Some(self.initialize(params, id)),
            "ping" => Some(rpc_result(id, json!({}))),
            "tools/list" if !self.initialized => Some(not_initialized(id)),
            "tools/call" if !self.initialized => Some(not_initialized(id)),
            "tools/list" => {
                let mut tools = tool_specs();
                b8::attach_output_schemas(&mut tools);
                Some(rpc_result(id, json!({ "tools": tools })))
            }
            "tools/call" => Some(self.tools_call(params, id)),
            "prompts/list" if !self.initialized => Some(not_initialized(id)),
            "prompts/list" => Some(rpc_result(id, json!({ "prompts": b8::prompt_specs() }))),
            "prompts/get" if !self.initialized => Some(not_initialized(id)),
            "prompts/get" => Some(self.prompts_get(params, id)),
            _ => Some(rpc_error(
                id,
                METHOD_NOT_FOUND,
                &format!("method not found: {method}"),
            )),
        }
    }

    /// `prompts/get` (B8): render a named prompt with the client's args.
    fn prompts_get(&mut self, params: Option<&Value>, id: Value) -> Value {
        let name = params
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let empty = json!({});
        let args = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);
        match b8::prompt_get(&name, args) {
            Some(prompt) => rpc_result(id, prompt),
            None => rpc_error(id, METHOD_NOT_FOUND, &format!("unknown prompt: {name}")),
        }
    }

    /// The handshake: version negotiation + capability declaration. The
    /// lifecycle flips here; the `initialized` notification that follows
    /// needs no state of its own (the operation phase may begin).
    fn initialize(&mut self, params: Option<&Value>, id: Value) -> Value {
        self.initialized = true;
        // A known requested version is echoed back from the static table
        // (kept 'static so it can live in `negotiated`); anything else
        // gets the server's latest.
        self.negotiated = params
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str)
            .and_then(|version| PROTOCOL_VERSIONS.iter().find(|known| **known == version))
            .copied()
            .unwrap_or(LATEST_PROTOCOL);
        rpc_result(
            id,
            json!({
                "protocolVersion": self.negotiated,
                "capabilities": { "tools": { "listChanged": false }, "prompts": { "listChanged": false } },
                "serverInfo": {
                    "name": "janitor",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": INSTRUCTIONS,
            }),
        )
    }

    fn tools_call(&mut self, params: Option<&Value>, id: Value) -> Value {
        let Some(params) = params.filter(|params| params.is_object()) else {
            return rpc_error(
                id,
                INVALID_PARAMS,
                "invalid params: tools/call needs {name, arguments}",
            );
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return rpc_error(
                id,
                INVALID_PARAMS,
                "invalid params: params.name must be a string",
            );
        };
        let arguments = params
            .get("arguments")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        let Some(arguments) = arguments.as_object() else {
            return rpc_error(
                id,
                INVALID_PARAMS,
                "invalid params: arguments must be an object",
            );
        };
        let arguments = Value::Object(arguments.clone());
        let envelope = match self.call_tool(name, &arguments) {
            Ok(envelope) => envelope,
            Err(message) => return rpc_error(id, INVALID_PARAMS, &message),
        };
        // The janitor.v1 envelope IS the tool result: structured content
        // for current clients, the same JSON as text for older ones.
        let is_error = !envelope.ok;
        let envelope_value = serde_json::to_value(&envelope).unwrap_or(Value::Null);
        let envelope_text = serde_json::to_string(&envelope_value).unwrap_or_default();
        let mut result = json!({
            "content": [ { "type": "text", "text": envelope_text } ],
            "isError": is_error,
        });
        if self.structured_content() {
            result["structuredContent"] = envelope_value;
        }
        rpc_result(id, result)
    }

    fn structured_content(&self) -> bool {
        self.negotiated == STRUCTURED_CONTENT_SINCE
    }

    /// Dispatch one tool. `Err` carries the JSON-RPC `-32602` message for
    /// schema-shaped argument violations; everything else — including
    /// every typed `E_*` operational refusal — is the `Ok` envelope with
    /// `ok: false`, surfaced to the agent as `isError: true`.
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Envelope<Value>, String> {
        match name {
            "janitor_wake" => self.tool_wake(args),
            "janitor_scan" => self.tool_scan(args),
            "janitor_apply" => self.tool_apply(args),
            "janitor_note" => self.tool_note(args),
            "janitor_nap" => self.tool_nap(args),
            "janitor_status" => self.tool_status(),
            _ => Err(format!("Unknown tool: {name}")),
        }
    }

    /// `janitor_wake {budget?, task?}` — the budgeted memory payload
    /// (ranked against `task` when given, C1), or the `E_NAP_PENDING`
    /// refusal whose details carry the actionable request.
    fn tool_wake(&self, args: &Value) -> Result<Envelope<Value>, String> {
        let budget = opt_u64(args, "budget")?.unwrap_or(DEFAULT_BUDGET);
        let task = opt_str(args, "task")?.map(str::to_owned);
        let session = Session::new();
        Ok(match session.wake(budget, task.as_deref()) {
            Ok(payload) => ok_envelope(&payload),
            Err(err) => err.into(),
        })
    }

    /// `janitor_scan` — rule mode or duplicates mode, mirroring the CLI's
    /// mode group exactly (docs/DSL.md §8, docs/SCHEMA.md).
    fn tool_scan(&self, args: &Value) -> Result<Envelope<Value>, String> {
        let rule = opt_str(args, "rule")?.map(str::to_owned);
        let duplicates = opt_bool(args, "duplicates")?.unwrap_or(false);
        let lang = opt_str(args, "lang")?.map(str::to_owned);
        let include_virtual = opt_bool(args, "include_virtual")?.unwrap_or(false);
        let strict_trivia = opt_bool(args, "strict_trivia")?.unwrap_or(false);
        let max_file_size = opt_u64(args, "max_file_size")?.unwrap_or(DEFAULT_MAX_FILE_SIZE);
        let min_size = opt_u32(args, "min_size")?.unwrap_or(DEFAULT_MIN_DUPLICATE_SIZE);
        let min_members = opt_u32(args, "min_members")?;
        let summary = opt_bool(args, "summary")?.unwrap_or(false);
        let root = match opt_str(args, "path")? {
            Some(spelled) => match self.resolve_inside(spelled) {
                Ok(root) => root,
                Err(refusal) => {
                    return Ok(refusal.envelope(&self.workspace, spelled));
                }
            },
            None => self.workspace.clone(),
        };
        if duplicates && rule.is_some() {
            return Ok(Envelope::error(
                "E_USAGE",
                "scan takes exactly one mode: rule (docs/DSL.md) or duplicates \
                 (docs/SCHEMA.md), never both",
                None,
            ));
        }
        let session = Session::new();
        Ok(match (duplicates, rule) {
            (true, _) => {
                let params = DuplicatesParams {
                    root,
                    lang_filter: lang,
                    include_virtual,
                    max_file_size,
                    min_size,
                    min_members,
                    summary,
                    retain_cache: false, // MCP is stateless: no warm cache to feed
                };
                match session.duplicates(&params) {
                    Ok(report) => ok_envelope(&report),
                    Err(err) => err.into(),
                }
            }
            (false, Some(rule)) => {
                let params = ScanParams {
                    rule,
                    root,
                    lang_filter: lang,
                    include_virtual,
                    strict_trivia,
                    max_file_size,
                    retain_cache: false, // MCP is stateless: no warm cache to feed
                };
                match session.scan(&params) {
                    Ok(report) => ok_envelope(&report),
                    Err(err) => err.into(),
                }
            }
            (false, None) => Envelope::error(
                "E_USAGE",
                "scan needs exactly one mode: rule \"<rule>\" (docs/DSL.md) or duplicates \
                 (docs/SCHEMA.md)",
                None,
            ),
        })
    }

    /// `janitor_apply {diff, dry_run?}` — the ADR-0014 payload as an
    /// object (docs/SCHEMA.md). Every operation path is containment-
    /// checked before the transaction starts.
    fn tool_apply(&self, args: &Value) -> Result<Envelope<Value>, String> {
        let diff = req_object(args, "diff")?;
        let dry_run = opt_bool(args, "dry_run")?.unwrap_or(false);
        // Re-serialize through the same typed parser the CLI uses, so the
        // validation contract is byte-for-byte identical.
        let text = serde_json::to_string(diff)
            .map_err(|err| format!("arguments.diff is not serializable: {err}"))?;
        let payload = match DiffPayload::from_json_str(&text) {
            Ok(payload) => payload,
            Err(err) => return Ok(err.into()),
        };
        for operation in &payload.operations {
            if let Err(refusal) = self.resolve_inside(&operation.path) {
                return Ok(refusal.envelope(&self.workspace, &operation.path));
            }
        }
        let session = Session::new();
        Ok(match session.apply(&ApplyParams { payload, dry_run }) {
            Ok(report) => ok_envelope(&report),
            Err(err) => err.into(),
        })
    }

    /// `janitor_note {text, ptr?, commit?, lock?, confidence?}` — the
    /// anchored append. The ptr's path is containment-checked before the
    /// anchor resolves. `confidence` takes exactly `"low"` (C4).
    fn tool_note(&self, args: &Value) -> Result<Envelope<Value>, String> {
        let text = req_str(args, "text")?.to_owned();
        let ptr = opt_str(args, "ptr")?;
        let commit = opt_str(args, "commit")?;
        let lock = opt_bool(args, "lock")?.unwrap_or(false);
        let confidence = match opt_str(args, "confidence")? {
            None => None,
            Some("low") => Some(crate::memory::log::Confidence::Low),
            Some(other) => {
                return Ok(Envelope::error(
                    "E_USAGE",
                    "confidence takes exactly \"low\" — the one asserted level",
                    Some(json!({ "confidence": other })),
                ));
            }
        };
        if ptr.is_some() && commit.is_some() {
            return Ok(Envelope::error(
                "E_USAGE",
                "note takes exactly one anchor: ptr or commit, not both",
                None,
            ));
        }
        if let Some(spelled) = ptr {
            match pointer::parse_cli_pointer(spelled) {
                Ok(CliPointer::Fresh { path, .. })
                | Ok(CliPointer::Stored(SyntaxPointer::Node { path, .. })) => {
                    if let Err(refusal) = self.resolve_inside(&path) {
                        return Ok(refusal.envelope(&self.workspace, &path));
                    }
                }
                Ok(CliPointer::Stored(SyntaxPointer::Commit { .. })) => {}
                Err(err) => return Ok(JanitorError::from(err).into()),
            }
        }
        let target = ptr
            .map(|spelled| NoteTarget::Ptr(spelled.to_owned()))
            .or_else(|| commit.map(|sha| NoteTarget::Commit(sha.to_owned())));
        let session = Session::new();
        Ok(
            match session.note(&NoteParams {
                target,
                text,
                lock,
                confidence,
            }) {
                Ok(report) => ok_envelope(&report),
                Err(err) => err.into(),
            },
        )
    }

    /// `janitor_nap {slot?, text?}` — bare returns the next pending
    /// request; slot + text settle the agent-written line (ADR-0013).
    fn tool_nap(&self, args: &Value) -> Result<Envelope<Value>, String> {
        let slot = opt_str(args, "slot")?;
        let text = opt_str(args, "text")?;
        let session = Session::new();
        Ok(match (slot, text) {
            (None, None) => match session.nap_next() {
                Ok(plan) => ok_envelope(&plan),
                Err(err) => err.into(),
            },
            (Some(slot), Some(text)) => match session.nap_settle(slot, text) {
                Ok(outcome) => ok_envelope(&outcome),
                Err(err) => err.into(),
            },
            // The CLI enforces the same pair rule at clap level (E_USAGE).
            (slot, text) => Envelope::error(
                "E_USAGE",
                "slot and text travel together or not at all",
                Some(json!({ "slot": slot, "text": text })),
            ),
        })
    }

    /// `janitor_status` — the cheap peek: version, grammars, memory layer
    /// summary (initialized, fact count, pending nap, STATE.json), global
    /// layer source. Never a full wake.
    fn tool_status(&self) -> Result<Envelope<Value>, String> {
        let mem = ContextManager::discover(&self.workspace);
        let initialized = mem.root().is_dir();
        let mut facts = Value::Null;
        let mut nap_pending = Value::Null;
        if initialized {
            if let Ok(state) = mem.log().load() {
                facts = json!(state.facts.len() as u64);
            }
            if let Ok(plan) = mem.nap_next() {
                nap_pending = json!(plan.pending);
            }
        }
        let global = ContextManager::global_layer();
        let mut memory = json!({
            "initialized": initialized,
            "root": mem.root().display().to_string(),
            "facts": facts,
            "nap_pending": nap_pending,
            "state": mem.read_state(),
        });
        if !initialized {
            memory["hint"] = json!("memory layer absent; run `janitor init` (CLI) to create it");
        }
        Ok(Envelope::ok(json!({
            "workspace": self.workspace.display().to_string(),
            "version": env!("CARGO_PKG_VERSION"),
            "grammars": ENABLED_GRAMMARS,
            "memory": memory,
            "global": { "source": global.source, "estimate": global.estimate },
        })))
    }

    /// Resolve a client-spelled path inside the workspace. Absolute paths
    /// and lexical escapes are the typed `E_VFS_INVALID_PATH` refusal
    /// (symlinks stay unresolved — the same lexical posture as the
    /// overlay's `VfsPath`, ADR-0016).
    fn resolve_inside(&self, spelled: &str) -> Result<PathBuf, PathRefusal> {
        let raw = Path::new(spelled);
        if raw.as_os_str().is_empty() {
            return Err(PathRefusal::new("the path is empty"));
        }
        if spelled.as_bytes().contains(&0) {
            return Err(PathRefusal::new("the path contains a NUL byte"));
        }
        if raw.is_absolute() {
            return Err(PathRefusal::new(
                "absolute paths are refused; spell paths workspace-relative",
            ));
        }
        let normalized = match VfsPath::new(self.workspace.join(raw)) {
            Ok(normalized) => normalized,
            Err(_) => return Err(PathRefusal::new("the path is not lexically normalizable")),
        };
        if !normalized.as_path().starts_with(&self.workspace) {
            return Err(PathRefusal::new("the path escapes the workspace"));
        }
        Ok(normalized.into_path_buf())
    }
}

/// The workspace-containment refusal: small and typed so it can travel in
/// a `Result` (the envelope it becomes is too large for the `Err` slot).
struct PathRefusal {
    message: String,
}

impl PathRefusal {
    fn new(message: &str) -> Self {
        Self {
            message: message.to_owned(),
        }
    }

    /// The typed refusal envelope: stable `E_VFS_INVALID_PATH` code, the
    /// reason, and the offending path + workspace as details.
    fn envelope(&self, workspace: &Path, spelled: &str) -> Envelope<Value> {
        Envelope::error(
            "E_VFS_INVALID_PATH",
            format!("MCP paths resolve inside the workspace: {}", self.message),
            Some(json!({
                "path": spelled,
                "workspace": workspace.display().to_string(),
            })),
        )
    }
}

/// The request id when present and non-null (a valid string/number);
/// `None` for notifications. Anything else is an invalid request.
fn id_of(object: &serde_json::Map<String, Value>) -> Option<Value> {
    let id = object.get("id")?;
    match id {
        Value::Null => None,
        Value::String(_) | Value::Number(_) => Some(id.clone()),
        _ => Some(Value::Null),
    }
}

fn not_initialized(id: Value) -> Value {
    rpc_error(
        id,
        NOT_INITIALIZED,
        "server not initialized: send an initialize request first",
    )
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Serialize a report into the success envelope; the only failure is the
/// `E_OUTPUT` envelope itself (never a panic, never a partial payload).
fn ok_envelope<T: Serialize>(data: &T) -> Envelope<Value> {
    match serde_json::to_value(data) {
        Ok(value) => Envelope::ok(value),
        Err(err) => JanitorError::from(OutputError::Serialization(err.to_string())).into(),
    }
}

// --- argument extraction (schema-shaped violations → -32602) ----------------

fn opt_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(format!("arguments.{key} must be a string")),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    opt_str(args, key)?.ok_or_else(|| format!("arguments.{key} is required (a string)"))
}

fn req_object<'a>(args: &'a Value, key: &str) -> Result<&'a Value, String> {
    match args.get(key) {
        Some(value @ Value::Object(_)) => Ok(value),
        None | Some(Value::Null) => Err(format!("arguments.{key} is required (an object)")),
        Some(_) => Err(format!("arguments.{key} must be an object")),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(format!("arguments.{key} must be a boolean")),
    }
}

fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("arguments.{key} must be a non-negative integer")),
        Some(_) => Err(format!("arguments.{key} must be an integer")),
    }
}

fn opt_u32(args: &Value, key: &str) -> Result<Option<u32>, String> {
    match opt_u64(args, key)? {
        None => Ok(None),
        Some(value) if value <= u32::MAX as u64 => Ok(Some(value as u32)),
        Some(_) => Err(format!("arguments.{key} must fit in a u32")),
    }
}

// --- the tools/list surface -------------------------------------------------
//
// The input schemas ARE the docs/SCHEMA.md + docs/DSL.md argument
// contracts; descriptions cite the owning document per the F10b brief.

fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "janitor_wake",
            "title": "janitor wake",
            "description": "Recall janitor memory under a token budget: raw facts and \
        summaries, oldest-first, plus standing guidance and last-run state (docs/SCHEMA.md, wake). \
        With task: a ranked pass below the 350-token ceiling — the best-scoring facts are served \
        raw first (scorer tfidf-store); at or above it the positional payload serves and the \
        task block reports the degradation (C1-R). On E_NAP_PENDING the result carries the \
        pending compression request — act on it via janitor_nap, then wake again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "budget": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Token budget (chars4 estimator); default 8192."
                    },
                    "task": {
                        "type": "string",
                        "description": "Query to rank the recall by (store-local TF-IDF, \
                    scorer tfidf-store); omit for the position-only tiling."
                    }
                }
            },
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "janitor_scan",
            "title": "janitor scan",
            "description": "Parallel structural search (docs/DSL.md): one DSL rule in \
        (s-expression form), match coordinates out; or duplicates mode: fingerprint-grouped \
        duplicate subtrees at or above min_size (docs/SCHEMA.md). Paths are workspace-relative.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rule": {
                        "type": "string",
                        "description": "The DSL rule (docs/DSL.md). Scan mode unless \
        duplicates is true; mutually exclusive with it."
                    },
                    "duplicates": {
                        "type": "boolean",
                        "description": "Duplicate-detection mode (docs/SCHEMA.md); \
        mutually exclusive with rule."
                    },
                    "path": {
                        "type": "string",
                        "description": "Scan root, workspace-relative (default \".\"); \
        must resolve inside the workspace."
                    },
                    "lang": {
                        "type": "string",
                        "description": "Restrict to one canonical language name \
        (e.g. rust); a grammar this build does not embed is a typed error."
                    },
                    "include_virtual": {
                        "type": "boolean",
                        "description": "Include overlay-staged virtual files (default \
        false: scan reports disk reality, ADR-0016)."
                    },
                    "strict_trivia": {
                        "type": "boolean",
                        "description": "Byte-strict Seq adjacency (default: \
        comment-skipping, ADR-0010). Rule mode only."
                    },
                    "max_file_size": {
                        "type": "integer",
                        "description": "Skip files larger than this many bytes \
        (default 4194304)."
                    },
                    "min_size": {
                        "type": "integer",
                        "description": "Duplicates mode: minimum subtree size in \
        participating nodes (default 8)."
                    },
                    "min_members": {
                        "type": "integer",
                        "description": "Duplicates mode: only groups with at least N \
        members survive (default 2)."
                    },
                    "summary": {
                        "type": "boolean",
                        "description": "Duplicates mode: prepend the per-kind aggregate \
        block — group/member counts per (language, kind) plus totals (default false)."
                    }
                }
            },
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": "janitor_apply",
            "title": "janitor apply",
            "description": "Transactional rewrite (docs/SCHEMA.md, ADR-0014): whole-diff \
        all-or-nothing — every file is staged, re-parse-gated, then flushed together; any failure \
        rolls everything back. Staged and rollback-safe, not destructive — confirm with your user \
        per your client's policy before a non-dry-run call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "diff": {
                        "type": "object",
                        "description": "The edit payload (docs/SCHEMA.md): \
        {operations: [{path, expect_sha?, edits: [{start_byte, old_end_byte, new_text, \
        old_text?}]}]}. Paths are workspace-relative."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Validate + gate + report, then roll back: \
        nothing is flushed (default false)."
                    }
                },
                "required": ["diff"]
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false
            }
        }),
        json!({
            "name": "janitor_note",
            "title": "janitor note",
            "description": "Append one validated fact to memory (docs/SCHEMA.md, note): \
        one line, at most 280 bytes. Optional anchor: ptr (`path:start-end:kind` from janitor's \
        own reports, or a stored pointer's JSON) or commit sha. A hallucinated span is refused \
        (E_POINTER_DANGLING), never repaired.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The fact: one line, at most 280 bytes."
                    },
                    "ptr": {
                        "type": "string",
                        "description": "Node anchor: `path:start-end:kind` (byte offsets \
        from janitor reports) or a stored pointer's JSON (re-anchorable)."
                    },
                    "commit": {
                        "type": "string",
                        "description": "Commit anchor: a hex object name resolved via \
        git rev-parse."
                    },
                    "lock": {
                        "type": "boolean",
                        "description": "Mark the fact architectural_lock: never \
        compressed, never dropped."
                    },
                    "confidence": {
                        "type": "string",
                        "enum": ["low"],
                        "description": "Tag the fact low confidence — your assertion, \
        never the binary's: ranked recall tie-breaks against it and drops it first under \
        budget pressure; positional recall seats it one split level coarser. It still serves \
        whenever the budget allows."
                    }
                },
                "required": ["text"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        }),
        json!({
            "name": "janitor_nap",
            "title": "janitor nap",
            "description": "The synchronous compression protocol (docs/SCHEMA.md, nap; \
        ADR-0013): no arguments returns the next pending request; slot + text stores the \
        one-line summary (at most 256 bytes) that YOU write — janitor plans and validates, \
        the agent summarizes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": {
                        "type": "string",
                        "description": "The slot to settle (`<lo>-<hi>`, from a pending \
        request); travels with text or not at all."
                    },
                    "text": {
                        "type": "string",
                        "description": "The one-line summary you wrote (at most 256 \
        bytes)."
                    }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        }),
        json!({
            "name": "janitor_status",
            "title": "janitor status",
            "description": "Cheap workspace peek without a full wake (the wake/status \
        surfaces of docs/SCHEMA.md): version, embedded grammars, memory layer summary \
        (initialized, fact count, pending nap, STATE.json last-run status), and the global \
        layer source.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::SCHEMA;

    /// Drive one session in-process: frames in (newline-delimited), frames
    /// out, nothing but frames.
    fn session(workspace: &Path, input: &str) -> Vec<Value> {
        let mut output = Vec::new();
        serve(
            std::io::BufReader::new(input.as_bytes()),
            &mut output,
            workspace,
        )
        .expect("session serves");
        String::from_utf8(output)
            .expect("frames are UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each frame is one JSON object"))
            .collect()
    }

    fn tmp_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn alpha(x: u32) -> u32 {\n    let base = x + 1;\n    base * 2\n}\n",
        )
        .expect("fixture");
        tmp
    }

    fn request(id: i32, method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
    }

    /// initialize → tools/list: the handshake answers with the negotiated
    /// version, the tools capability, and exactly the six janitor tools
    /// with schemas.
    #[test]
    fn handshake_lists_the_six_tools() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n",
                request(
                    1,
                    "initialize",
                    json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                            "clientInfo": { "name": "test", "version": "0" } })
                ),
                request(2, "tools/list", json!({})),
            ),
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(frames[0]["result"]["serverInfo"]["name"], "janitor");
        assert_eq!(
            frames[0]["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            frames[0]["result"]["capabilities"]["tools"]["listChanged"],
            false
        );

        let tools = frames[1]["result"]["tools"].as_array().expect("tools");
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
                "janitor_status"
            ]
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "schema per tool");
        }
        let apply = &tools[2];
        assert_eq!(apply["annotations"]["readOnlyHint"], false);
        assert_eq!(apply["annotations"]["destructiveHint"], false);
    }

    /// Version negotiation: an unknown requested version gets the server's
    /// latest; a known one is echoed; an older one drops structuredContent
    /// (the envelope stays in the text block).
    #[test]
    fn version_negotiation_echoes_known_and_defaults() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n",
                request(1, "initialize", json!({ "protocolVersion": "1999-01-01" })),
                request(2, "ping", json!({})),
            ),
        );
        assert_eq!(frames[0]["result"]["protocolVersion"], LATEST_PROTOCOL);
        assert_eq!(frames[1]["result"], json!({}));

        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n",
                request(1, "initialize", json!({ "protocolVersion": "2025-03-26" })),
                request(
                    2,
                    "tools/call",
                    json!({ "name": "janitor_status", "arguments": {} })
                ),
            ),
        );
        assert_eq!(frames[0]["result"]["protocolVersion"], "2025-03-26");
        assert!(
            frames[1]["result"].get("structuredContent").is_none(),
            "older dialects get text-only results"
        );
        assert!(frames[1]["result"]["isError"] == false);
    }

    /// Requests before initialize are refused; notifications (initialized,
    /// unknown) never produce responses; ping works pre-initialize.
    #[test]
    fn lifecycle_guards_and_notifications() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n{}\n{}\n{}\n",
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
                        "params": { "requestId": 9 } }),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
                json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }),
            ),
        );
        assert_eq!(frames.len(), 3, "notifications are never answered");
        assert_eq!(frames[0]["error"]["code"], NOT_INITIALIZED);
        assert_eq!(frames[1]["result"], json!({}));
        assert_eq!(frames[2]["error"]["code"], METHOD_NOT_FOUND);
    }

    /// tools/call round-trips: a scan over the fixture, structured content
    /// carrying the janitor.v1 envelope verbatim.
    #[test]
    fn tools_call_scan_returns_the_envelope_as_structured_content() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n",
                request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
                request(
                    2,
                    "tools/call",
                    json!({ "name": "janitor_scan", "arguments": {
                        "rule": "(under function_item (seq (capture @fn (kind identifier)) _))" } })
                ),
            ),
        );
        let result = &frames[1]["result"];
        assert_eq!(result["isError"], false);
        let envelope = &result["structuredContent"];
        assert_eq!(envelope["schema"], SCHEMA);
        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["matches"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(envelope["data"]["matches"][0]["path"], "lib.rs");
        // The text block repeats the same JSON for older clients.
        let text = result["content"][0]["text"].as_str().expect("text");
        let reparsed: Value = serde_json::from_str(text).expect("text is the envelope JSON");
        assert_eq!(reparsed, *envelope);
    }

    /// A bad rule is the typed E_DSL_* refusal inside structured content
    /// with isError — not a JSON-RPC error.
    #[test]
    fn bad_rule_is_a_typed_envelope_not_a_protocol_error() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n",
                request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
                request(
                    2,
                    "tools/call",
                    json!({ "name": "janitor_scan", "arguments": {
                    "rule": "(kind not_a_real_kind)" } })
                ),
            ),
        );
        let result = &frames[1]["result"];
        assert_eq!(result["isError"], true);
        let code = result["structuredContent"]["error"]["code"]
            .as_str()
            .expect("E_* code");
        assert!(code.starts_with("E_DSL_"), "got {code}");
    }

    /// Traversal and absolute paths are the typed workspace refusal.
    #[test]
    fn path_containment_is_enforced() {
        let tmp = tmp_workspace();
        for path in ["../outside", "a/../../outside", "/etc/passwd"] {
            let frames = session(
                tmp.path(),
                &format!(
                    "{}\n{}\n",
                    request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
                    request(
                        2,
                        "tools/call",
                        json!({ "name": "janitor_scan", "arguments": {
                        "rule": "_", "path": path } })
                    ),
                ),
            );
            let result = &frames[1]["result"];
            assert_eq!(result["isError"], true, "path {path}");
            assert_eq!(
                result["structuredContent"]["error"]["code"],
                "E_VFS_INVALID_PATH"
            );
        }
    }

    /// Unknown tools and malformed arguments are protocol errors (-32602).
    #[test]
    fn unknown_tool_and_bad_arguments_are_protocol_errors() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n{}\n",
                request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
                request(
                    2,
                    "tools/call",
                    json!({ "name": "janitor_nope", "arguments": {} })
                ),
                request(
                    3,
                    "tools/call",
                    json!({ "name": "janitor_wake", "arguments": {
                    "budget": "eight" } })
                ),
            ),
        );
        assert_eq!(frames[1]["error"]["code"], INVALID_PARAMS);
        assert_eq!(frames[2]["error"]["code"], INVALID_PARAMS);
    }

    /// A parse-failed frame is answered with -32700 and the session lives on.
    #[test]
    fn garbage_frame_is_answered_and_survives() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "NOT-JSON\n{}\n",
                request(1, "initialize", json!({ "protocolVersion": "2025-06-18" }))
            ),
        );
        assert_eq!(frames[0]["id"], Value::Null);
        assert_eq!(frames[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(frames[1]["result"]["protocolVersion"], "2025-06-18");
    }

    /// janitor_status never requires initialization and reports the layer.
    #[test]
    fn status_is_the_cheap_peek() {
        let tmp = tmp_workspace();
        let frames = session(
            tmp.path(),
            &format!(
                "{}\n{}\n",
                request(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
                request(
                    2,
                    "tools/call",
                    json!({ "name": "janitor_status", "arguments": {} })
                ),
            ),
        );
        let envelope = &frames[1]["result"]["structuredContent"];
        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["workspace"],
            tmp.path()
                .canonicalize()
                .expect("canonical")
                .display()
                .to_string()
        );
        assert_eq!(envelope["data"]["memory"]["initialized"], false);
        assert!(envelope["data"]["memory"]["hint"].as_str().is_some());
    }
}
