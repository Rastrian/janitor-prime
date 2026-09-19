//! `ContextManager` — the global + local layers (PLAN §1.5.2, §3 F6.6).
//!
//! Two read surfaces and one write surface:
//!
//! - **Global layer** — the embedded `share/GLOBAL.md` standing guidance
//!   (v0.1: generic lint rules + AST heuristics over the normalized
//!   FUNCTION/IDENT/LITERAL/BLOCK vocabulary), overridable per user at
//!   `$XDG_CONFIG_HOME/janitor/GLOBAL.md`. The global layer reads **only**
//!   those two places — never the scanned repo, never anything else
//!   (§10 data hygiene).
//! - **Local layer** — always `$PWD/.janitor/`: the fact log, the summary
//!   tree, and the advisory `STATE.json` the engine commands keep their
//!   last-run status in (surfaced by `wake`).
//!
//! `STATE.json` writes are atomic (tmp + rename, ADR-0016 (8)) and
//! advisory: a state write never fails the engine command that produced
//! it, and nothing is written when `.janitor/` does not exist (scan and
//! apply leave no trace in an uninitialized repo).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::error::{JanitorError, MemoryError, VfsError};
use crate::memory::density::{self, Density};
use crate::memory::environment::{self, EnvironmentNotes};
use crate::memory::log::{self, FactLog, FactRecord, LogWarning, MemoryFlag};
use crate::memory::nap::{self, NapPlan};
use crate::memory::pointer::SyntaxPointer;
use crate::memory::tree_store::{SettleOutcome, TREE_DIR, TreeStore};
use crate::memory::wake::{self, WakeReport};
use crate::vfs::Os;

/// The advisory state file's name (ADR-0016 (8): atomic via tmp+rename).
pub const STATE_FILE: &str = "STATE.json";

/// The embedded global-layer defaults (v0.1 ships generic lint rules +
/// normalized-vocabulary AST heuristics; the per-user override lives at
/// `$XDG_CONFIG_HOME/janitor/GLOBAL.md`).
pub const DEFAULT_GLOBAL: &str = include_str!("../../share/GLOBAL.md");

/// Standing hints every wake payload carries (agents free to ignore —
/// PLAN §9 noise list, default: include).
pub const HINTS: &[&str] = &[
    "run wake before every work session; the payload is your memory of this repo",
    "record durable decisions with note; anchor spans come from janitor reports, never memory",
    "facts flagged architectural_lock are load-bearing: never compressed, never dropped",
    "on E_NAP_PENDING, run janitor nap, write the requested line, and wake again — \
     nothing runs in the background",
];

/// The `janitor init` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InitReport {
    pub initialized: bool,
    /// The local layer root, as spelled.
    pub root: String,
    /// Whether an existing layer was reset.
    pub force: bool,
    /// The sharing hint (`.janitor/` is gitignored by default).
    pub hint: String,
}

/// The resolved global layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GlobalLayer {
    /// `"embedded"` or `"xdg_override"`.
    pub source: &'static str,
    /// The standing text in force.
    pub text: String,
    /// Its chars4 estimate — informational; the budget governs the memory
    /// payload, not the standing instructions (§1.5.2).
    pub estimate: u64,
}

/// The full `janitor wake` data: the budgeted recall core plus the
/// standing context around it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WakePayload {
    #[serde(flatten)]
    pub report: WakeReport,
    pub global: GlobalLayer,
    /// STATE.json's contents (or `null` when never written) — the linter /
    /// scan / apply status the orchestrator tracks.
    pub state: Value,
    pub hints: Vec<&'static str>,
    /// The environment notes (v0.3 B2): PATH probes for the language
    /// servers and git around the repo — the evidence the standing
    /// guidance's semantic-layer advice keys on.
    pub environment: EnvironmentNotes,
}

/// How a pointer anchored at note time (echoed in the report).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnchorMeta {
    pub reanchored: bool,
    pub drift: u64,
}

/// The `janitor note` result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NoteReport {
    pub appended: bool,
    /// The position-derived id the fact took (its line index).
    pub id: u64,
    pub text: String,
    /// The pointer as **stored** (re-anchored coordinates if it moved).
    pub ptr: Option<SyntaxPointer>,
    pub reanchored: bool,
    pub drift: u64,
    pub flags: Vec<MemoryFlag>,
    /// The agent-asserted confidence tag (C4) — echoed exactly as
    /// asserted; an append-only key, absent on untagged facts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<log::Confidence>,
    /// The density signal (C2): this fact's windowed novelty against the
    /// 64 facts before it, computed from the log at command time — never
    /// stored (the log stays append-only). Surfaced at write time, the
    /// moment the agent can still act on redundancy.
    pub density: Density,
    /// Log-load warnings (a torn tail may have been quarantined first).
    pub warnings: Vec<LogWarning>,
}

/// The layers' composition root over one local root (`<cwd>/.janitor`).
#[derive(Clone, Debug)]
pub struct ContextManager {
    root: PathBuf,
}

impl ContextManager {
    /// The local layer for a working directory (`<cwd>/.janitor`).
    pub fn discover(cwd: &Path) -> Self {
        Self::new(cwd.join(crate::memory::LOCAL_DIR))
    }

    /// The local layer at an explicit root (the test seam).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The `.janitor/` root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The fact log at this root.
    pub fn log(&self) -> FactLog {
        FactLog::new(&self.root)
    }

    /// The summary tree at this root.
    pub fn store(&self) -> TreeStore {
        TreeStore::new(&self.root)
    }

    /// `janitor init [--force]`: create the `.janitor/` skeleton (empty
    /// LOG.txt, TREE/, initial STATE.json); refuse when it exists unless
    /// `--force` resets it (destructive — the log and tree are deleted).
    pub fn init(&self, force: bool) -> Result<InitReport, JanitorError> {
        if self.root.exists() && !force {
            return Err(MemoryError::AlreadyInitialized {
                root: self.root.display().to_string(),
            }
            .into());
        }
        let io = |err: std::io::Error| -> JanitorError {
            VfsError::Io {
                path: self.root.display().to_string(),
                message: err.to_string(),
            }
            .into()
        };
        if force {
            fs::remove_dir_all(&self.root).map_err(io)?;
        }
        fs::create_dir_all(self.root.join(TREE_DIR)).map_err(io)?;
        fs::write(self.root.join(log::LOG_FILE), b"").map_err(io)?;
        let initial = serde_json::json!({
            "schema": crate::envelope::SCHEMA,
            "updated": crate::memory::timestamp(),
            "last_scan": Value::Null,
            "last_apply": Value::Null,
        });
        let bytes = serde_json::to_vec(&initial).map_err(|err| VfsError::Io {
            path: self.root.display().to_string(),
            message: err.to_string(),
        })?;
        Os.write_atomic(&self.root.join(STATE_FILE), &bytes)
            .map_err(JanitorError::from)?;
        Ok(InitReport {
            initialized: true,
            root: self.root.display().to_string(),
            force,
            hint: ".janitor/ is local and gitignored by default; share it per team only \
                   by opting in with your own .gitignore rules"
                .to_owned(),
        })
    }

    fn ensure_initialized(&self) -> Result<(), JanitorError> {
        if self.root.is_dir() {
            Ok(())
        } else {
            Err(MemoryError::NotInitialized {
                root: self.root.display().to_string(),
            }
            .into())
        }
    }

    /// `janitor wake --budget N [--task QUERY]`: the budgeted recall plus
    /// the global layer, STATE.json status, hints, and the environment
    /// notes. `task` switches the recall core to the ranked pass below
    /// the task ceiling and to the honest positional degradation at or
    /// above it (C1/C1-R); the standing context around it is the same
    /// either way.
    pub fn wake(&self, budget: u64, task: Option<&str>) -> Result<WakePayload, JanitorError> {
        self.ensure_initialized()?;
        let state = self.log().load()?;
        let report = wake::wake(&state.facts, &self.store(), task, budget, state.warnings)?;
        let repo = self.root.parent().unwrap_or_else(|| Path::new("."));
        Ok(WakePayload {
            report,
            global: Self::global_layer(),
            state: self.read_state(),
            hints: HINTS.to_vec(),
            environment: environment::probe(repo),
        })
    }

    /// `janitor note`: validate the line, load (possibly quarantining a
    /// torn tail), append at the next position, fsync. Pointer *resolution*
    /// (against the live tree / git) happens before this, in the session —
    /// this is the storage half; `meta` echoes how the anchor went.
    /// `confidence` (C4) is the agent's assertion, stored verbatim — the
    /// binary never invents or infers a tag.
    pub fn append_note(
        &self,
        ptr: Option<SyntaxPointer>,
        meta: AnchorMeta,
        text: &str,
        lock: bool,
        confidence: Option<log::Confidence>,
    ) -> Result<NoteReport, JanitorError> {
        log::validate_note_text(text)?;
        self.ensure_initialized()?;
        let state = self.log().load()?;
        let flags = if lock {
            vec![MemoryFlag::ArchitecturalLock]
        } else {
            Vec::new()
        };
        let record = FactRecord {
            v: 1,
            id: state.facts.len() as u64,
            ts: crate::memory::timestamp(),
            ptr: ptr.clone(),
            text: text.to_owned(),
            flags: flags.clone(),
            confidence,
        };
        self.log().append(&record)?;
        // C2: novelty against the window that preceded the append —
        // computed before the fact joins the log's history, from the log
        // alone, and carried in the report only (never persisted).
        let window_start = state.facts.len().saturating_sub(density::NOVELTY_WINDOW);
        let density = Density {
            novelty: density::novelty_of_text(text, &state.facts[window_start..]),
            window: density::NOVELTY_WINDOW as u64,
        };
        Ok(NoteReport {
            appended: true,
            id: record.id,
            text: record.text,
            ptr,
            reanchored: meta.reanchored,
            drift: meta.drift,
            flags,
            confidence,
            density,
            warnings: state.warnings,
        })
    }

    /// `janitor nap`: the next pending compression request (or the
    /// settled message).
    pub fn nap_next(&self) -> Result<NapPlan, JanitorError> {
        self.ensure_initialized()?;
        let state = self.log().load()?;
        nap::next_pending(&state.facts, &self.store())
    }

    /// `janitor nap --slot <lo-hi> --text <line>`.
    pub fn nap_settle(&self, slot: &str, text: &str) -> Result<SettleOutcome, JanitorError> {
        self.ensure_initialized()?;
        let state = self.log().load()?;
        nap::settle(&state.facts, &self.store(), slot, text)
    }

    // --- the global layer ----------------------------------------------------

    /// Resolve the global layer: the XDG override when present, else the
    /// embedded defaults. The only two places read.
    pub fn global_layer() -> GlobalLayer {
        match global_override_at(xdg_config_dir().as_deref()) {
            Some(text) => GlobalLayer {
                source: "xdg_override",
                estimate: wake::tokens(text.len()),
                text,
            },
            None => GlobalLayer {
                source: "embedded",
                estimate: wake::tokens(DEFAULT_GLOBAL.len()),
                text: DEFAULT_GLOBAL.to_owned(),
            },
        }
    }

    // --- STATE.json ----------------------------------------------------------

    /// The advisory state, as JSON (`null` shape when absent).
    pub fn read_state(&self) -> Value {
        match fs::read(self.root.join(STATE_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!(error = %err, "STATE.json unreadable; reporting null state");
                Value::Null
            }),
            Err(_) => Value::Null,
        }
    }

    /// Record the last scan into STATE.json (advisory: a failed write is a
    /// stderr warning, never a failed scan; uninitialized ⇒ no write).
    pub fn record_scan(&self, files: u64, matches: u64, skipped: u64) {
        self.record_event(
            "last_scan",
            serde_json::json!({
                "ts": crate::memory::timestamp(),
                "files": files,
                "matches": matches,
                "skipped": skipped,
            }),
        );
    }

    /// Record the last apply into STATE.json (same advisory contract).
    pub fn record_apply(&self, files: u64, edits: u64, dry_run: bool) {
        self.record_event(
            "last_apply",
            serde_json::json!({
                "ts": crate::memory::timestamp(),
                "files": files,
                "edits": edits,
                "dry_run": dry_run,
            }),
        );
    }

    fn record_event(&self, key: &str, event: Value) {
        if !self.root.is_dir() {
            return; // not initialized: engine commands leave no trace
        }
        let mut state = self.read_state();
        if !state.is_object() {
            state = serde_json::json!({ "schema": crate::envelope::SCHEMA });
        }
        state["updated"] = serde_json::json!(crate::memory::timestamp());
        state[key] = event;
        match serde_json::to_vec(&state) {
            Ok(bytes) => {
                if let Err(err) = Os
                    .write_atomic(&self.root.join(STATE_FILE), &bytes)
                    .map_err(JanitorError::from)
                {
                    tracing::warn!(code = err.code(), "STATE.json update failed (advisory)");
                }
            }
            Err(err) => tracing::warn!(error = %err, "STATE.json serialization failed (advisory)"),
        }
    }
}

/// `$XDG_CONFIG_HOME`, or `$HOME/.config` per the basedir convention.
fn xdg_config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

/// The override text under one config dir, when the file exists — the
/// resolution half of [`ContextManager::global_layer`], pure over its
/// argument so the selection is testable without touching the process env.
fn global_override_at(config_dir: Option<&Path>) -> Option<String> {
    let path = config_dir?.join("janitor").join("GLOBAL.md");
    (path.is_file())
        .then(|| fs::read_to_string(path).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_in(tmp: &Path) -> ContextManager {
        ContextManager::discover(tmp)
    }

    /// init creates the skeleton, refuses a second time, and --force
    /// resets it destructively (documented).
    #[test]
    fn init_creates_refuses_and_resets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = manager_in(tmp.path());

        let report = mem.init(false).expect("init");
        assert!(report.initialized && !report.force);
        assert!(
            tmp.path()
                .join(crate::memory::LOCAL_DIR)
                .join("LOG.txt")
                .exists()
        );
        assert!(
            tmp.path()
                .join(crate::memory::LOCAL_DIR)
                .join(TREE_DIR)
                .is_dir()
        );
        let state = fs::read_to_string(tmp.path().join(crate::memory::LOCAL_DIR).join(STATE_FILE))
            .expect("state");
        assert!(state.contains("\"last_scan\":null"));

        let err = mem.init(false).expect_err("second init");
        assert_eq!(err.code(), "E_ALREADY_INITIALIZED");

        // A note survives... and --force erases everything.
        mem.append_note(None, AnchorMeta::default(), "one fact", false, None)
            .expect("note");
        mem.init(true).expect("force");
        let plan = mem.nap_next().expect("empty log after force");
        assert!(!plan.pending, "the reset log has nothing to compress");
    }

    /// Uninitialized commands refuse with E_NOT_INITIALIZED.
    #[test]
    fn uninitialized_commands_refuse() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = manager_in(tmp.path());
        for code in [
            mem.wake(100, None).expect_err("wake").code(),
            mem.append_note(None, AnchorMeta::default(), "x", false, None)
                .expect_err("note")
                .code(),
            mem.nap_next().expect_err("nap").code(),
        ] {
            assert_eq!(code, "E_NOT_INITIALIZED");
        }
    }

    /// The wake payload composes the core report with the global layer,
    /// state, hints, and the environment notes — and STATE.json tracks the
    /// engine's last runs.
    #[test]
    fn wake_payload_carries_layers_state_and_hints() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = manager_in(tmp.path());
        mem.init(false).expect("init");
        mem.append_note(
            None,
            AnchorMeta::default(),
            "the overlay invariants hold",
            false,
            None,
        )
        .expect("note");
        mem.record_scan(7, 3, 1);
        mem.record_apply(2, 5, true);

        let payload = mem.wake(10_000, None).expect("wake");
        assert!(payload.report.awake && !payload.report.truncated);
        assert_eq!(payload.report.items.len(), 1);
        assert_eq!(payload.global.source, "embedded");
        assert!(payload.global.text.contains("standing guidance"));
        assert!(payload.report.estimate > 0);
        assert_eq!(payload.state["last_scan"]["files"], 7);
        assert_eq!(payload.state["last_apply"]["edits"], 5);
        assert_eq!(payload.state["last_apply"]["dry_run"], true);
        assert!(!payload.hints.is_empty());

        // The composed payload serializes flat: wake core + layers.
        let json = serde_json::to_value(&payload).expect("json");
        assert_eq!(json["awake"], true);
        assert_eq!(json["estimator"], "chars4");
        assert_eq!(json["state"]["last_scan"]["matches"], 3);
        assert!(json["global"]["text"].as_str().is_some());

        // The environment block (v0.3 B2): the documented shape, always
        // present, with one row per probed server.
        let servers = json["environment"]["servers"]
            .as_array()
            .expect("environment.servers is an array");
        assert_eq!(servers.len(), environment::PROBED_SERVERS.len());
        for (row, name) in servers.iter().zip(environment::PROBED_SERVERS) {
            assert_eq!(row["name"], *name);
            assert!(row["available"].is_boolean());
        }
        assert!(json["environment"]["git"]["available"].is_boolean());
        assert!(json["environment"]["git"]["dirty"].is_boolean());
        assert_eq!(json["environment"]["janitor"], env!("CARGO_PKG_VERSION"));
    }

    /// The note report: position-derived ids, lock flagging, the stored
    /// pointer echo, and the C4 confidence echo — present only when
    /// asserted.
    #[test]
    fn append_note_assigns_positions_flags_and_confidence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = manager_in(tmp.path());
        mem.init(false).expect("init");

        let first = mem
            .append_note(None, AnchorMeta::default(), "first fact", false, None)
            .expect("note");
        assert_eq!(first.id, 0);
        assert_eq!(first.confidence, None);
        let second = mem
            .append_note(
                Some(SyntaxPointer::Commit {
                    sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                }),
                AnchorMeta::default(),
                "second fact",
                true,
                None,
            )
            .expect("note");
        assert_eq!(second.id, 1);
        assert!(second.flags == vec![MemoryFlag::ArchitecturalLock]);
        assert!(matches!(second.ptr, Some(SyntaxPointer::Commit { .. })));

        // The C4 tag: echoed in the report, persisted in the log.
        let third = mem
            .append_note(
                None,
                AnchorMeta::default(),
                "third fact, imported unverified",
                false,
                Some(log::Confidence::Low),
            )
            .expect("note");
        assert_eq!(third.id, 2);
        assert_eq!(third.confidence, Some(log::Confidence::Low));
        let json = serde_json::to_value(&third).expect("json");
        assert_eq!(json["confidence"], "low");
        let untagged = serde_json::to_value(&first).expect("json");
        assert!(
            untagged.get("confidence").is_none(),
            "append-only: untagged reports carry no key"
        );
        let disk = fs::read_to_string(tmp.path().join(crate::memory::LOCAL_DIR).join("LOG.txt"))
            .expect("log");
        assert!(disk.contains("\"confidence\":\"low\""));

        // Discipline holds through the manager too.
        let err = mem
            .append_note(None, AnchorMeta::default(), "no\nlines", false, None)
            .expect_err("multi-line");
        assert_eq!(err.code(), "E_NOTE_INVALID");
    }

    /// C2: every note report carries the density block — novelty of the
    /// appended fact against its window, in `[0, 1]`, window 64, computed
    /// at command time (the log stores no score; recomputing over the
    /// same log reproduces it).
    #[test]
    fn append_note_reports_windowed_density() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = manager_in(tmp.path());
        mem.init(false).expect("init");

        let first = mem
            .append_note(
                None,
                AnchorMeta::default(),
                "[optmem #1] daemon socket timeout retry attempt failed again later count three",
                false,
                None,
            )
            .expect("note");
        assert_eq!(first.density.window, 64);
        assert_eq!(first.density.novelty, 1.0, "no precedent: fully novel");

        let near = mem
            .append_note(
                None,
                AnchorMeta::default(),
                "[optmem #2] daemon socket timeout retry attempt failed again later count four",
                false,
                None,
            )
            .expect("note");
        assert!(
            (0.0..=1.0).contains(&near.density.novelty),
            "novelty stays in [0, 1]"
        );
        assert!(
            near.density.novelty < 0.2,
            "a near-duplicate reads as redundant: {}",
            near.density.novelty
        );

        // The serialized shape: the append-only density key, floats and
        // window, next to the C4 confidence key's slot.
        let json = serde_json::to_value(&near).expect("json");
        assert_eq!(json["density"]["window"], 64);
        assert!(
            json["density"]["novelty"]
                .as_f64()
                .is_some_and(|n| (0.0..=1.0).contains(&n)),
            "density.novelty serializes as a number in [0, 1]"
        );

        // Never stored: the log line carries the fact only — recomputing
        // from the loaded log reproduces the reported score exactly.
        let state = mem.log().load().expect("load");
        assert!(
            !std::fs::read_to_string(
                tmp.path()
                    .join(crate::memory::LOCAL_DIR)
                    .join(log::LOG_FILE)
            )
            .expect("log")
            .contains("novelty")
        );
        assert_eq!(
            near.density.novelty,
            crate::memory::density::novelty_at(&state.facts, 1).expect("id 1")
        );
    }

    /// The global layer honors the XDG override and only that surface
    /// (the resolution is pure over the config dir, so no env mutation).
    #[test]
    fn global_layer_prefers_the_xdg_override() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = tmp.path().join("config");
        fs::create_dir_all(config.join("janitor")).expect("dirs");

        // No override file: embedded defaults.
        assert_eq!(global_override_at(Some(&config)), None);
        let embedded = ContextManager::global_layer();
        assert_eq!(embedded.source, "embedded");
        assert_eq!(embedded.text, DEFAULT_GLOBAL);
        assert!(embedded.estimate > 0);

        // With the override file: its text, verbatim.
        fs::write(config.join("janitor/GLOBAL.md"), "# team override\n").expect("override");
        assert_eq!(
            global_override_at(Some(&config)),
            Some("# team override\n".to_owned())
        );

        // No config dir at all: still embedded.
        assert_eq!(global_override_at(None), None);
    }
}
