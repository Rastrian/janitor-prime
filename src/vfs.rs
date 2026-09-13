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
//! - `txn` — [`Transaction`]: `stage(ops) / commit() / rollback()` with
//!   ordered tmp+rename flush and an `E_FLUSH_PARTIAL` report on partial OS
//!   failure (F2.6).
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
mod txn;

// The facade exists for the F3-F5 consumers (parse cache keys, scan walk,
// apply loop); until then only `VfsError` has an outside reader.
#[allow(unused_imports)]
pub use error::{VfsError, VfsResult};
#[allow(unused_imports)]
pub use fs::{DirEntry, Os, ProjectFs, ReadDirSnapshot};
#[allow(unused_imports)]
pub use overlay::{BufferEntry, Generation, Overlay};
#[allow(unused_imports)]
pub use path::VfsPath;
#[allow(unused_imports)]
pub use stat::Stat;
#[allow(unused_imports)]
pub use txn::{CommitReport, FlushFailure, FlushReport, Transaction, TxnOp};

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

    /// Invariant 1 — read-through: staged bytes win over disk, including
    /// when different bytes exist at the same path, and when no disk file
    /// exists at all (a purely virtual file still reads back exactly).
    #[test]
    fn read_through_set_then_read_file_returns_the_staged_bytes() {
        let overlay = Overlay::new(Arc::new(seeded_memfs()));

        // Same path as a real disk entry, different content.
        overlay
            .set("/proj/src/main.rs", b"NEW staged content")
            .unwrap();
        assert_eq!(
            overlay.read_file("/proj/src/main.rs").unwrap().as_ref(),
            b"NEW staged content"
        );

        // Path that does not exist on the base at all.
        overlay
            .set("/proj/src/virtual.rs", b"purely virtual")
            .unwrap();
        assert_eq!(
            overlay.read_file("/proj/src/virtual.rs").unwrap().as_ref(),
            b"purely virtual"
        );
    }

    /// Invariant 4 — fallback: with no buffer entry, reads are byte-identical
    /// to the base filesystem, for the in-memory fixture and for the real
    /// `Os` proxy over a tempdir.
    #[test]
    fn fallback_without_set_read_file_reads_the_base_byte_identically() {
        let base = seeded_memfs();
        let overlay = Overlay::new(Arc::new(base.clone()));
        for path in ["/proj/src/main.rs", "/proj/src/lib.rs", "/proj/README.md"] {
            assert_eq!(
                overlay.read_file(path).unwrap(),
                base.read_file(Path::new(path)).unwrap(),
                "fallback must be byte-identical to the base for {path}"
            );
        }

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("on_disk.rs"), b"disk truth").unwrap();
        let os_overlay = Overlay::new(Arc::new(super::fs::Os));
        let disk_path = tmp.path().join("on_disk.rs");
        assert_eq!(
            os_overlay.read_file(&disk_path).unwrap(),
            super::fs::Os.read_file(&disk_path).unwrap()
        );
    }

    /// Invariant 3 — tombstone delete (ADR-0009): after set + delete,
    /// `read_file` is the typed `BufferDeleted` error (the delete wins over
    /// the disk fallback) while `read_dir` is unchanged — the on-disk file
    /// still lists, because removing the entry would resurrect the disk
    /// copy.
    #[test]
    fn delete_tombstone_shadows_disk_for_reads_but_not_for_listings() {
        let overlay = Overlay::new(Arc::new(seeded_memfs()));
        let before = overlay.read_dir("/proj/src").unwrap();

        overlay
            .set("/proj/src/main.rs", b"edited then deleted")
            .unwrap();
        overlay.delete("/proj/src/main.rs").unwrap();

        let outcome = overlay.read_file("/proj/src/main.rs");
        assert!(
            matches!(outcome, Err(super::error::VfsError::BufferDeleted { .. })),
            "expected the typed BufferDeleted error after set + delete"
        );
        let after = overlay.read_dir("/proj/src").unwrap();
        assert_eq!(before, after, "read_dir must be unchanged by a tombstone");
        assert!(after.contains("main.rs"), "the disk copy still lists");
    }

    /// Generations (Rule T4): every mutation returns a strictly increasing
    /// per-path generation, `current_gen` agrees with the last returned
    /// value, and the global counter tracks total mutations.
    #[test]
    fn generations_bump_strictly_per_path_and_feed_the_global_counter() {
        let overlay = Overlay::new(Arc::new(seeded_memfs()));

        let g0 = overlay.current_gen("/proj/src/main.rs").unwrap();
        assert_eq!(g0.value(), 0, "never-mutated path has generation 0");

        let g1 = overlay.set("/proj/src/main.rs", b"one").unwrap();
        let g2 = overlay.set("/proj/src/main.rs", b"two").unwrap();
        let g3 = overlay.delete("/proj/src/main.rs").unwrap();
        assert!(g0 < g1 && g1 < g2 && g2 < g3);

        // A different path draws from the same global counter: strictly
        // greater than everything issued before, and the global count
        // matches the number of mutations issued.
        let g4 = overlay.set("/proj/src/lib.rs", b"other path").unwrap();
        assert!(g3 < g4);
        assert_eq!(overlay.global_gen(), 4);
        assert_eq!(overlay.current_gen("/proj/src/main.rs").unwrap(), g3);
        assert_eq!(overlay.current_gen("/proj/src/lib.rs").unwrap(), g4);
    }

    /// E6 (PLAN §3 F4 item 10): `set_string` validates UTF-8 and stages
    /// nothing on failure; `get_string` decodes or fails typed; `get`/`get_string`
    /// are pure buffer lookups (no disk fallback).
    #[test]
    fn string_access_validates_utf8_and_stays_a_pure_buffer_lookup() {
        let overlay = Overlay::new(Arc::new(seeded_memfs()));

        overlay
            .set_string("/proj/src/text.rs", "fn a() {}\n")
            .unwrap();
        assert_eq!(
            overlay.get_string("/proj/src/text.rs").unwrap().as_deref(),
            Some("fn a() {}\n")
        );

        let err = overlay
            .set_string("/proj/src/bad.rs", [0xff, 0xfe, 0x00])
            .unwrap_err();
        assert!(matches!(err, super::error::VfsError::InvalidUtf8 { .. }));
        assert!(
            overlay.get("/proj/src/bad.rs").unwrap().is_none(),
            "rejected bytes must not be staged"
        );

        // Raw bytes staged via `set` fail `get_string` typed, not lossily.
        overlay
            .set("/proj/src/raw.bin", [0x00, 0xff, 0xfe])
            .unwrap();
        assert!(matches!(
            overlay.get_string("/proj/src/raw.bin"),
            Err(super::error::VfsError::InvalidUtf8 { .. })
        ));

        // Pure buffer semantics: an unstaged disk file is Ok(None), and a
        // tombstone is the typed BufferDeleted.
        assert!(overlay.get("/proj/README.md").unwrap().is_none());
        overlay.set("/proj/README.md", b"x").unwrap();
        overlay.delete("/proj/README.md").unwrap();
        assert!(matches!(
            overlay.get("/proj/README.md"),
            Err(super::error::VfsError::BufferDeleted { .. })
        ));
    }

    /// `delete` is only defined for tracked paths: deleting an untracked
    /// path (or double-deleting) is the typed `NotStaged` error and leaves
    /// the buffer untouched.
    #[test]
    fn delete_on_untracked_paths_is_the_typed_not_staged_error() {
        let overlay = Overlay::new(Arc::new(seeded_memfs()));

        assert!(matches!(
            overlay.delete("/proj/src/never_staged.rs"),
            Err(super::error::VfsError::NotStaged { .. })
        ));
        // A real disk file is still "untracked": buffer deletes act on the
        // overlay's staged state only.
        assert!(matches!(
            overlay.delete("/proj/src/main.rs"),
            Err(super::error::VfsError::NotStaged { .. })
        ));

        overlay.set("/proj/src/main.rs", b"staged").unwrap();
        overlay.delete("/proj/src/main.rs").unwrap();
        assert!(matches!(
            overlay.delete("/proj/src/main.rs"),
            Err(super::error::VfsError::NotStaged { .. })
        ));
    }

    /// `virtual_paths` — the explicit visibility surface: sorted, staged
    /// files only; tombstones add nothing; disk files never appear.
    #[test]
    fn virtual_paths_lists_staged_files_sorted_and_excludes_tombstones() {
        let overlay = Overlay::new(Arc::new(seeded_memfs()));

        assert!(overlay.virtual_paths().is_empty());

        overlay.set("/proj/src/zebra.rs", b"z").unwrap();
        overlay.set("/proj/src/alpha.rs", b"a").unwrap();
        overlay.set("/proj/src/doomed.rs", b"d").unwrap();
        overlay.delete("/proj/src/doomed.rs").unwrap();

        let paths: Vec<String> = overlay
            .virtual_paths()
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert_eq!(paths, vec!["/proj/src/alpha.rs", "/proj/src/zebra.rs"]);
    }
}
