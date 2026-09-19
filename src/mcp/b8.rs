// B8 (PLAN-v0.3 item 8): outputSchema per tool + the prompts surface.
// The outputSchemas are the janitor.v1 payload shapes from docs/SCHEMA.md,
// expressed as JSON Schema (loose where payloads vary: additionalProperties
// stays open, the stable keys are pinned). Prompts are two guided loops.

use serde_json::{Value, json};

/// `outputSchema` for tools whose result is the janitor.v1 envelope with a
/// stable `data` shape. Loose-but-honest: the envelope keys are pinned, the
/// data specifics referenced to docs/SCHEMA.md in the description.
fn envelope_schema(data_description: &str) -> Value {
    json!({
        "type": "object",
        "required": ["schema", "ok"],
        "properties": {
            "schema": { "type": "string", "const": "janitor.v1" },
            "ok": { "type": "boolean" },
            "data": { "type": "object", "description": data_description },
            "error": {
                "type": "object",
                "description": "Present iff ok is false. Carries a stable E_* code, message and details (docs/SCHEMA.md error table)."
            }
        },
        "description": format!("janitor.v1 envelope — {data_description} (docs/SCHEMA.md)")
    })
}

/// Attach `outputSchema` to every tool spec (B8): keyed by tool name so the
/// schema matches the payload that tool actually returns.
pub fn attach_output_schemas(tools: &mut [Value]) {
    let schema_for = |name: &str| match name {
        "janitor_wake" => envelope_schema(
            "awake report: items[] (fact|summary, with id/text/flags/ptr), estimate, budget_tokens, truncated, warnings, global guidance, state",
        ),
        "janitor_scan" => envelope_schema(
            "matches[] with path/spans/points/kind/captures, skipped_for_language, skipped[], stats (files/bytes/wall_ms/cache when routed via the daemon) — or duplicates groups in --duplicates mode",
        ),
        "janitor_apply" => envelope_schema(
            "per-file results (flushed | refused with typed coordinates), sha_before/sha_after, edit counts, stats — all-or-nothing per diff",
        ),
        "janitor_note" => envelope_schema(
            "appended note id, text echo, ptr (stamped or re-anchored with drift), flags, confidence (only when asserted)",
        ),
        "janitor_nap" => envelope_schema(
            "next pending compression request (slot, covers, source facts/summaries, instruction) or the settled/nothing-pending report",
        ),
        "janitor_status" => envelope_schema(
            "cheap workspace peek: STATE.json summary (last scan/apply), cache/linter pointers",
        ),
        other => envelope_schema(&format!("janitor.v1 envelope for {other}")),
    };
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(obj) = tool.as_object_mut() {
            obj.insert("outputSchema".into(), schema_for(&name));
        }
    }
}

/// `prompts/list` (B8): two guided loops the client can render for its user.
pub fn prompt_specs() -> Vec<Value> {
    json!({
        "prompts": [
            {
                "name": "janitor_refactor_audit",
                "title": "Guided structural refactor (scan → apply)",
                "description": "Walks the agent through the janitor loop: wake for context, author a DSL rule, dry-run the diff, then apply and record an anchored note.",
                "arguments": [
                    { "name": "rule", "description": "The s-expression rule to scan for (docs/DSL.md)", "required": true },
                    { "name": "intent", "description": "What the refactor should accomplish (fed to the summary note)", "required": false }
                ]
            },
            {
                "name": "janitor_memory_recall",
                "title": "Budgeted project recall",
                "description": "Wakes janitor memory within a token budget and reports the durable facts a session must respect; settles any pending compression via nap.",
                "arguments": [
                    { "name": "budget", "description": "Token budget for the wake payload (default 4000)", "required": false }
                ]
            }
        ]
    })
    .get("prompts")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default()
}

/// `prompts/get`: render one prompt into MCP `messages` (text frames the
/// client turns into the conversation). Shape follows the MCP spec's
/// prompt result: `{ description, messages: [{role:"user", content:{type:
/// "text", text}}] }`.
pub fn prompt_get(name: &str, args: &Value) -> Option<Value> {
    let arg = |key: &str| -> String {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (description, text) = match name {
        "janitor_refactor_audit" => {
            let rule = arg("rule");
            let intent = arg("intent");
            if rule.is_empty() {
                return None;
            }
            (
                "Guided structural refactor through the janitor loop.".to_owned(),
                format!(
                    "Refactor pass using janitor:\n\
                     1. `janitor wake --budget 4000` — load the project memory first.\n\
                     2. `janitor scan --rule '{rule}'` — every match is a refactor site; read the spans, do not re-derive offsets.\n\
                     3. Build ONE whole-diff payload covering every site (all-or-nothing: a bad edit anywhere flushes nothing).\n\
                     4. `janitor apply --diff <payload> --dry-run` — read the report. On E_PARSE_INVALID, fix the flagged span and retry; the disk was never touched.\n\
                     5. Flush for real (drop --dry-run), then `janitor note --ptr <path:start-end:kind> \"<what you did{}>\"` so the next session knows.\n\
                     The syntax gate is janitor's; semantic checks are the project language server's (ADR-0017).",
                    if intent.is_empty() {
                        String::new()
                    } else {
                        format!(" — {intent}")
                    }
                ),
            )
        }
        "janitor_memory_recall" => {
            let budget = {
                let b = arg("budget");
                if b.is_empty() { "4000".to_owned() } else { b }
            };
            (
                "Budgeted project recall with compression settle.".to_owned(),
                format!(
                    "Recall pass using janitor:\n\
                     1. `janitor wake --budget {budget}` — the payload is your project memory (newest raw, oldest summarized).\n\
                     2. If it refuses with E_NAP_PENDING: run `janitor nap`, write the ONE-LINE summary the request asks for (≤256 bytes), submit with `janitor nap --slot <lo-hi> --text \"...\"`, then wake again.\n\
                     3. Facts flagged architectural_lock are load-bearing: carry them forward verbatim.\n\
                     4. Record anything durable you learn with `janitor note` before the session ends."
                ),
            )
        }
        _ => return None,
    };
    Some(json!({
        "description": description,
        "messages": [
            { "role": "user", "content": { "type": "text", "text": text } }
        ]
    }))
}
