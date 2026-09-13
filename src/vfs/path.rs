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
