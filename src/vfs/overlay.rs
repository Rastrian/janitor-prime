//! STUB — the F2 TDD *red* step (deliberately wrong, to be deleted).
//!
//! This `read_dir` splices buffer entries into the base listing. That is
//! exactly the phantom the anti-phantom invariant forbids (PLAN §1.3
//! invariant 2), which is why the test in `src/vfs.rs` fails against this
//! file. The *green* commit replaces the splice with a pure base passthrough
//! and fills in the real buffer semantics (read-through, tombstones,
//! generations).
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::vfs::error::VfsResult;
use crate::vfs::fs::{DirEntry, ProjectFs, ReadDirSnapshot};
use crate::vfs::path::VfsPath;

/// A staged buffer entry (ADR-0009).
#[derive(Clone, Debug)]
pub enum BufferEntry {
    /// Staged/edited content; `Arc` so snapshots are O(1).
    File(Arc<[u8]>),
    /// A staged delete, shadowing the base file.
    Tombstone,
}

pub struct Overlay {
    pub(crate) base_fs: Arc<dyn ProjectFs>,
    pub(crate) buffer: RwLock<HashMap<VfsPath, BufferEntry>>,
}

impl Overlay {
    pub fn new(base_fs: Arc<dyn ProjectFs>) -> Self {
        Self {
            base_fs,
            buffer: RwLock::new(HashMap::new()),
        }
    }

    /// Stage `bytes` at `path` (path canonicalized at this boundary).
    pub fn set(&self, path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> VfsResult<()> {
        let key = VfsPath::new(path)?;
        let mut buffer = self.buffer.write().map_err(|_| poisoned())?;
        buffer.insert(key, BufferEntry::File(Arc::from(bytes.as_ref())));
        Ok(())
    }

    /// **THE BUG UNDER TEST** — unions buffer files into the listing.
    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfsResult<ReadDirSnapshot> {
        let dir = VfsPath::new(path)?;
        let mut entries: Vec<DirEntry> = self.base_fs.read_dir(dir.as_path())?.entries().to_vec();
        let buffer = self.buffer.read().map_err(|_| poisoned())?;
        let phantoms = buffer
            .iter()
            .filter(|(_, entry)| matches!(entry, BufferEntry::File(_)))
            .filter(|(staged, _)| staged.parent().as_ref() == Some(&dir))
            .filter_map(|(staged, _)| {
                staged
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| DirEntry {
                        name: name.to_owned(),
                        is_dir: false,
                    })
            });
        entries.extend(phantoms);
        Ok(ReadDirSnapshot::new(entries))
    }

    /// Placeholder: base passthrough (the buffer-first semantics arrive with
    /// the green commit; nothing in the red step depends on them).
    pub fn read_file(&self, path: impl AsRef<Path>) -> VfsResult<Arc<[u8]>> {
        let key = VfsPath::new(path)?;
        self.base_fs.read_file(key.as_path())
    }

    /// Placeholder: base passthrough.
    pub fn stat(&self, path: impl AsRef<Path>) -> VfsResult<crate::vfs::stat::Stat> {
        let key = VfsPath::new(path)?;
        self.base_fs.stat(key.as_path())
    }
}

fn poisoned() -> crate::vfs::error::VfsError {
    crate::vfs::error::VfsError::Io {
        path: "<overlay>".to_owned(),
        message: "overlay lock poisoned".to_owned(),
    }
}
