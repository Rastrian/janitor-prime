//! The re-parse validation gate (PLAN §1.2.1 step 7, §1.6, §3 F5.3; KP-4
//! outcome) and the per-file work item that runs it.
//!
//! **The gate is whole-file** (KP-4): an edited file passes iff its
//! re-parsed tree has zero ERROR/MISSING nodes. The localized variant —
//! errors within the edited byte ranges only — measured a 1/10
//! false-negative rate (a MISSING `;` parks at the *next* statement's
//! start, outside the half-open range), which is exactly the dishonesty
//! risk R7 exists for; whole-file is also cheaper (`error_count` short-
//! circuits on the `has_error` flag). [`InvalidNode::in_edited_range`]
//! carries the localized view as *information* in the refusal payload —
//! the gate's decision never consults it.
//!
//! [`validate_one`] is the §1.2.1 steps 1–6 + 7 pipeline for one file,
//! exactly as the apply loop's rayon workers run it: read the snapshot →
//! validate the payload against it → splice descending → **take** the
//! `ParseEntry` (the ADR-0007 one-way door; a cache miss full-parses into
//! a fresh exclusive handle) → guarded multi-edit re-parse → gate. It
//! never stages and never touches disk: staging belongs to the flush
//! phase, and the transaction boundary is the whole diff (a failure here
//! leaves the overlay, the disk, and every other file untouched).

use std::ops::Range;
use std::sync::Arc;

use serde::Serialize;

use crate::apply::diff::FileEdit;
use crate::apply::splice::splice_descending;
use crate::ast::cache::ParseCache;
use crate::ast::errors::{error_count, error_reports};
use crate::ast::ffi_shim::{Parser, Tree};
use crate::ast::lang::{LangId, LangRegistry};
use crate::ast::parse::{parse_snapshot, reparse_edited_multi};
use crate::error::{JanitorError, ParseError, ValidateError};
use crate::scan::PointJson;
use crate::vfs::{Overlay, VfsPath};

/// One ERROR/MISSING node in the `E_PARSE_INVALID` payload — plain copied
/// values (Rule T4a: no `Node` handle crosses the work item). Coordinates
/// are in **new** bytes; points are zero-based with byte columns
/// (tree-sitter's units, KP-3). `kind` resolves at serialization time via
/// the registry and is omitted when the language cannot name the id.
#[derive(Clone, Debug, Serialize)]
pub struct InvalidNode {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub kind_id: u16,
    pub is_missing: bool,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: PointJson,
    pub end_point: PointJson,
    /// Informational only: whether the node's span intersects any edited
    /// range (new-byte coordinates). The gate stays whole-file (KP-4).
    pub in_edited_range: bool,
}

impl InvalidNode {
    /// As the `Value` the frozen `ValidateError::ParseInvalid` payload
    /// carries (the F0 skeleton's `TODO(F5)` slot, filled append-only).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self)
            .unwrap_or_else(|_| serde_json::json!({ "path": self.path, "kind_id": self.kind_id }))
    }
}

/// A file that passed the gate: everything the flush phase and the report
/// need, owned (the tree is the re-parsed clean tree; the entry inserted
/// after flush wraps it).
#[derive(Debug)]
pub struct PreparedFile {
    /// Canonical buffer key.
    pub key: VfsPath,
    /// The payload's path spelling, echoed in the report.
    pub display: String,
    pub new_bytes: Arc<[u8]>,
    pub tree: Tree,
    pub edits: usize,
    pub bytes_before: usize,
    /// [`crate::apply::diff::content_sha`] of the pre-edit content — the
    /// report's `sha_before`, and a natural `expect_sha` for a follow-up.
    pub sha_before: String,
    pub errors_before: u32,
}

/// §1.2.1 steps 1–7 for one file. `worker_parser` is the rayon worker's
/// lazily-materialized parser (`(LangId, Parser)` pair so a language
/// change on the same worker rebuilds it); everything that can fail is a
/// typed error and nothing is staged on any path through the function.
#[allow(clippy::too_many_arguments)]
pub fn validate_one(
    overlay: &Overlay,
    cache: &ParseCache,
    registry: &LangRegistry,
    worker_parser: &mut Option<(LangId, Parser)>,
    op: &FileEdit,
    key: &VfsPath,
) -> Result<PreparedFile, JanitorError> {
    // Step 1: the current snapshot (read-through: staged content wins).
    let old_bytes = overlay.read_file(key)?;
    if std::str::from_utf8(&old_bytes).is_err() {
        return Err(ParseError::SourceDecoding {
            path: op.path.clone(),
        }
        .into());
    }

    // Language routing before any parsing: a file no enabled grammar
    // covers cannot be gated, so it is a typed refusal, never a silent
    // ungated flush.
    let lang = registry.for_path(key.as_path()).ok_or_else(|| {
        let ext = key
            .as_path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        ParseError::NoGrammarForLang {
            lang: if ext.is_empty() {
                format!("{} (no extension)", op.path)
            } else {
                ext.to_owned()
            },
        }
    })?;

    // Step 2: payload validation against the snapshot.
    let validated = op.validate(&old_bytes)?;

    // Step 3: the new bytes, descending fold.
    let new_bytes = splice_descending(&old_bytes, &validated.splices)?;

    // Steps 4–6: exclusive ownership, then the guarded multi-edit
    // choreography. `take` is the one-way door (ADR-0007): the entry's
    // `Arc<Tree>` becomes the only handle — the guard inside the fold
    // asserts it before any `Tree::edit`. A missing or stale entry simply
    // full-parses into a fresh exclusive handle.
    let generation = overlay.current_gen(key)?;
    let taken = cache
        .take(key)
        .filter(|entry| entry.is_valid_for(generation, &old_bytes));
    let (mut tree_arc, errors_before) = match taken {
        Some(entry) => (entry.tree, entry.error_count),
        None => {
            let tree = parse_snapshot(parser_for(registry, lang, worker_parser)?, &old_bytes)?;
            let errors_before = error_count(tree.root_node());
            (Arc::new(tree), errors_before)
        }
    };

    let tree = reparse_edited_multi(
        parser_for(registry, lang, worker_parser)?,
        &old_bytes,
        &mut tree_arc,
        &validated.splices,
        &new_bytes,
    )?;

    // Step 7: the whole-file gate.
    gate(&tree, lang, registry, &op.path, &validated.new_ranges)?;

    Ok(PreparedFile {
        key: key.clone(),
        display: op.path.clone(),
        edits: validated.splices.len(),
        bytes_before: old_bytes.len(),
        sha_before: crate::apply::diff::content_sha(&old_bytes),
        errors_before,
        new_bytes,
        tree,
    })
}

/// The gate (KP-4): `error_count == 0` passes; anything else is
/// `E_PARSE_INVALID` carrying every offending node with coordinates and
/// the informational in-edited-range flag.
pub fn gate(
    tree: &Tree,
    lang: LangId,
    registry: &LangRegistry,
    path: &str,
    new_ranges: &[Range<usize>],
) -> Result<(), ValidateError> {
    let root = tree.root_node();
    if error_count(root) == 0 {
        return Ok(());
    }
    let nodes: Vec<serde_json::Value> = error_reports(root)
        .iter()
        .map(|report| {
            let in_edited_range = new_ranges
                .iter()
                .any(|range| report.start_byte < range.end && range.start < report.end_byte);
            InvalidNode {
                path: path.to_owned(),
                kind: registry
                    .kind_name(lang, report.kind_id)
                    .map(|name| name.to_string()),
                kind_id: report.kind_id,
                is_missing: report.is_missing,
                start_byte: report.start_byte,
                end_byte: report.end_byte,
                start_point: PointJson {
                    row: report.start_point.row as u32,
                    column: report.start_point.column as u32,
                },
                end_point: PointJson {
                    row: report.end_point.row as u32,
                    column: report.end_point.column as u32,
                },
                in_edited_range,
            }
            .to_value()
        })
        .collect();
    let count = nodes.len();
    Err(ValidateError::ParseInvalid { count, nodes })
}

/// The worker's parser for `lang`, materialized lazily and rebuilt when
/// the cached one targets a different language (one parser per language
/// per worker, §1.2's ownership shape).
fn parser_for<'a>(
    registry: &LangRegistry,
    lang: LangId,
    slot: &'a mut Option<(LangId, Parser)>,
) -> Result<&'a mut Parser, ParseError> {
    if slot.as_ref().is_none_or(|(held, _)| *held != lang) {
        *slot = Some((lang, registry.parser(lang)?));
    }
    slot.as_mut()
        .map(|(_, parser)| parser)
        .ok_or_else(|| ParseError::NoTree {
            detail: "worker parser vanished".to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::diff::{DiffPayload, EditSpec};
    use crate::ast::cache::ParseCache;
    use crate::vfs::{Os, Overlay};

    fn registry() -> LangRegistry {
        LangRegistry::new()
    }

    fn spec(start: u64, old_end: u64, new_text: &str) -> EditSpec {
        EditSpec {
            start_byte: start,
            old_end_byte: old_end,
            new_text: new_text.to_owned(),
            old_text: None,
        }
    }

    /// The gate passes clean trees and refuses dirty ones with every
    /// offending node's coordinates — kind, byte range, line/col — plus
    /// the informational in-edited-range flag.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn gate_reports_coordinates_and_range_flag() {
        let registry = registry();
        let lang = registry.for_extension("rs").expect("default feature");
        let mut parser = registry.parser(lang).expect("ABI pair");

        let clean: Arc<[u8]> = Arc::from(b"fn ok() -> u32 { 1 }\n".as_slice());
        let tree = parse_snapshot(&mut parser, &clean).expect("clean parses");
        let whole = 0..clean.len();
        assert!(gate(&tree, lang, &registry, "ok.rs", &[whole]).is_ok());

        // The KP-4 false-negative shape: delete the `;` of a let — the
        // MISSING node parks at the NEXT statement's start, outside the
        // edited range; the whole-file gate still refuses it.
        let old: Arc<[u8]> = Arc::from(
            b"fn gate_b() -> u32 {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n".as_slice(),
        );
        let at = old
            .windows(10)
            .position(|w| w == b"let a = 1;")
            .unwrap_or_default();
        let splices = vec![crate::ast::parse::Splice {
            start_byte: at,
            old_end_byte: at + 10,
            new_text: b"let a = 1".to_vec(),
        }];
        let new_bytes = splice_descending(&old, &splices).expect("splices fold");
        let mut owned = Arc::new(parse_snapshot(&mut parser, &old).expect("old parses"));
        let tree = reparse_edited_multi(&mut parser, &old, &mut owned, &splices, &new_bytes)
            .expect("choreography");

        let edited_range = at..at + "let a = 1".len();
        let new_ranges = [edited_range];
        let err = gate(&tree, lang, &registry, "gate_b.rs", &new_ranges)
            .expect_err("the MISSING ; must trip the whole-file gate");
        let ValidateError::ParseInvalid { count, nodes } = err else {
            unreachable!("asserted by construction");
        };
        assert_eq!(count, 1);
        let node = &nodes[0];
        assert_eq!(node["is_missing"], true);
        assert_eq!(
            node["in_edited_range"], false,
            "KP-4: parks outside the range"
        );
        // The MISSING `;` is zero-width at the end of the edited range —
        // row 1 (the `let a` line), one byte past the replacement, which
        // is exactly why the localized gate would have missed it.
        assert_eq!(node["start_point"]["row"], 1);
        assert_eq!(node["start_point"]["column"], "    let a = 1".len());
        assert!(node["kind"].is_string() || node.get("kind").is_none());
        assert!(node["end_byte"].as_u64().unwrap_or_default() > 0);
    }

    /// `validate_one` end to end over a real tempdir file: happy path
    /// prepares the file with report facts; missing files, non-UTF-8
    /// files, and grammar-less extensions are typed refusals; nothing is
    /// staged on any path.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn validate_one_prepares_or_refuses_typewise() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("lib.rs"),
            b"fn alpha(x: u32) -> u32 {\n    let base = x + 1;\n    base * 2\n}\n",
        )
        .expect("write");
        std::fs::write(tmp.path().join("binary.rs"), [0xff, 0xfe, 0x00]).expect("write");
        std::fs::write(tmp.path().join("notes.txt"), b"plain text\n").expect("write");

        let overlay = Overlay::new(Arc::new(Os));
        let cache = ParseCache::new();
        let registry = registry();
        let mut worker = None;

        let source = std::fs::read(tmp.path().join("lib.rs")).expect("read");
        let at = source
            .windows(5)
            .position(|w| w == b"alpha")
            .unwrap_or_default();
        let payload = DiffPayload::from_json_str(&format!(
            r#"{{"operations":[{{"path":"lib.rs","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"alpha_renamed","old_text":"alpha"}}]}}]}}"#,
            at + 5
        ))
        .expect("payload");
        let op = payload.operations[0].clone();
        let key = VfsPath::new(tmp.path().join("lib.rs")).expect("key");

        let prepared = validate_one(&overlay, &cache, &registry, &mut worker, &op, &key)
            .expect("valid edit prepares");
        assert_eq!(prepared.edits, 1);
        assert_eq!(prepared.bytes_before, source.len());
        assert_eq!(prepared.errors_before, 0);
        assert!(
            String::from_utf8(prepared.new_bytes.to_vec())
                .expect("utf-8")
                .contains("fn alpha_renamed(x: u32)")
        );
        // Nothing staged: the buffer is untouched (read-through still
        // serves disk) and the cache was never populated.
        assert!(overlay.get(&key).expect("lookup").is_none());
        assert!(cache.is_empty());

        // Missing file.
        let op = FileEdit {
            path: "gone.rs".into(),
            expect_sha: None,
            edits: vec![],
        };
        let key = VfsPath::new(tmp.path().join("gone.rs")).expect("key");
        let err = validate_one(&overlay, &cache, &registry, &mut worker, &op, &key)
            .expect_err("missing file");
        assert!(
            matches!(
                err,
                JanitorError::Vfs(crate::vfs::VfsError::NotFound { .. })
            ),
            "got {err:?}"
        );

        // Non-UTF-8 content (E6).
        let op = FileEdit {
            path: "binary.rs".into(),
            expect_sha: None,
            edits: vec![],
        };
        let key = VfsPath::new(tmp.path().join("binary.rs")).expect("key");
        let err = validate_one(&overlay, &cache, &registry, &mut worker, &op, &key)
            .expect_err("binary content");
        assert!(
            matches!(err, JanitorError::Parse(ParseError::SourceDecoding { .. })),
            "got {err:?}"
        );

        // No enabled grammar routes the extension.
        let op = FileEdit {
            path: "notes.txt".into(),
            expect_sha: None,
            edits: vec![spec(0, 0, "x")],
        };
        let key = VfsPath::new(tmp.path().join("notes.txt")).expect("key");
        let err = validate_one(&overlay, &cache, &registry, &mut worker, &op, &key)
            .expect_err("no grammar");
        assert!(
            matches!(
                err,
                JanitorError::Parse(ParseError::NoGrammarForLang { .. })
            ),
            "got {err:?}"
        );
    }
}
