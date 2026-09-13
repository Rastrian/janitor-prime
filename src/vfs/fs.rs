//! The read-side filesystem abstraction (PLAN §1.3, F2.3).
//!
//! [`ProjectFs`] is deliberately read-only: the overlay layers staged state
//! *on top* of an implementor, and the only production implementor, [`Os`],
//! is a thin, error-mapping proxy over [`std::fs`]. Write capability lives
//! on `Os` itself (tmp+rename flush, F2.6) — a transaction commits *to disk*,
//! never through the read trait the overlay virtualizes.
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::stat::Stat;

/// What the overlay (and every `read_dir`-based walk) sees of a directory.
pub trait ProjectFs: Send + Sync {
    /// List a directory's direct children, as a deterministic snapshot.
    fn read_dir(&self, path: &Path) -> VfsResult<ReadDirSnapshot>;
    /// Read a whole file. `Arc<[u8]>` so snapshots are O(1) and immutable
    /// (PLAN §1.1 guard discipline, ADR-0009).
    fn read_file(&self, path: &Path) -> VfsResult<Arc<[u8]>>;
    /// Stat a path.
    fn stat(&self, path: &Path) -> VfsResult<Stat>;
}

/// One child of a directory, name-only (the parent is the queried path).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DirEntry {
    /// Basename, UTF-8 (non-UTF-8 names are a typed `InvalidUtf8` error, per
    /// the E6 policy of never laundering invalid bytes).
    pub name: String,
    pub is_dir: bool,
}

/// A directory listing frozen at read time, canonically sorted by name
/// (ADR-0016's deterministic-order discipline applied at the source).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ReadDirSnapshot {
    entries: Vec<DirEntry>,
}

impl ReadDirSnapshot {
    /// Build a snapshot, sorting `entries` into canonical order.
    pub(crate) fn new(mut entries: Vec<DirEntry>) -> Self {
        entries.sort();
        Self { entries }
    }

    /// The sorted entries.
    pub fn entries(&self) -> &[DirEntry] {
        &self.entries
    }

    /// Entry basenames, in canonical order.
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// Whether a child with this basename is listed.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }
}

/// The production [`ProjectFs`]: a stateless proxy over [`std::fs`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Os;

impl Os {
    /// Map an [`io::Error`] onto the typed taxonomy (PLAN §1.7).
    fn map_io(err: io::Error, path: &Path) -> VfsError {
        let shown = path.display().to_string();
        match err.kind() {
            io::ErrorKind::NotFound => VfsError::NotFound { path: shown },
            io::ErrorKind::PermissionDenied => VfsError::Permission { path: shown },
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidFilename => {
                VfsError::InvalidPath { path: shown }
            }
            _ => VfsError::Io {
                path: shown,
                message: err.to_string(),
            },
        }
    }
}

impl ProjectFs for Os {
    fn read_dir(&self, path: &Path) -> VfsResult<ReadDirSnapshot> {
        let mut entries = Vec::new();
        let listing = fs::read_dir(path).map_err(|e| Self::map_io(e, path))?;
        for entry in listing {
            let entry = entry.map_err(|e| Self::map_io(e, path))?;
            // `DirEntry::file_type` does not follow symlinks (it is
            // `d_type`/lstat-shaped): a symlink lists as itself. The F4 walk
            // policy revisits that; the snapshot stays honest about it.
            let is_dir = entry
                .file_type()
                .map_err(|e| Self::map_io(e, &entry.path()))?
                .is_dir();
            let name = entry.file_name();
            let name = match name.into_string() {
                Ok(name) => name,
                Err(raw) => {
                    return Err(VfsError::InvalidUtf8 {
                        path: path.join(raw).display().to_string(),
                    });
                }
            };
            entries.push(DirEntry { name, is_dir });
        }
        Ok(ReadDirSnapshot::new(entries))
    }

    fn read_file(&self, path: &Path) -> VfsResult<Arc<[u8]>> {
        let bytes = fs::read(path).map_err(|e| Self::map_io(e, path))?;
        Ok(bytes.into())
    }

    fn stat(&self, path: &Path) -> VfsResult<Stat> {
        let meta = fs::metadata(path).map_err(|e| Self::map_io(e, path))?;
        Ok(Stat {
            len: meta.len(),
            is_file: meta.is_file(),
            is_dir: meta.is_dir(),
            is_virtual: false,
        })
    }
}
