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

pub mod norm;

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
    /// canonical order (`rust, python, go, javascript, typescript, tsx,
    /// java, c` — tsx is the one feature that registers two grammars).
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
            extensions: &["ts", "mts", "cts"],
            language: ffi_shim::typescript_language(),
            fixture: include_str!("lang/fixtures/typescript.ts"),
            // `html_comment` is measured-in at F9b (KP-6-typescript): the
            // grammar accepts Annex-B HTML-like comments (`<!-- … -->`) in
            // plain .ts, as named children between statement siblings —
            // comment-class, so ADR-0010's table needs both kinds. The
            // v0.1 seeded table said `comment` only; corrected here.
            trivia: &["comment", "html_comment"],
        });
        // tsx is a second registered language from the same cargo feature
        // (F9b decision, docs/SCHEMA.md "Language support — typescript"):
        // the typescript grammar does not parse JSX — measured, a .tsx
        // file under it yields ERROR nodes, which the apply re-parse gate
        // would misread as a broken diff — so .tsx routes to the crate's
        // JSX-bearing superset grammar, with its own kind table; its
        // trivia table additionally covers `html_comment` *inside* JSX
        // trees (between jsx_text siblings, measured in the tsx fixture).
        #[cfg(feature = "lang-typescript")]
        specs.push(LangSpec {
            name: "tsx",
            extensions: &["tsx"],
            language: ffi_shim::tsx_language(),
            fixture: include_str!("lang/fixtures/tsx.tsx"),
            trivia: &["comment", "html_comment"],
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
        // Experimental tier (F9c): appended after the stable seven so
        // registry order for any lang-all build is unchanged — the
        // pin-check --version fence stays byte-stable.
        #[cfg(feature = "lang-ruby")]
        specs.push(LangSpec {
            name: "ruby",
            extensions: &["rb"],
            language: ffi_shim::ruby_language(),
            fixture: include_str!("lang/fixtures/ruby.rb"),
            trivia: &["comment"],
        });
        // One crate, two LangIds (F9c mapping decision): .ml compiles as
        // `ocaml`, .mli as `ocaml_interface`. The crate's third grammar
        // (ocaml_type) carries no file types upstream, so it registers
        // nothing — documented in docs/SCHEMA.md.
        #[cfg(feature = "lang-ocaml")]
        specs.push(LangSpec {
            name: "ocaml",
            extensions: &["ml"],
            language: ffi_shim::ocaml_language(),
            fixture: include_str!("lang/fixtures/ocaml.ml"),
            trivia: &["comment"],
        });
        #[cfg(feature = "lang-ocaml")]
        specs.push(LangSpec {
            name: "ocaml_interface",
            extensions: &["mli"],
            language: ffi_shim::ocaml_interface_language(),
            fixture: include_str!("lang/fixtures/ocaml_interface.mli"),
            trivia: &["comment"],
        });
        // Experimental tier (F9d): appended after the stable seven so
        // registry order for any lang-all build is unchanged — the
        // pin-check --version fence stays byte-stable. One LangId routes
        // both elixir extensions: .ex and .exs are the same grammar (the
        // distinction is script-vs-compile at the BEAM level, not a
        // syntax split — unlike ocaml's .ml/.mli interface split).
        #[cfg(feature = "lang-elixir")]
        specs.push(LangSpec {
            name: "elixir",
            extensions: &["ex", "exs"],
            language: ffi_shim::elixir_language(),
            fixture: include_str!("lang/fixtures/elixir.ex"),
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

    /// The language's normalized-vocabulary table (§1.4.2), embedded at
    /// compile time from `lang/<name>.norm.toml` and parsed on demand.
    /// `None` when the build's languages carry no table for `id` (the
    /// file is the enablement); `Some(Err(_))` is a broken embedded
    /// table, which the norm-module conformance tests surface before any
    /// release.
    pub fn norm_table(&self, id: LangId) -> Option<Result<norm::NormTable, norm::NormError>> {
        let name = self.name(id)?;
        norm::embedded_source(name).map(norm::NormTable::parse)
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

    /// F9a: the python registration is exercised the same way rust's is —
    /// extension routing (.py/.pyi, case-insensitive), named-kind
    /// resolution against the real grammar, and the typed `UnknownKind`
    /// for a kind python does not know (a rust kind name is the honest
    /// probe: same shape, provably absent).
    #[test]
    #[cfg(feature = "lang-python")]
    fn python_registry_resolution_is_typed_and_routed() {
        let registry = LangRegistry::new();
        let id = registry
            .for_extension("py")
            .expect("lang-python builds register the python grammar");
        assert_eq!(registry.name(id), Some("python"));
        assert_eq!(registry.for_extension("PY"), Some(id));
        assert_eq!(registry.for_extension("pyi"), Some(id));
        assert_eq!(
            registry.for_path(Path::new("/proj/app/models.py")),
            Some(id),
        );

        // Kind names verified against tree-sitter-python 0.25.0's
        // node-types.json (the grammar's own symbol table is the source;
        // these are the norm-table and KP-6-python anchors).
        for kind in [
            "module",
            "function_definition",
            "decorated_definition",
            "decorator",
            "class_definition",
            "identifier",
            "block",
            "if_statement",
            "elif_clause",
            "else_clause",
            "import_statement",
            "import_from_statement",
            "expression_statement",
            "comment",
        ] {
            assert!(
                registry.resolve_kind(id, kind).is_ok_and(|k| k != 0),
                "python grammar must know {kind}"
            );
        }

        let err = registry
            .resolve_kind(id, "function_item")
            .expect_err("a rust kind name is unknown to the python grammar");
        assert!(matches!(err, DslError::UnknownKind { .. }));
        let umbrella: JanitorError = err.into();
        assert_eq!(umbrella.code(), "E_DSL_UNKNOWN_KIND");
        assert_eq!(
            umbrella.details(),
            Some(serde_json::json!({ "lang": "python", "kind": "function_item" }))
        );
    }

    /// F9b: the go registration is exercised the same way rust's and
    /// python's are — extension routing (.go, case-insensitive), named-kind
    /// resolution against the real grammar, and the typed `UnknownKind`
    /// for a kind go does not know (a rust kind name is the honest probe).
    #[test]
    #[cfg(feature = "lang-go")]
    fn go_registry_resolution_is_typed_and_routed() {
        let registry = LangRegistry::new();
        let id = registry
            .for_extension("go")
            .expect("lang-go builds register the go grammar");
        assert_eq!(registry.name(id), Some("go"));
        assert_eq!(registry.for_extension("GO"), Some(id));
        assert_eq!(
            registry.for_path(Path::new("/proj/pkg/widgets.go")),
            Some(id),
        );

        // Kind names verified against tree-sitter-go 0.25.0's
        // node-types.json (the grammar's own symbol table is the source;
        // these are the norm-table and KP-6-go anchors).
        for kind in [
            "source_file",
            "package_clause",
            "import_declaration",
            "import_spec_list",
            "import_spec",
            "function_declaration",
            "method_declaration",
            "identifier",
            "field_identifier",
            "type_identifier",
            "package_identifier",
            "block",
            "statement_list",
            "if_statement",
            "short_var_declaration",
            "field_declaration_list",
            "field_declaration",
            "struct_type",
            "interface_type",
            "comment",
            "int_literal",
            "interpreted_string_literal",
        ] {
            assert!(
                registry.resolve_kind(id, kind).is_ok_and(|k| k != 0),
                "go grammar must know {kind}"
            );
        }

        let err = registry
            .resolve_kind(id, "function_item")
            .expect_err("a rust kind name is unknown to the go grammar");
        assert!(matches!(err, DslError::UnknownKind { .. }));
        let umbrella: JanitorError = err.into();
        assert_eq!(umbrella.code(), "E_DSL_UNKNOWN_KIND");
        assert_eq!(
            umbrella.details(),
            Some(serde_json::json!({ "lang": "go", "kind": "function_item" }))
        );
    }

    /// F9b, E5 fence for go: the kind table is a function of the grammar
    /// alone — order-independent seeding, round-tripping ids, seeded from
    /// the bundled fixture corpus (never first-scan bias). The go fixture
    /// exercises methods with receivers, generics, grouped imports, struct
    /// tags and select/defer, so the fixture leg cross-checks a real parse.
    #[test]
    #[cfg(feature = "lang-go")]
    fn go_kind_tables_are_order_independent_and_round_trip() {
        let a = LangRegistry::new();
        let b = LangRegistry::new();
        let id_a = a.for_extension("go").expect("lang-go builds register go");
        let id_b = b.for_extension("go").expect("lang-go builds register go");

        let resolved: Vec<u16> = [
            "function_declaration",
            "method_declaration",
            "identifier",
            "block",
            "comment",
        ]
        .iter()
        .map(|kind| {
            a.resolve_kind(id_a, kind)
                .expect("kinds the go grammar knows")
        })
        .collect();
        let table_a = a.kind_table(id_a).expect("table seeds at first use");

        let table_b = b.kind_table(id_b).expect("table seeds at first use");
        assert_eq!(
            table_a, table_b,
            "E5: go seeding must not depend on usage order"
        );
        assert!(
            !table_a.is_empty() && table_a.len() > 100,
            "the go grammar carries a real symbol table (112 named kinds)"
        );

        for (kind, id) in [
            "function_declaration",
            "method_declaration",
            "identifier",
            "block",
            "comment",
        ]
        .iter()
        .zip(resolved)
        {
            let name = a.kind_name(id_a, id).expect("seeded table knows the kind");
            assert_eq!(name.as_str(), *kind, "id→name must round-trip");
        }
    }

    /// F9b: the typescript registration, and the tsx decision it forces.
    /// One cargo feature (`lang-typescript`) registers TWO languages:
    /// `.ts`/`.mts`/`.cts` → `typescript` (no JSX), `.tsx` → `tsx` (the
    /// crate's JSX-bearing grammar) — measured basis: a .tsx source under
    /// the typescript grammar yields ERROR nodes, so routing .tsx there
    /// would make every scan report junk and every valid JSX edit fail
    /// the apply re-parse gate. The routing fence asserts .tsx does NOT
    /// resolve to the typescript entry.
    #[test]
    #[cfg(feature = "lang-typescript")]
    fn typescript_and_tsx_registry_resolution_is_typed_and_routed() {
        let registry = LangRegistry::new();
        let ts = registry
            .for_extension("ts")
            .expect("lang-typescript builds register the typescript grammar");
        let tsx = registry
            .for_extension("tsx")
            .expect("…and the crate's second, JSX-bearing grammar");
        assert_ne!(ts, tsx, "tsx is its own LangId, not a second extension");
        assert_eq!(registry.name(ts), Some("typescript"));
        assert_eq!(registry.name(tsx), Some("tsx"));
        assert_eq!(registry.for_extension("TS"), Some(ts));
        assert_eq!(registry.for_extension("MTS"), Some(ts));
        assert_eq!(registry.for_extension("cts"), Some(ts));
        assert_eq!(
            registry.for_path(Path::new("/proj/app/format.ts")),
            Some(ts),
        );

        // Kind names verified against tree-sitter-typescript 0.23.2's
        // node-types.json (both grammars' symbol tables; these are the
        // norm-table and KP-6-typescript anchors).
        for kind in [
            "program",
            "function_declaration",
            "generator_function_declaration",
            "function_signature",
            "class_declaration",
            "abstract_class_declaration",
            "interface_declaration",
            "interface_body",
            "property_signature",
            "method_signature",
            "abstract_method_signature",
            "method_definition",
            "decorator",
            "arrow_function",
            "function_expression",
            "lexical_declaration",
            "variable_declarator",
            "type_annotation",
            "statement_block",
            "identifier",
            "type_identifier",
            "property_identifier",
            "import_statement",
            "export_statement",
            "comment",
            "hash_bang_line",
        ] {
            assert!(
                registry.resolve_kind(ts, kind).is_ok_and(|k| k != 0),
                "typescript grammar must know {kind}"
            );
        }

        // The tsx grammar is a superset: every typescript kind above
        // resolves there too, plus the JSX surface.
        for kind in ["jsx_element", "jsx_self_closing_element", "html_comment"] {
            assert!(
                registry.resolve_kind(tsx, kind).is_ok_and(|k| k != 0),
                "tsx grammar must know {kind}"
            );
        }

        // The honest probe in both directions: a rust kind name is
        // unknown to each grammar.
        for id in [ts, tsx] {
            let err = registry
                .resolve_kind(id, "function_item")
                .expect_err("a rust kind name is unknown to the ts grammars");
            assert!(matches!(err, DslError::UnknownKind { .. }));
            let umbrella: JanitorError = err.into();
            assert_eq!(umbrella.code(), "E_DSL_UNKNOWN_KIND");
            assert_eq!(
                umbrella.details(),
                Some(serde_json::json!({
                    "lang": registry.name(id).unwrap_or_default(),
                    "kind": "function_item"
                }))
            );
        }
    }

    /// F9b, E5 fence for typescript and tsx: kind tables are functions of
    /// their grammars alone — order-independent seeding, round-tripping
    /// ids — and the two grammars' tables genuinely differ (the tsx table
    /// carries the JSX kinds the typescript table lacks).
    #[test]
    #[cfg(feature = "lang-typescript")]
    fn typescript_and_tsx_kind_tables_are_independent_and_round_trip() {
        let a = LangRegistry::new();
        let b = LangRegistry::new();
        let ts_a = a.for_extension("ts").expect("typescript registered");
        let ts_b = b.for_extension("ts").expect("typescript registered");
        let tsx_a = a.for_extension("tsx").expect("tsx registered");

        let resolved: Vec<u16> = [
            "function_declaration",
            "identifier",
            "statement_block",
            "comment",
        ]
        .iter()
        .map(|kind| {
            a.resolve_kind(ts_a, kind)
                .expect("kinds the typescript grammar knows")
        })
        .collect();
        let table_a = a.kind_table(ts_a).expect("table seeds at first use");
        let table_b = b.kind_table(ts_b).expect("table seeds at first use");
        assert_eq!(
            table_a, table_b,
            "E5: typescript seeding must not depend on usage order"
        );
        assert!(
            !table_a.is_empty() && table_a.len() > 150,
            "the typescript grammar carries a real symbol table (183 named kinds)"
        );

        for (kind, id) in [
            "function_declaration",
            "identifier",
            "statement_block",
            "comment",
        ]
        .iter()
        .zip(resolved)
        {
            let name = a.kind_name(ts_a, id).expect("seeded table knows the kind");
            assert_eq!(name.as_str(), *kind, "id→name must round-trip");
        }

        // The two grammars' tables differ where it matters: the JSX
        // kinds are id-known to tsx, absent from typescript.
        let tsx_table = a.kind_table(tsx_a).expect("tsx table seeds");
        assert_ne!(table_a, tsx_table, "two grammars, two tables");
        assert!(
            tsx_table.len() > table_a.len(),
            "tsx's symbol table is the superset (191 vs 183 named kinds)"
        );
    }

    /// F9d: the elixir registration, same ladder as rust/python/go —
    /// extension routing (.ex/.exs, one grammar for both: the split is
    /// script-vs-compile at the BEAM level, not a syntax split),
    /// named-kind resolution against the real grammar, the typed
    /// `UnknownKind` for a kind elixir does not know, and the E5
    /// order-independent kind table. The brief's `definition` guess is
    /// fenced out here: that kind does NOT exist in tree-sitter-elixir
    /// 0.3.5 — named functions are `call` nodes (the node-types.json
    /// fact the norm table's FUNCTION decision stands on).
    #[test]
    #[cfg(feature = "lang-elixir")]
    fn elixir_registry_resolution_is_typed_routed_and_seeded() {
        let a = LangRegistry::new();
        let b = LangRegistry::new();
        let id = a
            .for_extension("ex")
            .expect("lang-elixir builds register the elixir grammar");
        assert_eq!(a.name(id), Some("elixir"));
        assert_eq!(a.for_extension("EX"), Some(id));
        assert_eq!(a.for_extension("exs"), Some(id));
        assert_eq!(a.for_extension("ExS"), Some(id));
        assert_eq!(a.for_path(Path::new("/proj/lib/widgetry.ex")), Some(id),);
        assert_eq!(
            a.for_path(Path::new("/proj/test/widgetry_test.exs")),
            Some(id),
        );

        // Kind names verified against tree-sitter-elixir 0.3.5's
        // node-types.json (45 named kinds) — the norm-table and KP-6
        // anchors: the def-clause `call`, the attribute wrapper, the
        // do-block and stab-clause interiors, the atom forms.
        for kind in [
            "source",
            "call",
            "do_block",
            "body",
            "block",
            "stab_clause",
            "anonymous_function",
            "unary_operator",
            "binary_operator",
            "identifier",
            "alias",
            "atom",
            "quoted_atom",
            "arguments",
            "keywords",
            "keyword",
            "pair",
            "list",
            "map",
            "struct",
            "tuple",
            "bitstring",
            "string",
            "charlist",
            "sigil",
            "interpolation",
            "boolean",
            "char",
            "float",
            "integer",
            "nil",
            "comment",
        ] {
            assert!(
                a.resolve_kind(id, kind).is_ok_and(|k| k != 0),
                "elixir grammar must know {kind}"
            );
        }

        // The honest probe, both directions: `definition` (the brief's
        // guessed def kind) and `function_item` (rust's) are unknown.
        for kind in ["definition", "function_item"] {
            let err = a
                .resolve_kind(id, kind)
                .expect_err("not an elixir kind name");
            assert!(matches!(err, DslError::UnknownKind { .. }));
            let umbrella: JanitorError = err.into();
            assert_eq!(umbrella.code(), "E_DSL_UNKNOWN_KIND");
            assert_eq!(
                umbrella.details(),
                Some(serde_json::json!({ "lang": "elixir", "kind": kind }))
            );
        }

        // E5 fence: order-independent seeding, round-tripping ids, seeded
        // from the bundled fixture (the corpus exercises modules,
        // behaviours, @doc/@spec clauses, keyword- and do-block-shaped
        // defs, guards, case/cond/try, pipes, heredocs and fn captures).
        let resolved: Vec<u16> = ["call", "identifier", "comment", "do_block"]
            .iter()
            .map(|kind| a.resolve_kind(id, kind).expect("kinds elixir knows"))
            .collect();
        let table_a = a.kind_table(id).expect("table seeds at first use");
        let id_b = b
            .for_extension("ex")
            .expect("lang-elixir builds register elixir");
        let table_b = b.kind_table(id_b).expect("table seeds at first use");
        assert_eq!(
            table_a, table_b,
            "E5: elixir seeding must not depend on usage order"
        );
        assert!(
            !table_a.is_empty() && table_a.len() >= 45,
            "the elixir grammar carries its symbol table (45 named kinds)"
        );
        for (kind, kind_id) in ["call", "identifier", "comment", "do_block"]
            .iter()
            .zip(resolved)
        {
            let name = a
                .kind_name(id, kind_id)
                .expect("seeded table knows the kind");
            assert_eq!(name.as_str(), *kind, "id→name must round-trip");
        }
    }

    /// F9a, E5 fence for python: the kind table is a function of the
    /// grammar alone — order-independent seeding, round-tripping ids, and
    /// seeded from the bundled fixture corpus (never first-scan bias). The
    /// python fixture exercises decorators, classes, if/elif/else and
    /// comprehensions, so the fixture leg cross-checks a real parse too.
    #[test]
    #[cfg(feature = "lang-python")]
    fn python_kind_tables_are_order_independent_and_round_trip() {
        let a = LangRegistry::new();
        let b = LangRegistry::new();
        let id_a = a
            .for_extension("py")
            .expect("lang-python builds register python");
        let id_b = b
            .for_extension("py")
            .expect("lang-python builds register python");

        let resolved: Vec<u16> = ["function_definition", "identifier", "block", "comment"]
            .iter()
            .map(|kind| {
                a.resolve_kind(id_a, kind)
                    .expect("kinds the python grammar knows")
            })
            .collect();
        let table_a = a.kind_table(id_a).expect("table seeds at first use");

        let table_b = b.kind_table(id_b).expect("table seeds at first use");
        assert_eq!(
            table_a, table_b,
            "E5: python seeding must not depend on usage order"
        );
        assert!(
            !table_a.is_empty() && table_a.len() > 100,
            "the python grammar carries a real symbol table (128 named kinds)"
        );

        for (kind, id) in ["function_definition", "identifier", "block", "comment"]
            .iter()
            .zip(resolved)
        {
            let name = a.kind_name(id_a, id).expect("seeded table knows the kind");
            assert_eq!(name.as_str(), *kind, "id→name must round-trip");
        }
    }

    /// F9c: the ruby registration, same ladder as rust/python — extension
    /// routing (.rb, case-insensitive), named-kind resolution against the
    /// real grammar (kind names verified against tree-sitter-ruby 0.23.1's
    /// node-types.json), the typed `UnknownKind` for a kind ruby does not
    /// know, and the E5 order-independent kind table. The experimental
    /// tier's registration is exercised exactly like the stable set's —
    /// the tier split (ADR-0015 note) is about CI fencing, not about a
    /// second-class registry path.
    #[test]
    #[cfg(feature = "lang-ruby")]
    fn ruby_registry_resolution_is_typed_routed_and_seeded() {
        let a = LangRegistry::new();
        let b = LangRegistry::new();
        let id = a
            .for_extension("rb")
            .expect("lang-ruby builds register the ruby grammar");
        assert_eq!(a.name(id), Some("ruby"));
        assert_eq!(a.for_extension("RB"), Some(id));
        assert_eq!(a.for_path(Path::new("/proj/app/models.rb")), Some(id),);

        // Kind names verified against tree-sitter-ruby 0.23.1's
        // node-types.json (149 named kinds) — the norm-table and KP-6
        // anchors: methods, the bodies they live in, calls, heredoc.
        for kind in [
            "program",
            "method",
            "singleton_method",
            "method_parameters",
            "class",
            "module",
            "body_statement",
            "block_body",
            "do_block",
            "identifier",
            "call",
            "argument_list",
            "comment",
            "heredoc_beginning",
            "heredoc_body",
        ] {
            assert!(
                a.resolve_kind(id, kind).is_ok_and(|k| k != 0),
                "ruby grammar must know {kind}"
            );
        }

        let err = a
            .resolve_kind(id, "function_definition")
            .expect_err("a python kind name is unknown to the ruby grammar");
        assert!(matches!(err, DslError::UnknownKind { .. }));
        let umbrella: JanitorError = err.into();
        assert_eq!(umbrella.code(), "E_DSL_UNKNOWN_KIND");
        assert_eq!(
            umbrella.details(),
            Some(serde_json::json!({ "lang": "ruby", "kind": "function_definition" }))
        );

        // E5 fence: order-independent seeding, round-tripping ids, seeded
        // from the bundled fixture (the corpus exercises modules, classes,
        // attr macros, defs, blocks, conditionals and a heredoc).
        let resolved: Vec<u16> = ["method", "identifier", "comment", "heredoc_body"]
            .iter()
            .map(|kind| a.resolve_kind(id, kind).expect("kinds ruby knows"))
            .collect();
        let table_a = a.kind_table(id).expect("table seeds at first use");
        let id_b = b
            .for_extension("rb")
            .expect("lang-ruby builds register ruby");
        let table_b = b.kind_table(id_b).expect("table seeds at first use");
        assert_eq!(
            table_a, table_b,
            "E5: ruby seeding must not depend on usage order"
        );
        assert!(
            !table_a.is_empty() && table_a.len() > 100,
            "the ruby grammar carries a real symbol table (149 named kinds)"
        );
        for (kind, kind_id) in ["method", "identifier", "comment", "heredoc_body"]
            .iter()
            .zip(resolved)
        {
            let name = a
                .kind_name(id, kind_id)
                .expect("seeded table knows the kind");
            assert_eq!(name.as_str(), *kind, "id→name must round-trip");
        }
    }

    /// F9c: the ocaml registration — one crate, two LangIds (the mapping
    /// decision: `.ml`→ocaml, `.mli`→ocaml_interface; the crate's third
    /// grammar carries no file types upstream). Same ladder as rust/
    /// python/ruby: extension routing, kind resolution against the real
    /// grammars (kind names verified against tree-sitter-ocaml 0.26.0's
    /// node-types.json — the brief's `value_binding` does not exist; the
    /// statement is `value_definition` wrapping a `let_binding`), the
    /// typed `UnknownKind`, and the E5 order-independent kind table for
    /// BOTH LangIds — the interface grammar is exercised as a first-class
    /// language, not an alias.
    #[test]
    #[cfg(feature = "lang-ocaml")]
    fn ocaml_registry_resolution_is_typed_routed_and_seeded() {
        let a = LangRegistry::new();
        let ml = a
            .for_extension("ml")
            .expect("lang-ocaml builds register the ocaml grammar");
        let mli = a
            .for_extension("mli")
            .expect("lang-ocaml builds register the ocaml_interface grammar");
        assert_eq!(a.name(ml), Some("ocaml"));
        assert_eq!(a.name(mli), Some("ocaml_interface"));
        assert_ne!(ml, mli, "two LangIds, not one grammar wearing two hats");
        assert_eq!(a.for_extension("ML"), Some(ml), "case-insensitive");
        assert_eq!(a.for_path(Path::new("/proj/lib/store.ml")), Some(ml),);
        assert_eq!(a.for_path(Path::new("/proj/lib/store.mli")), Some(mli),);

        // Implementation-grammar kinds: the definition wrapper, the
        // binding core, the let..in carrier, module bodies, names.
        for kind in [
            "compilation_unit",
            "value_definition",
            "let_binding",
            "let_expression",
            "structure",
            "module_definition",
            "module_binding",
            "value_name",
            "value_path",
            "parameter",
            "infix_expression",
            "comment",
            "string",
            "number",
        ] {
            assert!(
                a.resolve_kind(ml, kind).is_ok_and(|k| k != 0),
                "ocaml grammar must know {kind}"
            );
        }
        // Interface-grammar kinds: val specs and signatures, plus the
        // shared statement vocabulary.
        for kind in [
            "compilation_unit",
            "value_specification",
            "signature",
            "type_definition",
            "type_binding",
            "value_name",
            "function_type",
            "comment",
        ] {
            assert!(
                a.resolve_kind(mli, kind).is_ok_and(|k| k != 0),
                "ocaml_interface grammar must know {kind}"
            );
        }

        let err = a
            .resolve_kind(ml, "function_definition")
            .expect_err("a python kind name is unknown to the ocaml grammar");
        assert!(matches!(err, DslError::UnknownKind { .. }));
        let umbrella: JanitorError = err.into();
        assert_eq!(umbrella.code(), "E_DSL_UNKNOWN_KIND");
        assert_eq!(
            umbrella.details(),
            Some(serde_json::json!({ "lang": "ocaml", "kind": "function_definition" }))
        );

        // E5 fence, both LangIds: real tables that round-trip, seeded from
        // the bundled fixtures (module/type/val-heavy .ml and .mli bodies).
        let b = LangRegistry::new();
        for (id, anchors) in [
            (ml, ["value_definition", "value_name", "comment"].as_slice()),
            (
                mli,
                ["value_specification", "value_name", "comment"].as_slice(),
            ),
        ] {
            let resolved: Vec<u16> = anchors
                .iter()
                .map(|kind| a.resolve_kind(id, kind).expect("kinds ocaml knows"))
                .collect();
            let table = a.kind_table(id).expect("table seeds at first use");
            let table_b = b.kind_table(id).expect("table seeds at first use");
            assert_eq!(table, table_b, "E5: seeding must not depend on usage order");
            assert!(
                !table.is_empty() && table.len() > 200,
                "the ocaml grammars carry real symbol tables (276/275 named kinds)"
            );
            for (kind, kind_id) in anchors.iter().zip(resolved) {
                let name = a
                    .kind_name(id, kind_id)
                    .expect("seeded table knows the kind");
                assert_eq!(name.as_str(), *kind, "id→name must round-trip");
            }
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
