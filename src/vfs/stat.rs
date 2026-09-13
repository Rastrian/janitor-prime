//! `Stat` — the shape `ProjectFs::stat` returns (PLAN §1.3, F2.7).
//!
//! F2.7 fills in the overlay's synthetic half: a buffer-first `stat` whose
/// `is_virtual: true` flag keeps downstream consumers from ever mistaking a
/// staged file for a persisted one — the stat-level twin of the anti-phantom
/// invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Stat {
    /// Content length in bytes (buffer length for virtual entries).
    pub len: u64,
    pub is_file: bool,
    pub is_dir: bool,
    /// True only for entries synthesized from the overlay buffer; an `Os`
    /// stat is always disk truth, so this is `false` there by construction.
    pub is_virtual: bool,
}

impl Stat {
    /// The synthetic stat for a staged buffer entry: staged length, file
    /// shape, `is_virtual: true` — never mistakable for a disk file.
    pub const fn virtual_file(len: u64) -> Self {
        Self {
            len,
            is_file: true,
            is_dir: false,
            is_virtual: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::Stat;
    use crate::vfs::error::VfsError;
    use crate::vfs::fs::{Os, ProjectFs as _};
    use crate::vfs::overlay::Overlay;
    use crate::vfs::test_util::seeded_memfs;

    /// Buffer-first: a staged entry stats synthetic — staged length, file
    /// shape, `is_virtual: true` — even when a different file exists on disk
    /// at the same path (the read-through invariant, stat edition).
    #[test]
    fn stat_is_synthetic_for_staged_entries() {
        let overlay = Overlay::new(Arc::new(seeded_memfs()));

        overlay
            .set("/proj/src/main.rs", b"staged and longer than disk")
            .unwrap();
        assert_eq!(
            overlay.stat("/proj/src/main.rs").unwrap(),
            Stat::virtual_file(b"staged and longer than disk".len() as u64)
        );

        overlay.set("/proj/src/new.rs", b"12345").unwrap();
        assert_eq!(
            overlay.stat("/proj/src/new.rs").unwrap(),
            Stat::virtual_file(5)
        );

        // A tombstone stats as deleted, mirroring read_file.
        overlay.delete("/proj/src/new.rs").unwrap();
        assert!(matches!(
            overlay.stat("/proj/src/new.rs"),
            Err(VfsError::BufferDeleted { .. })
        ));
    }

    /// Disk fallback: without a buffer entry, stat is the base's — `Os`
    /// truth with `is_virtual: false` — byte-for-byte the same fields the
    /// base reports directly.
    #[test]
    fn stat_falls_back_to_disk_truth() {
        let base = seeded_memfs();
        let overlay = Overlay::new(Arc::new(base.clone()));

        assert_eq!(
            overlay.stat("/proj/src/main.rs").unwrap(),
            base.stat(std::path::Path::new("/proj/src/main.rs"))
                .unwrap()
        );
        assert!(overlay.stat("/proj").unwrap().is_dir);
        assert!(!overlay.stat("/proj").unwrap().is_virtual);
        assert!(matches!(
            overlay.stat("/proj/absent.rs"),
            Err(VfsError::NotFound { .. })
        ));

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("real.rs"), b"0123456789").unwrap();
        let os_overlay = Overlay::new(Arc::new(Os));
        let real = tmp.path().join("real.rs");
        assert_eq!(os_overlay.stat(&real).unwrap(), Os.stat(&real).unwrap());
        assert_eq!(os_overlay.stat(&real).unwrap().len, 10);
    }
}
