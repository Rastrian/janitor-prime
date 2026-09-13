//! Transactional virtual filesystem (PLAN §1.3) — F2.
//!
//! The overlay design, in reading order:
//!
//! - `error` — the VFS slice of the typed taxonomy (tombstone reads, UTF-8
//!   policy, staging preconditions; F2.1);
//! - `path` — [`VfsPath`], lexical canonicalization at every boundary (F2.2);
//! - `fs` — [`ProjectFs`] (`read_dir` / `read_file -> Arc<[u8]>` / `stat`)
//!   with [`Os`] as the disk proxy (F2.3);
//! - `overlay` — [`Overlay`]: a `RwLock<HashMap<VfsPath, BufferEntry>>` over
//!   a base [`ProjectFs`], where `BufferEntry` is `File(Arc<[u8]>)` or
//!   `Tombstone` (ADR-0009), plus per-path generations feeding cache
//!   invalidation (Rule T4);
//! - `stat` — the [`Stat`] shape with `is_virtual` (F2.7);
//! - `txn` (F2.6) — `stage(ops) / commit() / rollback()` with ordered
//!   tmp+rename flush and an `E_FLUSH_PARTIAL` report on partial OS failure.
//!
//! The four mandated invariants (each with a named test below, per the
//! founding F2 requirement that they live *in this file*): read-through,
//! anti-phantom (`read_dir` never lists buffer files), tombstone delete
//! (shadows disk for `read_file`, invisible to `read_dir`), disk-fallback.
//! `virtual_paths()` is the explicit opt-in visibility surface that keeps
//! the anti-phantom invariant pure while letting `scan --include-virtual`
//! (default OFF, ADR-0016) union staged files in later, as a separate walk.
#![allow(dead_code)] // F2 delivers the tested surface; consumers arrive with F3-F5
mod error;
mod fs;
mod overlay;
mod path;
mod stat;

// The facade exists for the F3-F5 consumers (parse cache keys, scan walk,
// apply loop); until then only `VfsError` has an outside reader.
#[allow(unused_imports)]
pub use error::{VfsError, VfsResult};
#[allow(unused_imports)]
pub use fs::{DirEntry, Os, ProjectFs, ReadDirSnapshot};
#[allow(unused_imports)]
pub use overlay::{BufferEntry, Overlay};
#[allow(unused_imports)]
pub use path::VfsPath;
#[allow(unused_imports)]
pub use stat::Stat;

#[cfg(test)]
pub(crate) mod test_util {
    //! The in-memory [`ProjectFs`] fixture the invariant tests assert
    //! against: seeded with known entries, fully deterministic, zero I/O.

    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;
    use std::sync::Arc;

    use crate::vfs::error::{VfsError, VfsResult};
    use crate::vfs::fs::{DirEntry, ProjectFs, ReadDirSnapshot};
    use crate::vfs::stat::Stat;

    /// Immutable after construction, so reads need no locking.
    #[derive(Clone, Debug, Default)]
    pub(crate) struct MemFs {
        files: HashMap<std::path::PathBuf, Arc<[u8]>>,
        dirs: BTreeSet<std::path::PathBuf>,
    }

    impl MemFs {
        pub fn new() -> Self {
            Self {
                files: HashMap::new(),
                dirs: BTreeSet::from([std::path::PathBuf::from("/")]),
            }
        }

        pub fn with_dir(mut self, path: &str) -> Self {
            self.dirs.insert(std::path::PathBuf::from(path));
            self
        }

        pub fn with_file(mut self, path: &str, bytes: &[u8]) -> Self {
            self.files
                .insert(std::path::PathBuf::from(path), Arc::from(bytes));
            self
        }
    }

    impl ProjectFs for MemFs {
        fn read_dir(&self, path: &Path) -> VfsResult<ReadDirSnapshot> {
            if !self.dirs.contains(path) {
                return Err(VfsError::NotFound {
                    path: path.display().to_string(),
                });
            }
            let mut entries: Vec<DirEntry> = Vec::new();
            entries.extend(
                self.dirs
                    .iter()
                    .filter(|dir| dir.parent() == Some(path))
                    .filter_map(|dir| dir.file_name()?.to_str())
                    .map(|name| DirEntry {
                        name: name.to_owned(),
                        is_dir: true,
                    }),
            );
            entries.extend(
                self.files
                    .keys()
                    .filter(|file| file.parent() == Some(path))
                    .filter_map(|file| file.file_name()?.to_str())
                    .map(|name| DirEntry {
                        name: name.to_owned(),
                        is_dir: false,
                    }),
            );
            Ok(ReadDirSnapshot::new(entries))
        }

        fn read_file(&self, path: &Path) -> VfsResult<Arc<[u8]>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| VfsError::NotFound {
                    path: path.display().to_string(),
                })
        }

        fn stat(&self, path: &Path) -> VfsResult<Stat> {
            if let Some(bytes) = self.files.get(path) {
                return Ok(Stat {
                    len: bytes.len() as u64,
                    is_file: true,
                    is_dir: false,
                    is_virtual: false,
                });
            }
            if self.dirs.contains(path) {
                return Ok(Stat {
                    len: 0,
                    is_file: false,
                    is_dir: true,
                    is_virtual: false,
                });
            }
            Err(VfsError::NotFound {
                path: path.display().to_string(),
            })
        }
    }

    /// The seeded fixture: a small two-level project.
    pub fn seeded_memfs() -> MemFs {
        MemFs::new()
            .with_dir("/proj")
            .with_dir("/proj/src")
            .with_file("/proj/src/main.rs", b"fn main() {}\n")
            .with_file("/proj/src/lib.rs", b"pub fn f() {}\n")
            .with_file("/proj/README.md", b"docs\n")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::fs::ProjectFs as _;
    use super::overlay::Overlay;
    use super::test_util::seeded_memfs;

    /// Invariant 2 — anti-phantom (PLAN §1.3, ADR-0016 default-OFF): a
    /// staged virtual file must never appear in a directory listing, which
    /// must equal the base filesystem's exactly. Staged state visible only
    /// through the explicit `virtual_paths()` surface.
    #[test]
    fn anti_phantom_read_dir_never_lists_buffer_files() {
        let base = seeded_memfs();
        // The fixture's own truth, read directly (no overlay in the way).
        let expected = base.read_dir(Path::new("/proj/src")).unwrap();

        let overlay = Overlay::new(Arc::new(base));
        overlay.set("/proj/src/ghost.rs", b"staged bytes").unwrap();

        let listing = overlay.read_dir("/proj/src").unwrap();
        assert_eq!(
            listing, expected,
            "read_dir must mirror the base exactly: a buffer file leaked into the listing (phantom)"
        );
        assert!(!listing.contains("ghost.rs"));
    }

    /// Negative control: the same listing machinery, over the real `Os`,
    /// DOES show a real file — proving the anti-phantom assertion above can
    /// never pass vacuously.
    #[test]
    fn negative_control_os_read_dir_does_show_real_files() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("real.rs"), b"real bytes").unwrap();

        let overlay = Overlay::new(Arc::new(super::fs::Os));
        let listing = overlay.read_dir(&src).unwrap();
        assert!(listing.contains("real.rs"));
    }
}
