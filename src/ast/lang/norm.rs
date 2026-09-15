//! Normalized-vocabulary tables (PLAN §1.4.2, F9): the four cross-language
//! classes the global layer's generic rules are phrased over — `FUNCTION`,
//! `IDENT`, `LITERAL`, `BLOCK` — mapped per language to concrete grammar
//! kind names, data files at `lang/<name>.norm.toml`.
//!
//! The mechanism is deliberately dumb and data-driven; it is the template
//! the next languages (go, typescript, …) copy: drop a `lang/<name>.norm.toml`
//! next to the others, add one `include_str!` arm to [`embedded_source`],
//! and the conformance test below fences the kind names against the
//! grammar. No code changes beyond the arm.
//!
//! **Format — a strict subset of TOML** (no TOML dependency exists in the
//! pinned set, ADR "no new deps"; the subset is enforced, not assumed):
//! a line is a blank, a `#` comment, or exactly `CLASS = ["kind", …]` with
//! `CLASS` one of the four vocabulary names, each present exactly once,
//! and every `kind` a snake_case named-kind shape. Anything else —
//! tables, strings, numbers, trailing junk — is the typed
//! [`NormError::Malformed`] with its line number; the loader never
//! guesses. The one consumer contract: kind names written here resolve
//! through [`super::LangRegistry::resolve_kind`] against the target
//! language (fenced by the conformance tests in this module).

use smol_str::SmolStr;

/// The normalized vocabulary — exactly four classes (§1.4.2); a fifth
/// class is a plan change, not a file edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NormClass {
    Function,
    Ident,
    Literal,
    Block,
}

impl NormClass {
    /// The class name as it appears in the table file (`FUNCTION`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "FUNCTION",
            Self::Ident => "IDENT",
            Self::Literal => "LITERAL",
            Self::Block => "BLOCK",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "FUNCTION" => Some(Self::Function),
            "IDENT" => Some(Self::Ident),
            "LITERAL" => Some(Self::Literal),
            "BLOCK" => Some(Self::Block),
            _ => None,
        }
    }
}

/// The classes in canonical order (file order never matters; iteration
/// order is fixed so reports and tests are deterministic).
const CLASSES: [NormClass; 4] = [
    NormClass::Function,
    NormClass::Ident,
    NormClass::Literal,
    NormClass::Block,
];

/// A parsed `lang/<name>.norm.toml`: every class mapped to its concrete
/// kind names. Dumb on purpose — no resolution happens here; resolution
/// belongs to the registry (§1.4.2's one-behavior rule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormTable {
    /// Slot per [`CLASSES`] position.
    entries: [Vec<SmolStr>; 4],
}

impl NormTable {
    /// Parse a table source under the strict subset (see module docs).
    pub fn parse(source: &str) -> Result<Self, NormError> {
        let mut entries: [Option<Vec<SmolStr>>; 4] = [None, None, None, None];
        for (number, raw) in source.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| NormError::Malformed {
                line: number + 1,
                message: "expected `CLASS = [\"kind\", …]`".to_owned(),
            })?;
            let class = NormClass::from_key(key.trim()).ok_or_else(|| NormError::Malformed {
                line: number + 1,
                message: format!("{} is not a normalized-vocabulary class", key.trim()),
            })?;
            let slot = &mut entries[index_of(class)];
            if slot.is_some() {
                return Err(NormError::Duplicate {
                    class: class.as_str().to_owned(),
                });
            }
            *slot = Some(parse_kind_list(value.trim(), number + 1)?);
        }
        let missing: Vec<&str> = CLASSES
            .iter()
            .zip(entries.iter())
            .filter(|(_, slot)| slot.is_none())
            .map(|(class, _)| class.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(NormError::Missing {
                classes: missing.join(", "),
            });
        }
        Ok(Self {
            // Unreachable `None`s are checked above; the default keeps
            // this panic-free rather than expect-flavored (Rule T6).
            entries: entries.map(|slot| slot.unwrap_or_default()),
        })
    }

    /// The concrete kind names for `class` (possibly empty — an author may
    /// genuinely have no mapping yet; the conformance test fences the
    /// non-empty classes).
    pub fn kinds(&self, class: NormClass) -> &[SmolStr] {
        &self.entries[index_of(class)]
    }

    /// Every (class, kinds) pair in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = (NormClass, &[SmolStr])> + '_ {
        CLASSES
            .iter()
            .zip(self.entries.iter())
            .map(|(class, kinds)| (*class, kinds.as_slice()))
    }
}

/// [`CLASSES`] position for `class` — the one place class→slot arithmetic
/// lives, bounded by construction (both sides enumerate the same
/// four-value type).
fn index_of(class: NormClass) -> usize {
    match class {
        NormClass::Function => 0,
        NormClass::Ident => 1,
        NormClass::Literal => 2,
        NormClass::Block => 3,
    }
}

/// Parse the `["a", "b"]` value side; strict shape, no escapes, no
/// trailing content after the closing bracket.
fn parse_kind_list(value: &str, line: usize) -> Result<Vec<SmolStr>, NormError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| NormError::Malformed {
            line,
            message: "the value must be a bracketed list of quoted kind names".to_owned(),
        })?;
    let mut kinds = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let kind = item
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .ok_or_else(|| NormError::Malformed {
                line,
                message: format!("{item} is not a double-quoted kind name"),
            })?;
        if !is_kind_shape(kind) {
            return Err(NormError::Malformed {
                line,
                message: format!("{kind} is not a named-kind shape (snake_case)"),
            });
        }
        kinds.push(SmolStr::from(kind));
    }
    if kinds.is_empty() {
        return Err(NormError::Malformed {
            line,
            message: "an empty mapping carries no information; remove the language instead"
                .to_owned(),
        });
    }
    Ok(kinds)
}

/// Named kinds are `snake_case` per every grammar we pin (tree-sitter
/// naming discipline); the shape check is a typo fence, not a policy.
fn is_kind_shape(kind: &str) -> bool {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The norm table embedded for `lang`, if this build enabled the language
/// and carries its table. One arm per language is the entire per-language
/// cost of the mechanism (see module docs).
pub fn embedded_source(lang: &str) -> Option<&'static str> {
    match lang {
        #[cfg(feature = "lang-rust")]
        "rust" => Some(include_str!("../../../lang/rust.norm.toml")),
        #[cfg(feature = "lang-python")]
        "python" => Some(include_str!("../../../lang/python.norm.toml")),
        #[cfg(feature = "lang-go")]
        "go" => Some(include_str!("../../../lang/go.norm.toml")),
        // `typescript` and `tsx` share one table: the tsx kind space is
        // the JSX-bearing superset of the same grammar family, and the
        // conformance test below resolves every kind against EACH
        // registry entry — so a kind that ever diverges between the two
        // grammars fails the fence, not a scan.
        #[cfg(feature = "lang-typescript")]
        "typescript" | "tsx" => Some(include_str!("../../../lang/typescript.norm.toml")),
        // Experimental tier (F9c) — same mechanism, fenced out of lang-all.
        #[cfg(feature = "lang-ruby")]
        "ruby" => Some(include_str!("../../../lang/ruby.norm.toml")),
        #[cfg(feature = "lang-ocaml")]
        "ocaml" => Some(include_str!("../../../lang/ocaml.norm.toml")),
        #[cfg(feature = "lang-ocaml")]
        "ocaml_interface" => Some(include_str!("../../../lang/ocaml_interface.norm.toml")),
        _ => None,
    }
}

/// Loader failures — loud and typed; a broken embedded table is a
/// build-time authoring bug the tests surface before any release.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NormError {
    #[error("norm table line {line}: {message}")]
    Malformed { line: usize, message: String },
    #[error("norm table is missing one or more classes: {classes}")]
    Missing { classes: String },
    #[error("norm table maps {class} twice")]
    Duplicate { class: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded tables parse under the strict subset and carry all
    /// four classes. Registry-driven (F9c): every language this build
    /// enabled and carries a table for is fenced — new languages join
    /// by existing, the way the conformance test below already works.
    #[test]
    fn embedded_tables_parse_complete() {
        let registry = crate::ast::lang::LangRegistry::new();
        for id in registry.ids() {
            let name = registry.name(id).expect("registered name");
            let Some(source) = embedded_source(name) else {
                continue;
            };
            let table = NormTable::parse(source).expect("embedded table must parse");
            assert_eq!(
                table.iter().filter(|(_, kinds)| !kinds.is_empty()).count(),
                4,
                "{name}: every class mapped"
            );
        }
    }

    /// The conformance fence (F9, fed by the grammar's own symbol table):
    /// every kind in every embedded table resolves against its language.
    #[test]
    fn embedded_table_kinds_resolve_against_their_grammars() {
        let registry = crate::ast::lang::LangRegistry::new();
        for id in registry.ids() {
            let name = registry.name(id).expect("registered name");
            let Some(source) = embedded_source(name) else {
                continue;
            };
            let table = NormTable::parse(source).expect("embedded table must parse");
            for (class, kinds) in table.iter() {
                assert!(
                    !kinds.is_empty(),
                    "{name}: {} must carry at least one kind",
                    class.as_str()
                );
                for kind in kinds {
                    assert!(
                        registry.resolve_kind(id, kind).is_ok(),
                        "{name}: {} must resolve against the grammar",
                        kind
                    );
                }
            }
        }
    }

    /// Malformed shapes refuse loudly with the line number — the loader
    /// never guesses.
    #[test]
    fn malformed_tables_refuse_with_line_numbers() {
        let cases: &[(&str, NormError)] = &[
            (
                "FUNCTION = [\"function_item\"]\nIDENT = [identifier]\nLITERAL = [\"x\"]\nBLOCK = [\"block\"]",
                NormError::Malformed {
                    line: 2,
                    message: "identifier is not a double-quoted kind name".to_owned(),
                },
            ),
            (
                "FUNCTION = [\"function_item\"]\n[table]\nIDENT = [\"identifier\"]\nLITERAL = [\"x\"]\nBLOCK = [\"block\"]",
                NormError::Malformed {
                    line: 2,
                    message: "expected `CLASS = [\"kind\", …]`".to_owned(),
                },
            ),
            (
                "FUNCTION = [\"function_item\"]\nXIDENT = [\"identifier\"]\nLITERAL = [\"x\"]\nBLOCK = [\"block\"]",
                NormError::Malformed {
                    line: 2,
                    message: "XIDENT is not a normalized-vocabulary class".to_owned(),
                },
            ),
            (
                "FUNCTION = [\"function_item\"]\nFUNCTION = [\"f\"]\nIDENT = [\"identifier\"]\nLITERAL = [\"x\"]\nBLOCK = [\"block\"]",
                NormError::Duplicate {
                    class: "FUNCTION".to_owned(),
                },
            ),
            (
                "FUNCTION = [\"function_item\"]",
                NormError::Missing {
                    classes: "IDENT, LITERAL, BLOCK".to_owned(),
                },
            ),
            (
                "FUNCTION = [\"Function\"]\nIDENT = [\"identifier\"]\nLITERAL = [\"x\"]\nBLOCK = [\"block\"]",
                NormError::Malformed {
                    line: 1,
                    message: "Function is not a named-kind shape (snake_case)".to_owned(),
                },
            ),
            (
                "FUNCTION = []\nIDENT = [\"identifier\"]\nLITERAL = [\"x\"]\nBLOCK = [\"block\"]",
                NormError::Malformed {
                    line: 1,
                    message: "an empty mapping carries no information; remove the language instead"
                        .to_owned(),
                },
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(
                NormTable::parse(source).expect_err("must refuse"),
                *expected,
                "source: {source:?}"
            );
        }
    }

    /// A valid table round-trips through the class accessor, comments and
    /// blank lines included.
    #[test]
    fn well_formed_table_parses_and_iterates_canonically() {
        let table = NormTable::parse(
            "# leading comment\n\nFUNCTION = [\"function_definition\"]\nIDENT = [\"identifier\"]\nLITERAL = [\"integer\", \"string\"]\nBLOCK = [\"block\"]\n",
        )
        .expect("parses");
        assert_eq!(
            table.kinds(NormClass::Literal),
            ["integer", "string"].map(SmolStr::from).as_slice()
        );
        assert_eq!(
            table
                .iter()
                .map(|(class, kinds)| (class.as_str(), kinds.len()))
                .collect::<Vec<_>>(),
            [("FUNCTION", 1), ("IDENT", 1), ("LITERAL", 2), ("BLOCK", 1)]
        );
    }
}
