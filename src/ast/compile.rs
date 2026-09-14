//! Pattern → Program compilation (PLAN §1.4.3 steps 1–2, ADR-0005, F4.3).
//!
//! **Compile, don't interpret.** A rule elaborates per target language into
//! one flat instruction vector (Thompson-style): [`Instr::Kind`] with the
//! kind pre-resolved to its `u16` id (zero string compares in the matcher —
//! §1.4.2), [`Instr::Any`], [`Instr::CapStart`]/[`Instr::CapEnd`] around
//! captured sub-patterns, [`Instr::Split`]/[`Instr::Jmp`] for alternation,
//! and [`Instr::Under`] — a scope *entry*: one consuming instruction whose
//! match test is "this child has the pivot kind AND its own child list
//! accepts the sub-program at `entry`". Sub-programs are appended after the
//! main region with their own [`Instr::Match`], so every region terminates
//! in place and control never falls through into the next region.
//!
//! Kind resolution follows the one-behavior rule (§1.4.2 / E5):
//! [`validate_kinds`] is the loud gate run once before scanning (a kind
//! unknown to *every* enabled grammar is the typed `E_DSL_UNKNOWN_KIND`
//! with a nearest-known-kind suggestion); [`compile_rule`] then resolves
//! against one target language and surfaces its own `UnknownKind` — the
//! scan layer's cue to skip that language's files and count them
//! (`skipped_for_language`, never a silent no-match).
//!
//! `unify` never compiles: any `(constraints (unify …))` fails here with
//! the versioned `E_UNSUPPORTED_VERSION` (ADR-0016 (6), F8/v0.2).

use smol_str::SmolStr;

use crate::ast::lang::{LangId, LangRegistry};
use crate::ast::pattern::{Pattern, Rule, WILDCARD_KIND};
use crate::envelope::SCHEMA;
use crate::error::DslError;

/// One compiled instruction. "Consuming" instructions eat exactly one child
/// of the current scope; the rest are epsilon moves. All jump targets are
/// forward — the program is acyclic per region, which is what lets the
/// evaluator's DP process `pc` in one descending pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instr {
    /// Consume one child of exactly this kind id.
    Kind(u16),
    /// Consume one child of any kind (`_`).
    Any,
    /// Consume one child of the pivot kind (`None` = any kind) whose own
    /// child list accepts the sub-program at `entry` — ADR-0005's scope
    /// entry, one level down.
    Under(Option<u16>, u32, u32),
    /// Epsilon: fork to both targets (alternation).
    Split(u32, u32),
    /// Epsilon: jump to one target.
    Jmp(u32),
    /// Epsilon: bind `symbol`'s start to the current child index.
    CapStart(u32),
    /// Epsilon: bind `symbol`'s end to the current child index (exclusive).
    CapEnd(u32),
    /// Accept. Every region (main and each `Under` sub-program) ends in its
    /// own `Match`; acceptance is identified by reaching that pc.
    Match,
}

impl Instr {
    /// Whether this instruction consumes a child of the scope.
    pub fn is_consuming(&self) -> bool {
        matches!(self, Self::Kind(_) | Self::Any | Self::Under(..))
    }
}

/// How the evaluator drives a program over a tree (docs/DSL.md §3 "top-level
/// shapes"; the classification comes from [`Pattern::is_single_node`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// `(under K …)`: every node of kind `K` is a pivot whose child list is
    /// the scope; `None` is the `_` pivot (every node).
    Pivot { kind: Option<u16> },
    /// A top-level single-node pattern: each node (root included) is tested
    /// as a one-element scope.
    Unit,
    /// A top-level multi-element pattern: every node's child list is a scope.
    EveryScope,
}

/// ADR-0010's two adjacency modes (KP-6 outcome: `Skip` is the default;
/// `--strict-trivia` restores `Strict`). Applies at every scope the matcher
/// enters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriviaPolicy {
    /// Comment-class kinds are removed from the sibling list before
    /// adjacency is evaluated.
    Skip,
    /// The sibling list is used as-is (a comment between pattern siblings
    /// breaks the sequence).
    Strict,
}

/// A rule compiled for one target language and one trivia policy. Plain
/// data (`Send + Sync` by construction) — shared across rayon workers.
#[derive(Clone, Debug)]
pub struct Program {
    instrs: Vec<Instr>,
    /// The entry mode and, for [`Entry::Pivot`], the pivot kind.
    pub entry: Entry,
    /// Symbol ids → names, for report rendering.
    pub symbols: Vec<SmolStr>,
    /// Comment-class kind ids removed from sibling lists under
    /// [`TriviaPolicy::Skip`] (empty under `Strict` — nothing to skip).
    pub trivia_skip: Vec<u16>,
    /// `(entry, exit)` per region — the main region and each `Under`
    /// sub-program — sorted by entry. The evaluator restricts a scope's
    /// DP and `Under` pre-pass to the entered region: without this, every
    /// scope would test every region's `Under` instructions against its
    /// children, making deep rules quadratic in pattern depth.
    regions: Vec<(u32, u32)>,
}

impl Program {
    /// The flat instruction vector.
    pub fn instrs(&self) -> &[Instr] {
        &self.instrs
    }

    /// Program size in instructions (the `states` factor of the
    /// `O(children × states)` bound).
    pub fn len(&self) -> usize {
        self.instrs.len()
    }

    /// Whether the program compiled to nothing (never — a rule always has
    /// at least its `Match`).
    pub fn is_empty(&self) -> bool {
        self.instrs.is_empty()
    }

    /// The inclusive exit pc of the region entered at `entry` (its
    /// `Match`); the whole region is the contiguous span `[entry, exit]`.
    pub fn region_exit(&self, entry: u32) -> u32 {
        match self.regions.binary_search_by_key(&entry, |(e, _)| *e) {
            Ok(idx) => self.regions[idx].1,
            Err(_) => self.instrs.len() as u32 - 1,
        }
    }

    /// Every `(entry, exit)` region, for tests and diagnostics.
    pub fn regions(&self) -> &[(u32, u32)] {
        &self.regions
    }
}

/// The loud, registry-wide gate (§1.4.2 / E5): every kind the rule mentions
/// must be known to at least one enabled grammar — otherwise the rule is
/// wrong everywhere and scanning with it would be a silent no-op. Carries
/// the nearest known kind as `suggestion` when one is close
/// (docs/DSL.md §7).
pub fn validate_kinds(rule: &Rule, registry: &LangRegistry) -> Result<(), DslError> {
    let mut kinds = Vec::new();
    rule.root.kind_names(&mut kinds);
    let enabled: Vec<&str> = registry.ids().filter_map(|id| registry.name(id)).collect();
    for kind in kinds {
        if !registry.kind_known_in_any_enabled(kind.as_str()) {
            return Err(DslError::UnknownKind {
                lang: if enabled.is_empty() {
                    "<no language enabled in this build>".to_owned()
                } else {
                    enabled.join(", ")
                },
                kind: kind.to_string(),
                suggestion: nearest_kind(kind.as_str(), registry),
            });
        }
    }
    Ok(())
}

/// Compile `rule` for one target language under one trivia policy. Kinds
/// unknown to *this* language surface as `UnknownKind` — the scan layer's
/// per-file skip cue ([`validate_kinds`] has already established the rule
/// is legal somewhere).
pub fn compile_rule(
    rule: &Rule,
    registry: &LangRegistry,
    lang: LangId,
    trivia: TriviaPolicy,
) -> Result<Program, DslError> {
    if !rule.constraints.is_empty() {
        // ADR-0016 (6): versioned refusal, never a silent no-match.
        return Err(DslError::UnsupportedVersion {
            feature: "unify".to_owned(),
            version: SCHEMA.to_owned(),
        });
    }

    // The top-level `(under K body)` is the ENTRY MODE, not an instruction:
    // its body is the main region; `Under` compiles to a consuming scope
    // entry only in element position.
    let entry = match &rule.root {
        Pattern::Under(node, _) => {
            let kind = if node.kind.as_str() == WILDCARD_KIND {
                None
            } else {
                Some(registry.resolve_kind(lang, node.kind.as_str())?)
            };
            Entry::Pivot { kind }
        }
        root if root.is_single_node() => Entry::Unit,
        _ => Entry::EveryScope,
    };
    let main = match &rule.root {
        Pattern::Under(_, body) => body,
        root => root,
    };

    let mut emitter = Emitter {
        instrs: Vec::new(),
        symbols: Vec::new(),
        pending: Vec::new(),
        regions: Vec::new(),
    };
    emitter.compile_element(main, registry, lang)?;
    let main_exit = emitter.emit(Instr::Match); // main region terminator
    emitter.regions.push((0, main_exit));

    // Sub-programs: FIFO for a deterministic layout; compiling a body may
    // enqueue deeper `Under`s, so the cursor walks a growing queue. Each
    // region is contiguous — a deeper `Under` scheduled from this body
    // compiles after this region's `Match`.
    let mut cursor = 0;
    while cursor < emitter.pending.len() {
        let (at, kind, body) = emitter.pending[cursor].clone();
        cursor += 1;
        let entry_pc = emitter.instrs.len() as u32;
        emitter.compile_element(&body, registry, lang)?;
        let exit_pc = emitter.emit(Instr::Match);
        emitter.instrs[at as usize] = Instr::Under(kind, entry_pc, exit_pc);
        emitter.regions.push((entry_pc, exit_pc));
    }

    let trivia_skip = match trivia {
        TriviaPolicy::Strict => Vec::new(),
        TriviaPolicy::Skip => {
            let mut ids: Vec<u16> = registry
                .spec(lang)
                .map(|spec| spec.trivia_kinds())
                .unwrap_or_default()
                .iter()
                .filter_map(|kind| registry.kind_id(lang, kind))
                .collect();
            ids.sort_unstable();
            ids
        }
    };

    Ok(Program {
        instrs: emitter.instrs,
        entry,
        symbols: emitter.symbols,
        trivia_skip,
        regions: emitter.regions,
    })
}

/// The emitter appends to one flat vector; `Under` bodies are *scheduled*
/// (placeholder in place, body compiled after the current region) so the
/// main region's control flow never runs into a sub-program.
struct Emitter {
    instrs: Vec<Instr>,
    symbols: Vec<SmolStr>,
    /// `(placeholder_pc, pivot kind, body)` — one per `Under` element.
    pending: Vec<(u32, Option<u16>, Pattern)>,
    /// `(entry, exit)` per region, appended as regions close.
    regions: Vec<(u32, u32)>,
}

impl Emitter {
    fn emit(&mut self, instr: Instr) -> u32 {
        self.instrs.push(instr);
        (self.instrs.len() - 1) as u32
    }

    fn resolve(
        &self,
        node: &crate::ast::pattern::NodeType,
        registry: &LangRegistry,
        lang: LangId,
    ) -> Result<u16, DslError> {
        registry.resolve_kind(lang, node.kind.as_str())
    }

    fn intern_symbol(&mut self, sym: &SmolStr) -> Result<u32, DslError> {
        if let Some(id) = self.symbols.iter().position(|s| s == sym) {
            return Ok(id as u32);
        }
        self.symbols.push(sym.clone());
        Ok((self.symbols.len() - 1) as u32)
    }

    /// Compile one pattern as an in-region element sequence.
    fn compile_element(
        &mut self,
        pattern: &Pattern,
        registry: &LangRegistry,
        lang: LangId,
    ) -> Result<(), DslError> {
        match pattern {
            Pattern::Kind(node) => {
                let id = self.resolve(node, registry, lang)?;
                self.emit(Instr::Kind(id));
            }
            Pattern::Any => {
                self.emit(Instr::Any);
            }
            Pattern::Capture(sym, inner) => {
                let id = self.capture_id(sym)?;
                self.emit(Instr::CapStart(id));
                self.compile_element(inner, registry, lang)?;
                self.emit(Instr::CapEnd(id));
            }
            Pattern::Seq(elems) => {
                for elem in elems {
                    self.compile_element(elem, registry, lang)?;
                }
            }
            Pattern::AnyOf(alts) => {
                let mut splits: Vec<(u32, u32)> = Vec::new(); // (split_pc, first_target)
                let mut jmps: Vec<u32> = Vec::new();
                for (i, alt) in alts.iter().enumerate() {
                    if i + 1 < alts.len() {
                        let split = self.emit(Instr::Split(0, 0));
                        splits.push((split, split + 1));
                    }
                    self.compile_element(alt, registry, lang)?;
                    if i + 1 < alts.len() {
                        jmps.push(self.emit(Instr::Jmp(0)));
                    }
                }
                let end = self.instrs.len() as u32;
                // Split second-arms point at the next alternative's split
                // (or, for the last split, at the final alternative).
                for (i, (split, first)) in splits.iter().enumerate() {
                    let second = if i + 1 < splits.len() {
                        splits[i + 1].0
                    } else {
                        jmps.last().map_or(end, |j| j + 1)
                    };
                    self.instrs[*split as usize] = Instr::Split(*first, second);
                }
                for j in jmps {
                    self.instrs[j as usize] = Instr::Jmp(end);
                }
            }
            Pattern::Under(node, body) => {
                let kind = if node.kind.as_str() == WILDCARD_KIND {
                    None
                } else {
                    Some(self.resolve(node, registry, lang)?)
                };
                let at = self.emit(Instr::Under(kind, 0, 0)); // patched after the body compiles
                self.pending.push((at, kind, (**body).clone()));
            }
            Pattern::Unify(_, _) => {
                return Err(DslError::UnsupportedVersion {
                    feature: "unify".to_owned(),
                    version: SCHEMA.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Capture symbols are rule-global and may be bound once; a duplicate
    /// name is ambiguous and fails `E_DSL_COMPILE` (docs/DSL.md §7).
    fn capture_id(&mut self, sym: &SmolStr) -> Result<u32, DslError> {
        if self.symbols.contains(sym) {
            return Err(DslError::Compile {
                message: format!(
                    "capture symbol @{sym} is bound twice — each symbol may be captured once per rule"
                ),
            });
        }
        self.intern_symbol(sym)
    }
}

/// Nearest known kind name across the enabled grammars' kind tables, within
/// edit distance 2 — the `suggestion` of the loud unknown-kind error
/// (nearest by distance, first grammar wins ties; the tables are the
/// grammar's own symbol tables, so the hint is never corpus-dependent).
fn nearest_kind(kind: &str, registry: &LangRegistry) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for id in registry.ids() {
        let Some(table) = registry.kind_table(id) else {
            continue;
        };
        for (_, name) in table.iter() {
            let d = crate::ast::dsl::edit_distance(kind, name.as_str());
            if d <= 2 && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((d, name.to_string()));
            }
        }
    }
    best.map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::dsl::parse_rule;

    #[cfg(feature = "lang-rust")]
    fn rust() -> (LangRegistry, LangId) {
        let registry = LangRegistry::new();
        let id = registry
            .for_extension("rs")
            .expect("lang-rust is the default feature set");
        (registry, id)
    }

    /// One seq element per instruction, kinds pre-resolved to ids, `Match`
    /// terminating the main region — the ADR-0005 flat shape.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn seq_compiles_to_flat_kind_instructions() {
        let (registry, lang) = rust();
        let rule = parse_rule("(under block (seq let_declaration let_declaration))").unwrap();
        let program = compile_rule(&rule, &registry, lang, TriviaPolicy::Skip).unwrap();

        let let_id = registry.resolve_kind(lang, "let_declaration").unwrap();
        let block_id = registry.resolve_kind(lang, "block").unwrap();
        assert_eq!(
            program.entry,
            Entry::Pivot {
                kind: Some(block_id)
            }
        );
        assert_eq!(
            program.instrs(),
            &[Instr::Kind(let_id), Instr::Kind(let_id), Instr::Match]
        );
        assert_eq!(
            program.trivia_skip,
            vec![
                registry.resolve_kind(lang, "line_comment").unwrap(),
                registry.resolve_kind(lang, "block_comment").unwrap(),
            ],
            "ADR-0010 default: the rust trivia table is baked into the program"
        );
        let strict = compile_rule(&rule, &registry, lang, TriviaPolicy::Strict).unwrap();
        assert!(strict.trivia_skip.is_empty());
    }

    /// Capture brackets its inner element with CapStart/CapEnd around the
    /// symbol's id; duplicates are the typed `E_DSL_COMPILE`.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn capture_brackets_and_duplicates_fail_compile() {
        let (registry, lang) = rust();
        let rule =
            parse_rule("(under function_item (seq (capture @fn identifier) block))").unwrap();
        let program = compile_rule(&rule, &registry, lang, TriviaPolicy::Skip).unwrap();
        let ident = registry.resolve_kind(lang, "identifier").unwrap();
        let block = registry.resolve_kind(lang, "block").unwrap();
        assert_eq!(
            program.instrs(),
            &[
                Instr::CapStart(0),
                Instr::Kind(ident),
                Instr::CapEnd(0),
                Instr::Kind(block),
                Instr::Match
            ]
        );
        assert_eq!(program.symbols, vec![SmolStr::from("fn")]);

        let dup = parse_rule("(under block (seq (capture @x _) (capture @x _)))").unwrap();
        let err = compile_rule(&dup, &registry, lang, TriviaPolicy::Skip).unwrap_err();
        assert!(
            matches!(err, DslError::Compile { ref message } if message.contains("@x")),
            "got {err:?}"
        );
    }

    /// Alternation compiles to the Thompson shape: forward-only Splits,
    /// first-arm = fall-through, second-arm = next alternative, Jmps to the
    /// shared end. Branches of different widths are legal (that is what
    /// local-maxima resolution exercises at eval time).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn any_of_compiles_to_thompson_splits() {
        let (registry, lang) = rust();
        let rule =
            parse_rule("(under block (seq (any-of identifier (seq identifier identifier)) _))")
                .unwrap();
        let program = compile_rule(&rule, &registry, lang, TriviaPolicy::Skip).unwrap();
        let ident = registry.resolve_kind(lang, "identifier").unwrap();
        // 0: Split(1, 3)  1: Kind   2: Jmp(5)   3: Kind 4: Kind  5: Any  6: Match
        //                    \— alt1 (1 instr)     \— alt2 (2 instrs)
        assert_eq!(
            program.instrs(),
            &[
                Instr::Split(1, 3),
                Instr::Kind(ident),
                Instr::Jmp(5),
                Instr::Kind(ident),
                Instr::Kind(ident),
                Instr::Any,
                Instr::Match,
            ]
        );
    }

    /// Nested `Under`: the element is one consuming `Under` instruction
    /// whose sub-program (body + Match) is appended after the main region,
    /// with the pivot kind pre-resolved — ADR-0005's scope entry.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn nested_under_compiles_to_a_scope_entry_instruction() {
        let (registry, lang) = rust();
        let rule =
            parse_rule("(under block (seq (under if_expression (seq _ block else_clause)) _))")
                .unwrap();
        let program = compile_rule(&rule, &registry, lang, TriviaPolicy::Skip).unwrap();
        let if_id = registry.resolve_kind(lang, "if_expression").unwrap();
        let block_id = registry.resolve_kind(lang, "block").unwrap();
        let else_id = registry.resolve_kind(lang, "else_clause").unwrap();

        // Main region: Under(...), Any, Match; then the sub-program:
        // Any, Kind(block), Kind(else), Match.
        assert_eq!(program.instrs().len(), 7);
        assert_eq!(program.instrs()[0], Instr::Under(Some(if_id), 3, 6));
        assert_eq!(program.instrs()[1], Instr::Any);
        assert_eq!(program.instrs()[2], Instr::Match);
        assert_eq!(
            &program.instrs()[3..7],
            &[
                Instr::Any,
                Instr::Kind(block_id),
                Instr::Kind(else_id),
                Instr::Match
            ]
        );
    }

    /// Entry modes follow the top-level shapes of docs/DSL.md §3: pivot
    /// kinds, wildcard pivots, single-node unit patterns, every-scope seqs.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn entry_modes_follow_the_top_level_shapes() {
        let (registry, lang) = rust();
        let cases = [
            (
                "(under function_item _)",
                Entry::Pivot {
                    kind: Some(registry.resolve_kind(lang, "function_item").unwrap()),
                },
            ),
            ("(under _ (seq _ _))", Entry::Pivot { kind: None }),
            ("identifier", Entry::Unit),
            ("_", Entry::Unit),
            ("(capture @x identifier)", Entry::Unit),
            ("(any-of identifier block)", Entry::Unit),
            (
                "(any-of identifier (seq identifier identifier))",
                Entry::EveryScope,
            ),
            ("(seq _ _)", Entry::EveryScope),
        ];
        for (src, want) in cases {
            let rule = parse_rule(src).unwrap();
            let program = compile_rule(&rule, &registry, lang, TriviaPolicy::Skip)
                .map_err(|err| format!("{src}: {err}"))
                .unwrap();
            assert_eq!(program.entry, want, "{src}");
        }
    }

    /// The E5 gate: unknown everywhere is loud (with the nearest-kind
    /// suggestion); known somewhere compiles per-language and the
    /// per-language `UnknownKind` is the scan layer's skip cue.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn kind_resolution_follows_the_one_behavior_rule() {
        let (registry, lang) = rust();
        let rule = parse_rule("(under function_itm _)").unwrap();
        let err = validate_kinds(&rule, &registry).unwrap_err();
        assert!(
            matches!(
                &err,
                DslError::UnknownKind {
                    lang,
                    kind,
                    suggestion: Some(hint),
                } if lang.split(", ").any(|l| l == "rust")
                    && kind == "function_itm"
                    && hint == "function_item"
            ),
            "nearest known kind within distance 2, got {err:?}"
        );

        // Per-language leg: a rust kind is valid for rust compilation.
        let rule = parse_rule("(under block _)").unwrap();
        assert!(validate_kinds(&rule, &registry).is_ok());
        assert!(compile_rule(&rule, &registry, lang, TriviaPolicy::Skip).is_ok());
    }

    /// `unify` is refused with the versioned error — from constraints and
    /// from a bare pattern-level unify alike (ADR-0016 (6)).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn unify_never_compiles() {
        let (registry, lang) = rust();
        let rule = parse_rule(
            "(under function_item (seq (capture @fn identifier) block) \
             (constraints (unify @fn @fn)))",
        )
        .unwrap();
        let err = compile_rule(&rule, &registry, lang, TriviaPolicy::Skip).unwrap_err();
        assert!(
            matches!(err, DslError::UnsupportedVersion { .. }),
            "got {err:?}"
        );

        let bare = Rule {
            root: Pattern::Unify(SmolStr::from("a"), SmolStr::from("b")),
            constraints: Vec::new(),
        };
        assert!(matches!(
            compile_rule(&bare, &registry, lang, TriviaPolicy::Skip).unwrap_err(),
            DslError::UnsupportedVersion { .. }
        ));
    }
}
