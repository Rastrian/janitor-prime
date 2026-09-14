//! Structural search/rewrite over tree-sitter (PLAN §1.4).
//!
//! F3 delivers the parse layer the §1.2 rules govern: the ffi boundary and
//! language registry, snapshot parsing with the guarded edit/re-parse
//! choreography, the generation-keyed cache, ERROR/MISSING counting, and the
//! pooled-cursor traversal. The DSL and evaluator remain F4 scope.
//!
//! **Import discipline (ADR-0015, risk R22):** `ffi_shim` is the only file
//! that may import tree-sitter types (plus `send_assert`, whose canaries
//! must name exactly what they fence). Everything else reaches the types
//! through `ffi_shim`'s re-exports — pinned by the fence test below, so a
//! future core pin-swap is a one-file edit by construction, not by hope.
pub mod ffi_shim;
pub mod lang;
pub mod parse;
pub mod send_assert;

// TODO(F3): generation-keyed ParseCache, error counting, pooled walk.
// TODO(F4): s-expression DSL, scoped state-machine evaluator, exec.

#[cfg(test)]
mod tests {
    /// R22 fence: outside the shim (and the `send_assert` canary), no
    /// `src/ast` source file may name the tree-sitter crate — every type
    /// flows through `ffi_shim`'s re-exports so a core pin-swap touches that
    /// file alone (ADR-0015). Scans the directory at test time, so F3/F4
    /// submodules are covered the moment they land (fixtures are data, not
    /// code, and live in a subdirectory the scan never enters). The needle is
    /// built by concat so this test's own source cannot trip itself.
    #[test]
    fn tree_sitter_imports_are_confined_to_the_shim() {
        let crate_path = concat!("tree_", "sitter::");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ast");
        let entries = std::fs::read_dir(&dir).expect("src/ast ships with the crate");
        let mut fenced = 0_u32;
        for entry in entries {
            let path = entry.expect("readable dir entry").path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let src = std::fs::read_to_string(&path).unwrap_or_default();
            match name {
                // The two sanctioned importers must keep importing — a
                // stale fence is worse than none.
                "ffi_shim.rs" => assert!(
                    src.contains(concat!("pub use ", "tree_", "sitter::")),
                    "ffi_shim must remain the re-export boundary"
                ),
                "send_assert.rs" => assert!(
                    src.contains(concat!("tree_", "sitter::Tree")),
                    "send_assert must keep fencing the real types"
                ),
                _ => {
                    assert!(
                        !src.contains(crate_path),
                        "{name} names the tree-sitter crate directly; route it through \
                         crate::ast::ffi_shim (ADR-0015 / R22)",
                    );
                    fenced += 1;
                }
            }
        }
        assert!(
            fenced >= 1,
            "the fence must have scanned at least this file"
        );
    }
}
