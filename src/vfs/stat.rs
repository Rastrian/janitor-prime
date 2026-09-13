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
