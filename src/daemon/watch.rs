//! Watch mode (F10a, ADR-0018 D2/D3): polling over the **warm set**, with
//! incremental re-parse on change.
//!
//! Every interval the daemon stats exactly the paths the cache holds
//! entries for (`(size, mtime)` via `std::fs::metadata`) and diffs against
//! the previous pass. Detection is polling — zero new dependencies,
//! portable across the release matrix, deterministic to test; inotify/
//! `notify` is the documented upgrade path (it can replace the *detection*
//! half without touching the re-parse half).
//!
//! Correctness never depends on the watcher (ADR-0018 D3): Rule T4
//! validates every served entry against the overlay's live bytes
//! (generation ∧ content-hash), so a file changed after the last poll is a
//! guaranteed miss and a fresh parse — the poll only decides *when* to
//! re-parse, never *whether* an entry is valid. Staging external content
//! into the overlay to force a generation bump was rejected: the buffer
//! would shadow disk with stale bytes between polls, degrading the daemon
//! below cold-CLI freshness.
//!
//! The re-parse itself is KP-1's winning incremental path: one
//! common-prefix/suffix `Splice`, `ParseCache::take` (the exclusivity
//! one-way door), `Tree::edit`, re-parse against the old tree, fresh entry
//! inserted. A watcher failure leaves the entry evicted — the next scan
//! re-parses cold; the daemon never crashes on watch errors. A scan in
//! flight holding a shared-read clone makes the tree unexclusive-able for
//! a moment: the choreography refuses with `E_PARSE_SHARED_TREE`, the
//! watcher logs and moves on (the next scan cold-parses — the documented
//! race cost, never corruption).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::ast::cache::ParseEntry;
use crate::ast::parse::{Splice, reparse_edited};
use crate::error::{JanitorError, ParseError};
use crate::orchestrator::Session;
use crate::vfs::VfsPath;

/// A file's poll stamp: content length + mtime in nanoseconds. Equal stamps
/// skip the file; anything else falls through to a content check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    mtime_nanos: u128,
}

impl FileStamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Self {
            len: meta.len(),
            mtime_nanos,
        }
    }
}

/// The watcher's cross-pass state: last-seen stamp per warm path. Owned by
/// the watcher thread; one [`pass`] mutates it.
#[derive(Debug, Default)]
pub struct WatchState {
    stamps: HashMap<PathBuf, FileStamp>,
}

/// The watcher's observable counters (surfaced through `ping`/`stats`).
#[derive(Debug, Default)]
pub struct WatchCounters {
    pub passes: std::sync::atomic::AtomicU64,
    pub reparses: std::sync::atomic::AtomicU64,
}

/// One poll pass over the warm set (the cache's paths, filtered to the
/// watch scope): stat each, drop entries whose file vanished, and for
/// stamps that changed since the last pass, verify by content (Rule T4 —
/// a `touch` with equal bytes is a no-op) and re-parse incrementally what
/// genuinely diverged. Returns the number of re-parses performed.
pub fn pass(
    session: &Session,
    workspace: &Path,
    state: &mut WatchState,
) -> Result<u64, JanitorError> {
    let overlay = session.overlay();
    let cache = session.cache();
    let mut reparses = 0_u64;

    for path in cache.paths() {
        let disk = path.as_path().to_path_buf();
        if !in_watch_scope(workspace, &disk) {
            // Outside the ignore-aware scope (`.git/`, `.janitor/`,
            // `target/`, …): not proactively warmed — still T4-validated
            // on every scan, just never by the watcher.
            state.stamps.remove(&disk);
            continue;
        }
        let stamp = match std::fs::metadata(&disk) {
            Ok(meta) => FileStamp::of(&meta),
            // Vanished (deleted or unreadable): drop the warmth; scans of a
            // deleted path already fail honestly through the overlay.
            Err(_) => {
                state.stamps.remove(&disk);
                drop(cache.take(&path));
                continue;
            }
        };
        let changed = state.stamps.insert(disk.clone(), stamp) != Some(stamp);
        if !changed {
            continue;
        }
        // Content flows through the overlay (the parsing-reads-Overlay
        // discipline); a mid-write read error just skips this pass.
        let Ok(bytes) = overlay.read_file(&disk) else {
            continue;
        };
        let Ok(generation) = overlay.current_gen(&disk) else {
            continue;
        };
        // Equal content at an equal generation (a `touch`, or the first
        // pass seeding stamps): the entry is valid, nothing to do.
        if cache.peek(&path, generation, &bytes) {
            continue;
        }
        if reparse(session, &path, &bytes)? {
            reparses += 1;
        }
    }
    Ok(reparses)
}

/// Replace `path`'s entry with an incrementally re-parsed one over `bytes`
/// (the current overlay-visible content). Returns whether a re-parse ran;
/// every failure mode is non-fatal to the daemon (ADR-0018 D3).
fn reparse(session: &Session, path: &VfsPath, bytes: &Arc<[u8]>) -> Result<bool, JanitorError> {
    let cache = session.cache();
    let registry = session.registry();
    let Some(lang) = registry.for_path(path.as_path()) else {
        // Extension no longer routes (a rename in place): drop the warmth.
        drop(cache.take(path));
        return Ok(false);
    };
    // The one-way door FIRST (§1.2.1 step 5): exclusive ownership before
    // `Tree::edit`. A concurrent `take` (another watcher pass, a scan's
    // stale eviction) means nothing to warm.
    let Some(old) = cache.take(path) else {
        return Ok(false);
    };

    let splice = diff_splice(&old.bytes, bytes);
    if splice.start_byte == splice.old_end_byte && splice.new_text.is_empty() {
        // Content identical after all (stamp granularity): keep the entry.
        cache.insert(path.clone(), old);
        return Ok(false);
    }

    let mut owned = old.tree;
    let mut parser = registry.parser(lang)?;
    match reparse_edited(&mut parser, &old.bytes, &mut owned, &splice) {
        Ok((new_bytes, tree)) => {
            let generation = session.overlay().current_gen(path.as_path())?;
            cache.insert(path.clone(), ParseEntry::new(generation, new_bytes, tree));
            Ok(true)
        }
        // A scan in flight holds a shared-read clone; editing a shared tree
        // is forbidden (Rule T1), so this file cold-parses on its next
        // scan. Logged, not fatal — the exact race ADR-0018 D5 documents.
        Err(ParseError::SharedTree { count }) => {
            tracing::debug!(
                count,
                path = %path.as_path().display(),
                "warm re-parse deferred: tree handles still shared; next scan cold-parses"
            );
            Ok(false)
        }
        Err(err) => Err(err.into()),
    }
}

/// The minimal single splice turning `old` into `new`: the common prefix
/// and suffix are kept, everything between is one replacement (the same
/// minimal-diff shape `apply`'s splice uses, refactree's proven ordering).
pub fn diff_splice(old: &[u8], new: &[u8]) -> Splice {
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let old_rest = &old[prefix..];
    let new_rest = &new[prefix..];
    let suffix = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    Splice {
        start_byte: prefix,
        old_end_byte: old.len() - suffix,
        new_text: new[prefix..new.len() - suffix].to_vec(),
    }
}

/// Whether `path` is inside the watch scope: not under a dot-prefixed
/// directory component and not under `target/` (the brief's ignore set;
/// dot-prefixed covers `.git/` and `.janitor/`). Pure — tested directly.
pub fn in_watch_scope(workspace: &Path, path: &Path) -> bool {
    let rel = match path.strip_prefix(workspace) {
        Ok(rel) => rel,
        Err(_) => return false,
    };
    for component in rel.components() {
        let name = component.as_os_str().to_string_lossy();
        if name.starts_with('.') || name == "target" {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::LangRegistry;
    use crate::ast::parse::parse_snapshot;
    use crate::orchestrator::Session;
    use crate::vfs::{Os, Overlay};

    /// The minimal single splice: prefix/suffix preserved, one replacement
    /// region — insertion, deletion, replacement, pure touch, and
    /// disjoint-double-change (which coalesces into one region) all behave.
    #[test]
    fn diff_splice_is_minimal_and_correct() {
        let splice = diff_splice(b"fn a() -> u32 { 1 }", b"fn b() -> u32 { 1 }");
        assert_eq!(splice.start_byte, 3);
        assert_eq!(splice.old_end_byte, 4);
        assert_eq!(splice.new_text, b"b".to_vec());

        // Pure insertion at the end.
        let splice = diff_splice(b"abc", b"abcd");
        assert_eq!((splice.start_byte, splice.old_end_byte), (3, 3));
        assert_eq!(splice.new_text, b"d".to_vec());

        // Pure deletion.
        let splice = diff_splice(b"abcd", b"abc");
        assert_eq!((splice.start_byte, splice.old_end_byte), (3, 4));
        assert!(splice.new_text.is_empty());

        // Identical: the empty splice (start == old_end, no text).
        let splice = diff_splice(b"same", b"same");
        assert_eq!((splice.start_byte, splice.old_end_byte), (4, 4));
        assert!(splice.new_text.is_empty());

        // Two disjoint changes coalesce into one covering region.
        let splice = diff_splice(b"let a = 1; let z = 2;", b"let b = 1; let y = 2;");
        assert_eq!(splice.start_byte, 4);
        assert_eq!(splice.old_end_byte, 16);
        assert_eq!(splice.new_text, b"b = 1; let y".to_vec());
    }

    /// The watch scope excludes dot-prefixed components at any depth and
    /// `target/`, keeps ordinary paths (ADR-0018 D2's ignore set).
    #[test]
    fn watch_scope_ignores_dot_dirs_and_target() {
        let ws = Path::new("/ws");
        assert!(in_watch_scope(ws, Path::new("/ws/src/lib.rs")));
        assert!(in_watch_scope(ws, Path::new("/ws/a/b/c.rs")));
        assert!(!in_watch_scope(ws, Path::new("/ws/.git/config")));
        assert!(!in_watch_scope(ws, Path::new("/ws/.janitor/STATE.json")));
        assert!(!in_watch_scope(ws, Path::new("/ws/src/.hidden/x.rs")));
        assert!(!in_watch_scope(ws, Path::new("/ws/target/debug/x.rs")));
        assert!(!in_watch_scope(ws, Path::new("/elsewhere/x.rs")));
    }

    /// The warm re-parse path end-to-end at the library level: a changed
    /// file's entry is replaced incrementally and the resulting tree is
    /// shape-identical to a cold parse of the same bytes (KP-1's
    /// byte-identical standard, asserted via `to_sexp`).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn warm_reparse_replaces_entry_with_shape_identical_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("m.rs");
        std::fs::write(&file, "fn one() -> u32 {\n    let a = 1;\n    a\n}\n").expect("seed");

        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let session = Session::with_overlay_cache_root(
            overlay,
            Arc::new(crate::ast::cache::ParseCache::new()),
            tmp.path().join(".janitor"),
        );
        let path = VfsPath::new(&file).expect("canonical path");

        // Prime the warmth exactly as a scan would.
        let bytes = session.overlay().read_file(&file).expect("bytes");
        let generation = session.overlay().current_gen(&file).expect("gen");
        let registry = session.registry();
        let lang = registry.for_extension("rs").expect("rust");
        let mut parser = registry.parser(lang).expect("parser");
        let tree = parse_snapshot(&mut parser, &bytes).expect("parse");
        session
            .cache()
            .insert(path.clone(), ParseEntry::new(generation, bytes, tree));

        // One local edit on disk.
        std::fs::write(
            &file,
            "fn one() -> u32 {\n    let renamed = 1;\n    renamed\n}\n",
        )
        .expect("edit");

        let mut state = WatchState::default();
        let reparsed = pass(&session, tmp.path(), &mut state).expect("watch pass");
        assert_eq!(reparsed, 1);
        assert_eq!(session.cache().len(), 1, "entry replaced, not dropped");

        // The warm entry is now valid for the new bytes, and its tree is
        // shape-identical to a cold parse.
        let new_bytes = session.overlay().read_file(&file).expect("new bytes");
        assert!(
            session.cache().peek(&path, generation, &new_bytes),
            "fresh entry validates against the changed content"
        );
        let entry = session
            .cache()
            .get(&path, generation, &new_bytes)
            .expect("served");
        let mut cold_parser = LangRegistry::new().parser(lang).expect("cold parser");
        let cold = parse_snapshot(&mut cold_parser, &new_bytes).expect("cold parse");
        assert_eq!(
            entry.tree.root_node().to_sexp(),
            cold.root_node().to_sexp(),
            "incremental and cold trees are shape-identical"
        );

        // A second pass with no further change re-parses nothing.
        let reparsed = pass(&session, tmp.path(), &mut state).expect("second pass");
        assert_eq!(reparsed, 0, "unchanged stamps are a no-op");
    }
}
