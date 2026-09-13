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
        let new_val = self.global_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let mut buffer = self.buffer.write().unwrap_or_else(|e| e.into_inner());
        match buffer.get(&key) {
            Some(BufferEntry::File(_)) => {
                buffer.insert(key.clone(), BufferEntry::Tombstone);
                // Keep the buffer guard across the bump (see module docs).
                let mut gens = self.gens.write().unwrap_or_else(|e| e.into_inner());
                gens.insert(key, Generation(new_val));
                Ok(Generation(new_val))
            }
            _ => Err(VfsError::NotStaged {
                path: key.to_string(),
            }),
        }
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
