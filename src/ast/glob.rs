//! The B1 glob matcher: `*`, `?`, and `[…]` classes over **bytes** —
//! dependency-free by the no-new-deps posture (PLAN-v0.3 §4: "a regex crate
//! would break the no-new-deps posture and reopen the linear-time question").
//!
//! Semantics (documented in docs/DSL.md §3, pinned by the tests below):
//!
//! - `*` matches any run of bytes, including the empty run.
//! - `?` matches exactly **one byte** (byte semantics — the same units as
//!   spans and columns; a multi-byte character needs `???` per its length).
//! - `[…]` is a class over single bytes: members, inclusive ranges (`a-z`),
//!   optional leading `!` negation, `]` as the first member is a literal
//!   (`[]]` is the class `{]}`), and `-` at either end is a literal. A
//!   descending range (`[z-a]`) is not a range — the three bytes are
//!   members.
//! - No escape character: `\` is an ordinary byte.
//! - A class without a closing `]` is refused by [`Glob::new`] — a
//!   malformed pattern fails rule-compile with `E_DSL_COMPILE`, never a
//!   silent always-false filter.
//!
//! Matching is the classic two-cursor wildcard algorithm (greedy `*` with a
//! single fallback point): no recursion, no per-call allocation, and
//! `O(pattern × text)` worst case — the `O(matches × filter)` bound of the
//! filter pass (docs/DSL.md §3) holds by construction.

/// Why a pattern is not a valid glob (surfaced as `E_DSL_COMPILE` by
/// `compile::compile_rule`, with the pattern named).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobError {
    /// A `[` never met its `]`.
    UnclosedClass,
}

impl std::fmt::Display for GlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnclosedClass => {
                write!(f, "character class never closes — every [ needs a ]")
            }
        }
    }
}

/// One compiled class: negated membership over inclusive byte ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Class {
    negated: bool,
    ranges: Vec<(u8, u8)>,
}

impl Class {
    fn contains(&self, byte: u8) -> bool {
        let member = self.ranges.iter().any(|&(lo, hi)| byte >= lo && byte <= hi);
        member != self.negated
    }
}

/// One pattern piece; `Star` is handled by the matcher's fallback point, so
/// [`Piece::matches`] answers `false` for it (the single-byte question).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Literal(u8),
    AnyOne,
    Star,
    Class(Class),
}

impl Piece {
    fn matches(&self, byte: u8) -> bool {
        match self {
            Self::Literal(b) => *b == byte,
            Self::AnyOne => true,
            Self::Class(class) => class.contains(byte),
            Self::Star => false,
        }
    }
}

/// A validated glob pattern. Build once at rule-compile time; matching is a
/// pure walk (no allocation, `Send + Sync` by construction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
    pieces: Vec<Piece>,
}

impl Glob {
    /// Compile `pattern` into pieces, refusing unclosed classes.
    pub fn new(pattern: &str) -> Result<Self, GlobError> {
        let bytes = pattern.as_bytes();
        let mut pieces = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'*' => {
                    pieces.push(Piece::Star);
                    i += 1;
                }
                b'?' => {
                    pieces.push(Piece::AnyOne);
                    i += 1;
                }
                b'[' => {
                    let (class, next) = Self::parse_class(bytes, i + 1)?;
                    pieces.push(Piece::Class(class));
                    i = next;
                }
                byte => {
                    pieces.push(Piece::Literal(byte));
                    i += 1;
                }
            }
        }
        Ok(Self { pieces })
    }

    /// The class body starts at `at` (just past `[`). Grammar: optional `!`,
    /// then members until `]` — where a `]` as the FIRST member character is
    /// a literal member, `-` binds as a range only between two member bytes,
    /// and a descending "range" degrades to three literal members.
    fn parse_class(bytes: &[u8], mut at: usize) -> Result<(Class, usize), GlobError> {
        let negated = bytes.get(at) == Some(&b'!');
        if negated {
            at += 1;
        }
        let mut ranges = Vec::new();
        // `pending` is the member byte that may still turn out to be a
        // range's low end (`a` in `a-z`); `first` marks the position where
        // a `]` is a literal member rather than the close (so `[a-z]`
        // closes at its `]`, `[]]` parks the first `]` as a member, and
        // `[]` never closes).
        let mut pending: Option<u8> = None;
        let mut first = true;
        loop {
            let Some(&byte) = bytes.get(at) else {
                return Err(GlobError::UnclosedClass);
            };
            if byte == b']' && !first {
                if let Some(low) = pending.take() {
                    ranges.push((low, low));
                }
                return Ok((Class { negated, ranges }, at + 1));
            }
            first = false;
            let range_high = byte == b'-'
                && bytes
                    .get(at + 1)
                    .is_some_and(|&next| next != b']' && pending.is_some_and(|low| next >= low));
            if range_high && let Some(low) = pending.take() {
                ranges.push((low, bytes[at + 1]));
                at += 2;
            } else {
                if let Some(low) = pending.replace(byte) {
                    ranges.push((low, low));
                }
                at += 1;
            }
        }
    }

    /// Whether `text` matches: the greedy-`*`-with-one-fallback-point walk.
    /// `star_p`/`star_s` remember the last `*` and how much it has consumed;
    /// a mismatch rewinds there and lets the star eat one more byte.
    pub fn is_match(&self, text: &[u8]) -> bool {
        let pat = &self.pieces;
        let (mut p, mut s) = (0_usize, 0_usize);
        let (mut star_p, mut star_s) = (usize::MAX, 0_usize);
        while s < text.len() {
            if p < pat.len() && pat[p].matches(text[s]) {
                p += 1;
                s += 1;
            } else if p < pat.len() && matches!(pat[p], Piece::Star) {
                star_p = p;
                star_s = s;
                p += 1;
            } else if star_p != usize::MAX {
                p = star_p + 1;
                star_s += 1;
                s = star_s;
            } else {
                return false;
            }
        }
        while p < pat.len() && matches!(pat[p], Piece::Star) {
            p += 1;
        }
        p == pat.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, text: &str) -> bool {
        Glob::new(pattern)
            .expect("test pattern compiles")
            .is_match(text.as_bytes())
    }

    /// Literals, `?`, `*` (including empty and trailing runs), and the
    /// anchor cases: full-string matching, not substring search.
    #[test]
    fn literals_question_and_star() {
        assert!(matches("panic", "panic"));
        assert!(!matches("panic", "panic!"));
        assert!(!matches("pan", "panic"));
        assert!(matches("panic*", "panic!"));
        assert!(matches("panic*", "panic"));
        assert!(matches("*panic*", "assert_macro_panic_site"));
        assert!(matches("*", ""));
        assert!(matches("*", "anything at all"));
        assert!(matches("a*b*c", "abc"));
        assert!(matches("a*b*c", "a-x-b-y-c"));
        assert!(!matches("a*b*c", "a-x-b-y"));
        assert!(matches("TODO*", "TODO: fix me"));
        assert!(matches("*TODO*", "// TODO: fix me"));
        assert!(!matches("*TODO*", "// FIXME: me"));
        assert!(matches("unwrap?", "unwrap1"));
        assert!(!matches("unwrap?", "unwrap"));
        // `?` is one BYTE: a two-byte character needs `??` per its length.
        assert!(matches("α??", "αβ"));
        assert!(!matches("α?", "αβ"));
        assert!(!matches("α?", "α"));
    }

    /// Classes: members, ranges, negation, the literal-`]`-first and
    /// literal-dash edge rules, and the descending-range degradation.
    #[test]
    fn classes_members_ranges_negation() {
        assert!(matches("set[AB]", "setA"));
        assert!(!matches("set[AB]", "setC"));
        assert!(matches("[a-z]*", "lower"));
        assert!(!matches("[a-z]*", "Upper"));
        assert!(matches("[!a-z]*", "Upper"));
        assert!(!matches("[!a-z]*", "lower"));
        assert!(matches("[]x]", "]"));
        assert!(matches("[]x]", "x"));
        assert!(matches("[-x]", "-"));
        assert!(matches("[x-]", "-"));
        assert!(matches("[z-a]", "z"));
        assert!(matches("[z-a]", "a"));
        assert!(matches("[z-a]", "-"));
        assert!(!matches("[z-a]", "b"));
        assert!(matches("[0-9]*", "42isanswer"));
        assert!(!matches("??[0-9]", "42x"));
        assert!(matches("??[0-9]", "421"));
        // Classes match one byte, never zero or two.
        assert!(!matches("[0-9]", ""));
        assert!(!matches("[0-9]", "12"));
    }

    /// The classic algorithm's hard cases: overlapping stars need the
    /// fallback point to rewind the LAST star while earlier ones stay
    /// maximal; mismatch past the last star exhausts honestly.
    #[test]
    fn star_backtracking_is_correct() {
        assert!(matches("*a*b*c*", "xaxbxcx"));
        assert!(!matches("*a*b*c*", "xaxcxb"));
        assert!(matches("a*bc*de", "aXbcYde"));
        assert!(!matches("a*bc*de", "aXbcYdx"));
        assert!(matches("********", ""));
        assert!(matches("********", "anything"));
        assert!(!matches("*a", ""));
        assert!(matches("*a", "aaaa"));
        assert!(matches("*aab", "aaab"), "\"a\" + \"aab\"");
        assert!(!matches("*aba", "aab"), "no aba suffix anywhere");
        assert!(!matches("*abc", "aaaaab"));
        // Pathological shape: many stars, long non-matching text — the
        // fallback walk stays bounded (no exponential blowup by design).
        let mut text = String::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab");
        text = text.repeat(4);
        assert!(matches("*a*a*a*a*a*a*a*a*a*a*b", &text));
        assert!(!matches("*a*a*a*a*a*a*a*a*a*a*c", &text));
    }

    /// Unclosed classes are refused at construction — never a silent
    /// always-false filter; the empty class is unwritable (`[]` parks its
    /// `]` as a literal and then never closes).
    #[test]
    fn malformed_patterns_are_refused() {
        assert_eq!(Glob::new("[a").unwrap_err(), GlobError::UnclosedClass);
        assert_eq!(Glob::new("x[!a-").unwrap_err(), GlobError::UnclosedClass);
        assert_eq!(Glob::new("[]").unwrap_err(), GlobError::UnclosedClass);
        assert_eq!(Glob::new("[!]").unwrap_err(), GlobError::UnclosedClass);
        assert!(Glob::new("[]]").is_ok(), "[]] is the class containing ]");
        assert!(Glob::new("").is_ok());
        assert!(Glob::new("no classes at all").is_ok());
    }
}
