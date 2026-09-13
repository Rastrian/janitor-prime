//! `VfsPath` — the lexical canonicalization newtype (PLAN §1.3, F2.2).
//!
//! Every path entering the overlay is normalized exactly once, at the
//! boundary, so `/a/b`, `/a//b/`, `/a/./b` and `/a/c/../b` hit the same
//! buffer slot and the same generation counter. Normalization is purely
//! **lexical**: separators collapse, `.` drops out, `..` pops the previous
//! component. Symlinks are deliberately *not* resolved — the overlay's
//! contract is view-consistency for spelled paths, not disk identity, and
//! `readlink` walks would reintroduce the I/O (and TOCTOU) the buffer exists
//! to avoid. A `..` that would escape above the root (or above an empty
//! relative base, e.g. `a/../../b`) is non-normalizable and rejected with
//! [`VfsError::InvalidPath`] — the property suite in this file pins all of
//! this down.
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::vfs::error::VfsError;

/// A path already in canonical form. Construct via [`VfsPath::new`], which
/// normalizes or rejects; the wrapped [`PathBuf`] is never re-normalized.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct VfsPath(PathBuf);

impl VfsPath {
    /// Normalize `path` into a `VfsPath`, or reject it as non-normalizable.
    ///
    /// Rejected inputs: the empty path, paths containing a NUL byte, and
    /// paths whose lexical `..` resolution would escape above the root.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, VfsError> {
        let raw = path.as_ref();
        let shown = raw.display().to_string();
        if raw.as_os_str().is_empty() {
            return Err(VfsError::InvalidPath { path: shown });
        }
        if raw.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(VfsError::InvalidPath { path: shown });
        }

        let mut normal = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                    normal.push(component.as_os_str());
                }
                // `std` already elides interior `.`; the one a leading `./`
                // yields is dropped the same way.
                Component::CurDir => {}
                Component::ParentDir => {
                    // `pop` fails on an empty stack or a bare root: the `..`
                    // escapes what the path can name.
                    if !normal.pop() {
                        return Err(VfsError::InvalidPath { path: shown });
                    }
                }
            }
        }
        // "." (and "./", "./." …) normalize to the empty stack; keep a
        // spelled identity so the path round-trips through std::fs.
        if normal.as_os_str().is_empty() {
            normal.push(".");
        }
        Ok(Self(normal))
    }

    /// The canonical path as a plain [`Path`].
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume into the canonical [`PathBuf`].
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    /// The final component, mirroring [`Path::file_name`] (`None` for `..`,
    /// `/` and `.` results).
    pub fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }

    /// The canonical parent directory (`None` for `/` and for single-component
    /// relative paths, whose parent would be the unnamed empty path).
    pub fn parent(&self) -> Option<VfsPath> {
        self.0.parent().and_then(|p| VfsPath::new(p).ok())
    }

    /// True when the path is rooted (`/…`).
    pub fn is_absolute(&self) -> bool {
        self.0.is_absolute()
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl AsRef<Path> for VfsPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use proptest::prelude::*;

    use super::VfsPath;
    use crate::vfs::error::VfsError;

    /// A path component that can never look like `.`, `..` or a separator,
    /// so generators build their intent unambiguously.
    fn segment() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{1,6}"
    }

    /// The clean-path generator: optional absolute root + 0..=5 segments
    /// (relative paths need at least one, "" is rejected by contract).
    fn clean_path() -> impl Strategy<Value = String> {
        (any::<bool>(), proptest::collection::vec(segment(), 0..=5)).prop_map(|(absolute, segs)| {
            if absolute {
                format!("/{}", segs.join("/"))
            } else if segs.is_empty() {
                ".".to_owned()
            } else {
                segs.join("/")
            }
        })
    }

    /// A dirty spelling of `segs`: `./` segments, doubled separators and
    /// balanced `junk/..` pairs interspersed before each real segment. Every
    /// insertion is depth-neutral, so the dirty path names the same file.
    fn dirty_spelling(absolute: bool, segs: &[String]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (i, seg) in segs.iter().enumerate() {
            if i % 3 == 0 {
                parts.push(format!("junk{i}"));
                parts.push("..".to_owned());
            }
            if i % 2 == 0 {
                parts.push(".".to_owned());
            }
            parts.push(seg.clone());
        }
        if absolute {
            format!("/{}", parts.join("/"))
        } else {
            parts.join("/")
        }
    }

    /// Clean paths round-trip unchanged and normalization is idempotent —
    /// re-normalizing a `VfsPath`'s own spelling is a no-op.
    #[test]
    fn normalization_is_idempotent_and_clean_paths_round_trip() {
        proptest!(|(path in clean_path())| {
            let once = VfsPath::new(&path).unwrap();
            let twice = VfsPath::new(once.as_path()).unwrap();
            prop_assert_eq!(once, twice);
        });
    }

    /// `./`, `//` and depth-neutral `j/..` pairs collapse to the clean path,
    /// and equivalent spellings are the same hash-map key — the reason the
    /// newtype exists (one buffer slot and one generation per file).
    #[test]
    fn dirty_spellings_collapse_to_the_clean_path_and_share_one_slot() {
        proptest!(|(absolute in any::<bool>(), segs in proptest::collection::vec(segment(), 1..=5))| {
            let clean = if absolute {
                format!("/{}", segs.join("/"))
            } else {
                segs.join("/")
            };
            let dirty = dirty_spelling(absolute, &segs);

            let clean_key = VfsPath::new(&clean).unwrap();
            let dirty_key = VfsPath::new(&dirty).unwrap();
            prop_assert_eq!(&clean_key, &dirty_key);

            let mut slots = HashMap::new();
            slots.insert(dirty_key, 7u8);
            prop_assert_eq!(slots.get(&clean_key), Some(&7u8));
        });
    }

    /// A `..` that would climb past the root (absolute) or past the empty
    /// base (relative) is non-normalizable: typed `InvalidPath`, never a
    /// silent clamp.
    #[test]
    fn escaping_dot_dot_is_rejected() {
        proptest!(|(absolute in any::<bool>(), segs in proptest::collection::vec(segment(), 0..=5))| {
            let depth = segs.len();
            let mut parts = segs;
            for _ in 0..depth + 1 {
                parts.push("..".to_owned());
            }
            let path = if absolute {
                format!("/{}", parts.join("/"))
            } else {
                parts.join("/")
            };
            let rejected = matches!(VfsPath::new(&path), Err(VfsError::InvalidPath { .. }));
            prop_assert!(rejected, "escaping path must be rejected: {path}");
        });
    }

    /// Deterministic edge cases the generators cannot hit.
    #[test]
    fn canonical_edges() {
        // The identity paths.
        assert_eq!(VfsPath::new("/").unwrap().as_path(), Path::new("/"));
        assert_eq!(VfsPath::new(".").unwrap().as_path(), Path::new("."));
        assert_eq!(VfsPath::new("./").unwrap().as_path(), Path::new("."));
        assert_eq!(VfsPath::new("/a/b/").unwrap().as_path(), Path::new("/a/b"));

        // Equivalence by hand, including a `..` walk-back: the `..` cancels
        // `c`, and the trailing `b` survives (lexical, not suffix-dedup).
        assert_eq!(
            VfsPath::new("/a//b/./c/../b").unwrap(),
            VfsPath::new("/a/b/b").unwrap()
        );
        assert_eq!(
            VfsPath::new("/a/c/../b").unwrap(),
            VfsPath::new("/a/b").unwrap()
        );
        assert_eq!(VfsPath::new("a/../b").unwrap(), VfsPath::new("b").unwrap());

        // `/a/..` resolves to the root, not an escape.
        assert_eq!(VfsPath::new("/a/..").unwrap().as_path(), Path::new("/"));

        // Rejections: empty, root escape, relative escape.
        assert!(matches!(
            VfsPath::new(""),
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            VfsPath::new("/.."),
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            VfsPath::new("a/../../b"),
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            VfsPath::new(".."),
            Err(VfsError::InvalidPath { .. })
        ));

        // parent()/file_name() on canonical results.
        let p = VfsPath::new("/proj/src/main.rs").unwrap();
        assert_eq!(p.parent().unwrap().to_string(), "/proj/src");
        assert_eq!(p.file_name().unwrap(), "main.rs");
        assert_eq!(VfsPath::new("/").unwrap().parent(), None);
        assert_eq!(VfsPath::new("a").unwrap().parent(), None);
        assert!(VfsPath::new(".").unwrap().file_name().is_none());
    }

    /// NUL bytes cannot name a file on the platforms we target; they are
    /// rejected at the boundary instead of surfacing as an io error later.
    #[cfg(unix)]
    #[test]
    fn nul_bytes_are_rejected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = OsStr::from_bytes(b"evil\0name");
        assert!(matches!(
            VfsPath::new(path),
            Err(VfsError::InvalidPath { .. })
        ));
    }
}
