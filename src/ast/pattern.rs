//! The elaborated pattern algebra (PLAN §1.4.1, ADR-0004, F4.2).
//!
//! [`dsl::parse_rule`] produces a [`Rule`] whose `root` is a [`Pattern`]
//! tree: the BRIEF's four primitives (`Under` / `Seq` / `Capture` / `Unify`)
//! plus the two atoms they imply (`Kind` / `Any`) and the v0.1 alternation
//! `AnyOf` (the surface producer of ADR-0005's `Split` instruction — see
//! docs/DSL.md §3).
//!
//! [`NodeType`] here is the *pre-binding* form: a bare kind name. ADR-0002's
//! `(LangId, InternedStr)` pair materializes at compile time
//! ([`crate::ast::compile`]) as a per-language `u16` kind id — the DSL
//! surface never carries a language, so the pattern layer cannot pretend to.
//!
//! `Unify` parses (it is in the grammar) but never executes: a rule carrying
//! unify constraints fails rule-compile with the versioned
//! `E_UNSUPPORTED_VERSION` (ADR-0005 v0.1 scope note, ADR-0016 (6)) — never
//! a silent no-match.

use smol_str::SmolStr;

/// The sentinel kind name for a wildcard pivot/element (`_`): every node
/// matches it and no grammar has such a named kind, so it never reaches
/// kind resolution (see [`Pattern::kind_names`] and `compile::KindTest`).
pub const WILDCARD_KIND: &str = "_";

/// A node-kind reference in a rule: the bare, per-grammar kind name
/// (`function_item`). Binding to a concrete language's `u16` kind id is a
/// compile-time act (§1.4.2); `kind` stays name-shaped until then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeType {
    pub kind: SmolStr,
}

impl NodeType {
    /// A named-kind reference.
    pub fn new(kind: impl AsRef<str>) -> Self {
        Self {
            kind: SmolStr::from(kind.as_ref()),
        }
    }
}

/// The elaborated internal AST (ADR-0004: `Seq` holds pattern elements, not
/// tree-node ids — tree nodes do not exist before matching).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// Structural pivot: every node of this kind scopes the inner pattern
    /// to its own child list. `_` as the pivot kind means every node.
    Under(NodeType, Box<Pattern>),
    /// Strict in-order adjacency among the scope's (trivia-filtered, named)
    /// siblings — one element per child.
    Seq(Vec<Pattern>),
    /// Bind the span the inner pattern covers to a symbol (once per rule).
    Capture(SmolStr, Box<Pattern>),
    /// Isomorphism constraint — parsed, v0.2 (F8). Rejected at compile.
    Unify(SmolStr, SmolStr),
    /// Atom: match one node of exactly this kind.
    Kind(NodeType),
    /// Atom: match any single node (`_`).
    Any,
    /// Alternation: one of the branches (≥2). Compiles to `Split`s.
    AnyOf(Vec<Pattern>),
}

/// A complete rule: one root pattern plus its (v0.2, rejected) unify
/// constraints, as written inside the top-level `(under …)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub root: Pattern,
    pub constraints: Vec<(SmolStr, SmolStr)>,
}

impl Pattern {
    /// Whether every accepting path of this pattern consumes **exactly one**
    /// child of the enclosing scope (docs/DSL.md §3 "top-level shapes").
    ///
    /// `Under` qualifies regardless of its body — the nested body is a
    /// *condition* on the consumed child (its own scope must match), not
    /// extra consumption in this scope. A top-level single-node pattern
    /// matches individual nodes (including the root) instead of running in
    /// every node's child list; compile picks the entry mode with this.
    pub fn is_single_node(&self) -> bool {
        match self {
            Self::Under(_, _) | Self::Kind(_) | Self::Any => true,
            Self::Capture(_, inner) => inner.is_single_node(),
            Self::AnyOf(alts) => !alts.is_empty() && alts.iter().all(Pattern::is_single_node),
            Self::Seq(elems) => elems.len() == 1 && elems[0].is_single_node(),
            Self::Unify(_, _) => false,
        }
    }

    /// Every kind name the rule mentions, in first-mention order (used for
    /// the loud unknown-in-every-language check, §1.4.2 / E5). A `_` pivot
    /// is a wildcard, not a kind reference — it is skipped.
    pub fn kind_names(&self, out: &mut Vec<SmolStr>) {
        match self {
            Self::Under(node, inner) => {
                if node.kind.as_str() != WILDCARD_KIND {
                    out.push(node.kind.clone());
                }
                inner.kind_names(out);
            }
            Self::Seq(elems) | Self::AnyOf(elems) => {
                for elem in elems {
                    elem.kind_names(out);
                }
            }
            Self::Capture(_, inner) => inner.kind_names(out),
            Self::Kind(node) => out.push(node.kind.clone()),
            Self::Any | Self::Unify(_, _) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn under(kind: &str, body: Pattern) -> Pattern {
        Pattern::Under(NodeType::new(kind), Box::new(body))
    }

    /// The single-node classification that fixes the top-level entry mode:
    /// kind/wildcard/under/capture-of-single/any-of-singles qualify (with
    /// `under`'s body being a condition, not consumption); sequences of 2+
    /// and unify do not.
    #[test]
    fn single_node_classification_follows_consumption() {
        assert!(Pattern::Any.is_single_node());
        assert!(under("block", Pattern::Any).is_single_node());
        assert!(Pattern::Kind(NodeType::new("identifier")).is_single_node());
        assert!(
            Pattern::Capture(
                SmolStr::from("fn"),
                Box::new(Pattern::Kind(NodeType::new("identifier")))
            )
            .is_single_node()
        );
        assert!(
            Pattern::AnyOf(vec![
                Pattern::Kind(NodeType::new("identifier")),
                under("function_item", Pattern::Any),
            ])
            .is_single_node()
        );

        // `under`'s body is a condition on the consumed child, so a
        // multi-element body does not disqualify the *element* — but a bare
        // top-level seq of 2+ elements is not a single-node pattern.
        assert!(under("block", Pattern::Seq(vec![Pattern::Any, Pattern::Any])).is_single_node());
        assert!(Pattern::Seq(vec![Pattern::Any]).is_single_node());
        assert!(!Pattern::Seq(vec![Pattern::Any, Pattern::Any]).is_single_node());
        assert!(
            !Pattern::AnyOf(vec![]).is_single_node(),
            "empty alternation"
        );
        assert!(!Pattern::Unify(SmolStr::from("a"), SmolStr::from("b")).is_single_node());
    }

    /// First-mention kind collection drives the E5 loud check — order is
    /// part of the error quality (the first unknown kind is reported).
    #[test]
    fn kind_names_collects_in_first_mention_order() {
        let rule = under(
            "function_item",
            Pattern::Seq(vec![
                Pattern::Capture(
                    SmolStr::from("name"),
                    Box::new(Pattern::Kind(NodeType::new("identifier"))),
                ),
                Pattern::AnyOf(vec![
                    Pattern::Kind(NodeType::new("block")),
                    Pattern::Kind(NodeType::new("identifier")),
                ]),
            ]),
        );
        let mut kinds = Vec::new();
        rule.kind_names(&mut kinds);
        assert_eq!(
            kinds,
            vec!["function_item", "identifier", "block", "identifier"]
                .into_iter()
                .map(SmolStr::from)
                .collect::<Vec<_>>(),
            "first-mention order, duplicates included (compile dedups resolution)"
        );
    }
}
