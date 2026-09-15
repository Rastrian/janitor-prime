//! F6 integration suites (PLAN §3 F6.6): the memory protocol exercised
//! end to end through the real binary — the golden agent loop (init →
//! notes with a lock, a node pointer stamped from live scan output, and a
//! commit pointer against a real git fixture → edit the pointed file →
//! re-anchor → budgeted wake with the completeness gate → the nap loop →
//! settled), the torn-tail recovery, and the anti-hallucination refusals
//! (nothing is ever appended for a pointer that will not anchor).

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_janitor")
}

/// One `janitor` invocation in `dir` (env-isolated: the XDG override
/// cannot leak this machine's config into the global-layer assertions).
fn janitor(dir: &Path, args: &[&str]) -> (Option<i32>, Value) {
    let out = Command::new(bin())
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg-absent"))
        .args(args)
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    (
        out.status.code(),
        serde_json::from_str(&stdout).unwrap_or(Value::Null),
    )
}

fn code_of(envelope: &Value) -> &str {
    envelope["error"]["code"].as_str().unwrap_or("<no code>")
}

/// A real git fixture: one commit over a small rust file. Returns its sha.
fn git_fixture(dir: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "--quiet"])
        .status()
        .expect("git init")
        .success()
        .then_some(())
        .expect("git init succeeds");
    for (key, value) in [("user.name", "e2e"), ("user.email", "e2e@example.invalid")] {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", key, value])
            .status()
            .expect("git runs");
        assert!(status.success(), "git config {key}");
    }
    std::fs::write(
        dir.join("lib.rs"),
        "fn alpha(x: u32) -> u32 {\n    let base = x + 1;\n    base * 2\n}\n",
    )
    .expect("write lib.rs");
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["add", "."])
        .status()
        .expect("git add")
        .success()
        .then_some(())
        .expect("git add succeeds");
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["commit", "--quiet", "-m", "fixture"])
        .status()
        .expect("git commit")
        .success()
        .then_some(())
        .expect("git commit succeeds");
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The span of the first `function_item` node, from janitor's own scan —
/// the only sanctioned source of pointer spans.
fn first_function_span(dir: &Path) -> (u64, u64) {
    let (code, envelope) = janitor(dir, &["scan", "--rule", "(kind function_item)"]);
    assert_eq!(code, Some(0), "scan for spans: {envelope}");
    let first = &envelope["data"]["matches"][0];
    (
        first["start_byte"].as_u64().expect("start"),
        first["end_byte"].as_u64().expect("end"),
    )
}

fn log_lines(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(".janitor/LOG.txt"))
        .expect("LOG.txt")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The golden agent loop, exactly the PLAN §3 F6.6 acceptance shape.
#[test]
fn agent_loop_init_note_reanchor_gate_nap_wake() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let sha = git_fixture(dir);

    // init: skeleton + refusal semantics.
    let (code, init) = janitor(dir, &["init"]);
    assert_eq!(code, Some(0));
    assert_eq!(init["data"]["initialized"], true);
    assert!(dir.join(".janitor/LOG.txt").exists());
    assert!(dir.join(".janitor/TREE").is_dir());
    assert!(dir.join(".janitor/STATE.json").exists());
    let (code, refused) = janitor(dir, &["init"]);
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&refused), "E_ALREADY_INITIALIZED");

    // A scan first, so STATE.json carries linter status for wake.
    let (code, _) = janitor(dir, &["scan", "--rule", "(kind function_item)"]);
    assert_eq!(code, Some(0));

    // Notes: a node pointer stamped from live scan output, a commit
    // pointer against the real fixture, and one architectural lock —
    // ordered so the first block is lock-free and compression exists.
    let (start, end) = first_function_span(dir);
    let notation = format!("lib.rs:{start}-{end}:function_item");
    let (code, node_note) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &notation,
            &format!("alpha doubles its incremented input; keep its shape stable ({start})"),
        ],
    );
    assert_eq!(code, Some(0), "{node_note}");
    assert_eq!(node_note["data"]["id"], 0);
    assert_eq!(
        node_note["data"]["ptr"]["span"],
        serde_json::json!([start, end])
    );
    let content_sha = node_note["data"]["ptr"]["content_sha"]
        .as_str()
        .expect("stamped sha")
        .to_owned();

    let (code, commit_note) = janitor(
        dir,
        &[
            "note",
            "--commit",
            &sha[..12], // abbreviated hex: git rev-parse resolves it to full
            "the fixture commit is the baseline the refactor will rewrite",
        ],
    );
    assert_eq!(code, Some(0), "{commit_note}");
    assert_eq!(commit_note["data"]["ptr"]["sha"], serde_json::json!(sha));

    let (code, lock_note) = janitor(
        dir,
        &[
            "note",
            "--lock",
            "overlay invariants are architectural: read_dir never consults the buffer",
        ],
    );
    assert_eq!(code, Some(0), "{lock_note}");
    assert_eq!(
        lock_note["data"]["flags"],
        serde_json::json!(["architectural_lock"])
    );
    assert_eq!(log_lines(dir).len(), 3);

    // Edit the pointed file (bytes inserted above the function) and
    // re-note with the STORED pointer JSON: it re-anchors with drift, and
    // the stored span moves.
    let lib = dir.join("lib.rs");
    let old = std::fs::read(&lib).expect("lib");
    let inserted = b"fn zeta() -> u8 {\n    1\n}\n\n";
    let mut moved = Vec::with_capacity(old.len() + inserted.len());
    moved.extend_from_slice(inserted);
    moved.extend_from_slice(&old);
    std::fs::write(&lib, &moved).expect("write moved");

    let stored = serde_json::json!({
        "path": "lib.rs",
        "span": [start, end],
        "kind_name": "function_item",
        "content_sha": content_sha,
    })
    .to_string();
    let (code, reanchored) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &stored,
            "alpha survived the insert above it; its anchor followed the move",
        ],
    );
    assert_eq!(code, Some(0), "{reanchored}");
    assert_eq!(reanchored["data"]["reanchored"], true);
    let drift = reanchored["data"]["drift"].as_u64().expect("drift");
    assert_eq!(drift, inserted.len() as u64);
    let new_span = reanchored["data"]["ptr"]["span"].clone();
    assert_eq!(new_span[0].as_u64(), Some(start + inserted.len() as u64));

    // Generous wake: everything raw, awake, the global layer, state.
    let (code, full) = janitor(dir, &["wake", "--budget", "100000"]);
    assert_eq!(code, Some(0), "{full}");
    assert_eq!(full["data"]["awake"], true);
    assert_eq!(full["data"]["truncated"], false);
    let items = full["data"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 4); // 3 notes + the re-anchored note
    assert!(items.iter().all(|item| item["kind"] == "fact"));
    assert_eq!(full["data"]["global"]["source"], "embedded");
    assert!(
        full["data"]["global"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("standing guidance"))
    );
    assert_eq!(full["data"]["state"]["last_scan"]["files"], 1);

    // Tiny wake: the completeness gate — refuse with the pending request,
    // never holes. Then the agent loop settles until wake answers.
    let (code, gate) = janitor(dir, &["wake", "--budget", "40"]);
    assert_eq!(code, Some(1), "{gate}");
    assert_eq!(code_of(&gate), "E_NAP_PENDING");
    let needed = gate["error"]["details"]["slot"].as_str().expect("slot");
    let actionable = gate["error"]["details"]["request"]["slot"]
        .as_str()
        .expect("actionable slot");

    let mut guard = 0;
    let mut slot = Some(actionable.to_owned());
    let budgeted = loop {
        guard += 1;
        assert!(guard < 32, "the nap loop must converge");
        let settle_slot = slot.clone().unwrap_or_else(|| {
            let (_, plan) = janitor(dir, &["nap"]);
            plan["data"]["request"]["slot"]
                .as_str()
                .expect("next pending slot")
                .to_owned()
        });
        let text = format!("{settle_slot}: the durable decisions of the early run");
        let (code, settled) = janitor(dir, &["nap", "--slot", &settle_slot, "--text", &text]);
        assert_eq!(code, Some(0), "{settled}");
        let (code, woke) = janitor(dir, &["wake", "--budget", "40"]);
        if code == Some(0) {
            break woke;
        }
        assert_eq!(code_of(&woke), "E_NAP_PENDING", "only the gate may refuse");
        slot = woke["error"]["details"]["request"]["slot"]
            .as_str()
            .map(str::to_owned);
    };

    // The budgeted payload: coarsened oldest, raw newest + lock, budget kept.
    assert_eq!(budgeted["data"]["awake"], true);
    assert_eq!(budgeted["data"]["truncated"], false);
    assert!(
        budgeted["data"]["estimate"].as_u64().expect("estimate") <= 40,
        "{budgeted}"
    );
    let items = budgeted["data"]["items"].as_array().expect("items");
    assert!(
        items.iter().any(|item| item["kind"] == "summary"),
        "the oldest block is compressed: {budgeted}"
    );
    let lock_seen = items.iter().any(|item| {
        item["kind"] == "fact"
            && item["flags"].is_array()
            && item["flags"].to_string().contains("architectural_lock")
    });
    assert!(lock_seen, "the lock is raw and present: {budgeted}");
    for item in items {
        if item["kind"] == "summary" {
            let covers = item["covers"].as_array().expect("covers");
            let (lo, hi) = (
                covers[0].as_u64().expect("lo"),
                covers[1].as_u64().expect("hi"),
            );
            assert!(hi <= 3 || lo >= 3, "no summary covers the locked fact 2");
        }
    }
    let _ = needed;

    // Idempotent resubmission through the binary, and the settled message.
    let first_settled = {
        let (_, plan) = janitor(dir, &["nap"]);
        plan["data"]["request"]["slot"].as_str().map(str::to_owned)
    };
    if let Some(slot) = first_settled {
        let text = format!("{slot}: written twice on purpose");
        let (_, once) = janitor(dir, &["nap", "--slot", &slot, "--text", &text]);
        assert_eq!(once["data"]["idempotent"], false, "{once}");
        let (_, twice) = janitor(dir, &["nap", "--slot", &slot, "--text", &text]);
        assert_eq!(twice["data"]["idempotent"], true, "{twice}");
        let (_, divergent) = janitor(
            dir,
            &[
                "nap",
                "--slot",
                &slot,
                "--text",
                "a different line that must be refused",
            ],
        );
        assert_eq!(code_of(&divergent), "E_SLOT_INVALID");
    }

    // Settle everything the protocol still wants, then the exact message.
    for _ in 0..64 {
        let (_, plan) = janitor(dir, &["nap"]);
        if plan["data"]["pending"] == false {
            assert_eq!(plan["data"]["message"], "Nothing left to compress.");
            return;
        }
        let slot = plan["data"]["request"]["slot"].as_str().expect("slot");
        let text = format!("{slot}: final settlement line");
        let (code, _) = janitor(dir, &["nap", "--slot", slot, "--text", &text]);
        assert_eq!(code, Some(0));
    }
    unreachable!("the protocol must settle within 64 slots");
}

/// The torn tail through the binary: a crash-truncated final line
/// quarantines with a warning, the log stays usable, and the next wake
/// carries the warning forward.
#[test]
fn torn_tail_recovery_through_the_binary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    janitor(dir, &["init"]);
    let (code, _) = janitor(dir, &["note", "first complete fact for the log"]);
    assert_eq!(code, Some(0));

    let log = dir.join(".janitor/LOG.txt");
    let mut torn = std::fs::read(&log).expect("read");
    torn.extend_from_slice(b"{\"v\":1,\"id\":1,\"ts\":\"2026-09"); // crash mid-append
    std::fs::write(&log, &torn).expect("write torn");

    let (code, woke) = janitor(dir, &["wake", "--budget", "1000"]);
    assert_eq!(code, Some(0), "the torn tail never blocks wake: {woke}");
    let warnings = woke["data"]["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["kind"], "torn_tail_quarantined");
    assert!(dir.join(".janitor/QUARANTINE.txt").exists());

    // The recovered log appends with the right position id.
    let (code, note) = janitor(dir, &["note", "appended after quarantine"]);
    assert_eq!(code, Some(0), "{note}");
    assert_eq!(note["data"]["id"], 1);
    let lines = log_lines(dir);
    assert_eq!(lines.len(), 2);
    let (_, clean) = janitor(dir, &["wake", "--budget", "1000"]);
    assert_eq!(clean["data"]["warnings"].as_array().map(Vec::len), Some(0));
}

/// The anti-hallucination gate: a hallucinated span (not a node boundary)
/// and an unresolvable commit are refused with NOTHING appended.
#[test]
fn refusable_pointers_append_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git_fixture(dir);
    janitor(dir, &["init"]);
    let lib = dir.join("lib.rs");
    let len = std::fs::read(&lib).expect("lib").len() as u64;

    // Off-by-one span: not a node boundary.
    let (code, dangling) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            &format!("lib.rs:1-{}:function_item", len),
            "hallucinated span",
        ],
    );
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&dangling), "E_POINTER_DANGLING");

    // 40 hex chars that resolve to no object in this repo.
    let (code, commit) = janitor(
        dir,
        &["note", "--commit", &"0".repeat(40), "hallucinated commit"],
    );
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&commit), "E_COMMIT_INVALID");

    // Malformed notation is a usage-level typed refusal, not dangling.
    let (code, notation) = janitor(
        dir,
        &[
            "note",
            "--ptr",
            "lib.rs:not-a-span:function_item",
            "typo span",
        ],
    );
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&notation), "E_POINTER_INVALID");

    assert!(
        !dir.join(".janitor/LOG.txt").exists()
            || std::fs::read_to_string(dir.join(".janitor/LOG.txt"))
                .is_ok_and(|text| text.trim().is_empty()),
        "refused notes append nothing"
    );

    // Discipline refusals also append nothing: over-length and multi-line.
    let over = "x".repeat(281);
    let (code, toolong) = janitor(dir, &["note", &over]);
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&toolong), "E_NOTE_INVALID");
    let (code, multiline) = janitor(dir, &["note", "two\nlines"]);
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&multiline), "E_NOTE_INVALID");
}

/// The note text and summary discipline rides the binary unchanged; nap
/// out-of-order refusals name the frontier.
#[test]
fn nap_ordering_is_enforced_through_the_binary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    janitor(dir, &["init"]);
    for id in 0..4 {
        let (code, _) = janitor(
            dir,
            &["note", &format!("fact {id}: a decision with weight")],
        );
        assert_eq!(code, Some(0));
    }

    // Rank 1 before rank 0: out of order, refused with the frontier named.
    let (code, out_of_order) = janitor(
        dir,
        &[
            "nap",
            "--slot",
            "2-4",
            "--text",
            "skipping ahead is refused",
        ],
    );
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&out_of_order), "E_SLOT_INVALID");
    assert!(
        out_of_order["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("out of order"))
    );

    // Parent before children: refused with the child named.
    let (code, parent_first) = janitor(
        dir,
        &[
            "nap",
            "--slot",
            "0-4",
            "--text",
            "children first is the rule",
        ],
    );
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&parent_first), "E_SLOT_INVALID");

    // Over-length summary line: refused.
    let (code, overlong) = janitor(dir, &["nap", "--slot", "0-2", "--text", &"y".repeat(257)]);
    assert_eq!(code, Some(1));
    assert_eq!(code_of(&overlong), "E_SLOT_INVALID");

    // The canonical order still works when followed.
    let (code, first) = janitor(
        dir,
        &[
            "nap",
            "--slot",
            "0-2",
            "--text",
            "facts 0-1 summarized in order",
        ],
    );
    assert_eq!(code, Some(0), "{first}");
    assert_eq!(first["data"]["slot"], "0-2");
}

/// `janitor init --force` resets destructively; the memory layer is always
/// `$PWD/.janitor` (never `$HOME`).
#[test]
fn force_resets_and_memory_stays_local() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    janitor(dir, &["init"]);
    janitor(dir, &["note", "will be erased by force"]);
    let (code, forced) = janitor(dir, &["init", "--force"]);
    assert_eq!(code, Some(0), "{forced}");
    assert_eq!(forced["data"]["force"], true);

    let (_, plan) = janitor(dir, &["nap"]);
    assert_eq!(plan["data"]["pending"], false, "the reset log is empty");

    // And a home directory that happens to exist never gets a layer: the
    // local layer answers to $PWD alone (wake works because dir has one).
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let out = Command::new(bin())
        .current_dir(dir)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", dir.join("xdg-absent"))
        .args(["wake", "--budget", "10"])
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(0), "wake reads $PWD/.janitor");
    assert!(
        !home.join(".janitor").exists(),
        "no layer ever lands in $HOME"
    );
}
