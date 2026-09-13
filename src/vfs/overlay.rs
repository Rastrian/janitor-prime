//! The overlay (PLAN §1.3, F2.4) — staged state layered over a base
//! [`ProjectFs`].
//!
//! # The four invariants (each pinned by a named test in `src/vfs.rs`)
//!
//! 1. **Read-through:** `read_file` consults the buffer first; a miss falls
//!    to `base_fs`. A staged file returns staged bytes even when different
//!    bytes exist on disk at the same path.
//! 2. **Anti-phantom:** `read_dir` ignores the buffer entirely. Listings stay
//!    a faithful mirror of disk so an agent can never mistake a staged file
//!    for a persisted one; `virtual_paths()` is the separate, explicit
//!    visibility surface (ADR-0016 default-OFF union).
//! 3. **Tombstone delete (ADR-0009):** `delete` writes a `Tombstone`, which
//!    shadows the base file for `read_file` (typed `BufferDeleted`) while
//!    leaving `read_dir` untouched — removing the entry instead would
//!    resurrect the disk copy, violating the mandated semantics.
//! 4. **Fallback:** with no buffer entry, reads are byte-identical to the
//!    base filesystem.
//!
//! # Locking and the guard discipline (§1.1)
//!
//! Read guards are held only to look up and `clone` an `Arc` — never across
//! base-filesystem I/O (the fallback in `read_file` runs after the guard is
//! dropped). Mutations hold the `buffer` write guard across the generation
//! bump in `gens`, which serializes (content, generation) pairs: a reader
//! observes either the fully-old or the fully-new state of a path, and
//! per-path generations are strictly monotonic under `set`/`delete` — the
//! property F2.8 hammers. All paths are canonicalized to [`VfsPath`] at the
//! boundary, so spelling variants of one file share one slot and one
//! generation.
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fs::{ProjectFs, ReadDirSnapshot};
use crate::vfs::path::VfsPath;
use crate::vfs::stat::Stat;

/// Rule T4's invalidation feed: a per-path counter bumped by every mutation
/// (plus the global counter, [`Overlay::global_gen`]). Never read heuristics
/// into it — a cache entry is valid iff its generation matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(u64);

impl Generation {
    /// The counter value (0 = never mutated).
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// A staged buffer entry (ADR-0009).
#[derive(Clone, Debug)]
pub enum BufferEntry {
    /// Staged/edited content; `Arc` so snapshots are O(1) and contention-free.
    File(Arc<[u8]>),
    /// A staged delete, shadowing the base file until commit or rollback.
    Tombstone,
}

pub struct Overlay {
    pub(crate) base_fs: Arc<dyn ProjectFs>,
    pub(crate) buffer: RwLock<HashMap<VfsPath, BufferEntry>>,
    pub(crate) gens: RwLock<HashMap<VfsPath, Generation>>,
    /// Global mutation counter (Rule T4: "and a global one"). Per-path
    /// generations are drawn from it, so they are globally ordered too.
    global_gen: AtomicU64,
}

impl Overlay {
    pub fn new(base_fs: Arc<dyn ProjectFs>) -> Self {
        Self {
            base_fs,
            buffer: RwLock::new(HashMap::new()),
            gens: RwLock::new(HashMap::new()),
            global_gen: AtomicU64::new(0),
        }
    }

    /// Stage `bytes` at `path`, bumping the path's generation. Returns the
    /// new generation (the T4 feed).
    pub fn set(&self, path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> VfsResult<Generation> {
        let key = VfsPath::new(path)?;
        let (_, _, new_gen) = self.mutate(&key, BufferEntry::File(Arc::from(bytes.as_ref())));
        Ok(new_gen)
    }

    /// [`Self::set`], but the bytes must be valid UTF-8 (E6 policy, PLAN §3
    /// F4 item 10): invalid input is a typed `InvalidUtf8` error and stages
    /// nothing — never a lossy coercion.
    pub fn set_string(
        &self,
        path: impl AsRef<Path>,
        text: impl AsRef<[u8]>,
    ) -> VfsResult<Generation> {
        let bytes = text.as_ref();
        if std::str::from_utf8(bytes).is_err() {
            return Err(VfsError::InvalidUtf8 {
                path: path.as_ref().display().to_string(),
            });
        }
        self.set(path, bytes)
    }

    /// Stage a delete (tombstone). Only a path the overlay tracks — a staged
    /// `File` entry — can be deleted; anything else is a typed `NotStaged`
    /// caller bug, and the buffer is left untouched.
    pub fn delete(&self, path: impl AsRef<Path>) -> VfsResult<Generation> {
        let key = VfsPath::new(path)?;
        let (_, _, new_gen) = self.swap_delete(&key)?;
        Ok(new_gen)
    }

    /// Pure buffer lookup (no disk fallback): `Ok(None)` = not staged,
    /// `Ok(Some(bytes))` = staged content, `Err(BufferDeleted)` = tombstoned.
    /// The merged view is [`Self::read_file`].
    pub fn get(&self, path: impl AsRef<Path>) -> VfsResult<Option<Arc<[u8]>>> {
        let key = VfsPath::new(path)?;
        match self.entry(&key)? {
            Some(BufferEntry::File(bytes)) => Ok(Some(bytes)),
            Some(BufferEntry::Tombstone) => Err(VfsError::BufferDeleted {
                path: key.to_string(),
            }),
            None => Ok(None),
        }
    }

    /// [`Self::get`] decoded as UTF-8; invalid staged bytes (or a tombstone)
    /// are typed errors, never lossy.
    pub fn get_string(&self, path: impl AsRef<Path>) -> VfsResult<Option<String>> {
        let staged = self.get(path.as_ref())?;
        match staged {
            None => Ok(None),
            Some(bytes) => match std::str::from_utf8(&bytes) {
                Ok(text) => Ok(Some(text.to_owned())),
                Err(_) => Err(VfsError::InvalidUtf8 {
                    path: VfsPath::new(path)?.to_string(),
                }),
            },
        }
    }

    /// Invariant 1 — read-through: buffer first, disk fallback. The read
    /// guard is dropped before the (possibly real-I/O) fallback runs.
    pub fn read_file(&self, path: impl AsRef<Path>) -> VfsResult<Arc<[u8]>> {
        let key = VfsPath::new(path)?;
        match self.entry(&key)? {
            Some(BufferEntry::File(bytes)) => Ok(bytes),
            Some(BufferEntry::Tombstone) => Err(VfsError::BufferDeleted {
                path: key.to_string(),
            }),
            None => self.base_fs.read_file(key.as_path()),
        }
    }

    /// Invariant 2 — anti-phantom: the buffer is not consulted at all.
    /// Staged files are reachable only via [`Self::virtual_paths`].
    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfsResult<ReadDirSnapshot> {
        let key = VfsPath::new(path)?;
        self.base_fs.read_dir(key.as_path())
    }

    /// Buffer-first stat (F2.7): staged entries stat synthetic
    /// (`Stat::virtual_file`, `is_virtual: true`, staged length), tombstones
    /// stat deleted, and a miss falls to the base — disk truth,
    /// `is_virtual: false`.
    pub fn stat(&self, path: impl AsRef<Path>) -> VfsResult<Stat> {
        let key = VfsPath::new(path)?;
        match self.entry(&key)? {
            Some(BufferEntry::File(bytes)) => Ok(Stat::virtual_file(bytes.len() as u64)),
            Some(BufferEntry::Tombstone) => Err(VfsError::BufferDeleted {
                path: key.to_string(),
            }),
            None => self.base_fs.stat(key.as_path()),
        }
    }

    /// The explicit virtual-file visibility surface: staged `File` paths
    /// (tombstones add nothing), canonically sorted. `scan` unions this with
    /// the disk walk only under `--include-virtual` (ADR-0016, default OFF).
    pub fn virtual_paths(&self) -> Vec<VfsPath> {
        let buffer = self.buffer.read().unwrap_or_else(|e| e.into_inner());
        let mut paths: Vec<VfsPath> = buffer
            .iter()
            .filter(|(_, entry)| matches!(entry, BufferEntry::File(_)))
            .map(|(path, _)| path.clone())
            .collect();
        paths.sort();
        paths
    }

    /// The path's current generation (0 = never mutated) — Rule T4's cache
    /// validation key.
    pub fn current_gen(&self, path: impl AsRef<Path>) -> VfsResult<Generation> {
        let key = VfsPath::new(path)?;
        let gens = self.gens.read().unwrap_or_else(|e| e.into_inner());
        Ok(gens.get(&key).copied().unwrap_or_default())
    }

    /// The global mutation counter (Rule T4).
    pub fn global_gen(&self) -> u64 {
        self.global_gen.load(Ordering::Acquire)
    }

    /// Clone the buffer entry for `key` under a read guard that is dropped
    /// before returning (guard discipline: clone the `Arc`, never hold the
    /// guard onward).
    fn entry(&self, key: &VfsPath) -> VfsResult<Option<BufferEntry>> {
        // Lock poisoning is recovered uniformly (into_inner): the buffered
        // `HashMap` ops cannot tear, `panic = "abort"` makes poisoning
        // unreachable in production, and a panicked test's leaked state is
        // the test's failure, not a VFS error.
        let buffer = self.buffer.read().unwrap_or_else(|e| e.into_inner());
        Ok(buffer.get(key).cloned())
    }

    /// The tombstone swap behind [`Self::delete`], exposing the journal
    /// triple (prior entry, prior generation, new generation) that
    /// transactions roll back to. Same critical-section shape as
    /// [`Self::mutate`]: the `gens` bump happens under the `buffer` write
    /// guard.
    pub(crate) fn swap_delete(
        &self,
        key: &VfsPath,
    ) -> VfsResult<(Option<BufferEntry>, Generation, Generation)> {
        let new_val = self.global_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let mut buffer = self.buffer.write().unwrap_or_else(|e| e.into_inner());
        match buffer.get(key) {
            Some(BufferEntry::File(prior_bytes)) => {
                let prior = Some(BufferEntry::File(Arc::clone(prior_bytes)));
                buffer.insert(key.clone(), BufferEntry::Tombstone);
                let mut gens = self.gens.write().unwrap_or_else(|e| e.into_inner());
                let prior_gen = gens.get(key).copied().unwrap_or_default();
                gens.insert(key.clone(), Generation(new_val));
                Ok((prior, prior_gen, Generation(new_val)))
            }
            _ => Err(VfsError::NotStaged {
                path: key.to_string(),
            }),
        }
    }

    /// Rewind one journal entry: put `prior` (or nothing) back and restore
    /// the generation it had. Txn rollback/flush-failure path only — the
    /// content returns to its pre-stage state, and so does its T4 key.
    pub(crate) fn restore(
        &self,
        key: &VfsPath,
        prior: Option<BufferEntry>,
        generation: Generation,
    ) {
        let mut buffer = self.buffer.write().unwrap_or_else(|e| e.into_inner());
        match prior {
            Some(entry) => {
                buffer.insert(key.clone(), entry);
            }
            None => {
                buffer.remove(key);
            }
        }
        drop(buffer);
        let mut gens = self.gens.write().unwrap_or_else(|e| e.into_inner());
        gens.insert(key.clone(), generation);
    }

    /// Drop a successfully flushed buffer entry (commit success path). The
    /// generation is deliberately left alone: disk now holds the same bytes
    /// the buffer did, so (generation, content) cache pairs stay valid.
    pub(crate) fn clear_entry(&self, key: &VfsPath) {
        let mut buffer = self.buffer.write().unwrap_or_else(|e| e.into_inner());
        buffer.remove(key);
    }

    /// Buffer snapshot for `key` (transactions read what a stage left).
    pub(crate) fn buffer_entry(&self, key: &VfsPath) -> Option<BufferEntry> {
        let buffer = self.buffer.read().unwrap_or_else(|e| e.into_inner());
        buffer.get(key).cloned()
    }

    /// Insert `entry` at `key`, returning (prior entry, prior generation,
    /// new generation). The whole swap — including the `gens` bump — happens
    /// under the `buffer` write guard, so mutations are serialized with
    /// their generation writes (strict per-path monotonicity).
    pub(crate) fn mutate(
        &self,
        key: &VfsPath,
        entry: BufferEntry,
    ) -> (Option<BufferEntry>, Generation, Generation) {
        let new_val = self.global_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let mut buffer = self.buffer.write().unwrap_or_else(|e| e.into_inner());
        let prior = buffer.insert(key.clone(), entry);
        let mut gens = self.gens.write().unwrap_or_else(|e| e.into_inner());
        let prior_gen = gens.get(key).copied().unwrap_or_default();
        gens.insert(key.clone(), Generation(new_val));
        (prior, prior_gen, Generation(new_val))
    }
}

#[cfg(test)]
mod concurrency {
    //! F2.8: the shared-`Arc<Overlay>` hammer, plus the §1.1 guard-discipline
    //! fence. Worker counts 1 and 8 are fixed as **explicit rayon pools**
    //! (`ThreadPoolBuilder::num_threads`), which control the real worker
    //! count regardless of environment — the `RAYON_NUM_THREADS=1/8` runs
    //! the gates add exercise the same code through the env knob. The
    //! torn-read detector was mutation-checked: a writer injecting a foreign
    //! buffer fails this test.

    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use rayon::prelude::*;

    use super::{Generation, Overlay};
    use crate::vfs::error::{VfsError, VfsResult};
    use crate::vfs::fs::{DirEntry, ProjectFs, ReadDirSnapshot};
    use crate::vfs::stat::Stat;
    use crate::vfs::test_util::MemFs;

    const PATTERN_A_LEN: usize = 512;
    const PATTERN_B_LEN: usize = 256;

    fn pattern_a() -> Arc<[u8]> {
        Arc::from(vec![b'A'; PATTERN_A_LEN])
    }

    fn pattern_b() -> Arc<[u8]> {
        Arc::from(vec![b'B'; PATTERN_B_LEN])
    }

    fn hammer_paths() -> Vec<String> {
        ["a", "b", "c", "d"]
            .iter()
            .map(|n| format!("/proj/src/{n}.rs"))
            .collect()
    }

    /// One hammer round: a single writer hammers `set`/`delete` (with
    /// tombstone transitions) on four overlapping paths while 8 rayon
    /// workers take `read_file`/`current_gen`/`read_dir` snapshots. Valid
    /// reader observations: exactly pattern A, exactly pattern B, or the
    /// transient typed `BufferDeleted` — anything else is a torn read. The
    /// writer asserts its returned generations strictly increase per path;
    /// readers assert their samples never regress.
    fn hammer_round(threads: usize, writer_iters: usize, reader_iters: usize) {
        let paths = hammer_paths();
        let a = pattern_a();
        let b = pattern_b();

        let mut fs = MemFs::new().with_dir("/proj").with_dir("/proj/src");
        for path in &paths {
            fs = fs.with_file(path, &a);
        }
        let overlay = Arc::new(Overlay::new(Arc::new(fs)));

        let violations: Mutex<Vec<String>> = Mutex::new(Vec::new());

        let writer_overlay = Arc::clone(&overlay);
        let writer_paths = paths.clone();
        let writer_a = Arc::clone(&a);
        let writer_b = Arc::clone(&b);
        let writer = thread::spawn(move || {
            let mut last_gen: HashMap<String, Generation> = HashMap::new();
            for i in 0..writer_iters {
                for (pi, path) in writer_paths.iter().enumerate() {
                    let g = writer_overlay
                        .set(
                            path,
                            if (i + pi) % 2 == 0 {
                                &writer_a
                            } else {
                                &writer_b
                            },
                        )
                        .unwrap();
                    assert!(
                        last_gen.get(path).is_none_or(|&l| g > l),
                        "writer generation regressed on {path}"
                    );
                    last_gen.insert(path.clone(), g);

                    // Tombstone transitions on a subset: delete is a valid
                    // transient state readers must observe typed, and the
                    // next iteration's set resurrects the file.
                    if i % 3 == 0 && pi % 2 == 0 {
                        let g = writer_overlay.delete(path).unwrap();
                        assert!(
                            g > last_gen[path],
                            "writer generation regressed on {path} (delete)"
                        );
                        last_gen.insert(path.clone(), g);
                    }
                }
            }
        });

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let reader_overlay = Arc::clone(&overlay);
        pool.install(|| {
            (0..8usize).into_par_iter().for_each(|reader| {
                let mut last: HashMap<String, Generation> = HashMap::new();
                for k in 0..reader_iters {
                    let path = &paths[(reader + k) % paths.len()];
                    match reader_overlay.read_file(path) {
                        Ok(bytes) => {
                            if bytes != a && bytes != b {
                                violations
                                    .lock()
                                    .unwrap()
                                    .push(format!("torn read on {path}: {} bytes", bytes.len()));
                            }
                        }
                        Err(VfsError::BufferDeleted { .. }) => {}
                        Err(err) => violations
                            .lock()
                            .unwrap()
                            .push(format!("unexpected read error on {path}: {err}")),
                    }
                    let g = reader_overlay.current_gen(path).unwrap();
                    if let Some(&l) = last.get(path)
                        && g < l
                    {
                        violations
                            .lock()
                            .unwrap()
                            .push(format!("generation regressed on {path}: {l:?} -> {g:?}"));
                    }
                    last.insert(path.clone(), g);
                    if k % 64 == 0 {
                        // Listings run concurrently too: pure base, but the
                        // call itself must never wedge behind the writer.
                        reader_overlay.read_dir("/proj/src").unwrap();
                    }
                }
            });
        });

        writer.join().unwrap();
        let found = violations.lock().unwrap();
        assert!(
            found.is_empty(),
            "{} violation(s) under {threads} rayon thread(s): {found:#?}",
            found.len()
        );
    }

    /// The mandated concurrency gate: run the hammer under 1 and 8 explicit
    /// rayon worker threads. Completing at all is the no-deadlock proof —
    /// enforced with a hard outer timeout so a deadlock fails the test
    /// instead of hanging CI.
    #[test]
    fn concurrent_writer_and_readers_no_torn_reads_no_deadlock_monotonic_gens() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            hammer_round(1, 1_500, 800);
            hammer_round(8, 1_500, 800);
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("deadlock suspected: hammer did not finish in 60s");
    }

    /// §1.1 guard discipline, deterministically: a base whose `read_file`
    /// blocks until released. The overlay's read fallback must have dropped
    /// its buffer guard *before* entering base I/O — if it had not, the
    /// concurrent `set` below would deadlock against the held read guard,
    /// and the timeout turns that into a test failure instead of a hang.
    #[test]
    fn read_guards_are_never_held_across_base_io() {
        /// Blocks on `exit` whenever `/missing` is read; everything else is
        /// inert. `Receiver` is `Send` but not `Sync`, hence the `Mutex`.
        struct BlockFs {
            enter: mpsc::Sender<()>,
            exit: Mutex<mpsc::Receiver<()>>,
        }

        impl ProjectFs for BlockFs {
            fn read_dir(&self, _path: &Path) -> VfsResult<ReadDirSnapshot> {
                Ok(ReadDirSnapshot::new(vec![DirEntry {
                    name: "static".to_owned(),
                    is_dir: false,
                }]))
            }

            fn read_file(&self, path: &Path) -> VfsResult<Arc<[u8]>> {
                if path == Path::new("/missing") {
                    self.enter.send(()).ok();
                    self.exit.lock().unwrap().recv().ok();
                    return Err(VfsError::NotFound {
                        path: path.display().to_string(),
                    });
                }
                Ok(Arc::from(b"present".as_slice()))
            }

            fn stat(&self, _path: &Path) -> VfsResult<Stat> {
                Ok(Stat {
                    len: 7,
                    is_file: true,
                    is_dir: false,
                    is_virtual: false,
                })
            }
        }

        let (enter_tx, enter_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();

        let overlay = Arc::new(Overlay::new(Arc::new(BlockFs {
            enter: enter_tx,
            exit: Mutex::new(exit_rx),
        })));

        // Reader A: falls through the (empty) buffer into the blocking base.
        let overlay_a = Arc::clone(&overlay);
        let reader = thread::spawn(move || overlay_a.read_file("/missing"));

        // A is now inside the base call.
        enter_rx.recv().unwrap();

        // Writer B: needs the buffer write lock. If A still held its read
        // guard across the base I/O, B would block forever.
        let overlay_b = Arc::clone(&overlay);
        thread::spawn(move || {
            overlay_b.set("/other", b"unrelated").unwrap();
            done_tx.send(()).unwrap();
        });
        done_rx.recv_timeout(Duration::from_secs(5)).expect(
            "guard discipline violated: set() blocked while a read fallback sat in base I/O",
        );

        // Release A and confirm it observed the typed fallback result.
        exit_tx.send(()).unwrap();
        assert!(matches!(
            reader.join().unwrap(),
            Err(VfsError::NotFound { .. })
        ));
    }
}
