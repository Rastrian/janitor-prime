//! Transactional virtual filesystem (PLAN §1.3) — F2 scope.
//!
//! The overlay design this module will implement:
//!
//! - `trait ProjectFs` (`read_dir` / `read_file -> Arc<[u8]>` / `stat`) with
//!   `Os` as the thin disk proxy;
//! - `Overlay` over a base `ProjectFs`: a `RwLock<HashMap<VfsPath,
//!   BufferEntry>>` where `BufferEntry` is `File(Arc<[u8]>)` or `Tombstone`
//!   (ADR-0009), plus per-path `Generation` counters feeding cache
//!   invalidation (Rule T4);
//! - the four mandated invariants (each with a named test, F2.5):
//!   read-through, anti-phantom (`read_dir` never lists buffer files),
//!   tombstone delete (shadows disk for `read_file`, invisible to
//!   `read_dir`), fallback;
//! - `virtual_paths()` as the explicit opt-in union for `scan`
//!   `--include-virtual` (default OFF, ADR-0016);
//! - `stage(ops) / commit() / rollback()` transactions with ordered
//!   tmp+rename flush and an `E_FLUSH_PARTIAL` report on partial OS failure.
//!
//! F0 declares the module so the §1.0 dependency direction
//! (`orchestrator -> vfs`, `ast -> vfs`) is compile-checked from day one.
// TODO(F2): ProjectFs, Os, Overlay, VfsPath canonicalization newtype,
// generation counters, txn (PLAN §1.3, §3 F2).
