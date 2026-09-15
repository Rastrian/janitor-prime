//! The multi-edit byte splice (PLAN §1.2.1 step 3, ADR-0014, §3 F5.2).
//!
//! Edits fold over the old bytes **descending by offset** — refactree's
//! proven ordering (A.4), adopted because a later edit can never shift the
//! byte indices of an earlier one: when the fold reaches edit *i*, every
//! edit already applied lives entirely at offsets `>= old_end_i`, so
//! `[start_i, old_end_i)` still addresses the original bytes the payload
//! was validated against. Ascending application would need every offset
//! re-based after each edit; descending needs none.
//!
//! The fold is pure bytes: no overlay contact, no tree contact, no locks.
//! It consumes already-validated [`Splice`]s (ascending, non-overlapping,
//! in range — `FileEdit::validate`'s output) and produces the new snapshot
//! as an `Arc<[u8]>`, the (bytes, tree) unit's byte half (Rule T2). The
//! result being valid UTF-8 is checked by the caller that knows the path
//! (E6 typed error) — a check here would be true by construction whenever
//! the payload validated against UTF-8 source, since `new_text` is a
//! `String` and validated ranges sit on character boundaries.

use std::sync::Arc;

use crate::ast::parse::Splice;
use crate::error::ParseError;

/// Splice `splices` (ascending, non-overlapping) into `old`, applying them
/// descending by offset. Each splice's range is validated against the
/// **original** length first — validation before any allocation of the
/// output, the same contract as the single-edit `splice_bytes`.
pub fn splice_descending(old: &[u8], splices: &[Splice]) -> Result<Arc<[u8]>, ParseError> {
    for splice in splices {
        splice.validate_against(old.len())?;
    }
    let mut out = old.to_vec();
    for splice in splices.iter().rev() {
        apply_descending(&mut out, splice);
    }
    Ok(Arc::from(out))
}

/// One descending application: replace `[start_byte, old_end_byte)` of
/// `out` with `new_text`. Precondition: the range was validated against
/// the fold's starting length and every previously applied splice lies
/// entirely at offsets `>= old_end_byte`, so both indices are in bounds
/// and address original bytes — which is why this needs no indexing
/// arithmetic beyond the split.
fn apply_descending(out: &mut Vec<u8>, splice: &Splice) {
    let mut tail = out.split_off(splice.start_byte);
    // tail == old[start..old_end) + everything after it; drop the replaced
    // prefix, then reassemble: head + new_text + remainder.
    tail.drain(..splice.old_end_byte - splice.start_byte);
    out.extend_from_slice(&splice.new_text);
    out.extend_from_slice(&tail);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn splice(start: usize, old_end: usize, new_text: &str) -> Splice {
        Splice {
            start_byte: start,
            old_end_byte: old_end,
            new_text: new_text.as_bytes().to_vec(),
        }
    }

    /// The single-edit case reduces to the plain replacement.
    #[test]
    fn single_edit_splices() {
        let old = b"fn a() -> u32 { 1 }".as_slice();
        let at = old
            .windows(5)
            .position(|w| w == b"u32 {")
            .unwrap_or_default();
        let out = splice_descending(old, &[splice(at, at + 3, "u64")]).expect("splices");
        assert_eq!(out.as_ref(), b"fn a() -> u64 { 1 }".as_slice());
    }

    /// The ordering claim, made falsifiable: an earlier edit that changes
    /// the length (insertion) must not disturb a later edit's ORIGINAL
    /// offsets. If the fold applied ascending, the second edit's offset
    /// would need re-basing and this test would splice into the wrong site.
    #[test]
    fn earlier_growth_does_not_shift_later_offsets() {
        let old = b"fn one() {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n".as_slice();
        let one_at = old
            .windows(6)
            .position(|w| w == b"fn one")
            .unwrap_or_default();
        let b_at = old
            .windows(7)
            .position(|w| w == b"let b =")
            .unwrap_or_default();

        // First edit grows the file by 7 bytes; second edit sits later and
        // keeps its original offset.
        let out = splice_descending(
            old,
            &[
                splice(one_at, one_at + 6, "fn one_renamed"),
                splice(b_at, b_at + 5, "let bb"),
            ],
        )
        .expect("splices");
        let text = String::from_utf8(out.to_vec()).expect("valid UTF-8");
        assert_eq!(
            text,
            "fn one_renamed() {\n    let a = 1;\n    let bb = 2;\n    a + b\n}\n"
        );
    }

    /// The mirror case: a later deletion shrinks the tail, and an earlier
    /// insertion at the very start still lands correctly; adjacent ranges
    /// (touching, non-overlapping) both apply.
    #[test]
    fn adjacent_edits_and_deletions_compose() {
        let old = b"const LIMIT_MIN: u32 = 1;".as_slice();
        let limit_at = old
            .windows(5)
            .position(|w| w == b"LIMIT")
            .unwrap_or_default();
        // Touching ranges: [limit_at, limit_at+5) and [limit_at+5, limit_at+9).
        let out = splice_descending(
            old,
            &[
                splice(limit_at, limit_at + 5, "MAX"),
                splice(limit_at + 5, limit_at + 9, "_ABS"),
            ],
        )
        .expect("splices");
        assert_eq!(out.as_ref(), b"const MAX_ABS: u32 = 1;".as_slice());

        // Pure deletion later in the file plus insertion at offset 0.
        let out = splice_descending(
            old,
            &[
                splice(0, 0, "// header\n"),
                splice(old.len() - 2, old.len(), ""),
            ],
        )
        .expect("splices");
        assert_eq!(
            out.as_ref(),
            b"// header\nconst LIMIT_MIN: u32 = ".as_slice()
        );
    }

    /// Whole-file replacement and an empty splice set are both legal.
    #[test]
    fn whole_file_and_empty_set() {
        let old = b"fn a() {}".as_slice();
        let out = splice_descending(old, &[splice(0, old.len(), "fn b() {}")]).expect("splices");
        assert_eq!(out.as_ref(), b"fn b() {}".as_slice());
        let out = splice_descending(old, &[]).expect("empty set is a no-op");
        assert_eq!(out.as_ref(), old);
    }

    /// Ranges beyond the original length are typed errors before the
    /// output is allocated (the structural half of §1.2.1 step 2).
    #[test]
    fn out_of_range_splices_are_typed_errors() {
        let old = b"fn a() {}".as_slice();
        assert!(matches!(
            splice_descending(old, &[splice(0, old.len() + 1, "x")]),
            Err(ParseError::EditRange { .. })
        ));
        assert!(matches!(
            splice_descending(old, &[splice(4, 2, "x")]),
            Err(ParseError::EditRange { .. })
        ));
    }
}
