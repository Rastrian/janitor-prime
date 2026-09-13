//! KP-4 (PLAN §5 KP-4, §1.6 validation semantics, risk R7): is the
//! **localized** apply gate — "no ERROR/MISSING nodes within the edited byte
//! ranges after re-parse" (§1.2.1 step 7) — an honest validity gate, or does
//! grammar error-recovery park error nodes far from the edit that caused
//! them, producing false negatives?
//!
//! A 10-file mangled corpus: each file starts valid, then one edit at a
//! known position introduces a real syntax error of a distinct class
//! (deleted brace, missing initializer, unterminated string, …). For each
//! file we count ERROR/MISSING nodes (a) anywhere in the tree vs (b)
//! restricted to the edited range mapped into new-byte coordinates — the
//! exact gate F5 would run. A **false negative** is a file where errors
//! exist somewhere but none land in the edited range; the FN rate over the
//! corpus is the honesty measurement the memo reports. The `Outcome`
//! payload (per-file error positions + distances from the edit) is what
//! turns "gate is/isn't honest" into inspectable data.

use super::{SpikeError, Splice, input_edit_for, splice};
use serde::Serialize;

/// One mangled corpus file: valid source, one erroring edit, a note on the
/// error class it represents.
struct Mangle {
    name: &'static str,
    /// Valid Rust, parse-only.
    valid: String,
    /// The edit that breaks it.
    splice: fn(valid: &[u8]) -> Result<Splice, SpikeError>,
}

/// Where an ERROR/MISSING node landed, in bytes.
struct ErrorNode {
    is_missing: bool,
    start: usize,
    end: usize,
}

/// Per-file measurement: counts under both gates plus error positions.
#[derive(Serialize)]
pub struct FileOutcome {
    name: String,
    edit_start: usize,
    edit_new_end: usize,
    errors_total: usize,
    missing_total: usize,
    errors_in_range: usize,
    missing_in_range: usize,
    /// min byte distance from the edited range to any error node (0 =
    /// adjacent/inside); `None` when no error nodes exist at all.
    min_distance: Option<usize>,
    false_negative: bool,
}

#[derive(Serialize)]
pub struct Outcome {
    files: Vec<FileOutcome>,
    files_with_real_errors: usize,
    false_negatives: usize,
    false_negative_rate: f64,
}

/// Locate `needle` once (first occurrence).
fn find(haystack: &[u8], needle: &[u8]) -> Result<usize, SpikeError> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .ok_or_else(|| SpikeError::AnchorMissing {
            needle: String::from_utf8_lossy(needle).into_owned(),
        })
}

/// Replace the first occurrence of `needle` with `replacement` (a
/// replacement shorter/longer than the needle is the edit; the byte delta
/// is what makes the error-recovery positions non-trivial).
fn replace_at(valid: &[u8], needle: &[u8], replacement: &[u8]) -> Result<Splice, SpikeError> {
    let at = find(valid, needle)?;
    Ok(Splice {
        start_byte: at,
        old_end_byte: at + needle.len(),
        new_text: replacement.to_vec(),
    })
}

/// The 10-file corpus: each `Mangle` is a distinct error class at a known
/// position. Source deliberately spans the shapes real edits hit: items,
/// statements, expressions, literals, match arms.
fn corpus() -> Vec<Mangle> {
    vec![
        Mangle {
            name: "deleted-fn-close-brace",
            valid: "fn gate_a(x: u32) -> u32 {\n    let y = x + 1;\n    y\n}\n".into(),
            // drop the fn's closing brace line entirely
            splice: |v| replace_at(v, b"    y\n}\n", b"    y\n"),
        },
        Mangle {
            name: "deleted-let-semicolon",
            valid: "fn gate_b() -> u32 {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n"
                .into(),
            // drop the first `;` (the following statement makes it a real error)
            splice: |v| replace_at(v, b"let a = 1;", b"let a = 1"),
        },
        Mangle {
            name: "deleted-call-close-paren",
            valid: "fn gate_c(v: Vec<u32>) -> u32 {\n    v.len() + v.len()\n}\n".into(),
            splice: |v| replace_at(v, b"v.len() +", b"v.len( +"),
        },
        Mangle {
            name: "missing-initializer",
            valid: "fn gate_d() -> u32 {\n    let a = 1;\n    a\n}\n".into(),
            // `let b = ;` -> MISSING expression node
            splice: |v| replace_at(v, b"let a = 1;", b"let b = ;"),
        },
        Mangle {
            name: "extra-open-brace",
            valid: "fn gate_e() -> u32 {\n    let a = 1;\n    a\n}\n".into(),
            splice: |v| replace_at(v, b"let a = 1;", b"{ let a = 1;"),
        },
        Mangle {
            name: "deleted-let-equals",
            valid: "fn gate_f() -> u32 {\n    let a = 1;\n    a\n}\n".into(),
            splice: |v| replace_at(v, b"let a = 1;", b"let a  1;"),
        },
        Mangle {
            name: "deleted-match-arrow",
            valid: "fn gate_g(n: u32) -> u32 {\n    match n {\n        0 => 0,\n        _ => 1,\n    }\n}\n"
                .into(),
            splice: |v| replace_at(v, b"0 => 0", b"0  0"),
        },
        Mangle {
            name: "unterminated-string",
            valid: "fn gate_h() -> &'static str {\n    \"closed\"\n}\n".into(),
            splice: |v| replace_at(v, b"\"closed\"", b"\"opened"),
        },
        Mangle {
            name: "deleted-array-close-bracket",
            valid: "fn gate_i() -> u32 {\n    let xs = [1, 2, 3];\n    xs.len() as u32\n}\n"
                .into(),
            splice: |v| replace_at(v, b"[1, 2, 3];", b"[1, 2, 3 ;"),
        },
        Mangle {
            name: "deleted-fn-name",
            valid: "fn gate_j() -> u32 {\n    7\n}\n".into(),
            splice: |v| replace_at(v, b"fn gate_j", b"fn "),
        },
    ]
}

/// Collect every ERROR/MISSING node in the tree (spike-scale walk: recursion
/// over children; depth is bounded by the small corpus).
fn collect_error_nodes(node: tree_sitter::Node<'_>, out: &mut Vec<ErrorNode>) {
    if node.is_error() {
        out.push(ErrorNode {
            is_missing: false,
            start: node.start_byte(),
            end: node.end_byte(),
        });
    }
    if node.is_missing() {
        out.push(ErrorNode {
            is_missing: true,
            start: node.start_byte(),
            end: node.end_byte(),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_error_nodes(child, out);
    }
}

/// Run the KP-4 experiment over the corpus.
pub fn run() -> Result<Outcome, SpikeError> {
    let mut parser = super::rust_parser()?;
    let mut files = Vec::new();

    for mangle in corpus() {
        let valid = mangle.valid.as_bytes().to_vec();
        // Sanity: the pristine file must parse clean, else the corpus is
        // wrong, not the gate.
        let base =
            crate::ast::ffi_shim::parse_bytes(&mut parser, &valid, None).ok_or_else(|| {
                SpikeError::NoTree {
                    what: mangle.name.into(),
                }
            })?;
        if base.root_node().has_error() {
            return Err(SpikeError::CorrectnessDivergence {
                detail: format!("{}: pristine source already has errors", mangle.name),
            });
        }

        let splice_spec = (mangle.splice)(&valid)?;
        let mangled = splice(&valid, &splice_spec);
        let input_edit = input_edit_for(&valid, &mangled, &splice_spec);

        // The F5 choreography shape: edit an exclusively owned old tree,
        // re-parse incrementally, then count error nodes.
        let mut tree =
            crate::ast::ffi_shim::parse_bytes(&mut parser, &valid, None).ok_or_else(|| {
                SpikeError::NoTree {
                    what: mangle.name.into(),
                }
            })?;
        tree.edit(&input_edit);
        let reparsed = crate::ast::ffi_shim::parse_bytes(&mut parser, &mangled, Some(&tree))
            .ok_or_else(|| SpikeError::NoTree {
                what: format!("{} (mangled)", mangle.name),
            })?;

        let mut errors = Vec::new();
        collect_error_nodes(reparsed.root_node(), &mut errors);

        // The edited range in NEW coordinates: [start, new_end).
        let range_start = splice_spec.start_byte;
        let range_end = splice_spec.start_byte + splice_spec.new_text.len();
        let in_range = |e: &ErrorNode| e.start < range_end && range_start < e.end;

        let errors_in_range = errors
            .iter()
            .filter(|e| !e.is_missing && in_range(e))
            .count();
        let missing_in_range = errors
            .iter()
            .filter(|e| e.is_missing && in_range(e))
            .count();
        let errors_total = errors.iter().filter(|e| !e.is_missing).count();
        let missing_total = errors.iter().filter(|e| e.is_missing).count();

        // Distance from the edited range: 0 when overlapping, otherwise the
        // gap to the nearer side (saturating: overlap is 0, never underflow).
        let min_distance = errors
            .iter()
            .map(|e| {
                if e.end <= range_start {
                    range_start - e.end
                } else {
                    e.start.saturating_sub(range_end)
                }
            })
            .min();

        files.push(FileOutcome {
            name: mangle.name.into(),
            edit_start: range_start,
            edit_new_end: range_end,
            errors_total,
            missing_total,
            errors_in_range,
            missing_in_range,
            min_distance,
            false_negative: (errors_total + missing_total) > 0
                && (errors_in_range + missing_in_range) == 0,
        });
    }

    let files_with_real_errors = files
        .iter()
        .filter(|f| f.errors_total + f.missing_total > 0)
        .count();
    let false_negatives = files.iter().filter(|f| f.false_negative).count();
    let false_negative_rate = if files_with_real_errors == 0 {
        0.0
    } else {
        false_negatives as f64 / files_with_real_errors as f64
    };

    Ok(Outcome {
        files,
        files_with_real_errors,
        false_negatives,
        false_negative_rate,
    })
}

/// KP-4 entry point: every mangled file must produce at least one
/// ERROR/MISSING node somewhere (the corpus is honest), and the per-file
/// data drives the memo's honesty verdict for the localized gate. The
/// verdict itself is a *decision*, not an assert — the spike's job is to
/// measure, and the outcome is recorded in docs/planning/spikes/KP-4.md and
/// PLAN §5. What IS asserted is that the measurement ran on a sound corpus.
#[test]
fn validity_gate_corpus_is_sound_and_measured() {
    let outcome = run().expect("corpus parses and measurement completes");

    for file in &outcome.files {
        assert!(
            file.errors_total + file.missing_total > 0,
            "{}: mangled file must produce error nodes somewhere",
            file.name
        );
    }
    assert_eq!(outcome.files.len(), 10, "the mandated 10-file corpus");

    println!(
        "KP-4 localized-gate honesty ({} files):",
        outcome.files.len()
    );
    for file in &outcome.files {
        println!(
            "{:>28}  errors {} (in-range {})  missing {} (in-range {})  min-distance {:?}  FN {}",
            file.name,
            file.errors_total,
            file.errors_in_range,
            file.missing_total,
            file.missing_in_range,
            file.min_distance,
            file.false_negative
        );
    }
    println!(
        "false negatives: {}/{} files with real errors (rate {:.2})",
        outcome.false_negatives, outcome.files_with_real_errors, outcome.false_negative_rate
    );

    // The measured contract this spike exists to establish for F5: if the
    // localized gate were fully honest, FN rate would be 0. The PLAN branch
    // (whole-file gate) is taken in the memo if it is not — recorded, not
    // assumed either way here.
    let json = serde_json::to_string_pretty(&outcome).expect("outcome serializes");
    // Test-path writes go to temp; benches/spike-kp4-outcome.json is frozen F1 evidence.
    let path = std::env::temp_dir().join("janitor-spike-kp4-outcome.json");
    std::fs::write(&path, json).expect("write outcome json");
    println!("wrote {}", path.display());
}
