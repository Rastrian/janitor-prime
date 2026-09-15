//! The append-only fact log — `.janitor/LOG.txt` (ADR-0006, PLAN §1.5.1).
//!
//! One JSONL fact per line:
//!
//! ```json
//! {"v":1,"id":42,"ts":"2026-09-13T12:00:00Z","ptr":{"path":"src/vfs/overlay.rs","span":[120,480],"kind_name":"impl_item","content_sha":"…"},"text":"Overlay read_dir must never consult the buffer","flags":[]}
//! ```
//!
//! - **fsync on append** — a fact is only reported appended after the line
//!   reached the disk (crash window = the torn tail, handled below);
//! - **position is identity** (OptMem's rule, kept): note *i* is line *i*
//!   (0-based), so the next id is the line count and nothing in the file
//!   may ever be rewritten or removed — except the torn tail;
//! - **torn-tail quarantine**: a crash mid-append leaves an unterminated
//!   (or JSON-invalid) *trailing* line. On load it is moved to
//!   `.janitor/QUARANTINE.txt` and LOG.txt is rewritten without it (tmp +
//!   rename), with a warning the surfaces (`wake`/`note`) carry forward.
//!   Corruption *before* the trailing line can never be repaired without
//!   renumbering every later fact, so it is the typed
//!   [`MemoryError::LogUnrecoverable`] refusal — never a silent rewrite;
//! - **flags from day one**: `flags: ["architectural_lock"]` is honored by
//!   the whole stack (exempt from compression, always surfaced raw,
//!   §1.5.2).
//!
//! Why JSONL and not OptMem's fixed 320-byte records: our facts carry
//! structured pointers that do not fit fixed width, and the local layer is
//! repo-scale (thousands of notes) where a full load is cheap. The fixed
//! width lives where it pays — the summary tree ([`super::tree_store`]).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{JanitorError, MemoryError, VfsError};
use crate::memory::pointer::SyntaxPointer;
use crate::vfs::Os;

/// The one-line note limit (OptMem discipline, §1.5.3): `wake`'s budget
/// math depends on it.
pub const MAX_NOTE_BYTES: usize = 280;

/// How many chars of an over-limit note the refusal previews (v0.3 B5) —
/// enough to orient, never the whole evidence.
pub const NOTE_PREVIEW_CHARS: usize = 60;

/// The raw-fact log's on-disk name.
pub const LOG_FILE: &str = "LOG.txt";

/// Fact flags honored from day one (§1.5.1). `architectural_lock` facts are
/// exempt from compression: never merged into a summary block, always
/// surfaced raw at their position — never diluted, by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFlag {
    ArchitecturalLock,
}

/// One logged fact: the JSONL line, verbatim shape (ADR-0006).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactRecord {
    /// Log format version (1; a future incompatible record bumps this).
    pub v: u32,
    /// Position-derived identity: line index of this record (0-based).
    pub id: u64,
    /// RFC 3339 UTC append time (`super::timestamp`).
    pub ts: String,
    /// The anti-hallucination anchor, when the fact has one (ADR-0012).
    #[serde(default)]
    pub ptr: Option<SyntaxPointer>,
    /// The fact itself: one line, ≤ [`MAX_NOTE_BYTES`] bytes.
    pub text: String,
    #[serde(default)]
    pub flags: Vec<MemoryFlag>,
}

impl FactRecord {
    /// Whether this fact carries [`MemoryFlag::ArchitecturalLock`].
    pub fn is_locked(&self) -> bool {
        self.flags.contains(&MemoryFlag::ArchitecturalLock)
    }
}

/// A non-fatal load finding surfaced to the agent (the torn-tail
/// quarantine is the first tenant).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogWarning {
    /// Stable machine-readable kind (`"torn_tail_quarantined"`).
    pub kind: &'static str,
    /// One-line human explanation.
    pub message: String,
    /// Where the evidence went, when something was quarantined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantined: Option<String>,
}

/// A loaded log: the facts (oldest first, `facts[i].id == i`) plus any
/// warnings collected on the way (quarantine happened; the log is usable).
#[derive(Clone, Debug, Default)]
pub struct LogState {
    pub facts: Vec<FactRecord>,
    pub warnings: Vec<LogWarning>,
}

/// Validate a `note` text against the line discipline: non-empty, single
/// line, ≤ [`MAX_NOTE_BYTES`] bytes (§1.5.3 — the typed refusal, never a
/// silent truncation). The over-limit refusal carries the structured
/// split evidence (v0.3 B5): actual bytes, the limit, a head preview.
pub fn validate_note_text(text: &str) -> Result<(), MemoryError> {
    if text.is_empty() {
        return Err(MemoryError::NoteInvalid {
            detail: "note text is empty".to_owned(),
            size: None,
        });
    }
    if text.contains('\n') || text.contains('\r') {
        return Err(MemoryError::NoteInvalid {
            detail: "note text must be a single line".to_owned(),
            size: None,
        });
    }
    if text.len() > MAX_NOTE_BYTES {
        return Err(MemoryError::NoteInvalid {
            detail: format!(
                "note text is {len} bytes; the limit is {MAX_NOTE_BYTES} bytes, one line",
                len = text.len()
            ),
            size: Some(crate::error::NoteSize {
                bytes: text.len(),
                limit: MAX_NOTE_BYTES,
                // Chars, not bytes: never splits a multibyte character.
                preview: text.chars().take(NOTE_PREVIEW_CHARS).collect(),
            }),
        });
    }
    Ok(())
}

/// The append-only log at `<root>/{LOG_FILE}` (`root` is the `.janitor/`
/// directory). All I/O maps onto the shared VFS taxonomy.
#[derive(Clone, Debug)]
pub struct FactLog {
    root: PathBuf,
}

impl FactLog {
    /// The log for the local layer rooted at `.janitor/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The log file path.
    pub fn path(&self) -> PathBuf {
        self.root.join(LOG_FILE)
    }

    /// The quarantine file path (torn tails; diagnostic, never machine-read).
    pub fn quarantine_path(&self) -> PathBuf {
        self.root.join("QUARANTINE.txt")
    }

    /// Load the log. Missing `.janitor/` root ⇒ [`MemoryError::NotInitialized`];
    /// missing LOG.txt inside an existing root reads as empty (init creates
    /// it; a deleted log restarts position ids at 0). Torn tail ⇒ quarantined,
    /// warned, and `Ok`; corruption before the trailing line ⇒ the typed
    /// [`MemoryError::LogUnrecoverable`].
    pub fn load(&self) -> Result<LogState, JanitorError> {
        if !self.root.is_dir() {
            return Err(MemoryError::NotInitialized {
                root: self.root.display().to_string(),
            }
            .into());
        }
        let bytes = match fs::read(self.path()) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LogState::default());
            }
            Err(err) => {
                return Err(VfsError::Io {
                    path: self.path().display().to_string(),
                    message: err.to_string(),
                }
                .into());
            }
        };

        let mut state = LogState::default();
        // Walk terminated lines; anything after the final '\n' is an
        // unterminated tail (the crash-mid-append signature). `good_bytes`
        // tracks the intact prefix; `torn` remembers a quarantined tail.
        let mut good_bytes = 0usize;
        let mut torn: Option<(u64, String)> = None;
        let mut line_start = 0usize;
        let mut id = 0u64;
        while let Some(nl) = bytes[line_start..].iter().position(|b| *b == b'\n') {
            let line_end = line_start + nl;
            let is_last_line = line_end + 1 == bytes.len();
            match parse_line(&bytes[line_start..line_end], id) {
                Ok(fact) => {
                    state.facts.push(fact);
                    good_bytes = line_end + 1;
                }
                Err(reason) if is_last_line => {
                    // The torn-tail window: remembered, handled after the
                    // walk — append-only is never blocked by it.
                    torn = Some((id, reason));
                }
                Err(reason) => {
                    tracing::warn!(line = id, %reason, "unrecoverable LOG.txt corruption");
                    return Err(MemoryError::LogUnrecoverable { line: id }.into());
                }
            }
            id += 1;
            line_start = line_end + 1;
        }

        // An unterminated trailing fragment is also a torn tail (it can
        // never be a complete fact: append always writes the '\n').
        if line_start < bytes.len() {
            torn = Some((id, "unterminated".to_owned()));
        }

        if let Some((line, reason)) = torn {
            let fragment = &bytes[good_bytes..];
            let quarantined = self.quarantine(line, &reason, fragment)?;
            // Rewrite the log as its intact prefix so the next append
            // starts clean — the only legal rewrite: the removed bytes
            // were never a fact, and the prefix is byte-identical, so no
            // position moves (tmp + rename, ADR-0016 (8)).
            Os.write_atomic(&self.path(), &bytes[..good_bytes])
                .map_err(JanitorError::from)?;
            state.warnings.push(LogWarning {
                kind: "torn_tail_quarantined",
                message: format!(
                    "trailing LOG.txt line {line} was torn ({reason}); quarantined and \
                     removed — every complete fact is intact"
                ),
                quarantined: Some(quarantined),
            });
        }

        Ok(state)
    }

    /// Append one fact line and fsync. The caller assigns the id
    /// (`facts.len()` of a freshly loaded state — position is identity).
    pub fn append(&self, fact: &FactRecord) -> Result<(), JanitorError> {
        let mut line = serde_json::to_string(fact).map_err(|err| VfsError::Io {
            path: self.path().display().to_string(),
            message: err.to_string(),
        })?;
        line.push('\n');
        let io_err = |err: std::io::Error| -> JanitorError {
            VfsError::Io {
                path: self.path().display().to_string(),
                message: err.to_string(),
            }
            .into()
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())
            .map_err(io_err)?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(io_err)?;
        Ok(())
    }

    /// Append a torn fragment to `QUARANTINE.txt` (with a header line,
    /// fsync) and return the quarantine path for the warning. The LOG.txt
    /// rewrite is the caller's — one atomic prefix rewrite, not two.
    fn quarantine(&self, line: u64, reason: &str, fragment: &[u8]) -> Result<String, JanitorError> {
        let qpath = self.quarantine_path();
        let header = format!(
            "--- quarantined {} from LOG.txt line {line} ({reason}, {} bytes) ---\n",
            crate::memory::timestamp(),
            fragment.len()
        );
        let q_err = |err: std::io::Error| -> JanitorError {
            VfsError::Io {
                path: qpath.display().to_string(),
                message: err.to_string(),
            }
            .into()
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&qpath)
            .map_err(q_err)?;
        file.write_all(header.as_bytes())
            .and_then(|()| file.write_all(fragment))
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(q_err)?;
        Ok(qpath.display().to_string())
    }
}

/// Parse one terminated line as the fact at position `id`. The error
/// string doubles as the quarantine header reason.
fn parse_line(line: &[u8], id: u64) -> Result<FactRecord, String> {
    let text = std::str::from_utf8(line).map_err(|_| "not valid UTF-8".to_owned())?;
    let fact: FactRecord =
        serde_json::from_str(text).map_err(|err| format!("not valid JSON ({err})"))?;
    if fact.v != 1 {
        return Err(format!("unknown record version v={}", fact.v));
    }
    if fact.id != id {
        return Err(format!("id {} is out of position (line {id})", fact.id));
    }
    if fact.text.contains('\n') {
        return Err("embedded newline in text".to_owned());
    }
    Ok(fact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The E_* code of a memory error, through the umbrella (tests speak
    /// the agent-facing contract).
    fn code(err: MemoryError) -> &'static str {
        JanitorError::from(err).code()
    }

    fn log_in(tmp: &Path) -> FactLog {
        let root = tmp.join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        FactLog::new(&root)
    }

    fn fact(id: u64, text: &str) -> FactRecord {
        FactRecord {
            v: 1,
            id,
            ts: "2026-09-14T00:00:00Z".to_owned(),
            ptr: None,
            text: text.to_owned(),
            flags: Vec::new(),
        }
    }

    /// The round trip: append (fsync) then load — ids are positions, flags
    /// and pointers round-trip, and the appended text survives byte-exact.
    #[test]
    fn append_then_load_round_trips_positions_and_flags() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = log_in(tmp.path());

        let locked = FactRecord {
            flags: vec![MemoryFlag::ArchitecturalLock],
            ..fact(0, "overlay read_dir never consults the buffer")
        };
        let pointed = FactRecord {
            ptr: Some(crate::memory::pointer::SyntaxPointer::Commit {
                sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            }),
            ..fact(1, "flush is two-phase by design")
        };
        log.append(&locked).expect("append 0");
        log.append(&pointed).expect("append 1");
        log.append(&fact(2, "plain")).expect("append 2");

        let state = log.load().expect("load");
        assert_eq!(state.facts.len(), 3);
        assert_eq!(state.warnings, Vec::new());
        assert_eq!(state.facts[0], locked);
        assert_eq!(state.facts[1], pointed);
        assert!(state.facts[0].is_locked());
        assert!(!state.facts[1].is_locked());
        assert_eq!(state.facts[2].text, "plain");
    }

    /// Position is identity: ids come from the line index, not the stored
    /// field — an out-of-position id before the trailing line is refused
    /// loudly (a forged id on the *last* line is just a torn tail).
    #[test]
    fn load_refuses_out_of_position_ids() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = log_in(tmp.path());
        log.append(&fact(0, "a")).expect("append");
        log.append(&fact(1, "guard")).expect("append");
        let forged = serde_json::to_string(&fact(7, "b")).expect("json");
        // Splice the forged line between the two intact facts.
        let bytes = std::fs::read(log.path()).expect("read");
        let mut with_forge = Vec::with_capacity(bytes.len() + forged.len() + 1);
        let first_line_end = bytes.iter().position(|b| *b == b'\n').expect("line 0");
        with_forge.extend_from_slice(&bytes[..=first_line_end]);
        with_forge.extend_from_slice(forged.as_bytes());
        with_forge.push(b'\n');
        with_forge.extend_from_slice(&bytes[first_line_end + 1..]);
        std::fs::write(log.path(), &with_forge).expect("write");
        let err = log.load().expect_err("id 7 on line 1");
        assert_eq!(err.code(), "E_LOG_UNRECOVERABLE");
        assert_eq!(err.details(), Some(serde_json::json!({ "line": 1 })));
        assert!(!log.quarantine_path().exists());
    }

    /// The torn tail, both signatures: an unterminated fragment and a
    /// JSON-invalid trailing line. Each quarantines, drops, warns — and
    /// the log stays usable (the next note appends with the right id).
    #[test]
    fn torn_tail_is_quarantined_and_the_log_stays_usable() {
        for (name, tail, newline) in [
            (
                "unterminated fragment",
                b"{\"v\":1,\"id\":2,\"ts\"".as_slice(),
                false,
            ),
            (
                "invalid json trailing line",
                b"{torn not json".as_slice(),
                true,
            ),
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let log = log_in(tmp.path());
            log.append(&fact(0, "fact zero")).expect("append 0");
            log.append(&fact(1, "fact one")).expect("append 1");
            let mut bytes = std::fs::read(log.path()).expect("read");
            bytes.extend_from_slice(tail);
            if newline {
                bytes.push(b'\n');
            }
            std::fs::write(log.path(), &bytes).expect("write torn");

            let state = log.load().expect("torn tail is recoverable");
            assert_eq!(state.facts.len(), 2, "{name}: intact facts survive");
            assert_eq!(state.warnings.len(), 1, "{name}");
            assert_eq!(state.warnings[0].kind, "torn_tail_quarantined");
            assert!(
                state.warnings[0]
                    .quarantined
                    .as_deref()
                    .is_some_and(|q| q.ends_with("QUARANTINE.txt"))
            );

            // Evidence moved, log clean, next id correct.
            let quarantined = std::fs::read_to_string(log.quarantine_path()).expect("q");
            assert!(quarantined.contains("quarantined"), "{name}");
            log.append(&fact(2, "after recovery"))
                .expect("append after quarantine");
            let reloaded = log.load().expect("reload");
            assert_eq!(reloaded.facts.len(), 3, "{name}");
            assert_eq!(reloaded.facts[2].id, 2);
            assert_eq!(reloaded.facts[2].text, "after recovery");
            assert!(reloaded.warnings.is_empty(), "{name}: clean after recovery");
        }
    }

    /// Corruption before the trailing line can only be "repaired" by
    /// renumbering every later fact — refused, nothing quarantined.
    #[test]
    fn mid_line_corruption_is_refused_not_renumbered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = log_in(tmp.path());
        std::fs::write(log.path(), "garbage\n").expect("corrupt line 0");
        log.append(&fact(1, "good")).expect("append");

        let err = log.load().expect_err("line 0 is not a fact");
        assert_eq!(err.code(), "E_LOG_UNRECOVERABLE");
        assert_eq!(
            err.details().and_then(|d| d["line"].as_u64()),
            Some(0),
            "the corrupt line index is the detail"
        );
        assert!(
            !log.quarantine_path().exists(),
            "mid-file corruption is never quarantined (ids must not move)"
        );
    }

    /// Missing root is NotInitialized; a missing LOG.txt inside the root
    /// is an empty log (documented restart semantics).
    #[test]
    fn missing_root_is_not_initialized_and_missing_log_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nowhere = FactLog::new(tmp.path().join("nope/.janitor"));
        let err = nowhere.load().expect_err("no root");
        assert_eq!(err.code(), "E_NOT_INITIALIZED");

        let log = log_in(tmp.path());
        let state = log.load().expect("root exists, LOG.txt missing");
        assert!(state.facts.is_empty());
    }

    /// The note line discipline: empty, multi-line, and over-budget texts
    /// are the typed E_NOTE_INVALID; 280 bytes exactly is fine. The
    /// over-budget refusal carries the v0.3 B5 split evidence — actual
    /// bytes, the limit, a head preview — so the note can be split on the
    /// first try (the ≤280 behavior itself is unchanged).
    #[test]
    fn note_text_discipline_is_typed() {
        assert_eq!(code(validate_note_text("").unwrap_err()), "E_NOTE_INVALID");
        assert_eq!(
            code(validate_note_text("two\nlines").unwrap_err()),
            "E_NOTE_INVALID"
        );
        assert_eq!(
            code(validate_note_text("carriage\rreturn").unwrap_err()),
            "E_NOTE_INVALID"
        );
        // The other refusals stay sentence-only (no structured payload).
        for text in ["", "two\nlines"] {
            let err: JanitorError = validate_note_text(text).unwrap_err().into();
            assert!(err.details().is_some_and(|d| d.get("bytes").is_none()));
        }

        let over = "x".repeat(MAX_NOTE_BYTES + 1);
        let err: JanitorError = validate_note_text(&over).unwrap_err().into();
        assert_eq!(err.code(), "E_NOTE_INVALID");
        assert!(
            err.details()
                .and_then(|d| d["detail"].as_str().map(str::to_owned))
                .is_some_and(|d| d.contains("281 bytes"))
        );
        assert_eq!(
            err.details().unwrap()["bytes"],
            serde_json::json!(MAX_NOTE_BYTES + 1)
        );
        assert_eq!(
            err.details().unwrap()["limit"],
            serde_json::json!(MAX_NOTE_BYTES)
        );
        assert_eq!(
            err.details().unwrap()["preview"],
            serde_json::json!("x".repeat(NOTE_PREVIEW_CHARS))
        );

        // Multibyte evidence: the preview counts chars, never splitting
        // one; the byte count is the real length.
        let multibyte = "é".repeat(MAX_NOTE_BYTES / 2 + 1); // 2 bytes/char ⇒ over
        let err: JanitorError = validate_note_text(&multibyte).unwrap_err().into();
        let details = err.details().unwrap();
        assert_eq!(details["bytes"], serde_json::json!(multibyte.len()));
        assert_eq!(
            details["preview"].as_str().map(|p| p.chars().count()),
            Some(NOTE_PREVIEW_CHARS)
        );

        validate_note_text(&"x".repeat(MAX_NOTE_BYTES)).expect("exactly at the limit");
        validate_note_text("in-flight fact").expect("ordinary text");
    }

    /// The serialized line shape is the ADR-0006 contract: exact keys, in
    /// order, with the pointer as the nested object the plan sketches.
    #[test]
    fn record_serializes_to_the_documented_shape() {
        let record = FactRecord {
            v: 1,
            id: 42,
            ts: "2026-09-13T12:00:00Z".to_owned(),
            ptr: Some(crate::memory::pointer::SyntaxPointer::Node {
                path: "src/vfs/overlay.rs".to_owned(),
                span: (120, 480),
                kind_name: "impl_item".to_owned(),
                content_sha: "0f3ac1d8e5b24719".to_owned(),
            }),
            text: "Overlay read_dir must never consult the buffer".to_owned(),
            flags: vec![MemoryFlag::ArchitecturalLock],
        };
        let line = serde_json::to_string(&record).expect("json");
        assert_eq!(
            line,
            "{\"v\":1,\"id\":42,\"ts\":\"2026-09-13T12:00:00Z\",\
             \"ptr\":{\"path\":\"src/vfs/overlay.rs\",\"span\":[120,480],\
             \"kind_name\":\"impl_item\",\"content_sha\":\"0f3ac1d8e5b24719\"},\
             \"text\":\"Overlay read_dir must never consult the buffer\",\
             \"flags\":[\"architectural_lock\"]}"
        );
    }
}
