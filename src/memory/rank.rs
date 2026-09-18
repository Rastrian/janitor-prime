//! The `--task` scorer — deterministic store-local TF-IDF (C1,
//! PLAN-C2C §2; the `estimator` in-band-declaration precedent, ADR-0011).
//!
//! Lexical, not semantic, and it says so in every payload it shapes
//! (`"scorer": "tfidf-store"`, [`SCORER`]): a PT query does not rank EN
//! facts, and "wouldblock" is not "send timeout". The competitor being
//! replaced is *position-only* recall — deterministic lexical relevance
//! strictly dominates ignoring content (PLAN-C2C §4's argument). No
//! regex, no embeddings, no network, no new deps: tokens are lowercase
//! alphanumeric runs, `idf` is `ln(1 + N/df)` over the store's own
//! facts, and the whole statistic is recomputed per wake from LOG alone
//! — reproducible, forgettable, honest.
//!
//! The ordering lives here too ([`scored_first`]) so a later tie-break
//! (C4's confidence) can slot into the serve order without touching the
//! wake pass.

use std::collections::HashMap;

use crate::memory::log::FactRecord;

/// The scorer identity declared in every ranked wake payload (the
/// `"estimator": "chars4"` precedent — the limitation is auditable
/// in-band, ADR-0011).
pub const SCORER: &str = "tfidf-store";

/// Tokens of `text`: maximal runs of alphanumeric chars, lowercased,
/// kept when at least 2 chars (single-char runs are almost pure noise —
/// hex digits, versions, initials). Unicode-aware, std-only.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut run = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            run.extend(ch.to_lowercase());
        } else if !run.is_empty() {
            push_token(&mut tokens, &run);
            run.clear();
        }
    }
    if !run.is_empty() {
        push_token(&mut tokens, &run);
    }
    tokens
}

fn push_token(tokens: &mut Vec<String>, run: &str) {
    if run.chars().count() >= 2 {
        tokens.push(run.to_owned());
    }
}

/// Document frequencies over the store's own facts: `df(t)` counts each
/// fact at most once per distinct token, `n` is the fact count — the
/// `N` and `df` of the idf formula.
#[derive(Clone, Debug)]
pub struct TermStats {
    df: HashMap<String, u64>,
    n: u64,
}

impl TermStats {
    /// Count document frequencies over `facts` (a fact's token set, not
    /// its token multiset).
    pub fn new(facts: &[FactRecord]) -> Self {
        Self::from_token_lists(&facts.iter().map(|f| tokenize(&f.text)).collect::<Vec<_>>())
    }

    /// The same count over already-tokenized texts — the ranked pass
    /// tokenizes each fact once and feeds both this and scoring from the
    /// same lists (one pass over the bytes, not two).
    pub fn from_token_lists(lists: &[Vec<String>]) -> Self {
        let mut df = HashMap::new();
        for tokens in lists {
            // A borrowed, sorted copy: distinct tokens without owning a
            // second list of Strings.
            let mut distinct: Vec<&str> = tokens.iter().map(String::as_str).collect();
            distinct.sort_unstable();
            distinct.dedup();
            for token in distinct {
                *df.entry(token.to_owned()).or_insert(0) += 1;
            }
        }
        Self {
            df,
            n: lists.len() as u64,
        }
    }

    fn idf(&self, token: &str) -> Option<f64> {
        let df = *self.df.get(token)?;
        Some((1.0 + self.n as f64 / df as f64).ln())
    }
}

/// One query against one store: the query's distinct tokens (repetition
/// in a handful of query words is noise the idf already prices) with the
/// store's document statistics — scores facts, never stores state.
#[derive(Clone, Debug)]
pub struct Query {
    terms: Vec<String>,
    stats: TermStats,
}

impl Query {
    /// Parse `query` against `facts` — the one-call form callers outside
    /// the ranked pass use.
    pub fn new(query: &str, facts: &[FactRecord]) -> Self {
        Self::over_token_lists(
            query,
            &facts.iter().map(|f| tokenize(&f.text)).collect::<Vec<_>>(),
        )
    }

    /// The same parse over pre-tokenized texts (see
    /// [`TermStats::from_token_lists`]). Drops every term the store does
    /// not carry (`df = 0` would make `idf` infinite against an all-zero
    /// `tf` — the term contributes nothing, so it is skipped, keeping
    /// the arithmetic finite).
    pub fn over_token_lists(query: &str, lists: &[Vec<String>]) -> Self {
        let stats = TermStats::from_token_lists(lists);
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        terms.retain(|token| stats.df.contains_key(token));
        Self { terms, stats }
    }

    /// `score(q, f) = Σ_{t ∈ q} idf(t) · tf(t, f)` — the query's terms
    /// summed in token order (fixed), so two runs over the same store
    /// produce bit-identical scores.
    pub fn score(&self, fact: &FactRecord) -> f64 {
        self.score_tokens(&tokenize(&fact.text))
    }

    /// [`Query::score`] over the fact's tokens (already in hand when the
    /// caller tokenized the store once).
    pub fn score_tokens(&self, tokens: &[String]) -> f64 {
        if self.terms.is_empty() {
            return 0.0;
        }
        self.terms
            .iter()
            .map(|term| {
                let tf = tokens.iter().filter(|token| *token == term).count() as f64;
                self.stats.idf(term).unwrap_or(0.0) * tf
            })
            .sum()
    }
}

/// One fact's entry in the ranked serve order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scored {
    pub id: u64,
    pub score: f64,
}

/// The serve order: score desc, ties to the newer id (present-bias
/// preserved under ranking). A named comparator — not a derived `Ord` —
/// so a later tie-break (C4's agent-asserted confidence) can slot
/// between the two keys without touching any caller. Scores are finite
/// sums of finite terms, so `partial_cmp` never sees NaN; should one
/// ever appear it reads as `Equal` and the id tie decides, keeping the
/// order total and deterministic either way.
pub fn scored_first(a: &Scored, b: &Scored) -> std::cmp::Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(b.id.cmp(&a.id))
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
        }
    }

    /// Runs, lowercasing, and the ≥ 2-char floor: punctuation and any
    /// other non-alphanumeric char breaks runs, single-char runs drop,
    /// mixed-case runs collapse to one lowercase token.
    #[test]
    fn tokenize_takes_lowercase_runs_of_two_or_more() {
        assert_eq!(
            tokenize("Daemon socket; ECONNRESET on fd 7!"),
            ["daemon", "socket", "econnreset", "on", "fd"]
        );
        assert_eq!(tokenize("a I x 9"), Vec::<String>::new());
        // `_` is not alphanumeric: separators, per the token rule.
        assert_eq!(
            tokenize("no-dot file_name 42x"),
            ["no", "dot", "file", "name", "42x"]
        );
        // Unicode runs survive as single tokens, lowercased.
        assert_eq!(tokenize("Não CONFUNDIR çã"), ["não", "confundir", "çã"]);
    }

    /// df counts facts, not occurrences: a token in one fact three times
    /// still has df 1; idf is ln(1 + N/df), monotone down in df.
    #[test]
    fn df_counts_facts_and_idf_is_ln_of_one_plus_n_over_df() {
        let facts = vec![
            fact(0, "daemon socket timeout daemon"),
            fact(1, "socket retry again"),
            fact(2, "unrelated entirely"),
            fact(3, "socket socket"),
        ];
        let stats = TermStats::new(&facts);
        // df: socket 3, daemon 1 (counted once), timeout 1, retry 1…
        assert_eq!(stats.df["socket"], 3);
        assert_eq!(stats.df["daemon"], 1);
        let idf = |token: &str| stats.idf(token);
        let daemon: f64 = (1.0 + 4.0f64).ln();
        let socket: f64 = (1.0 + 4.0f64 / 3.0).ln();
        assert!((idf("daemon").unwrap() - daemon).abs() < 1e-12);
        assert!((idf("socket").unwrap() - socket).abs() < 1e-12);
        assert!(
            idf("daemon").unwrap() > idf("socket").unwrap(),
            "rarer ⇒ heavier"
        );
        assert_eq!(idf("absent"), None);
    }

    /// score(q, f) = Σ idf(t)·tf(t, f) over the query's distinct terms;
    /// terms the store never carries contribute nothing (and are dropped
    /// at parse, keeping the arithmetic finite).
    #[test]
    fn score_sums_idf_weighted_term_frequencies() {
        let facts = vec![
            fact(0, "daemon socket timeout daemon"),
            fact(1, "socket retry"),
            fact(2, "unrelated entirely"),
        ];
        let query = Query::new("daemon socket errors", &facts);
        // "errors" is absent from the store: skipped entirely.
        let idf_daemon: f64 = (1.0 + 3.0f64).ln();
        let idf_socket: f64 = (1.0 + 3.0f64 / 2.0).ln();
        let expect =
            |tf_daemon: f64, tf_socket: f64| idf_daemon * tf_daemon + idf_socket * tf_socket;
        assert!((query.score(&facts[0]) - expect(2.0, 1.0)).abs() < 1e-12);
        assert!((query.score(&facts[1]) - expect(0.0, 1.0)).abs() < 1e-12);
        assert_eq!(query.score(&facts[2]), 0.0);
        // An all-unknown query scores everything zero.
        assert_eq!(Query::new("zzz qqq", &facts).score(&facts[0]), 0.0);
    }

    /// The serve order: score desc, ties to the newer id.
    #[test]
    fn scored_first_orders_by_score_desc_then_newer_id() {
        let mut ranked = vec![
            Scored { id: 0, score: 2.0 },
            Scored { id: 5, score: 9.0 },
            Scored { id: 3, score: 2.0 },
            Scored { id: 9, score: 2.0 },
            Scored { id: 7, score: 0.5 },
        ];
        ranked.sort_by(scored_first);
        let ids: Vec<u64> = ranked.iter().map(|s| s.id).collect();
        assert_eq!(ids, [5, 9, 3, 0, 7]);
    }
}
