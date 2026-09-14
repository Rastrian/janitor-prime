//! Pooled-cursor traversal (PLAN §1.2 Rule T5, §3 F3.6; KP-8's walk leg).
//!
//! `Node::walk()` heap-allocates a cursor's internal stack; the API's own
//! reuse prescription is caller-owned cursors (`children(&mut cursor)`,
//! `TreeCursor::reset`) — this module is that discipline as a structure:
//!
//! - a [`CursorPool`] owns a `Vec` of spare cursors plus recycled stack
//!   storage, tied to one tree's lifetime;
//! - [`CursorPool::dfs`] hands out a [`PooledDfs`] preorder iterator with
//!   **one cursor per depth level** on its stack — pushing reuses a spare
//!   via `reset`, popping recycles it back;
//! - dropping the iterator returns every cursor and the stack `Vec` to the
//!   pool, so steady-state traversals of the same tree allocate **zero**
//!   bytes beyond the caller's result vectors — the budget KP-8's counting
//!   allocator asserts in `tests/alloc.rs` (F4 re-arms the fence on the
//!   matcher itself, per eval-2 amendment 7).
//!
//! The F4 matcher's shape: one pool per work item (per tree), pivot
//! discovery through `dfs`, flat per-pivot child iteration through
//! `node.children(&mut cursor)` — both cursor-reuse patterns.

use crate::ast::ffi_shim::{Node, TreeCursor};

/// One depth level of the DFS: the cursor is positioned at the node being
/// visited; `expanded` distinguishes "emit + descend pending" from
/// "children done, advance".
struct Layer<'tree> {
    cursor: TreeCursor<'tree>,
    expanded: bool,
}

/// A per-tree cursor pool (Rule T5). The `'tree` parameter is the point:
/// cursors borrow their tree, so a pool serves traversals of one tree —
/// the F4 work item's exact ownership shape (owned `Arc<Tree>`, pool
/// local, plain data out).
pub struct CursorPool<'tree> {
    spares: Vec<TreeCursor<'tree>>,
    stacks: Vec<Vec<Layer<'tree>>>,
}

impl<'tree> CursorPool<'tree> {
    /// An empty pool.
    pub fn new() -> Self {
        Self {
            spares: Vec::new(),
            stacks: Vec::new(),
        }
    }

    /// Begin a preorder DFS of `root`, drawing cursors and stack storage
    /// from the pool and returning them on drop.
    pub fn dfs(&mut self, root: Node<'tree>) -> PooledDfs<'_, 'tree> {
        let stack = self.stacks.pop().unwrap_or_default();
        let mut dfs = PooledDfs { pool: self, stack };
        let cursor = match dfs.pool.spares.pop() {
            Some(mut cursor) => {
                cursor.reset(root);
                cursor
            }
            None => root.walk(),
        };
        dfs.stack.push(Layer {
            cursor,
            expanded: false,
        });
        dfs
    }
}

impl Default for CursorPool<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Preorder iterator over every node of a tree, driven by pooled cursors.
/// Yields the same sequence a recursive `children`-walk would.
pub struct PooledDfs<'pool, 'tree> {
    pool: &'pool mut CursorPool<'tree>,
    stack: Vec<Layer<'tree>>,
}

impl<'pool, 'tree> PooledDfs<'pool, 'tree> {
    /// Descend into `node`'s first child, reusing a pooled cursor when one
    /// is spare. `goto_first_child` cannot fail right after a
    /// `child_count() > 0` check; if the contract ever broke, the node is
    /// simply treated as a leaf (advance instead of descend) — no panic.
    fn descend(&mut self, node: Node<'tree>) {
        let entered = match self.pool.spares.pop() {
            Some(mut cursor) => {
                cursor.reset(node);
                cursor.goto_first_child().then_some(cursor)
            }
            None => {
                let mut cursor = node.walk();
                cursor.goto_first_child().then_some(cursor)
            }
        };
        if let Some(cursor) = entered {
            self.stack.push(Layer {
                cursor,
                expanded: false,
            });
        }
    }
}

impl<'pool, 'tree> Iterator for PooledDfs<'pool, 'tree> {
    type Item = Node<'tree>;

    fn next(&mut self) -> Option<Node<'tree>> {
        loop {
            let top = self.stack.len().checked_sub(1)?;
            if !self.stack[top].expanded {
                // Fresh layer: emit the node, then prepare its first child
                // for the next call (preorder).
                self.stack[top].expanded = true;
                let node = self.stack[top].cursor.node();
                if node.child_count() > 0 {
                    self.descend(node);
                }
                return Some(node);
            }
            // Current node emitted and expanded: advance to its next
            // sibling, or finish this depth and recycle the cursor.
            if self.stack[top].cursor.goto_next_sibling() {
                self.stack[top].expanded = false;
                continue;
            }
            if let Some(layer) = self.stack.pop() {
                self.pool.spares.push(layer.cursor);
            }
        }
    }
}

impl Drop for PooledDfs<'_, '_> {
    fn drop(&mut self) {
        for layer in self.stack.drain(..) {
            self.pool.spares.push(layer.cursor);
        }
        // The drained Vec keeps its capacity — hand it back for the next
        // traversal instead of freeing it.
        self.pool.stacks.push(std::mem::take(&mut self.stack));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::LangRegistry;
    use crate::ast::parse::parse_snapshot;
    use std::sync::Arc;

    #[cfg(feature = "lang-rust")]
    fn parsed() -> crate::ast::ffi_shim::Tree {
        let registry = LangRegistry::new();
        let id = registry
            .for_extension("rs")
            .expect("lang-rust is the default feature set");
        let mut parser = registry.parser(id).expect("ABI-verified pair");
        let source: Arc<[u8]> = Arc::from(
            b"fn alpha(x: u32) -> u32 {\n    let y = x + 1;\n    y * 2\n}\n\
              \n// comment between items\n\
              fn beta() {\n    if true {\n        ();\n    }\n}\n\
              struct Pair { a: u32, b: u32 }\n"
                .as_slice(),
        );
        parse_snapshot(&mut parser, &source).expect("corpus parses")
    }

    /// Reference traversal: the recursive `children(&mut cursor)` walk —
    /// the T5 flat pattern in its own right.
    fn reference_preorder(node: Node<'_>, out: &mut Vec<(u16, usize, usize)>) {
        out.push((node.kind_id(), node.start_byte(), node.end_byte()));
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            reference_preorder(child, out);
        }
    }

    /// PooledDfs yields exactly the recursive preorder — every node, same
    /// order, same spans — and repeated traversals from one pool agree.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn pooled_dfs_matches_the_recursive_preorder() {
        let tree = parsed();
        let root = tree.root_node();

        let mut expected = Vec::new();
        reference_preorder(root, &mut expected);
        assert!(expected.len() > 50, "corpus exercises real depth");

        let mut pool = CursorPool::new();
        let first: Vec<(u16, usize, usize)> = pool
            .dfs(root)
            .map(|n| (n.kind_id(), n.start_byte(), n.end_byte()))
            .collect();
        assert_eq!(first, expected);

        // Second traversal from the same pool: identical, from recycled
        // cursors and stack storage.
        let second: Vec<(u16, usize, usize)> = pool
            .dfs(root)
            .map(|n| (n.kind_id(), n.start_byte(), n.end_byte()))
            .collect();
        assert_eq!(second, expected);

        // The flat pattern agrees on coverage: every direct child read
        // through a reused cursor is a node the dfs yields.
        let mut cursor = root.walk();
        let top_level: Vec<_> = root.children(&mut cursor).collect();
        let yielded: Vec<(u16, usize, usize)> = pool
            .dfs(root)
            .map(|n| (n.kind_id(), n.start_byte(), n.end_byte()))
            .collect();
        assert_eq!(yielded.len(), expected.len());
        for flat in &top_level {
            let key = (flat.kind_id(), flat.start_byte(), flat.end_byte());
            assert!(
                yielded.contains(&key),
                "top-level child {key:?} must be yielded"
            );
        }
    }

    /// Edge shapes: a leaf root (empty source parses to a bare tree), and
    /// interleaved partial iterations (drop mid-walk returns the cursors).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn pooled_dfs_handles_leaf_roots_and_partial_drains() {
        // A leaf root (empty source parses to a bare tree) still yields.
        {
            let registry = LangRegistry::new();
            let id = registry
                .for_extension("rs")
                .expect("lang-rust is the default feature set");
            let mut parser = registry.parser(id).expect("ABI-verified pair");
            let empty: Arc<[u8]> = Arc::from(b"".as_slice());
            let leaf = parse_snapshot(&mut parser, &empty).expect("empty source parses");
            let mut pool = CursorPool::new();
            assert_eq!(
                pool.dfs(leaf.root_node()).count(),
                1,
                "the root still yields"
            );
        }

        // The tree outlives its pool (the pool borrows it — the F4
        // per-work-item shape), and a partial drain must not corrupt
        // recycling.
        let tree = parsed();
        let root = tree.root_node();
        let mut pool = CursorPool::new();
        let total = pool.dfs(root).count();
        assert!(total > 50);

        let mut partial = pool.dfs(root);
        partial.next();
        partial.next();
        drop(partial);
        assert_eq!(
            pool.dfs(root).count(),
            total,
            "dropping a mid-walk iterator returns its cursors intact"
        );
    }
}
