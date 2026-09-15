//! The `apply` edit payload (PLAN §1.6, ADR-0014, §3 F5.1): the dumb,
//! agent-proof wire format the plan deliberately chose over tree-diff
//! formats — half-open byte-offset edits against verified content, with all
//! the cleverness living in re-parse validation (§1.2.1 step 7).
//!
//! ```json
//! { "operations": [ { "path": "src/x.rs",
//!     "expect_sha": "0123456789abcdef",
//!     "edits": [ { "start_byte": 10, "old_end_byte": 24,
//!                  "new_text": "const",
//!                  "old_text": "static" } ] } ] }
//! ```
//!
//! - **Offsets are half-open `[start_byte, old_end_byte)` UTF-8 byte
//!   ranges** into the file's current overlay-visible content (ADR-0014,
//!   refactree's span model, A.4). `new_text` arrives as a JSON string, so
//!   serde has already validated it as UTF-8 before any bytes move (E6).
//! - **`expect_sha`** (optional, per file) is [`content_sha`] of the current
//!   content: a mismatched hash is the typed `E_STALE_VIEW` — an agent
//!   acting on a stale view fails fast instead of corrupting.
//! - **`old_text`** (optional, per edit) is the bytes the agent expects at
//!   the replaced range: a mismatch is the typed `E_EDIT_MISMATCH`. This is
//!   the edit-level staleness guard — `expect_sha` pins the whole file,
//!   `old_text` pins each edit site, and either leg catching drift is a
//!   refusal, never a guess.
//! - **Edits must be sorted ascending by `start_byte` and non-overlapping**
//!   (adjacent ranges are fine): violations are the typed
//!   `E_EDIT_OVERLAP`. Rejecting unsorted input keeps the payload a
//!   verified assertion about content, not something janitor silently
//!   reinterprets.
//!
//! Validation happens against the **current snapshot bytes** (§1.2.1 step
//! 2) before anything is staged or edited; the exact JSON schema and the
//! whole-diff transaction boundary are documented in `docs/SCHEMA.md`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::ast::parse::Splice;
use crate::error::{EditError, JanitorError};

/// Content hash for [`FileEdit::expect_sha`]: SipHash-1-3 (std's
/// `DefaultHasher`, zero keys) of the bytes as 16 lowercase hex chars.
///
/// Identity check, not a security property (the same stance as the parse
/// cache's T4 leg). Agents obtain the current value from janitor's own
/// output — an apply report carries `sha_before`/`sha_after` per file, so a
/// dry-run's `sha_before` is the next apply's `expect_sha`.
pub fn content_sha(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// One whole-diff payload: every file's edits in one transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPayload {
    /// Per-file edit sets. Paths must be unique (a duplicate is the typed
    /// `E_EDIT_PAYLOAD` — janitor never guesses an interleaving).
    #[serde(default)]
    pub operations: Vec<FileEdit>,
}

/// One file's edit set (ADR-0014).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    /// The target path, as the agent spelled it (report paths echo this
    /// verbatim; resolution to an absolute path happens once, at the
    /// orchestrator boundary).
    pub path: String,
    /// Optional staleness guard: [`content_sha`] of the content the agent
    /// based its offsets on. Mismatch ⇒ `E_STALE_VIEW`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_sha: Option<String>,
    /// Sorted ascending by `start_byte`, non-overlapping.
    #[serde(default)]
    pub edits: Vec<EditSpec>,
}

/// One half-open byte replacement (ADR-0014's three-key wire shape, plus
/// the optional `old_text` edit-level guard).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditSpec {
    /// First replaced byte (UTF-8 byte offset into the current content).
    pub start_byte: u64,
    /// One past the last replaced byte; `start_byte == old_end_byte` is a
    /// pure insertion.
    pub old_end_byte: u64,
    /// The replacement bytes (UTF-8 by serde construction — E6).
    pub new_text: String,
    /// The bytes the agent expects at `[start_byte, old_end_byte)`;
    /// mismatch ⇒ `E_EDIT_MISMATCH`. Absent ⇒ not checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
}

/// A validated [`FileEdit`]: every offset in range, every expected byte
/// verified, sorted and non-overlapping — ready to splice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedFile {
    /// The splices, payload order (== ascending offset order).
    pub splices: Vec<Splice>,
    /// Each edit's replaced range in **new-byte** coordinates,
    /// `[start, start + new_text.len())` — the informational
    /// `in_edited_range` basis for the gate report (the gate itself stays
    /// whole-file, KP-4).
    pub new_ranges: Vec<Range<usize>>,
}

impl DiffPayload {
    /// Parse the payload from JSON. Malformed JSON, wrong types, or a
    /// non-unique path set is the typed `E_EDIT_PAYLOAD` — never a panic,
    /// never a silent partial parse.
    pub fn from_json_str(text: &str) -> Result<Self, JanitorError> {
        let payload: Self = serde_json::from_str(text).map_err(|err| {
            JanitorError::Edit(EditError::Payload {
                detail: format!("{err}"),
            })
        })?;
        let mut seen: Vec<&str> = payload
            .operations
            .iter()
            .map(|op| op.path.as_str())
            .collect();
        seen.sort_unstable();
        if seen.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(JanitorError::Edit(EditError::Payload {
                detail: "duplicate path in operations; each file appears at most once".to_owned(),
            }));
        }
        Ok(payload)
    }
}

impl FileEdit {
    /// §1.2.1 step 2: validate against `old_bytes`, the file's current
    /// snapshot. Checks, in order: `expect_sha` (file-level staleness),
    /// then per edit the range shape, bounds, UTF-8 boundaries and expected
    /// old bytes, then the sorted/non-overlapping discipline across edits.
    /// All refusals are typed [`EditError`]s; nothing is staged on any of
    /// them.
    pub fn validate(&self, old_bytes: &[u8]) -> Result<ValidatedFile, EditError> {
        if let Some(expected) = &self.expect_sha
            && *expected != content_sha(old_bytes)
        {
            return Err(EditError::StaleView {
                path: self.path.clone(),
                expected_sha: expected.clone(),
            });
        }

        let mut splices = Vec::with_capacity(self.edits.len());
        for edit in &self.edits {
            let start = usize::try_from(edit.start_byte).unwrap_or(usize::MAX);
            let old_end = usize::try_from(edit.old_end_byte).unwrap_or(usize::MAX);
            if old_end > old_bytes.len() || start == usize::MAX || old_end == usize::MAX {
                return Err(EditError::OutOfRange {
                    path: self.path.clone(),
                    offset: edit.old_end_byte,
                    len: old_bytes.len() as u64,
                });
            }
            if start > old_end {
                return Err(EditError::Mismatch {
                    path: self.path.clone(),
                    detail: format!("inverted range [{start}, {old_end})"),
                });
            }
            // UTF-8 boundary sanity (E6 adjacent): a range that cuts a
            // multi-byte rune in half cannot be a valid replacement site —
            // no `old_text` needed to refuse it.
            if !is_char_boundary(old_bytes, start) || !is_char_boundary(old_bytes, old_end) {
                return Err(EditError::Mismatch {
                    path: self.path.clone(),
                    detail: format!(
                        "range [{start}, {old_end}) does not start and end on UTF-8 character boundaries"
                    ),
                });
            }
            if let Some(expected) = &edit.old_text
                && &old_bytes[start..old_end] != expected.as_bytes()
            {
                return Err(EditError::Mismatch {
                    path: self.path.clone(),
                    detail: format!(
                        "expected old bytes at [{start}, {old_end}) differ from the current \
                         snapshot ({}/{} bytes)",
                        expected.len(),
                        old_end - start
                    ),
                });
            }
            splices.push(Splice {
                start_byte: start,
                old_end_byte: old_end,
                new_text: edit.new_text.as_bytes().to_vec(),
            });
        }

        // Sorted + non-overlapping (ADR-0014). Adjacent ranges are legal:
        // half-open ranges that touch do not share a byte.
        for pair in splices.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            if b.start_byte < a.old_end_byte {
                let detail = if b.start_byte < a.start_byte {
                    "edits must be sorted ascending by start_byte".to_owned()
                } else {
                    format!(
                        "[{}, {}) and [{}, {}) overlap",
                        a.start_byte, a.old_end_byte, b.start_byte, b.old_end_byte
                    )
                };
                return Err(EditError::Overlap {
                    path: self.path.clone(),
                    detail,
                });
            }
        }

        let new_ranges = splices
            .iter()
            .map(|s| s.start_byte..s.new_end_byte())
            .collect();
        Ok(ValidatedFile {
            splices,
            new_ranges,
        })
    }
}

/// Byte-offset char-boundary test for UTF-8 content: an offset is on a
/// boundary iff the byte there does not start with `10` (a continuation
/// byte) — the slice-level equivalent of `str::is_char_boundary`, valid
/// because `validate` runs on sources that decode as UTF-8.
fn is_char_boundary(bytes: &[u8], offset: usize) -> bool {
    offset >= bytes.len() || bytes[offset] & 0xC0 != 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "fn alpha(x: u32) -> u32 {\n    let base = x + 1;\n    base * 2\n}\n";

    fn spec(start: u64, old_end: u64, new_text: &str) -> EditSpec {
        EditSpec {
            start_byte: start,
            old_end_byte: old_end,
            new_text: new_text.to_owned(),
            old_text: None,
        }
    }

    /// Round-trip: the documented wire shape parses, and the exact keys
    /// survive serialization (agents bake formats; R12).
    #[test]
    fn documented_payload_shape_round_trips() {
        let json = r#"{
            "operations": [
                { "path": "src/x.rs",
                  "expect_sha": "0123456789abcdef",
                  "edits": [
                      { "start_byte": 10, "old_end_byte": 24,
                        "new_text": "const", "old_text": "static mut count" }
                  ] }
            ]
        }"#;
        let payload = DiffPayload::from_json_str(json).expect("documented shape parses");
        assert_eq!(payload.operations.len(), 1);
        let op = &payload.operations[0];
        assert_eq!(op.path, "src/x.rs");
        assert_eq!(op.expect_sha.as_deref(), Some("0123456789abcdef"));
        assert_eq!(op.edits.len(), 1);
        assert_eq!(op.edits[0].start_byte, 10);
        assert_eq!(op.edits[0].old_end_byte, 24);
        assert_eq!(op.edits[0].new_text, "const");
        assert_eq!(op.edits[0].old_text.as_deref(), Some("static mut count"));

        let back = serde_json::to_value(&payload).expect("serializes");
        assert_eq!(back["operations"][0]["edits"][0]["start_byte"], 10);
        assert_eq!(back["operations"][0]["edits"][0]["new_text"], "const");
        // Optional guards stay absent when unset (exact key sets, R12).
        let bare = DiffPayload::from_json_str(r#"{"operations":[{"path":"a","edits":[]}]}"#)
            .expect("minimal shape parses");
        let bare = serde_json::to_value(&bare).expect("serializes");
        assert!(bare["operations"][0].get("expect_sha").is_none());
        assert!(bare["operations"][0]["edits"].is_array());
    }

    /// Malformed payloads are the typed `E_EDIT_PAYLOAD`: bad JSON, wrong
    /// types, missing required keys, and duplicate paths (janitor never
    /// guesses an interleaving of two edit sets on one file).
    #[test]
    fn malformed_payloads_are_typed_errors() {
        for json in [
            "not json at all",
            r#"{"operations": "no"}"#,
            r#"{"operations": [{"path": 3, "edits": []}]}"#,
            r#"{"operations": [{"edits": []}]}"#,
            r#"{"operations": [{"path": "a", "edits": [{"start_byte": 1}]}]}"#,
            r#"{"operations": [{"path": "a", "edits": []}, {"path": "a", "edits": []}]}"#,
        ] {
            let outcome = DiffPayload::from_json_str(json);
            assert!(
                matches!(&outcome, Err(JanitorError::Edit(EditError::Payload { .. }))),
                "{json:?}: must be the typed payload error, got {outcome:?}"
            );
        }
    }

    /// The happy path validates: offsets in range, old bytes verified, and
    /// the new-coordinate ranges the gate report needs.
    #[test]
    fn validation_produces_sorted_splices_and_new_ranges() {
        let alpha_at = SOURCE.find("alpha").unwrap_or_default() as u64;
        let base_at = (SOURCE.find("    let base").unwrap_or_default() + 4) as u64;
        let op = FileEdit {
            path: "src/lib.rs".into(),
            expect_sha: Some(content_sha(SOURCE.as_bytes())),
            edits: vec![
                spec(alpha_at, alpha_at + "alpha".len() as u64, "alpha_renamed"),
                spec(base_at, base_at + "let".len() as u64, "let"),
            ],
        };
        let validated = op.validate(SOURCE.as_bytes()).expect("validates");
        assert_eq!(validated.splices.len(), 2);
        assert_eq!(validated.splices[0].start_byte as u64, alpha_at);
        assert_eq!(validated.splices[0].new_text, b"alpha_renamed".to_vec());
        // New ranges: [start, start + len(new_text)).
        assert_eq!(
            validated.new_ranges[0],
            alpha_at as usize..alpha_at as usize + "alpha_renamed".len()
        );
        assert_eq!(
            validated.new_ranges[1],
            base_at as usize..base_at as usize + "let".len()
        );
    }

    /// The three plan-named refusals, plus out-of-range: stale view,
    /// overlap, unsorted, mismatched old bytes, beyond-length offsets.
    #[test]
    fn every_typed_refusal_fires() {
        let bytes = SOURCE.as_bytes();
        let at = SOURCE.find("alpha").unwrap_or_default();

        // E_STALE_VIEW: expect_sha present and mismatched.
        let op = FileEdit {
            path: "a.rs".into(),
            expect_sha: Some("ffffffffffffffff".into()),
            edits: vec![],
        };
        assert!(matches!(
            op.validate(bytes),
            Err(EditError::StaleView { .. })
        ));

        // E_EDIT_OVERLAP: two edits sharing a byte.
        let op = FileEdit {
            path: "a.rs".into(),
            expect_sha: None,
            edits: vec![
                spec(at as u64, at as u64 + 8, "x"),
                spec(at as u64 + 5, at as u64 + 9, "y"),
            ],
        };
        assert!(matches!(op.validate(bytes), Err(EditError::Overlap { .. })));

        // E_EDIT_OVERLAP: unsorted (descending) edits.
        let op = FileEdit {
            path: "a.rs".into(),
            expect_sha: None,
            edits: vec![
                spec(at as u64 + 20, at as u64 + 24, "y"),
                spec(at as u64, at as u64 + 5, "x"),
            ],
        };
        let err = op.validate(bytes).expect_err("unsorted refused");
        assert!(matches!(err, EditError::Overlap { ref detail, .. } if detail.contains("sorted")));

        // E_EDIT_MISMATCH: old_text disagrees with the snapshot.
        let op = FileEdit {
            path: "a.rs".into(),
            expect_sha: None,
            edits: vec![EditSpec {
                start_byte: at as u64,
                old_end_byte: at as u64 + 5,
                new_text: "beta".into(),
                old_text: Some("beta?".into()),
            }],
        };
        assert!(matches!(
            op.validate(bytes),
            Err(EditError::Mismatch { .. })
        ));

        // E_EDIT_MISMATCH: inverted range.
        let op = FileEdit {
            path: "a.rs".into(),
            expect_sha: None,
            edits: vec![spec(20, 10, "x")],
        };
        assert!(matches!(
            op.validate(bytes),
            Err(EditError::Mismatch { .. })
        ));

        // E_EDIT_OUT_OF_RANGE: past the end of the snapshot.
        let op = FileEdit {
            path: "a.rs".into(),
            expect_sha: None,
            edits: vec![spec(0, bytes.len() as u64 + 1, "x")],
        };
        assert!(matches!(
            op.validate(bytes),
            Err(EditError::OutOfRange { .. })
        ));

        // Adjacent ranges are legal: half-open ranges that touch share no
        // byte, and a correct old_text passes.
        let op = FileEdit {
            path: "a.rs".into(),
            expect_sha: None,
            edits: vec![
                EditSpec {
                    start_byte: at as u64,
                    old_end_byte: at as u64 + 5,
                    new_text: "beta_".into(),
                    old_text: Some("alpha".into()),
                },
                spec(at as u64 + 5, at as u64 + 8, "ren"),
            ],
        };
        assert!(op.validate(bytes).is_ok());
    }

    /// UTF-8 boundary sanity: a range cutting a multi-byte rune in half is
    /// an `E_EDIT_MISMATCH` even with no `old_text` to compare against.
    #[test]
    fn ranges_must_cut_on_character_boundaries() {
        let source = "const S: &str = \"αβγ\";\n";
        let bytes = source.as_bytes();
        let alpha = source.find('α').unwrap_or_default();
        // Cut one byte into the 2-byte rune.
        let op = FileEdit {
            path: "s.rs".into(),
            expect_sha: None,
            edits: vec![spec(alpha as u64 + 1, alpha as u64 + 2, "x")],
        };
        assert!(matches!(
            op.validate(bytes),
            Err(EditError::Mismatch { .. })
        ));
        // Whole-rune replacement is fine.
        let op = FileEdit {
            path: "s.rs".into(),
            expect_sha: None,
            edits: vec![spec(alpha as u64, alpha as u64 + 2, "ω")],
        };
        assert!(op.validate(bytes).is_ok());
    }

    /// `content_sha` is the documented 16-hex-char form and stable for
    /// equal bytes (the within-session contract; agents read the value from
    /// janitor's own reports rather than recomputing it).
    #[test]
    fn content_sha_is_stable_sixteen_hex() {
        let a = content_sha(b"fn main() {}\n");
        let b = content_sha(b"fn main() {}\n");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, content_sha(b"fn main2() {}\n"));
    }
}
