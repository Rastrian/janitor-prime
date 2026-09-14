//! Language registry (PLAN §1.4.2, §3 F3.2): the feature-gated grammar set,
//! the extension→language map, and kind resolution over `u16` kind ids.
//!
//! Hot-loop matching compares `u16` kind ids resolved ahead of time (zero
//! string compares in the matcher, F4); this module owns the name↔id edges:
//!
//! - **Resolution** (name → id) goes straight through the grammar's own
//!   symbol table (`id_for_node_kind`, named kinds only — DSL kind names are
//!   named kinds); `0` means the grammar does not know the kind and maps to
//!   the typed [`DslError::UnknownKind`] in [`LangRegistry::resolve_kind`].
//! - **Serialization** (id → name) reads the per-language [`KindTable`],
//!   seeded **at first use** and deterministically: from the grammar's own
//!   symbol table, enumerated exhaustively, plus the bundled per-grammar
//!   fixture corpus parsed through the ffi boundary (which also exercises
//!   runtime language registration — the KP-5 runtime upgrade, PLAN A.5).
//!   Never from "whichever file a scan hits first" (E5 disposition: no
//!   corpus bias, no scan-order dependence).
//!
//! One parser per language per worker is the §1.2 ownership-map shape;
//! [`LangRegistry::parser`] is the factory the F4 scan pool will draw from.
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use smol_str::SmolStr;

use crate::ast::ffi_shim::{self, Language, Node, Parser};
use crate::error::{DslError, ParseError};

/// Index into a [`LangRegistry`]; handed out only by the registry itself, so
/// it is always in range for the registry that minted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LangId(usize);

/// One enabled language: its grammar, its file extensions, and the bundled
/// fixture corpus its kind table is seeded from (E5).
pub struct LangSpec {
    name: &'static str,
    extensions: &'static [&'static str],
    language: Language,
    fixture: &'static str,
    /// Comment-class kind names excluded from `Seq` adjacency by default
    /// (ADR-0010; the KP-6-pinned rust table plus the seeded v0.1 tables
    /// for the other grammars — full conformance lands at F9).
    trivia: &'static [&'static str],
}

impl LangSpec {
    /// The canonical language name (build.rs registry order).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// File extensions (lowercase) routed to this language.
    pub fn extensions(&self) -> &'static [&'static str] {
        self.extensions
    }

    /// The registered grammar.
    pub fn language(&self) -> &Language {
        &self.language
    }

    /// Comment-class kind names for ADR-0010's default adjacency
    /// (docs/DSL.md §5 carries the same table).
    pub fn trivia_kinds(&self) -> &'static [&'static str] {
        self.trivia
    }
}

/// id→name table for serialization-time kind lookups (§1.4.2: kind names
/// never touch the hot loop). Seeded once per language; see the module docs.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct KindTable {
    names: HashMap<u16, SmolStr>,
}

impl KindTable {
    /// The kind's name, if the grammar knows the id.
    pub fn get(&self, id: u16) -> Option<&SmolStr> {
        self.names.get(&id)
    }

    /// Number of known kinds.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the table is empty (only possible if seeding ran dry).
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Iterate every id↔name pair (compile-time suggestions; deterministic
    /// order is not required, only coverage of the grammar's symbol table).
    pub fn iter(&self) -> impl Iterator<Item = (&u16, &SmolStr)> {
        self.names.iter()
    }
}

/// The registry of every grammar the build enabled (ADR-0015: one cargo
/// feature per language; with none enabled it is legitimately empty and
/// every lookup is `None`/typed error, never a panic).
pub struct LangRegistry {
    specs: Vec<LangSpec>,
    kind_tables: Vec<OnceLock<KindTable>>,
}

impl LangRegistry {
    /// Register every grammar its cargo feature gated in, in build.rs's
    /// canonical order (`rust, python, go, javascript, typescript, java, c`).
    // The per-language pushes are cfg-gated, so from any single feature
    // set's view the "first" push is conditional — the lint cannot see that.
    #[allow(clippy::vec_init_then_push)]
    pub fn new() -> Self {
        let mut specs = Vec::new();
        #[cfg(feature = "lang-rust")]
        specs.push(LangSpec {
            name: "rust",
            extensions: &["rs"],
            language: ffi_shim::rust_language(),
            fixture: include_str!("lang/fixtures/rust.rs"),
            trivia: &["line_comment", "block_comment"],
        });
        #[cfg(feature = "lang-python")]
        specs.push(LangSpec {
            name: "python",
            extensions: &["py", "pyi"],
            language: ffi_shim::python_language(),
            fixture: include_str!("lang/fixtures/python.py"),
            trivia: &["comment"],
        });
        #[cfg(feature = "lang-go")]
        specs.push(LangSpec {
            name: "go",
            extensions: &["go"],
            language: ffi_shim::go_language(),
            fixture: include_str!("lang/fixtures/go.go"),
            trivia: &["comment"],
        });
        #[cfg(feature = "lang-javascript")]
        specs.push(LangSpec {
            name: "javascript",
            extensions: &["js", "jsx", "mjs", "cjs"],
            language: ffi_shim::javascript_language(),
            fixture: include_str!("lang/fixtures/javascript.js"),
            trivia: &["comment"],
        });
        #[cfg(feature = "lang-typescript")]
        specs.push(LangSpec {
            name: "typescript",
            extensions: &["ts", "tsx"],
            language: ffi_shim::typescript_language(),
            fixture: include_str!("lang/fixtures/typescript.ts"),
            trivia: &["comment"],
        });
        #[cfg(feature = "lang-java")]
        specs.push(LangSpec {
            name: "java",
            extensions: &["java"],
            language: ffi_shim::java_language(),
            fixture: include_str!("lang/fixtures/java.java"),
            trivia: &["line_comment", "block_comment"],
        });
        #[cfg(feature = "lang-c")]
        specs.push(LangSpec {
            name: "c",
            extensions: &["c", "h"],
            language: ffi_shim::c_language(),
            fixture: include_str!("lang/fixtures/c.c"),
            trivia: &["comment"],
        });
        let kind_tables = specs.iter().map(|_| OnceLock::new()).collect();
        Self { specs, kind_tables }
    }

    /// Number of enabled grammars.
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Whether no grammar is enabled (a no-language build is valid).
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The spec for `id`, if this registry minted it.
    pub fn spec(&self, id: LangId) -> Option<&LangSpec> {
        self.specs.get(id.0)
    }

    /// Every enabled language's id, in canonical registry order — the only
    /// way outside this module to enumerate the enabled set (ids are minted
    /// here and nowhere else).
    pub fn ids(&self) -> impl Iterator<Item = LangId> {
        (0..self.specs.len()).map(LangId)
    }

    /// Resolve a file extension (case-insensitive) to its language.
    pub fn for_extension(&self, ext: &str) -> Option<LangId> {
        self.specs
            .iter()
            .position(|spec| spec.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .map(LangId)
    }

    /// Resolve a path's language from its extension (`src/lib.rs` → rust).
    /// Unknown or absent extensions are `None` — the caller decides whether
    /// that is a skip or an error.
    pub fn for_path(&self, path: &Path) -> Option<LangId> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| self.for_extension(ext))
    }

    /// The language's canonical name.
    pub fn name(&self, id: LangId) -> Option<&'static str> {
        self.spec(id).map(|spec| spec.name)
    }

    /// A fresh parser primed with the language — one per language per worker
    /// is the ownership-map shape (§1.2). `set_language` failures surface as
    /// the typed `ParseError::Setup` (the runtime leg of the KP-5 ABI fence).
    pub fn parser(&self, id: LangId) -> Result<Parser, ParseError> {
        let spec = self.spec(id).ok_or_else(|| ParseError::NoGrammarForLang {
            lang: format!("registry index {}", id.0),
        })?;
        seeded_parser(spec)
    }

    /// Resolve a (named) kind name to its `u16` id in this language;
    /// `None` when the grammar does not know the kind (`id_for_node_kind`
    /// returns 0 — A.3.9).
    pub fn kind_id(&self, id: LangId, kind: &str) -> Option<u16> {
        self.spec(id)
            .map(|spec| spec.language.id_for_node_kind(kind, true))
            .filter(|resolved| *resolved != 0)
    }

    /// [`Self::kind_id`] with the loud contract (§1.4.2 one-behavior rule):
    /// a kind unknown to the target language is the typed
    /// [`DslError::UnknownKind`], never a silent 0.
    pub fn resolve_kind(&self, id: LangId, kind: &str) -> Result<u16, DslError> {
        self.kind_id(id, kind).ok_or_else(|| DslError::UnknownKind {
            lang: self.name(id).unwrap_or("<unknown>").to_owned(),
            kind: kind.to_owned(),
            suggestion: None,
        })
    }

    /// Whether any enabled grammar knows the kind — the rule-compile-time
    /// half of the §1.4.2 behavior (unknown everywhere ⇒ the rule is wrong
    /// everywhere; known somewhere ⇒ per-file resolution decides skips).
    pub fn kind_known_in_any_enabled(&self, kind: &str) -> bool {
        self.specs
            .iter()
            .any(|spec| spec.language.id_for_node_kind(kind, true) != 0)
    }

    /// The language's kind table, seeded at first use (E5; see module docs).
    pub fn kind_table(&self, id: LangId) -> Option<&KindTable> {
        let spec = self.spec(id)?;
        let slot = self.kind_tables.get(id.0)?;
        Some(slot.get_or_init(|| build_kind_table(spec)))
    }

    /// The kind's name in this language, seeding the table at first use.
    pub fn kind_name(&self, id: LangId, kind: u16) -> Option<&SmolStr> {
        self.kind_table(id)?.get(kind)
    }
}

impl Default for LangRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Prime a parser with `spec`'s grammar, mapping setup failures to the typed
/// error (ABI drift is a CI `pin-check` matter; a runtime failure here is
/// still reported loudly, never swallowed).
fn seeded_parser(spec: &LangSpec) -> Result<Parser, ParseError> {
    let mut parser = Parser::new();
    parser
        .set_language(&spec.language)
        .map_err(|err| ParseError::Setup {
            lang: spec.name.to_owned(),
            message: err.to_string(),
        })?;
    Ok(parser)
}

/// Build the kind table for `spec` (first use). Deterministic by
/// construction: the exhaustive grammar enumeration cannot depend on input,
/// and the fixture corpus is bundled in the binary (E5 — not whichever file
/// a scan hits first). The enumeration being grammar-complete makes the
/// table corpus-independent; the fixture leg additionally warms runtime
/// registration and cross-checks real parses against the symbol table.
fn build_kind_table(spec: &LangSpec) -> KindTable {
    let mut names: HashMap<u16, SmolStr> = HashMap::new();
    for id in 0..=u16::MAX {
        if let Some(name) = spec.language.node_kind_for_id(id) {
            names.insert(id, SmolStr::from(name));
        }
    }

    match seeded_parser(spec) {
        Ok(mut parser) => match ffi_shim::parse_bytes(&mut parser, spec.fixture.as_bytes(), None) {
            Some(tree) => collect_kinds(tree.root_node(), &mut names),
            None => tracing::warn!(
                lang = spec.name,
                "fixture kind-seed parse returned no tree; table is grammar-enumeration only"
            ),
        },
        Err(err) => tracing::warn!(
            lang = spec.name,
            error = %err,
            "fixture kind-seed parser setup failed; table is grammar-enumeration only"
        ),
    }

    KindTable { names }
}

/// Record every kind id↔name pair a real parse of the fixture produces.
/// Recursion is bounded by the fixture's tree depth (dozens); the general
/// pooled traversal arrives with F3.6 and takes over the engine's walks.
/// The caller-owned cursor reused across `children` calls is the T5 flat
/// pattern (§1.2).
fn collect_kinds(node: Node<'_>, out: &mut HashMap<u16, SmolStr>) {
    out.insert(node.kind_id(), SmolStr::from(node.kind()));
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_kinds(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::JanitorError;

    #[cfg(feature = "lang-rust")]
    fn rust_id(registry: &LangRegistry) -> LangId {
        registry
            .for_extension("rs")
            .expect("lang-rust builds register the rust grammar")
    }

    /// Extension routing: canonical, case-insensitive, and `None` for
    /// unknown/absent extensions — the F4 scan language filter.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn for_extension_routes_paths_to_languages() {
        let registry = LangRegistry::new();
        let id = rust_id(&registry);
        assert_eq!(registry.name(id), Some("rust"));
        assert_eq!(registry.for_extension("RS"), Some(id));
        assert_eq!(
            registry.for_path(Path::new("/proj/src/lib.rs")),
            Some(id),
            "dotfiles with no extension are not rust"
        );
        assert_eq!(registry.for_extension("md"), None);
        assert_eq!(registry.for_path(Path::new("/proj/README")), None);
        assert_eq!(registry.for_path(Path::new("/proj/.hidden")), None);
    }

    /// An empty registry (no-language build) is valid and inert: lookups are
    /// `None`, never panics.
    #[test]
    fn empty_registry_lookups_are_none_not_panics() {
        let registry = LangRegistry::new();
        if registry.is_empty() {
            assert!(registry.for_extension("rs").is_none());
            assert!(registry.spec(LangId(0)).is_none());
            assert!(!registry.kind_known_in_any_enabled("function_item"));
        }
    }

    /// Resolution contract: known named kinds resolve to nonzero ids;
    /// unknown kinds are the typed `E_DSL_UNKNOWN_KIND` (§1.4.2's loud
    /// behavior), carrying the language and the kind.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn resolve_kind_is_typed_and_loud_for_unknown_kinds() {
        let registry = LangRegistry::new();
        let id = rust_id(&registry);

        let function_item = registry.resolve_kind(id, "function_item");
        assert!(function_item.is_ok_and(|k| k != 0));

        let err = registry
            .resolve_kind(id, "definitely_not_a_rust_kind")
            .expect_err("unknown kind must be a typed error");
        assert!(matches!(err, DslError::UnknownKind { .. }));
        let umbrella: JanitorError = err.into();
        assert_eq!(umbrella.code(), "E_DSL_UNKNOWN_KIND");
        assert_eq!(
            umbrella.details(),
            Some(serde_json::json!({ "lang": "rust", "kind": "definitely_not_a_rust_kind" }))
        );
    }

    /// The rule-compile-time check: a kind known to *some* enabled language
    /// is known at all (per-file skips are the F4 scan's decision).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn kind_known_in_any_enabled_spans_the_enabled_set() {
        let registry = LangRegistry::new();
        assert!(registry.kind_known_in_any_enabled("function_item"));
        assert!(registry.kind_known_in_any_enabled("identifier"));
        assert!(!registry.kind_known_in_any_enabled("no_such_kind_really"));
    }

    /// E5 fence: the kind table is a function of the grammar alone — two
    /// registries seeded in different usage orders produce identical tables,
    /// and every fixture-observed kind round-trips id→name. Seeding order
    /// (resolve-first vs table-first) must not matter.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn kind_tables_are_order_independent_and_round_trip() {
        let a = LangRegistry::new();
        let b = LangRegistry::new();
        let id_a = rust_id(&a);
        let id_b = rust_id(&b);

        // a: resolve kinds first, then read the table.
        let resolved: Vec<u16> = ["function_item", "identifier", "line_comment", "impl_item"]
            .iter()
            .map(|kind| {
                a.resolve_kind(id_a, kind)
                    .expect("kinds the rust grammar knows")
            })
            .collect();
        let table_a = a.kind_table(id_a).expect("table seeds at first use");

        // b: table first, resolutions after.
        let table_b = b.kind_table(id_b).expect("table seeds at first use");
        assert_eq!(
            table_a, table_b,
            "E5: seeding must not depend on usage order"
        );
        assert!(
            !table_a.is_empty() && table_a.len() > 100,
            "the rust grammar carries a real symbol table"
        );

        for (kind, id) in ["function_item", "identifier", "line_comment", "impl_item"]
            .iter()
            .zip(resolved)
        {
            let name = a.kind_name(id_a, id).expect("seeded table knows the kind");
            assert_eq!(name.as_str(), *kind, "id→name must round-trip");
        }
    }

    /// The bundled fixture corpus parses clean under its grammar — the E5
    /// corpus is sound, and this exercises runtime `set_language` for every
    /// enabled language (the KP-5 runtime upgrade, PLAN A.5).
    #[test]
    fn bundled_fixtures_parse_clean_under_their_grammars() {
        let registry = LangRegistry::new();
        for spec in 0..registry.len() {
            let id = LangId(spec);
            let name = registry.name(id).unwrap_or_default();
            let mut parser = registry.parser(id).expect("enabled grammar sets up");
            let fixture = registry.spec(id).map(|s| s.fixture).unwrap_or_default();
            let tree = ffi_shim::parse_bytes(&mut parser, fixture.as_bytes(), None)
                .expect("the bundled fixture must parse to a tree");
            assert!(
                !tree.root_node().has_error(),
                "{name}: bundled fixture must parse clean"
            );
        }
    }
}
