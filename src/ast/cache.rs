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
//!
//! **F10a (ADR-0018):** the cache grows an optional warm shape — exact
//! hit/miss counters, and a byte bound with LRU eviction over accounted
//! entry cost (snapshot bytes + tree estimate). The CLI stays unbounded +
//! streaming bit-for-bit; only the daemon binds a budget and retains
//! entries across scans.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use serde::Serialize;

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
    /// Lazily-computed node count (F10a, ADR-0018 D4): the retained-cost
    /// estimate's tree leg. Once set, it propagates to later clones.
    nodes: OnceLock<u64>,
}

/// The per-node share of a tree's retained memory a warm entry is charged
/// (ADR-0018 D4): tree-sitter stores each subtree inline in the tree's node
/// arena; ~16 B/slot is the documented approximation. The bound is a
/// *budget over accounted bytes*, an honest estimate — not an RSS promise.
pub const TREE_BYTES_PER_NODE: u64 = 16;

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
            nodes: OnceLock::new(),
        }
    }

    /// The entry's retained-cost estimate (ADR-0018 D4): snapshot bytes
    /// plus the tree's accounted share. Computed once (the C-level
    /// descendant count is a walk), then cached.
    pub fn cost_bytes(&self) -> u64 {
        let nodes = *self
            .nodes
            .get_or_init(|| self.tree.root_node().descendant_count() as u64);
        self.bytes.len() as u64 + TREE_BYTES_PER_NODE * nodes
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

/// One cache slot: the entry plus its LRU tick (monotonic; refreshed on
/// [`ParseCache::get`] and stamped on insert — the recency the daemon's
/// bound evicts by, ADR-0018 D4).
#[derive(Clone, Debug)]
struct Slot {
    entry: ParseEntry,
    tick: u64,
}

/// The in-process parse cache (ADR-0007: no disk cache; cold each CLI
/// invocation — the daemon, F10a/ADR-0018, is the warm process). Concurrent-
/// safe for rayon workers under the ADR-0016 insertion rule — shard guards
/// are held only to clone/tick in [`Self::get`].
///
/// F10a additions (all additive; the CLI's unbounded + streaming shape is
/// unchanged): exact hit/miss counters, and an optional **byte bound** with
/// LRU eviction over accounted entry cost. An unbounded cache (the CLI)
/// never computes costs or sweeps — zero overhead on the cold path; a
/// bounded cache (the daemon) pays one cost computation per retained entry.
pub struct ParseCache {
    entries: DashMap<VfsPath, Slot>,
    /// Monotonic LRU clock.
    tick: AtomicU64,
    /// `None` = unbounded (the CLI; the streaming discipline is the bound).
    bound_bytes: Option<u64>,
    /// Running accounted-bytes total, maintained only when bounded.
    total_cost: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

/// A cache's stats snapshot (the daemon's `ping`/`stats` payload, ADR-0018).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CacheStats {
    pub entries: usize,
    /// Accounted bytes (snapshot + tree estimate); 0-semantics honest only
    /// for a bounded cache — the CLI's streaming cache stays ~empty.
    pub bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub bound_bytes: Option<u64>,
}

impl ParseCache {
    /// An empty, unbounded cache (the CLI's shape — the streaming
    /// discipline is the bound, ADR-0007).
    pub fn new() -> Self {
        Self::unbounded()
    }

    /// An empty cache with no bound.
    pub fn unbounded() -> Self {
        Self {
            entries: DashMap::new(),
            tick: AtomicU64::new(0),
            bound_bytes: None,
            total_cost: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// An empty cache bounded to `max_bytes` accounted bytes: inserts
    /// evict least-recently-used entries until the total fits (ADR-0018
    /// D4). Eviction is free for correctness — nothing is ever locked; a
    /// later query just re-parses.
    pub fn bounded(max_bytes: u64) -> Self {
        Self {
            bound_bytes: Some(max_bytes),
            ..Self::unbounded()
        }
    }

    fn next_tick(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed) + 1
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

    /// The paths the cache currently holds entries for (the daemon
    /// watcher's warm set, ADR-0018 D2).
    pub fn paths(&self) -> Vec<VfsPath> {
        self.entries
            .iter()
            .map(|guard| guard.key().clone())
            .collect()
    }

    /// Store an entry (collection phase only, per the ADR-0016 insertion
    /// rule; last writer wins among identical-content racers). Under a
    /// bound, the insert stamps the slot's recency and enforces the LRU
    /// budget.
    pub fn insert(&self, path: VfsPath, entry: ParseEntry) {
        let cost = self.accounted_cost(&entry);
        let slot = Slot {
            entry,
            tick: self.next_tick(),
        };
        let replaced = self.entries.insert(path, slot);
        if let (Some(bound), Some(cost)) = (self.bound_bytes, cost) {
            if let Some(old) = replaced {
                self.total_cost.fetch_sub(
                    self.accounted_cost(&old.entry).unwrap_or_default(),
                    Ordering::Relaxed,
                );
            }
            self.total_cost.fetch_add(cost, Ordering::Relaxed);
            self.enforce_bound(bound);
        }
    }

    /// A fresh entry's accounted cost — computed (and cached on the entry)
    /// only when a bound makes it meaningful; the CLI path never pays this.
    fn accounted_cost(&self, entry: &ParseEntry) -> Option<u64> {
        self.bound_bytes.map(|_| entry.cost_bytes())
    }

    /// Evict least-recently-used slots until the accounted total fits
    /// `bound`. Skipped entirely while under budget (the common case). The
    /// `remove_if` tick check refuses to evict a slot that was refreshed
    /// after the snapshot — a racing reader can keep its file warm.
    fn enforce_bound(&self, bound: u64) {
        if self.total_cost.load(Ordering::Relaxed) <= bound {
            return;
        }
        let mut order: Vec<(u64, u64, VfsPath)> = self
            .entries
            .iter()
            .map(|guard| (guard.tick, guard.entry.cost_bytes(), guard.key().clone()))
            .collect();
        order.sort_unstable_by_key(|&(tick, _, _)| tick);
        for (tick, cost, path) in order {
            if self.total_cost.load(Ordering::Relaxed) <= bound {
                break;
            }
            // The tick match guarantees the removed slot is the snapshot's
            // entry, so the snapshot's cost is the exact accounting delta.
            if self
                .entries
                .remove_if(&path, |_, slot| slot.tick == tick)
                .is_some()
            {
                self.total_cost.fetch_sub(cost, Ordering::Relaxed);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Look up the entry for `path` validated against the overlay's current
    /// `(generation, bytes)` — a **shared-read clone** of the Arcs (Rule T1:
    /// concurrent reads through shared handles are the blessed pattern).
    /// A stale entry is never served and is evicted on sight. Hits and
    /// misses are counted exactly (the daemon's `ping` payload).
    pub fn get(
        &self,
        path: &VfsPath,
        generation: Generation,
        current: &Arc<[u8]>,
    ) -> Option<ParseEntry> {
        // Guard scope ends at the tick + clone — never held across a parse
        // or traversal (§1.1).
        let candidate = match self.entries.get_mut(path) {
            Some(mut guard) => {
                guard.tick = self.next_tick();
                Some(guard.entry.clone())
            }
            None => None,
        };
        match candidate {
            Some(entry) if entry.is_valid_for(generation, current) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(entry)
            }
            _ => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                // Stale-on-sight eviction is T4 doing its job, not an LRU
                // eviction — deliberately not counted in `evictions`.
                drop(self.remove_slot(path));
                None
            }
        }
    }

    /// Validity check without the counters or the LRU refresh — the
    /// watcher's content probe (F10a, ADR-0018 D3): a stamp change alone
    /// must not register as cache traffic or keep a file artificially warm.
    pub fn peek(&self, path: &VfsPath, generation: Generation, current: &Arc<[u8]>) -> bool {
        let candidate = match self.entries.get(path) {
            Some(guard) => guard.entry.clone(),
            None => return false,
        };
        candidate.is_valid_for(generation, current)
    }

    /// Remove a slot, keeping the accounted total honest under a bound.
    fn remove_slot(&self, path: &VfsPath) -> Option<ParseEntry> {
        let (_, slot) = self.entries.remove(path)?;
        if let Some(cost) = self.accounted_cost(&slot.entry) {
            self.total_cost.fetch_sub(cost, Ordering::Relaxed);
        }
        Some(slot.entry)
    }

    /// Remove the entry and hand over ownership — the apply path's one-way
    /// door (§1.2.1 step 5 / ADR-0007), and the daemon watcher's re-parse
    /// path (ADR-0018 D3): the caller ends up holding the only `Arc<Tree>`
    /// handle chain, which is what the exclusivity guard demands before
    /// `Tree::edit`.
    pub fn take(&self, path: &VfsPath) -> Option<ParseEntry> {
        self.remove_slot(path)
    }

    /// The counters + size snapshot (`ping`/`stats`, ADR-0018 D1).
    pub fn stats(&self) -> CacheStats {
        let bytes = match self.bound_bytes {
            // Maintained incrementally (approximate under insert races; the
            // budget is a budget, not a ledger).
            Some(_) => self.total_cost.load(Ordering::Relaxed),
            // The CLI's streaming cache holds ~nothing; a precise sum is
            // cheap at that size.
            None => self.entries.iter().map(|g| g.entry.cost_bytes()).sum(),
        };
        CacheStats {
            entries: self.entries.len(),
            bytes,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            bound_bytes: self.bound_bytes,
        }
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

    /// The shared rust-registry helper (B6: one definition, in
    /// `crate::ast::testing`).
    #[cfg(feature = "lang-rust")]
    use crate::ast::testing::rust as setup;

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

    /// F10a (ADR-0018 D4): hit/miss counters are exact — a served lookup
    /// is a hit, an absent or stale lookup is a miss, and the stale one is
    /// evicted on sight without counting as an LRU eviction.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn counters_track_hits_and_misses_exactly() {
        let (registry, id) = setup();
        let cache = ParseCache::new();
        let path = VfsPath::new("/proj/counters.rs").expect("canonical path");
        let generation = Generation::default();
        let bytes: Arc<[u8]> = Arc::from(b"fn hit() -> u32 { 1 }\n".as_slice());
        cache.insert(path.clone(), entry_for(&registry, id, generation, &bytes));

        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.evictions), (0, 0, 0));

        assert!(cache.get(&path, generation, &bytes).is_some());
        assert!(cache.get(&path, generation, &bytes).is_some());
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses), (2, 0));

        // Absent path: a miss.
        let absent = VfsPath::new("/proj/absent.rs").expect("canonical path");
        assert!(cache.get(&absent, generation, &bytes).is_none());
        assert_eq!(cache.stats().misses, 1);

        // Stale bytes at the same generation: a miss, evicted on sight,
        // NOT an LRU eviction.
        let diverged: Arc<[u8]> = Arc::from(b"fn other() -> u32 { 2 }\n".as_slice());
        assert!(cache.get(&path, generation, &diverged).is_none());
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.evictions), (2, 2, 0));
        assert_eq!(cache.len(), 0, "stale entry evicted on sight");
    }

    /// F10a (ADR-0018 D4): a bounded cache evicts least-recently-used
    /// entries when an insert crosses the budget; a `get` refresh keeps a
    /// file warm past its insertion age. Costs are accounted (snapshot +
    /// tree estimate), so the test derives its budget from the entries'
    /// own reported costs rather than hand-computing node counts.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn bounded_cache_evicts_lru_order() {
        let (registry, id) = setup();
        let generation = Generation::default();
        let sources: Vec<(VfsPath, Arc<[u8]>)> = (0..3_u32)
            .map(|i| {
                (
                    VfsPath::new(format!("/proj/lru{i}.rs")).expect("canonical path"),
                    Arc::from(
                        format!("fn l{i}() -> u32 {{\n    let x{i} = {i};\n    x{i} + 1\n}}\n")
                            .into_bytes(),
                    ),
                )
            })
            .collect();
        let entries: Vec<ParseEntry> = sources
            .iter()
            .map(|(_, bytes)| entry_for(&registry, id, generation, bytes))
            .collect();
        let cost: Vec<u64> = entries.iter().map(ParseEntry::cost_bytes).collect();
        assert!(cost.iter().all(|c| *c > 0));

        // Budget: exactly two of the three entries fit.
        let bound = cost[0] + cost[1];
        let cache = ParseCache::bounded(bound);

        cache.insert(sources[0].0.clone(), entries[0].clone());
        cache.insert(sources[1].0.clone(), entries[1].clone());
        assert_eq!(cache.len(), 2, "two fit exactly");
        assert_eq!(cache.stats().evictions, 0);

        // Touch entry 0 so entry 1 becomes the LRU, then insert entry 2.
        assert!(
            cache
                .get(&sources[0].0, generation, &sources[0].1)
                .is_some()
        );
        cache.insert(sources[2].0.clone(), entries[2].clone());

        assert_eq!(cache.len(), 2, "the bound still holds");
        assert_eq!(cache.stats().evictions, 1, "one LRU eviction");
        assert_eq!(cache.stats().bytes, cost[0] + cost[2]);
        // The refreshed entry 0 survived; entry 1 (older tick) was evicted;
        // the just-inserted entry 2 is present.
        assert!(
            cache
                .get(&sources[0].0, generation, &sources[0].1)
                .is_some()
        );
        assert!(
            cache
                .get(&sources[2].0, generation, &sources[2].1)
                .is_some()
        );
        assert!(
            cache
                .get(&sources[1].0, generation, &sources[1].1)
                .is_none()
        );

        // A later query on the evicted path re-parses on demand (here:
        // re-insert) — eviction never loses data, only warmth.
        cache.insert(sources[1].0.clone(), entries[1].clone());
        assert!(
            cache
                .get(&sources[1].0, generation, &sources[1].1)
                .is_some()
        );
    }

    /// F10a: the unbounded (CLI) shape never computes costs — `stats` on a
    /// streaming-shaped cache stays honest, and `take` keeps the one-way
    /// door behavior the apply choreography relies on.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn unbounded_cache_streams_and_takes_as_before() {
        let (registry, id) = setup();
        let generation = Generation::default();
        let cache = ParseCache::new();
        assert_eq!(cache.stats().bound_bytes, None);
        let path = VfsPath::new("/proj/stream.rs").expect("canonical path");
        let bytes: Arc<[u8]> = Arc::from(b"fn stream() -> u32 { 9 }\n".as_slice());
        cache.insert(path.clone(), entry_for(&registry, id, generation, &bytes));

        let entry = cache.take(&path).expect("one-way door still hands over");
        assert_eq!(Arc::strong_count(&entry.tree), 1);
        assert!(cache.take(&path).is_none());
        assert_eq!(cache.len(), 0);
    }
}
