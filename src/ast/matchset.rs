//! Match-set semantics (ADR-0016, F4.5) — the canonical shape of the set
//! of matches a scan reports, fixed so an LLM consumer can never mistake an
//! under-specified match set for a bug.
//!
//! 1. **Local maxima per pivot.** The evaluator reports at most one match
//!    per (pivot, start position): the *longest* accepting run from that
//!    start. Alternation offering shorter runs does not multiply matches.
//! 2. **Overlap is flagged, never deduplicated.** Matches from different
//!    pivots (or adjacent starts in one pivot) may overlap in bytes; every
//!    match whose half-open span intersects another reported match's span
//!    in the same file carries `overlap: true`.
//! 3. **Canonical order.** Within a file: `(start_byte, end_byte,
//!    rule_id)`. The scan layer orders files by path, making the whole
//!    report byte-identical across rayon thread counts (the determinism
//!    fence asserts this under `RAYON_NUM_THREADS=1` vs `=8`).
//! 4. **Capture priority.** When several accepting paths bind the same
//!    symbol, the earliest-position binding wins — implemented as
//!    Pike-VM-style thread priority: the evaluator walks the canonical
//!    accepting path in program order (alternation branches left to
//!    right; the first branch preserving the maximal run is taken), and
//!    captures bind along that path. Untaken branches bind nothing.

use crate::ast::eval::MatchReport;

/// Sort one file's matches into the canonical ADR-0016 order
/// `(start_byte, end_byte, rule_id)`. Stable input order beyond the key is
/// already deterministic (preorder pivots, ascending starts).
pub fn sort_matches(reports: &mut [MatchReport]) {
    reports.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then_with(|| a.end_byte.cmp(&b.end_byte))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
}

/// Flag `overlap: true` on every match whose half-open byte span intersects
/// another match's span in the same slice. Two linear sweeps over the
/// canonically sorted set: a match overlaps iff some earlier match ends
/// after it starts, or the next match (in start order) starts before it
/// ends.
pub fn flag_overlaps(reports: &mut [MatchReport]) {
    sort_matches(reports);
    // Prefix maxima: `ends_after[i]` = any match in 0..i ends strictly
    // after match i starts (an intersection with an earlier match).
    let mut max_end: Option<usize> = None;
    for report in &mut *reports {
        let overlaps_before = max_end.is_some_and(|end| end > report.start_byte);
        report.overlap |= overlaps_before;
        if max_end.is_none_or(|cur| report.end_byte > cur) {
            max_end = Some(report.end_byte);
        }
    }
    // Suffix: sorted by start, the nearest following match is the earliest
    // possible intersector — if even it starts after this match ends,
    // nothing later can intersect.
    for i in 0..reports.len().saturating_sub(1) {
        let next_starts = reports[i + 1].start_byte;
        if next_starts < reports[i].end_byte {
            reports[i].overlap = true;
            reports[i + 1].overlap = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    fn report(start: usize, end: usize, rule_id: u32) -> MatchReport {
        MatchReport {
            path: SmolStr::from("t.rs"),
            start_byte: start,
            end_byte: end,
            start_point: crate::ast::eval::PointData { row: 0, column: 0 },
            end_point: crate::ast::eval::PointData { row: 0, column: 0 },
            kind_id: 0,
            captures: Vec::new(),
            overlap: false,
            rule_id,
        }
    }

    /// Canonical order is the ADR key; identical spans keep rule-id order.
    #[test]
    fn canonical_order_is_start_end_rule() {
        let mut reports = vec![
            report(10, 20, 1),
            report(0, 5, 2),
            report(10, 12, 1),
            report(10, 12, 0),
        ];
        sort_matches(&mut reports);
        let order: Vec<(usize, usize, u32)> = reports
            .iter()
            .map(|r| (r.start_byte, r.end_byte, r.rule_id))
            .collect();
        assert_eq!(
            order,
            vec![(0, 5, 2), (10, 12, 0), (10, 12, 1), (10, 20, 1)]
        );
    }

    /// Overlap flagging: intersecting spans (nesting, partial, identical)
    /// all flag; disjoint spans stay false; the flag is idempotent under
    /// re-running.
    #[test]
    fn overlap_flags_every_intersecting_pair() {
        // [0,10) nests [2,5); [12,14) touches [14,16) at a point (no
        // intersection — half-open spans); [20,30) overlaps [25,35).
        let mut reports = vec![
            report(20, 30, 0),
            report(12, 14, 0),
            report(0, 10, 0),
            report(2, 5, 0),
            report(25, 35, 0),
            report(14, 16, 0),
        ];
        flag_overlaps(&mut reports);
        let by_span: Vec<(usize, usize, bool)> = reports
            .iter()
            .map(|r| (r.start_byte, r.end_byte, r.overlap))
            .collect();
        assert_eq!(
            by_span,
            vec![
                (0, 10, true),
                (2, 5, true),
                (12, 14, false),
                (14, 16, false),
                (20, 30, true),
                (25, 35, true),
            ],
            "touching half-open spans do not overlap; intersecting ones flag both sides"
        );

        // Idempotent: a second pass changes nothing.
        let before: Vec<bool> = reports.iter().map(|r| r.overlap).collect();
        flag_overlaps(&mut reports);
        let after: Vec<bool> = reports.iter().map(|r| r.overlap).collect();
        assert_eq!(before, after);
    }

    /// Identical spans from different pivots: both flagged, both kept —
    /// deduplication is expressly rejected by ADR-0016.
    #[test]
    fn identical_spans_are_both_kept_and_flagged() {
        let mut reports = vec![report(5, 9, 0), report(5, 9, 0)];
        flag_overlaps(&mut reports);
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|r| r.overlap));
    }
}
