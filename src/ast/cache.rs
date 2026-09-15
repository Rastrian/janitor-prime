//! Generation-keyed parse cache (PLAN §1.4.4, ADR-0007, §3 F3.4).
//!
//! `ParseCache` is a dashmap keyed by [`VfsPath`]; entries are validated by
//! **generation first, then Arc-pointer identity, then content hash** (Rule
//! T4 — invalidation is never heuristic: no mtime guessing, no partial
//! trust of stale offsets). A `ParseEntry` is the (bytes, tree) unit of
//! Rule T2: the `Arc<[u8]>` snapshot the tree indexes is kept alive in the
//! entry, so there is no window where offsets dangle.
//!
//! Two disciplines are load-bearing here:
//!
//! - **Streaming retention (ADR-0007, eval-1 F1):** the *caller* drops each
//!   entry after that file's report is collected — the cache is a lookup
//!   structure, never an O(repo) retention vector. The streaming test
//!   encodes the discipline: a scan-shaped loop that evicts keeps
//!   `len()` at the working set; the dashmap pays off only for multi-rule
//!   scans / warm processes (F10).
//! - **Apply-path ownership (eval-2 amendment 1):** the edit choreography
//!   needs the tree *exclusively*; [`ParseCache::take`] removes the entry
//!   and hands ownership over BEFORE any `Tree::edit` (§1.2.1 step 5) —
//!   `get` clones Arc handles for shared reads, `take` is the one-way door.
//!
//! dashmap×rayon insertion rule (ADR-0016 note 7): workers never hold shard
//! guards across a `par_iter` join — `get` scopes its guard to the clone
//! and inserts happen at collection. The race test pins both.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;

use crate::ast::errors::error_count;
use crate::ast::ffi_shim::Tree;
use crate::ast::fingerprint::FingerprintTable;
use crate::vfs::{Generation, VfsPath};

/// One parsed snapshot, version-pinned (Rule T2: bytes and tree die
/// together; Rule T4: the generation it was parsed at).
#[derive(Clone, Debug)]
pub struct ParseEntry {
    /// The overlay generation `bytes` was read at — the primary validation
    /// key.
    pub generation: Generation,
    /// The exact snapshot the tree indexes.
    pub bytes: Arc<[u8]>,
    /// Offsets in `tree` are only meaningful against `bytes`.
    pub tree: Arc<Tree>,
    /// ERROR/MISSING nodes precomputed at parse time — the whole-file gate
    /// basis (KP-4 outcome; §1.2.1 step 7). Zero = parses clean.
    pub error_count: u32,
    /// Content hash of `bytes` — the belt-and-suspenders leg of T4
    /// validation (computed once at construction).
    content_hash: u64,
    /// Lazily-computed Merkle fingerprints (F8, §1.4.3 item 6): the pass
    /// is O(n) and only `Unify` rules and `--duplicates` scans need it, so
    /// it runs on first use and never for rule-only scans. Shared across
    /// clones (one table per parse, however the entry travels).
    fingerprints: Arc<OnceLock<Arc<FingerprintTable>>>,
}

impl ParseEntry {
    /// Pin a fresh parse: computes `error_count` and the content hash here
    /// so no consumer can forget them.
    pub fn new(generation: Generation, bytes: Arc<[u8]>, tree: Tree) -> Self {
        let error_count = error_count(tree.root_node());
        Self {
            generation,
            content_hash: content_hash(&bytes),
            bytes,
            tree: Arc::new(tree),
            error_count,
            fingerprints: Arc::new(OnceLock::new()),
        }
    }

    /// The entry's Merkle fingerprint table, computed on first use against
    /// `trivia` (the language's resolved comment-class kind ids — the same
    /// table `Seq` adjacency skips, ADR-0010). Later calls — with any
    /// arguments — return the first table: the fingerprint is a property
    /// of the parse, and per-grammar trivia tables are constants, so in
    /// practice every caller passes the same list.
    pub fn fingerprints(&self, trivia: &[u16]) -> Arc<FingerprintTable> {
        self.fingerprints
            .get_or_init(|| Arc::new(FingerprintTable::build(&self.tree, &self.bytes, trivia)))
            .clone()
    }

    /// Rule T4 validation: generation match, then Arc-pointer identity
    /// (the O(1) case — the overlay handed out the very same snapshot),
    /// then content hash (equal bytes at an equal generation are the same
    /// logical snapshot, whatever Arc they arrived in).
    pub fn is_valid_for(&self, generation: Generation, current: &Arc<[u8]>) -> bool {
        self.generation == generation
            && (Arc::ptr_eq(&self.bytes, current) || self.content_hash == content_hash(current))
    }
}

/// The in-process parse cache (ADR-0007: no disk cache in v0.1; cold each
/// CLI invocation). Concurrent-safe for rayon workers under the ADR-0016
/// insertion rule — shard guards are held only to clone in [`Self::get`].
pub struct ParseCache {
    entries: DashMap<VfsPath, ParseEntry>,
}

impl ParseCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Entries currently held (the streaming discipline keeps this at the
    /// working set, not the repo size).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Store an entry (collection phase only, per the ADR-0016 insertion
    /// rule; last writer wins among identical-content racers).
    pub fn insert(&self, path: VfsPath, entry: ParseEntry) {
        self.entries.insert(path, entry);
    }

    /// Look up the entry for `path` validated against the overlay's current
    /// `(generation, bytes)` — a **shared-read clone** of the Arcs (Rule T1:
    /// concurrent reads through shared handles are the blessed pattern).
    /// A stale entry is never served and is evicted on sight.
    pub fn get(
        &self,
        path: &VfsPath,
        generation: Generation,
        current: &Arc<[u8]>,
    ) -> Option<ParseEntry> {
        // Guard scope ends at the clone — never held across anything else.
        let candidate = {
            let guard = self.entries.get(path)?;
            guard.value().clone()
        };
        if candidate.is_valid_for(generation, current) {
            Some(candidate)
        } else {
            self.entries.remove(path);
            None
        }
    }

    /// Remove the entry and hand over ownership — the apply path's one-way
    /// door (§1.2.1 step 5 / ADR-0007): the caller ends up holding the only
    /// `Arc<Tree>` handle, which is what the exclusivity guard demands
    /// before `Tree::edit`.
    pub fn take(&self, path: &VfsPath) -> Option<ParseEntry> {
        self.entries.remove(path).map(|(_, entry)| entry)
    }
}

impl Default for ParseCache {
    fn default() -> Self {
        Self::new()
    }
}

/// SipHash of the snapshot bytes — identity check only (T4's second leg),
/// not a security property.
fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::LangRegistry;
    use crate::ast::parse::parse_snapshot;
    use crate::vfs::Overlay;

    #[cfg(feature = "lang-rust")]
    fn setup() -> (LangRegistry, crate::ast::lang::LangId) {
        let registry = LangRegistry::new();
        let id = registry
            .for_extension("rs")
            .expect("lang-rust is the default feature set");
        (registry, id)
    }

    #[cfg(feature = "lang-rust")]
    fn entry_for(
        registry: &LangRegistry,
        id: crate::ast::lang::LangId,
        generation: Generation,
        source: &[u8],
    ) -> ParseEntry {
        let mut parser = registry.parser(id).expect("ABI-verified pair");
        let bytes: Arc<[u8]> = Arc::from(source);
        let tree = parse_snapshot(&mut parser, &bytes).expect("valid rust");
        ParseEntry::new(generation, bytes, tree)
    }

    /// Rule T4, primary leg: a generation bump (any overlay mutation)
    /// invalidates; the stale entry is never served and is evicted.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn gen_bump_invalidates_and_evicts() {
        let (_registry, _id) = setup();
        let overlay = Overlay::new(Arc::new(crate::vfs::test_util::seeded_memfs()));
        let path = VfsPath::new("/proj/src/main.rs").expect("canonical path");

        let bytes = overlay.read_file(&path).expect("disk file");
        let generation = overlay.current_gen(&path).expect("generation");
        let cache = ParseCache::new();
        cache.insert(path.clone(), entry_for(&_registry, _id, generation, &bytes));

        assert!(
            cache.get(&path, generation, &bytes).is_some(),
            "same generation + same snapshot: served"
        );
        assert_eq!(cache.len(), 1);

        overlay
            .set(&path, "fn main() { restaged() }\n")
            .expect("mutation bumps the generation");
        let new_bytes = overlay.read_file(&path).expect("staged bytes");
        let new_gen = overlay.current_gen(&path).expect("bumped generation");

        assert!(
            cache.get(&path, new_gen, &new_bytes).is_none(),
            "stale entry never served after a generation bump"
        );
        assert_eq!(cache.len(), 0, "invalid entry evicted on sight");
    }

    /// Rule T4, belt-and-suspenders legs: pointer identity serves equal
    /// content instantly; equal content in a different Arc still serves;
    /// different content at the same generation is stale (the txn-rollback
    /// shape) and is evicted.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn content_hash_leg_validates_equal_and_rejects_divergent_bytes() {
        let (registry, id) = setup();
        let source = b"fn one() -> u32 { 1 }\n";
        let generation = Generation::default();
        let path = VfsPath::new("/proj/a.rs").expect("canonical path");
        let entry = entry_for(&registry, id, generation, source);
        let cache = ParseCache::new();
        cache.insert(path.clone(), entry);

        let live = cache
            .get(&path, generation, &Arc::from(source.as_slice()))
            .expect("entry still live");
        let same_arc = Arc::clone(&live.bytes);
        drop(live);
        assert!(
            cache.get(&path, generation, &same_arc).is_some(),
            "same Arc: pointer-identity fast path"
        );

        let equal_elsewhere: Arc<[u8]> = Arc::from(source.as_slice());
        assert!(
            cache.get(&path, generation, &equal_elsewhere).is_some(),
            "equal content in a different Arc: hash leg serves it"
        );

        let diverged: Arc<[u8]> = Arc::from(b"fn two() -> u32 { 2 }\n".as_slice());
        assert!(
            cache.get(&path, generation, &diverged).is_none(),
            "same generation, different bytes: stale, never served"
        );
        assert_eq!(cache.len(), 0, "divergent entry evicted");
    }

    /// ADR-0016 note 7 + T1: workers parse independently with no cache
    /// contact, then race inserts at collection — one entry survives, it is
    /// valid, and shared-read clones coexist (the blessed concurrent-read
    /// pattern).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn concurrent_inserts_race_to_one_valid_entry() {
        use rayon::prelude::*;

        let (registry, id) = setup();
        let bytes: Arc<[u8]> = Arc::from(b"fn race() -> u32 { 7 }\n".as_slice());
        let generation = Generation::default();
        let cache = Arc::new(ParseCache::new());
        let path = Arc::new(VfsPath::new("/proj/race.rs").expect("canonical path"));

        // Snapshot-then-parse-then-insert: no shard guard is ever held
        // across the join.
        let readers: Vec<_> = (0..8_u32)
            .into_par_iter()
            .map(|round| {
                let mut parser = registry.parser(id).expect("parser per worker");
                let tree = parse_snapshot(&mut parser, &bytes).expect("valid rust");
                let entry = ParseEntry::new(generation, Arc::clone(&bytes), tree);
                cache.insert((*path).clone(), entry);
                // Every worker also takes a shared-read clone afterwards.
                cache
                    .get(&path, generation, &bytes)
                    .filter(|_| round % 2 == 0)
            })
            .collect();

        assert_eq!(
            cache.len(),
            1,
            "racing identical-content inserts leave one entry"
        );
        assert_eq!(readers.len(), 8);
        assert!(
            readers.iter().filter(|r| r.is_some()).all(|r| r
                .as_ref()
                .is_some_and(|e| e.is_valid_for(generation, &bytes))),
            "shared-read clones observe valid entries"
        );
        assert!(cache.get(&path, generation, &bytes).is_some());
    }

    /// ADR-0007 streaming discipline, as the scan loop will run it: parse
    /// (or hit), collect the file's report, evict. The cache never grows
    /// past the working set — and the negative control proves the bound is
    /// the eviction, not vacuity.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn scan_shaped_eviction_keeps_the_cache_at_the_working_set() {
        let (registry, id) = setup();
        let files: Vec<(VfsPath, Arc<[u8]>)> = (0..12_u32)
            .map(|i| {
                (
                    VfsPath::new(format!("/proj/src/file{i}.rs")).expect("canonical path"),
                    Arc::from(format!("fn f{i}() -> u32 {{ {i} }}\n").into_bytes()),
                )
            })
            .collect();
        let generation = Generation::default();
        let cache = ParseCache::new();

        // The scan shape: streaming eviction, working set of one file.
        let mut peak = 0_usize;
        let reports: Vec<(u16, u32)> = files
            .iter()
            .map(|(path, bytes)| {
                let entry = match cache.get(path, generation, bytes) {
                    Some(hit) => hit,
                    None => {
                        let mut parser = registry.parser(id).expect("parser");
                        let tree = parse_snapshot(&mut parser, bytes).expect("valid rust");
                        let fresh = ParseEntry::new(generation, Arc::clone(bytes), tree);
                        cache.insert(path.clone(), fresh.clone());
                        fresh
                    }
                };
                // The "report": plain data collected from the entry, then
                // the entry is dropped and evicted (ADR-0007).
                let report = (entry.tree.root_node().kind_id(), entry.error_count);
                drop(entry);
                cache.take(path);
                peak = peak.max(cache.len());
                report
            })
            .collect();

        assert_eq!(reports.len(), files.len());
        assert!(
            reports.iter().all(|(_, errors)| *errors == 0),
            "the corpus parses clean"
        );
        assert_eq!(peak, 0, "with per-file eviction the cache holds nothing");
        assert!(cache.is_empty());

        // Negative control: without the eviction, the same loop WOULD
        // retain one entry per file — the bound above is the discipline.
        for (path, bytes) in &files {
            let mut parser = registry.parser(id).expect("parser");
            let tree = parse_snapshot(&mut parser, bytes).expect("valid rust");
            cache.insert(
                path.clone(),
                ParseEntry::new(generation, Arc::clone(bytes), tree),
            );
        }
        assert_eq!(
            cache.len(),
            files.len(),
            "retention without eviction is O(files)"
        );
    }

    /// The apply-path one-way door: `take` hands over exclusive ownership
    /// (§1.2.1 step 5) — the choreography then accepts the tree, and a
    /// second take is `None` (no phantom entry left to edit twice).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn take_hands_over_exclusive_ownership_for_the_choreography() {
        let (registry, id) = setup();
        let cache = ParseCache::new();
        let path = VfsPath::new("/proj/taken.rs").expect("canonical path");
        let generation = Generation::default();
        cache.insert(
            path.clone(),
            entry_for(&registry, id, generation, b"fn a() -> u32 { 1 }\n"),
        );

        let entry = cache.take(&path).expect("entry removed and returned");
        assert_eq!(cache.len(), 0);
        assert!(cache.take(&path).is_none(), "one-way door");

        // Exclusive: exactly this handle — the choreography's precondition.
        assert_eq!(Arc::strong_count(&entry.tree), 1);
        let mut owned = entry.tree;
        let splice = crate::ast::parse::Splice {
            start_byte: 3,
            old_end_byte: 4,
            new_text: b"b".to_vec(),
        };
        let mut parser = registry.parser(id).expect("parser");
        let (new_bytes, _tree) =
            crate::ast::parse::reparse_edited(&mut parser, &entry.bytes, &mut owned, &splice)
                .expect("exclusively owned tree edits cleanly");
        assert_eq!(new_bytes.as_ref(), b"fn b() -> u32 { 1 }\n".as_slice());
    }

    /// `error_count` is precomputed at construction: clean sources count 0
    /// without a walk; a mangled one counts its ERROR/MISSING nodes.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn error_count_is_precomputed_on_the_entry() {
        let (registry, id) = setup();
        let clean = entry_for(
            &registry,
            id,
            Generation::default(),
            b"fn ok() -> u32 { 1 }\n",
        );
        assert_eq!(clean.error_count, 0);

        let mangled = entry_for(
            &registry,
            id,
            Generation::default(),
            b"fn broken( -> u32 {\n",
        );
        assert!(
            mangled.error_count > 0,
            "the grammar recovers with error nodes"
        );
        assert_eq!(
            crate::ast::errors::error_count(mangled.tree.root_node()),
            mangled.error_count,
            "the stored count matches a fresh walk"
        );
    }
}
