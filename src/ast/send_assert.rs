//! KP-2 regression fence: the `Send`/`Sync` contracts the threading design
//! depends on (PLAN §1.2 Rule T3, §5 KP-2).
//!
//! Facts verified against tree-sitter 0.27.0 (PLAN A.3.1): `Tree`, `Parser`,
//! `Node<'_>`, `TreeCursor<'_>`, `Language` all carry unconditional
//! `unsafe impl Send`/`Sync`. The zero-cost-concurrency design (§1.1 — rayon
//! workers passing owned `Arc<Tree>` + `Arc<[u8]>` snapshots and returning
//! plain data) rests on those impls; this file pins them at compile time so a
//! future tree-sitter release that narrows them breaks the build loudly
//! (risk R2) instead of silently changing the product's threading contracts.
//!
//! This is the one `src/ast` file besides `ffi_shim` that names
//! `tree_sitter` types: a canary must reference exactly the types it fences.
//! The pre-designed fallback if this fence ever trips (parse and match fused
//! into one work item, per-worker cache with a merge at collection) is
//! documented in §1.2 Rule T3 and is not expected to trigger on 0.27.

/// Compile-time canaries. Never called: the fence is that this file
/// *compiles*. The underscore prefix exempts the anchor from `dead_code`.
fn _canaries() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    // `Tree`: shared read-only across rayon workers — the blessed pattern
    // (concurrent reads through shared handles, A.3.2).
    assert_send::<tree_sitter::Tree>();
    assert_sync::<tree_sitter::Tree>();
    // `Parser`: one per language per worker (§1.2 ownership map).
    assert_send::<tree_sitter::Parser>();
    assert_sync::<tree_sitter::Parser>();
    // `TreeCursor` and `Node`: traversal-local handles (Rules T3/T5). The
    // `'static` instantiation is the strongest form the type system lets a
    // canary express for a lifetime-parameterized type.
    assert_send::<tree_sitter::TreeCursor<'static>>();
    assert_sync::<tree_sitter::TreeCursor<'static>>();
    assert_send::<tree_sitter::Node<'static>>();
    assert_sync::<tree_sitter::Node<'static>>();
}

#[cfg(test)]
mod tests {
    use rayon::prelude::*;
    use std::sync::Arc;

    /// The rayon work-item shape of §1.1/§1.2 Rule T3: owned snapshot + owned
    /// tree in, plain serializable data out. Being `Send` is the contract.
    struct ScanItem {
        bytes: Arc<[u8]>,
        tree: Arc<tree_sitter::Tree>,
    }

    /// Plain-data match result — values, no borrows into the tree (Rule T3).
    #[derive(Debug, PartialEq, Eq)]
    struct MatchReport {
        kind_id: u16,
        start_byte: usize,
        end_byte: usize,
        start_point: tree_sitter::Point,
        end_point: tree_sitter::Point,
    }

    fn assert_send<T: Send>() {}

    /// KP-2 runtime leg: parse once, then fan workers over shared clones.
    /// Each worker borrows the root locally, walks with its own cursor, and
    /// returns plain data — the exact `scan` shape the evaluator will use.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn rayon_mini_scan_passes_owned_trees_and_collects_plain_data() {
        let source: Arc<[u8]> = r#"
/// First target.
fn alpha() -> u32 { 1 }

// an interleaved comment, as real corpora have
fn beta(x: u32) -> u32 { x + 2 }

fn gamma() -> u32 {
    let y = 3;
    y * 4
}
"#
        .as_bytes()
        .to_vec()
        .into();

        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .expect("rust grammar x core 0.27 is an ABI-verified pair (A.3.8)");
        let tree = parser
            .parse(&*source, None)
            .expect("mini-scan corpus is valid Rust");

        let item = ScanItem {
            bytes: Arc::clone(&source),
            tree: Arc::new(tree),
        };
        assert_send::<ScanItem>();

        let function_kind = language.id_for_node_kind("function_item", true);
        assert_ne!(function_kind, 0, "rust grammar must know function_item");

        let per_worker: Vec<Vec<MatchReport>> = (0..8_u32)
            .into_par_iter()
            .map(|_| {
                // Owned clones cross the thread boundary; the borrow of
                // `root_node()` stays inside the worker.
                let item = ScanItem {
                    bytes: Arc::clone(&item.bytes),
                    tree: Arc::clone(&item.tree),
                };
                let root = item.tree.root_node();
                let mut cursor = root.walk();
                root.children(&mut cursor)
                    .filter(|child| child.kind_id() == function_kind)
                    .map(|child| MatchReport {
                        kind_id: child.kind_id(),
                        start_byte: child.start_byte(),
                        end_byte: child.end_byte(),
                        start_point: child.start_position(),
                        end_point: child.end_position(),
                    })
                    .collect()
            })
            .collect();

        assert_eq!(per_worker.len(), 8);
        let first = &per_worker[0];
        assert_eq!(first.len(), 3, "corpus has exactly three functions");
        for reports in &per_worker {
            assert_eq!(reports, first, "every worker reads the same shared tree");
            for hit in reports {
                assert!(hit.end_byte <= item.bytes.len(), "offsets stay in bounds");
            }
        }
    }
}
