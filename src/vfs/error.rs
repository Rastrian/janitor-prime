//! VFS domain errors (PLAN §1.7, F2.1). Typed, thiserror-only; every variant
//! maps onto the frozen `E_*` code table in `crate::error::JanitorError`.
//!
//! F2 adds the three variants the scaffold could not name yet: the ADR-0009
//! tombstone read (`BufferDeleted`), the E6 UTF-8 policy error
//! (`InvalidUtf8`, PLAN §3 F4 item 10), and the delete-precondition error
//! (`NotStaged`). The transactional flush report variant (`FlushPartial`)
//! lands with `txn.rs` (F2.6).
use thiserror::Error;

/// Result alias used across the VFS surface.
pub type VfsResult<T> = Result<T, VfsError>;

#[derive(Debug, Error)]
pub enum VfsError {
    #[error("path not found: {path}")]
    NotFound { path: String },
    #[error("permission denied: {path}")]
    Permission { path: String },
    #[error("io error on {path}: {message}")]
    Io { path: String, message: String },
    #[error("invalid path: {path}")]
    InvalidPath { path: String },
    /// Reading a path whose buffer entry is a tombstone (ADR-0009): the
    /// staged delete must win over the disk fallback, and it must be
    /// distinguishable from "never existed" — hence a variant of its own
    /// instead of the `NotFound` PLAN §1.3 sketched (F2 outcome note).
    #[error("path deleted in the overlay buffer (tombstone): {path}")]
    BufferDeleted { path: String },
    /// E6 UTF-8 policy (PLAN §3 F4 item 10): text-typed entry points
    /// (`set_string`, `get_string`) validate and fail typed; they never
    /// coerce, replace, or silently skip invalid bytes.
    #[error("content of {path} is not valid UTF-8")]
    InvalidUtf8 { path: String },
    /// Buffer mutations have a precondition: `delete` only acts on paths the
    /// overlay tracks (a staged `File` entry). Deleting an untracked path is
    /// a caller bug, not a missing file.
    #[error("path not staged in the overlay buffer: {path}")]
    NotStaged { path: String },
}
