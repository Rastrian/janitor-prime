//! The s-expression DSL front-end (PLAN §1.4.1, ADR-0003, F4.1): a
//! dependency-free lexer + recursive-descent parser producing the
//! elaborated [`Rule`] of [`crate::ast::pattern`].
//!
//! **Error quality is product surface** (ADR-0003): the DSL's users are LLM
//! agents that *will* hallucinate syntax, so every failure is a typed
//! [`DslError`] carrying the byte position, the expected-token set, and —
//! whenever the input is close to something legal — a did-you-mean
//! suggestion good enough to retry from (docs/DSL.md §7 is the contract).
//! `(unify …)` parses but never executes: compile refuses it with the
//! versioned `E_UNSUPPORTED_VERSION` (ADR-0016 (6)), never a silent
//! no-match — the grammar accepts it, v0.1 rejects it.
//!
//! The surface (complete, docs/DSL.md §2): `under` / `seq` / `capture` /
//! `kind` / `any-of` / `_` / bare-identifier kind sugar, `;` line comments,
//! `@sym` capture symbols, and `(constraints (unify @a @b)…)`.

use smol_str::SmolStr;

use crate::ast::pattern::{NodeType, Pattern, Rule, WILDCARD_KIND};
use crate::error::DslError;

/// Operator names, as they may appear as list heads (reserved only there —
/// a bare identifier atom is always a kind name, so these also power the
/// did-you-mean suggestions).
const OPERATORS: &[&str] = &[
    "under",
    "seq",
    "capture",
    "kind",
    "any-of",
    "constraints",
    "unify",
];

/// The lexer's whole vocabulary (the `expected` set of `E_DSL_LEX`).
const LEX_TOKENS: &[&str] = &["(", ")", "@symbol", "identifier", "_"];

/// One lexed token with the byte offset it starts at (error positions are
/// byte offsets into the rule text, docs/DSL.md §7).
struct Tok {
    token: Token,
    pos: u32,
}

enum Token {
    LParen,
    RParen,
    Wildcard,
    Ident(SmolStr),
    Symbol(SmolStr),
}

/// Lex the rule text. `;` runs a comment to end of line; identifiers are
/// `[A-Za-z_][A-Za-z0-9_-]*` (grammar kinds may contain `-`, e.g.
/// javascript's `statement-block`); symbols are `@` + `[A-Za-z0-9_-]+`.
fn lex(src: &str) -> Result<Vec<Tok>, DslError> {
    let bytes = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0_usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b';' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'(' {
            toks.push(Tok {
                token: Token::LParen,
                pos: i as u32,
            });
            i += 1;
        } else if b == b')' {
            toks.push(Tok {
                token: Token::RParen,
                pos: i as u32,
            });
            i += 1;
        } else if b == b'@' {
            let start = i + 1;
            let mut end = start;
            while ident_continues(bytes, end) {
                end += 1;
            }
            if end == start {
                return Err(lex_error(i));
            }
            toks.push(Tok {
                token: Token::Symbol(SmolStr::from(&src[start..end])),
                pos: i as u32,
            });
            i = end;
        } else if b == b'_' && !ident_continues(bytes, i + 1) {
            toks.push(Tok {
                token: Token::Wildcard,
                pos: i as u32,
            });
            i += 1;
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            let mut end = i + 1;
            while ident_continues(bytes, end) {
                end += 1;
            }
            toks.push(Tok {
                token: Token::Ident(SmolStr::from(&src[start..end])),
                pos: i as u32,
            });
            i = end;
        } else {
            return Err(lex_error(i));
        }
    }
    Ok(toks)
}

fn lex_error(pos: usize) -> DslError {
    DslError::Lex {
        pos: pos as u32,
        expected: LEX_TOKENS.iter().map(|t| (*t).to_owned()).collect(),
    }
}

/// Whether `bytes[at]` continues an identifier (`[A-Za-z0-9_-]`).
fn ident_continues(bytes: &[u8], at: usize) -> bool {
    bytes
        .get(at)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

/// Parse `src` into one complete [`Rule`]. The whole input must be consumed
/// (trailing tokens are a parse error, not silently ignored).
pub fn parse_rule(src: &str) -> Result<Rule, DslError> {
    let mut parser = Parser {
        toks: lex(src)?,
        at: 0,
        constraints: Vec::new(),
    };
    let root = parser.parse_atom()?;
    if let Some(trailing) = parser.toks.get(parser.at) {
        return Err(DslError::Parse {
            pos: trailing.pos,
            expected: vec!["end of rule".to_owned()],
            suggestion: Some(format!(
                "unexpected {} after a complete rule — one rule per --rule argument",
                describe(&trailing.token)
            )),
        });
    }
    Ok(Rule {
        root,
        constraints: parser.constraints,
    })
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    /// Every `(unify @a @b)` collected from any `(under …)` body — carried
    /// on the [`Rule`] for compile to reject (`E_UNSUPPORTED_VERSION`).
    constraints: Vec<(SmolStr, SmolStr)>,
}

impl Parser {
    /// The token at the cursor, or `None` at end of input; `pos` is the
    /// cursor position either way (one past the last token at EOF).
    fn peek(&self) -> (Option<&Token>, u32) {
        match self.toks.get(self.at) {
            Some(tok) => (Some(&tok.token), tok.pos),
            None => (None, self.toks.last().map_or(0, |t| t.pos + 1)),
        }
    }

    fn bump(&mut self) {
        self.at += 1;
    }

    /// An atom: a parenthesized form, a bare kind name, or `_`.
    fn parse_atom(&mut self) -> Result<Pattern, DslError> {
        let (token, pos) = self.peek();
        match token {
            Some(Token::LParen) => self.parse_paren_atom(),
            Some(Token::Wildcard) => {
                self.bump();
                Ok(Pattern::Any)
            }
            Some(Token::Ident(name)) => {
                let name = name.clone();
                if OPERATORS.contains(&name.as_str()) {
                    return Err(DslError::Parse {
                        pos,
                        expected: vec!["identifier".to_owned(), "(".to_owned(), "_".to_owned()],
                        suggestion: Some(format!(
                            "{name} is an operator, not a kind name — use it as a list head: ({name} …)"
                        )),
                    });
                }
                self.bump();
                Ok(Pattern::Kind(NodeType::new(name)))
            }
            Some(Token::RParen) | Some(Token::Symbol(_)) | None => Err(DslError::Parse {
                pos,
                expected: vec!["identifier".to_owned(), "(".to_owned(), "_".to_owned()],
                suggestion: None,
            }),
        }
    }

    /// A parenthesized form; the cursor sits on `(`.
    fn parse_paren_atom(&mut self) -> Result<Pattern, DslError> {
        self.bump(); // consume `(`
        let (token, pos) = self.peek();
        let Some(Token::Ident(name)) = token else {
            return Err(DslError::Parse {
                pos,
                expected: OPERATORS.iter().map(|o| format!("({o}")).collect(),
                suggestion: None,
            });
        };
        let name = name.clone();
        match name.as_str() {
            "under" => self.parse_under(),
            "seq" => self.parse_seq(),
            "capture" => self.parse_capture(),
            "kind" => self.parse_kind(),
            "any-of" => self.parse_any_of(),
            "constraints" | "unify" => Err(DslError::Parse {
                pos,
                expected: OPERATORS.iter().map(|o| format!("({o}")).collect(),
                suggestion: Some(format!("{name} is only legal inside an (under …) body")),
            }),
            other => {
                let suggestion =
                    did_you_mean(other).map(|close| format!("did you mean: ({close} …)?"));
                Err(DslError::Parse {
                    pos,
                    expected: OPERATORS.iter().map(|o| format!("({o}")).collect(),
                    suggestion,
                })
            }
        }
    }

    /// `(under <kind-or-_> <atom> [<constraints>])` — exactly one body atom
    /// (multiple must wrap themselves in `seq`; the suggestion says so).
    fn parse_under(&mut self) -> Result<Pattern, DslError> {
        self.bump(); // consume the head
        let (token, pos) = self.peek();
        let node = match token {
            Some(Token::Ident(name)) if !OPERATORS.contains(&name.as_str()) => {
                let name = name.clone();
                self.bump();
                NodeType::new(name)
            }
            Some(Token::Wildcard) => {
                self.bump();
                NodeType::new(WILDCARD_KIND)
            }
            _ => {
                return Err(DslError::Parse {
                    pos,
                    expected: vec!["identifier".to_owned(), "_".to_owned()],
                    suggestion: None,
                });
            }
        };
        let body = self.parse_atom()?;
        let at_rparen = matches!(self.peek().0, Some(Token::RParen));
        // `(constraints …)` is a paren form: the cursor sits on its `(`.
        let at_constraints = {
            let toks = &self.toks;
            matches!(toks.get(self.at).map(|t| &t.token), Some(Token::LParen))
                && matches!(
                    toks.get(self.at + 1).map(|t| &t.token),
                    Some(Token::Ident(n)) if n.as_str() == "constraints"
                )
        };
        if at_constraints {
            self.parse_constraints()?;
        } else if !at_rparen {
            let (_, pos) = self.peek();
            return Err(DslError::Parse {
                pos,
                expected: vec![")".to_owned(), "(constraints …)".to_owned()],
                suggestion: Some(
                    "an (under …) body is exactly one atom — wrap multiple atoms in (seq …)"
                        .to_owned(),
                ),
            });
        }
        self.expect_rparen()?;
        Ok(Pattern::Under(node, Box::new(body)))
    }

    /// `(seq <atom>+)`.
    fn parse_seq(&mut self) -> Result<Pattern, DslError> {
        self.bump(); // consume the head
        let mut elems = Vec::new();
        while !self.at_rparen() && !self.at_end() {
            elems.push(self.parse_atom()?);
        }
        if elems.is_empty() {
            let (_, pos) = self.peek();
            return Err(DslError::Parse {
                pos,
                expected: vec!["atom".to_owned()],
                suggestion: Some("a (seq …) needs at least one element".to_owned()),
            });
        }
        self.expect_rparen()?;
        Ok(Pattern::Seq(elems))
    }

    /// `(capture @sym <atom>)`.
    fn parse_capture(&mut self) -> Result<Pattern, DslError> {
        self.bump(); // consume the head
        let (token, pos) = self.peek();
        let Some(Token::Symbol(name)) = token else {
            return Err(DslError::Parse {
                pos,
                expected: vec!["@symbol".to_owned()],
                suggestion: Some(
                    "capture symbols start with @ — e.g. (capture @fn identifier)".to_owned(),
                ),
            });
        };
        let name = name.clone();
        self.bump();
        let inner = self.parse_atom()?;
        self.expect_rparen()?;
        Ok(Pattern::Capture(name, Box::new(inner)))
    }

    /// `(kind <name>)`.
    fn parse_kind(&mut self) -> Result<Pattern, DslError> {
        self.bump(); // consume the head
        let (token, pos) = self.peek();
        let Some(Token::Ident(name)) = token else {
            return Err(DslError::Parse {
                pos,
                expected: vec!["identifier".to_owned()],
                suggestion: Some(
                    "for any single node, use the _ wildcard instead of (kind _)".to_owned(),
                ),
            });
        };
        let name = name.clone();
        self.bump();
        self.expect_rparen()?;
        Ok(Pattern::Kind(NodeType::new(name)))
    }

    /// `(any-of <atom> <atom>+)` — at least two alternatives.
    fn parse_any_of(&mut self) -> Result<Pattern, DslError> {
        self.bump(); // consume the head
        let mut alts = Vec::new();
        while !self.at_rparen() && !self.at_end() {
            alts.push(self.parse_atom()?);
        }
        if alts.len() < 2 {
            let (_, pos) = self.peek();
            return Err(DslError::Parse {
                pos,
                expected: vec!["atom".to_owned(), "atom".to_owned()],
                suggestion: Some("an (any-of …) alternates between two or more atoms".to_owned()),
            });
        }
        self.expect_rparen()?;
        Ok(Pattern::AnyOf(alts))
    }

    /// `(constraints (unify @a @b)…)` — parsed into `self.constraints`
    /// (compile rejects their presence wholesale, ADR-0016 (6)).
    fn parse_constraints(&mut self) -> Result<(), DslError> {
        self.bump(); // consume `(`
        self.bump(); // consume `constraints`
        while !self.at_rparen() && !self.at_end() {
            let (token, pos) = self.peek();
            if !matches!(token, Some(Token::LParen)) {
                return Err(DslError::Parse {
                    pos,
                    expected: vec!["(unify @a @b)".to_owned(), ")".to_owned()],
                    suggestion: None,
                });
            }
            self.bump();
            let (token, pos) = self.peek();
            if !matches!(token, Some(Token::Ident(n)) if n.as_str() == "unify") {
                return Err(DslError::Parse {
                    pos,
                    expected: vec!["unify".to_owned()],
                    suggestion: None,
                });
            }
            self.bump();
            let a = self.expect_symbol()?;
            let b = self.expect_symbol()?;
            self.expect_rparen()?;
            self.constraints.push((a, b));
        }
        self.expect_rparen()?;
        Ok(())
    }

    fn expect_symbol(&mut self) -> Result<SmolStr, DslError> {
        let (token, pos) = self.peek();
        match token {
            Some(Token::Symbol(name)) => {
                let name = name.clone();
                self.bump();
                Ok(name)
            }
            _ => Err(DslError::Parse {
                pos,
                expected: vec!["@symbol".to_owned()],
                suggestion: None,
            }),
        }
    }

    fn expect_rparen(&mut self) -> Result<(), DslError> {
        let (token, pos) = self.peek();
        match token {
            Some(Token::RParen) => {
                self.bump();
                Ok(())
            }
            _ => Err(DslError::Parse {
                pos,
                expected: vec![")".to_owned()],
                suggestion: None,
            }),
        }
    }

    fn at_rparen(&self) -> bool {
        matches!(
            self.toks.get(self.at).map(|t| &t.token),
            Some(Token::RParen)
        )
    }

    fn at_end(&self) -> bool {
        self.at >= self.toks.len()
    }
}

fn describe(token: &Token) -> &'static str {
    match token {
        Token::LParen => "\"(\"",
        Token::RParen => "\")\"",
        Token::Wildcard => "\"_\"",
        Token::Ident(_) => "identifier",
        Token::Symbol(_) => "@symbol",
    }
}

/// Did-you-mean over the operator names: the closest within edit distance 2
/// (typed errors + suggestions are the agent-retry surface, ADR-0003).
fn did_you_mean(input: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for op in OPERATORS {
        let d = edit_distance(input, op);
        if d <= 2 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, op));
        }
    }
    best.map(|(_, op)| op)
}

/// Plain two-row Levenshtein distance (no dependency; bounded input — rule
/// text is small).
fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0_usize; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five KP-6 worked examples (docs/DSL.md §9) parse to exactly the
    /// elaborated shapes the evaluator will run.
    #[test]
    fn kp6_worked_examples_parse() {
        let cases: Vec<(&str, Pattern)> = vec![
            (
                "(under block (seq (kind let_declaration) (kind let_declaration)))",
                Pattern::Under(
                    NodeType::new("block"),
                    Box::new(Pattern::Seq(vec![
                        Pattern::Kind(NodeType::new("let_declaration")),
                        Pattern::Kind(NodeType::new("let_declaration")),
                    ])),
                ),
            ),
            (
                "(under match_block (seq (kind match_arm) (kind match_arm)))",
                Pattern::Under(
                    NodeType::new("match_block"),
                    Box::new(Pattern::Seq(vec![
                        Pattern::Kind(NodeType::new("match_arm")),
                        Pattern::Kind(NodeType::new("match_arm")),
                    ])),
                ),
            ),
            (
                "(under source_file (seq (kind attribute_item) (kind function_item)))",
                Pattern::Under(
                    NodeType::new("source_file"),
                    Box::new(Pattern::Seq(vec![
                        Pattern::Kind(NodeType::new("attribute_item")),
                        Pattern::Kind(NodeType::new("function_item")),
                    ])),
                ),
            ),
            (
                "(under field_expression (seq (kind call_expression) (kind field_identifier)))",
                Pattern::Under(
                    NodeType::new("field_expression"),
                    Box::new(Pattern::Seq(vec![
                        Pattern::Kind(NodeType::new("call_expression")),
                        Pattern::Kind(NodeType::new("field_identifier")),
                    ])),
                ),
            ),
            (
                "(under if_expression (seq _ (kind block) (kind else_clause)))",
                Pattern::Under(
                    NodeType::new("if_expression"),
                    Box::new(Pattern::Seq(vec![
                        Pattern::Any,
                        Pattern::Kind(NodeType::new("block")),
                        Pattern::Kind(NodeType::new("else_clause")),
                    ])),
                ),
            ),
        ];
        for (src, want) in cases {
            let rule = parse_rule(src)
                .map_err(|err| format!("{src}: {err}"))
                .expect("KP-6 worked example parses");
            assert_eq!(rule.root, want, "{src}");
            assert!(rule.constraints.is_empty(), "{src}");
        }
    }

    /// Sugar, comments, wildcard pivots, nested under, any-of, capture.
    #[test]
    fn surface_forms_parse() {
        // bare identifier sugar + `;` comments
        let rule = parse_rule(
            "; find every fn\n(under function_item ; pivot\n (seq (capture @fn identifier)\n      block))",
        )
        .expect("comments and sugar parse");
        assert_eq!(
            rule.root,
            Pattern::Under(
                NodeType::new("function_item"),
                Box::new(Pattern::Seq(vec![
                    Pattern::Capture(
                        SmolStr::from("fn"),
                        Box::new(Pattern::Kind(NodeType::new("identifier")))
                    ),
                    Pattern::Kind(NodeType::new("block")),
                ])),
            )
        );

        // wildcard pivot and nested under (scope descent)
        let rule = parse_rule("(under _ (seq _ (under if_expression (seq _ block else_clause))))")
            .expect("wildcard pivot + nested under");
        assert_eq!(
            rule.root,
            Pattern::Under(
                NodeType::new(WILDCARD_KIND),
                Box::new(Pattern::Seq(vec![
                    Pattern::Any,
                    Pattern::Under(
                        NodeType::new("if_expression"),
                        Box::new(Pattern::Seq(vec![
                            Pattern::Any,
                            Pattern::Kind(NodeType::new("block")),
                            Pattern::Kind(NodeType::new("else_clause")),
                        ])),
                    ),
                ])),
            )
        );

        // any-of, hyphenated kind names, hyphenated symbols
        let rule =
            parse_rule("(under statement-block (any-of identifier (seq identifier identifier)))")
                .expect("hyphenated kinds and any-of");
        assert!(
            matches!(rule.root, Pattern::Under(node, _) if node.kind.as_str() == "statement-block")
        );
        let rule = parse_rule("(under block (capture @first-stmt _))").unwrap();
        assert!(
            matches!(
                rule.root,
                Pattern::Under(_, ref inner)
                    if matches!(**inner, Pattern::Capture(ref sym, _) if sym.as_str() == "first-stmt")
            ),
            "hyphenated capture symbol round-trips, got {:?}",
            rule.root
        );

        // top-level bare atom and top-level seq
        assert_eq!(
            parse_rule("identifier").unwrap().root,
            Pattern::Kind(NodeType::new("identifier"))
        );
        assert!(matches!(
            parse_rule("(seq _ _)").unwrap().root,
            Pattern::Seq(_)
        ));
    }

    /// `unify` parses and rides on the rule's constraints — compile's
    /// `E_UNSUPPORTED_VERSION` has something real to reject (ADR-0016 (6)).
    #[test]
    fn unify_parses_onto_the_rule_for_compile_to_reject() {
        let rule = parse_rule(
            "(under function_item (seq (capture @fn identifier) block) \
             (constraints (unify @fn @other)))",
        )
        .expect("the grammar accepts unify; only compile refuses");
        assert_eq!(
            rule.constraints,
            vec![(SmolStr::from("fn"), SmolStr::from("other"))]
        );

        let empty = parse_rule("(under block (seq _ _) (constraints))").unwrap();
        assert!(
            empty.constraints.is_empty(),
            "an empty constraints form is legal"
        );
    }

    /// Error quality is product surface: position, expected set, suggestion.
    #[test]
    fn typed_errors_carry_position_expected_and_suggestions() {
        // E_DSL_LEX: `#` at a known byte.
        let err = parse_rule("(under block (seq a #))").unwrap_err();
        assert!(matches!(err, DslError::Lex { pos: 20, .. }), "got {err:?}");

        // E_DSL_PARSE + did-you-mean: `sq` for `seq`.
        let err = parse_rule("(under block (sq a b))").unwrap_err();
        assert!(
            matches!(err, DslError::Parse { .. }),
            "expected E_DSL_PARSE, got {err:?}"
        );
        if let DslError::Parse {
            pos,
            expected,
            suggestion,
        } = err
        {
            assert_eq!(pos, 14, "points at the head");
            assert!(expected.contains(&"(seq".to_owned()), "{expected:?}");
            assert_eq!(suggestion.as_deref(), Some("did you mean: (seq …)?"));
        }

        // Two body atoms in `under` → the wrap-in-seq suggestion.
        let err = parse_rule("(under block (kind a) (kind b))").unwrap_err();
        assert!(
            matches!(&err, DslError::Parse { suggestion: Some(s), .. } if s.contains("wrap multiple atoms in (seq")),
            "got {err:?}"
        );

        // Missing @ on a capture symbol.
        let err = parse_rule("(under function_item (capture fn identifier))").unwrap_err();
        assert!(matches!(err, DslError::Parse { .. }), "got {err:?}");

        // Empty seq / short any-of.
        assert!(matches!(
            parse_rule("(seq)").unwrap_err(),
            DslError::Parse { .. }
        ));
        assert!(matches!(
            parse_rule("(any-of a)").unwrap_err(),
            DslError::Parse { .. }
        ));

        // Unclosed form: position at end of input.
        let err = parse_rule("(under block (seq a b").unwrap_err();
        assert!(
            matches!(err, DslError::Parse { pos: 21, ref expected, .. } if *expected == vec![")".to_owned()]),
            "got {err:?}"
        );

        // Reserved word as a bare atom.
        let err = parse_rule("(under block seq)").unwrap_err();
        assert!(
            matches!(&err, DslError::Parse { suggestion: Some(s), .. } if s.contains("use it as a list head")),
            "got {err:?}"
        );

        // Trailing tokens after a complete rule.
        assert!(matches!(
            parse_rule("identifier identifier").unwrap_err(),
            DslError::Parse { .. }
        ));
    }

    /// The did-you-mean distance gate: close names suggest, far names do not
    /// (a bogus suggestion is worse than none).
    #[test]
    fn did_you_mean_only_suggests_close_operators() {
        assert_eq!(did_you_mean("sq"), Some("seq"));
        assert_eq!(did_you_mean("Unde"), Some("under"));
        assert_eq!(did_you_mean("captrue"), Some("capture"));
        assert_eq!(did_you_mean("anyof"), Some("any-of"));
        assert_eq!(did_you_mean("zzzzzzz"), None);
    }
}
