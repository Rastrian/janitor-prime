//! The density statistics (C2, PLAN-C2C §2) — novelty as a computed
//! signal and emptiness as a floor, never meaning as a judge.
//!
//! Two surfaces share this module:
//!
//! - **Windowed novelty** — `novelty(fact_i) = 1 − max_{j ∈
//!   window64(i)} Jaccard(W(i), W(j))`: how much lexical information a
//!   fact carries that the 64 facts before it did not already carry.
//!   Computed from LOG at command time, **never stored** — the log stays
//!   append-only and the statistic stays reproducible from the log alone
//!   (the same forgettable-honesty posture as C1's `tfidf-store`). Full
//!   pairwise is O(n²) and declined (PLAN-C2C §3 anti-backlog): the
//!   window makes the pass linear and misses only duplicates older than
//!   64 facts — guidance, not truth.
//! - **The nap floor** — [`floor`]: a summary of ≤
//!   [`FLOOR_MIN_DISTINCT`] distinct words over a block of ≥
//!   [`FLOOR_BLOCK_FACTS`] facts is the placeholder signature the real
//!   migration store settled 14-byte lines into ("Fatos 0-32: "), and
//!   is refused with the typed `E_NAP_LOW_DENSITY`; a summary below the
//!   [`THIN_RATIO`] distinct-word ratio against its block is accepted
//!   with a payload warning. The binary never scores *meaning* — it
//!   floors *emptiness*, and any summary with ≥
//!   [`ALWAYS_PASS_DISTINCT`] distinct words passes unconditionally, so
//!   the agent/nap loop converges by construction (PLAN-C2C §4's
//!   brick-risk exit, pinned by tests).
//!
//! Tokens are C1's one tokenizer discipline ([`crate::memory::rank::tokenize`]:
//! lowercase alphanumeric runs ≥ 2 chars — no regex, no deps), taken
//! after a deterministic bracket-prefix strip: leading `[…]` groups
//! (`[optmem #12] `, dates in brackets) are migration boilerplate that
//! inflates overlap between facts that differ everywhere else (the
//! §4 risk), so they are cut before tokenization. Words are counted as
//! a *set* — repetition is not information.

use serde::Serialize;

use crate::error::MemoryError;
use crate::memory::log::FactRecord;

/// The novelty window: the statistic looks back exactly this many facts.
pub const NOVELTY_WINDOW: usize = 64;

/// Refusal threshold: a summary with at most this many distinct words
/// over a block of at least [`FLOOR_BLOCK_FACTS`] facts is refused
/// (`E_NAP_LOW_DENSITY`) — the "Fatos 0-32: " placeholder signature.
pub const FLOOR_MIN_DISTINCT: usize = 3;

/// The floor's block size: only blocks covering at least this many facts
/// can be refused — a 2- or 4-fact block may honestly summarize to very
/// few words.
pub const FLOOR_BLOCK_FACTS: usize = 8;

/// The convergence exit (pinned): any summary with at least this many
/// distinct words always passes the floor, whatever its ratio.
pub const ALWAYS_PASS_DISTINCT: usize = 10;

/// Below this summary/block distinct-word ratio a settled summary is
/// *thin*: accepted, with a payload warning.
pub const THIN_RATIO: f64 = 0.25;

/// The `note` report's density block — the redundancy signal surfaced at
/// the moment the agent can still act on it (write-time, the P3 posture).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Density {
    /// `1 − max Jaccard against the window` — in `[0, 1]`.
    pub novelty: f64,
    /// The window length the score was computed against.
    pub window: u64,
}

/// The accepted-but-thin notice a settled summary may carry (the ratio
/// tier of the floor: a warning, never a refusal). Shape follows
/// [`crate::memory::log::LogWarning`] — stable `kind`, human sentence,
/// evidence fields.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DensityWarning {
    /// Stable machine-readable kind (`"nap_low_word_ratio"`).
    pub kind: &'static str,
    /// One-line human explanation, carrying the observed counts.
    pub message: String,
    /// Distinct words in the submitted summary.
    pub distinct_words: usize,
    /// Distinct words across the covered block's facts.
    pub source_distinct_words: usize,
    /// `distinct_words / source_distinct_words` — the ratio below
    /// [`THIN_RATIO`].
    pub ratio: f64,
}

/// Strip leading bracket groups (`[…]`, each optionally followed by
/// whitespace) from `text` — migration boilerplate prefixes, cut before
/// tokenization so they cannot inflate inter-fact overlap. Deterministic
/// byte scanning, no regex; an unclosed leading bracket strips nothing.
pub fn strip_bracket_prefix(text: &str) -> &str {
    let mut rest = text;
    while let Some(stripped) = rest.trim_start().strip_prefix('[') {
        let Some(close) = stripped.find(']') else {
            break; // unclosed group: leave the text as it stands
        };
        rest = &stripped[close + 1..];
    }
    rest
}

/// The word set `W(text)`: stripped, tokenized (C1's discipline), sorted
/// distinct — a set, not a bag: repetition is not information.
fn word_set(text: &str) -> Vec<String> {
    let mut tokens = crate::memory::rank::tokenize(strip_bracket_prefix(text));
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

/// `|a ∩ b| / |a ∪ b|` over two sorted-distinct sets. The empty/empty
/// case is 1.0 — two wordless texts are identical (carry the same, zero,
/// information), so the later one is not novel.
fn jaccard(a: &[String], b: &[String]) -> f64 {
    let (mut i, mut j, mut inter) = (0usize, 0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].as_str().cmp(b[j].as_str()) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - inter;
    if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    }
}

/// The novelty of `text` against `window` (the facts that precede it —
/// only the last [`NOVELTY_WINDOW`] of them are consulted). An empty
/// window means no precedent: the first fact is fully novel (1.0).
pub fn novelty_of_text(text: &str, window: &[FactRecord]) -> f64 {
    let start = window.len().saturating_sub(NOVELTY_WINDOW);
    let subject = word_set(text);
    let mut best = 0.0f64;
    for fact in &window[start..] {
        let similarity = jaccard(&subject, &word_set(&fact.text));
        if similarity > best {
            best = similarity;
        }
    }
    1.0 - best
}

/// The novelty of `facts[i]` within the full log — its window is the
/// [`NOVELTY_WINDOW`] facts before index `i`, per the log (not the
/// block) it lives in. `None` when `i` is out of range.
pub fn novelty_at(facts: &[FactRecord], i: usize) -> Option<f64> {
    let start = i.saturating_sub(NOVELTY_WINDOW);
    facts
        .get(i)
        .map(|fact| novelty_of_text(&fact.text, &facts[start..i]))
}

/// Judge one `nap --text` submission (for `slot`, by notation) against
/// the block it would settle:
///
/// - **refused** ([`MemoryError::NapLowDensity`], the typed
///   `E_NAP_LOW_DENSITY`) when the summary carries ≤
///   [`FLOOR_MIN_DISTINCT`] distinct words over ≥ [`FLOOR_BLOCK_FACTS`]
///   covered facts — the placeholder signature;
/// - **thin** (`Ok(Some(warning))`) when the summary/block distinct-word
///   ratio is below [`THIN_RATIO`] — accepted, never refused;
/// - **dense** (`Ok(None)`) otherwise.
///
/// The refusal tier tests the summary's own shape (≤ 3 distinct words
/// over a big block is a placeholder whatever the block says); the
/// warning tier needs an anchor — a block with no distinct words at all
/// cannot ground a ratio, so its summaries are dense. ≥
/// [`ALWAYS_PASS_DISTINCT`] distinct words can never be refused — only
/// thinned — by construction.
pub fn floor(
    slot: &str,
    block: &[FactRecord],
    text: &str,
) -> Result<Option<DensityWarning>, MemoryError> {
    let distinct_words = word_set(text).len();
    // The block's distinct vocabulary: the union of its facts' word sets.
    let mut source_words: Vec<String> =
        block.iter().flat_map(|fact| word_set(&fact.text)).collect();
    source_words.sort_unstable();
    source_words.dedup();
    let source_distinct_words = source_words.len();

    if block.len() >= FLOOR_BLOCK_FACTS && distinct_words <= FLOOR_MIN_DISTINCT {
        return Err(MemoryError::NapLowDensity {
            slot: slot.to_owned(),
            distinct_words,
            source_distinct_words,
            covered_facts: block.len(),
        });
    }
    if source_distinct_words == 0 {
        return Ok(None);
    }
    let ratio = distinct_words as f64 / source_distinct_words as f64;
    if ratio >= THIN_RATIO {
        return Ok(None);
    }
    Ok(Some(DensityWarning {
        kind: "nap_low_word_ratio",
        message: format!(
            "summary carries {distinct_words} distinct word(s) against the block's \
             {source_distinct_words} (ratio {ratio:.3} < {THIN_RATIO}) — accepted, but the line \
             is thin; a summary with at least {ALWAYS_PASS_DISTINCT} distinct words is never \
             refused"
        ),
        distinct_words,
        source_distinct_words,
        ratio,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(id: u64, text: &str) -> FactRecord {
        FactRecord {
            v: 1,
            id,
            ts: "2026-09-18T00:00:00Z".to_owned(),
            ptr: None,
            text: text.to_owned(),
            flags: Vec::new(),
            confidence: None,
        }
    }

    /// Leading bracket groups go, inner brackets stay, unclosed groups
    /// strip nothing. Whitespace after a stripped group survives (the
    /// tokenizer never sees it as a token) — pinned exactly.
    #[test]
    fn bracket_prefix_strip_is_deterministic_and_prefix_only() {
        assert_eq!(
            strip_bracket_prefix("[optmem #12] overlay rule"),
            " overlay rule"
        );
        assert_eq!(
            strip_bracket_prefix("[a] [2026-09-15] the fact"),
            " the fact"
        );
        assert_eq!(strip_bracket_prefix("[one][two] tail"), " tail");
        assert_eq!(strip_bracket_prefix("keep [x] mid"), "keep [x] mid");
        assert_eq!(strip_bracket_prefix("[ unclosed stays"), "[ unclosed stays");
        assert_eq!(strip_bracket_prefix(""), "");
    }

    /// The ∅ conventions: a wordless text against another wordless text
    /// is identical (novelty 0); against a wordful window it shares
    /// nothing (novelty 1); an empty window is no precedent (novelty 1).
    #[test]
    fn empty_word_sets_and_windows_follow_the_documented_conventions() {
        let wordless = fact(0, "9");
        let wordful = fact(1, "daemon socket timeout");
        assert_eq!(novelty_of_text("7", std::slice::from_ref(&wordless)), 0.0);
        assert_eq!(novelty_of_text("7", std::slice::from_ref(&wordful)), 1.0);
        assert_eq!(novelty_of_text("anything at all", &[]), 1.0);
    }

    /// Acceptance (3), unit leg: ten near-duplicates then one novel fact.
    /// Duplicates score < 0.2, the novel one > 0.9, and two runs produce
    /// bit-identical scores. The bracket prefix does not mask the
    /// duplication, and it does not fabricate it for distinct facts.
    #[test]
    fn near_duplicates_score_low_and_the_novel_fact_scores_high() {
        let mut facts: Vec<FactRecord> = (0..10)
            .map(|i| {
                fact(
                    i,
                    &format!("[optmem #{i}] daemon socket timeout retry attempt {i}"),
                )
            })
            .collect();
        facts.push(fact(10, "quark gluon lattice bayesian prior"));

        let run1: Vec<f64> = (0..facts.len())
            .map(|i| novelty_at(&facts, i).unwrap())
            .collect();
        let run2: Vec<f64> = (0..facts.len())
            .map(|i| novelty_at(&facts, i).unwrap())
            .collect();
        assert_eq!(run1, run2, "deterministic across runs");

        // Fact 0 has no precedent: fully novel. Facts 1..=9 repeat the
        // window's vocabulary: near zero. Fact 10 shares no token: ~1.
        assert_eq!(run1[0], 1.0);
        for score in &run1[1..10] {
            assert!(*score < 0.2, "duplicate scored {score}");
        }
        assert!(run1[10] > 0.9, "novel fact scored {}", run1[10]);
    }

    /// The window is exactly 64 facts: an identical fact at distance 64
    /// still suppresses novelty, at distance 65 it does not — the fence
    /// against both a shorter window and the declined O(n²) full pairwise.
    #[test]
    fn novelty_looks_back_exactly_sixty_four_facts() {
        let mut facts = vec![fact(0, "alpha beta gamma delta epsilon")];
        for i in 1..65u64 {
            // 64 filler facts, vocabulary disjoint from fact 0's.
            facts.push(fact(i, &format!("filler note number {i:03} arrangement")));
        }
        // Distance 65: fact 0 is OUTSIDE fact 65's window.
        facts.push(fact(65, "alpha beta gamma delta epsilon"));
        // Distance 64: fact 2 is INSIDE fact 66's window, word-for-word.
        facts.push(fact(66, "filler note number 002 arrangement"));

        let far = novelty_at(&facts, 65).unwrap();
        let near = novelty_at(&facts, 66).unwrap();
        assert!(
            far > 0.9,
            "duplicate beyond the window is still novel: {far}"
        );
        assert!(near < 0.2, "duplicate inside the window is caught: {near}");
    }

    /// The floor's refusal tier: ≤ 3 distinct words over ≥ 8 covered
    /// facts is the placeholder signature, refused with the observed
    /// counts. The migration store's exact placeholder is the fixture.
    #[test]
    fn floor_refuses_the_placeholder_signature_with_counts() {
        let block: Vec<FactRecord> = (0..32u64)
            .map(|i| fact(i, &format!("fact {i}: distinct decision delta {i}")))
            .collect();

        let err = floor("0-32", &block, "Fatos 0-32: ").expect_err("the placeholder is refused");
        let MemoryError::NapLowDensity {
            slot,
            distinct_words,
            source_distinct_words,
            covered_facts,
        } = &err
        else {
            unreachable!("the floor refuses with NapLowDensity, got {err:?}");
        };
        assert_eq!(*slot, "0-32");
        assert_eq!(*distinct_words, 2, "'fatos' + '32'");
        assert!(*source_distinct_words > 10);
        assert_eq!(*covered_facts, 32);
        assert_eq!(
            crate::error::JanitorError::from(err).code(),
            "E_NAP_LOW_DENSITY"
        );
    }

    /// The floor never judges small blocks: a one-word summary over four
    /// facts is accepted (thin warning at most — the ratio tier).
    #[test]
    fn floor_never_refuses_blocks_below_eight_facts() {
        let block: Vec<FactRecord> = (0..4u64)
            .map(|i| {
                fact(
                    i,
                    &format!("fact {i}: separate vocabulary items, well chosen"),
                )
            })
            .collect();
        let verdict = floor("0-4", &block, "ok").expect("small blocks take no floor");
        let warning = verdict.expect("but the ratio warning may fire");
        assert_eq!(warning.kind, "nap_low_word_ratio");
        assert!(warning.ratio < THIN_RATIO);
    }

    /// The convergence pin (acceptance (2)): a ≥ 10-distinct-word
    /// summary ALWAYS passes — even when its ratio against the block is
    /// far below the warning threshold, it is warned, never refused.
    #[test]
    fn ten_distinct_words_always_pass_even_below_the_ratio() {
        let block: Vec<FactRecord> = (0..32u64)
            .map(|i| {
                fact(
                    i,
                    &format!("fact {i}: separate vocabulary entry unique{i:02} chosen here"),
                )
            })
            .collect();
        // 11 distinct words, chosen to share nothing with the block.
        let text = "quark gluon lattice bayesian prior sigma delta omega kappa iota rho";
        let verdict = floor("0-32", &block, text).expect(">= 10 distinct words always passes");
        let warning = verdict.expect("the ratio tier warns instead");
        assert!(warning.distinct_words >= ALWAYS_PASS_DISTINCT);
        assert!(warning.ratio < THIN_RATIO, "and genuinely below the ratio");
    }

    /// The dense middle: a summary at or above the ratio is clean — no
    /// refusal, no warning. A wordless block cannot anchor a ratio (no
    /// warning), but its placeholder-tier summaries are still refused:
    /// the refusal tests the summary's shape, not the block's.
    #[test]
    fn floor_passes_dense_summaries_silently() {
        let block: Vec<FactRecord> = (0..8u64)
            .map(|i| fact(i, &format!("fact {i} alpha beta gamma")))
            .collect();
        // 4 distinct of the block's ~5: ratio 0.8 ≥ 0.25.
        assert_eq!(
            floor("0-8", &block, "alpha beta gamma delta").expect("dense"),
            None
        );

        let wordless: Vec<FactRecord> = (0..8u64).map(|i| fact(i, "7")).collect();
        assert_eq!(
            floor("0-8", &wordless, "numbers only, no words anywhere").expect("no ratio anchor"),
            None
        );
        assert_eq!(
            crate::error::JanitorError::from(
                floor("0-8", &wordless, "any summary here")
                    .expect_err("the refusal tier is block-independent")
            )
            .code(),
            "E_NAP_LOW_DENSITY"
        );
    }
}
