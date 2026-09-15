//! Persistent memory (PLAN §1.5) — the F6 module set.
//!
//! The model is OptMem-inspired, re-engineered from scratch (ideas and
//! formats only — OptMem ships no license, so zero code and zero verbatim
//! format reuse; ADR-0013/R21 — our raw log is JSONL where theirs is
//! fixed-320-byte records, precisely because our facts carry structured
//! `SyntaxPointer`s that do not fit fixed width, ADR-0006):
//!
//! - [`log`] — `.janitor/LOG.txt`: append-only JSONL facts, fsync on
//!   append, position-derived ids (note *i* is line *i*), torn-tail
//!   quarantine to `.janitor/QUARANTINE.txt` with a warning on load;
//! - [`pointer`] — [`SyntaxPointer`] (ADR-0012): `Node { path, span,
//!   kind_name, content_sha }` re-anchored by content hash, or `Commit`
//!   validated through a `git rev-parse` subprocess (ADR-0013);
//! - [`tree_store`] — `TREE/<size>` fixed-width (256 B) summary records;
//!   dense prefix per level is the build frontier; a pure cache
//!   rebuildable from LOG.txt alone;
//! - [`wake`] — token-budgeted recall (`ceil(bytes/4)`, ADR-0011) with
//!   OptMem-style power-of-two block tiling, present-bias, lock
//!   force-split, and the completeness gate (`E_NAP_PENDING`);
//! - [`nap`] — the synchronous compression protocol (ADR-0013): the tool
//!   sequences and stores, the consuming agent writes every summary;
//!   nothing ever runs in the background and no LLM lives in the binary;
//! - [`environment`] — the `wake` payload's environment notes (v0.3):
//!   PATH probes for the language servers and git around the repo —
//!   guidance with evidence (ADR-0017's delivery), never a runtime.
//!
//! Storage I/O is plain `std::fs` (append + fsync for the log; tmp+rename
//! via [`crate::vfs::Os::write_atomic`] where a whole file is rewritten —
//! the ADR-0016 (8) discipline) with failures mapped onto the shared
//! [`crate::vfs::VfsError`] taxonomy. The local layer is **always**
//! `$PWD/.janitor/` — never `$HOME` (§10 data hygiene); the global layer
//! reads only the embedded `share/GLOBAL.md` defaults and its
//! `$XDG_CONFIG_HOME/janitor/GLOBAL.md` override, nothing else.

pub mod context;
pub mod environment;
pub mod log;
pub mod nap;
pub mod pointer;
pub mod tree_store;
pub mod wake;

use std::time::{SystemTime, UNIX_EPOCH};

/// The local memory directory name (always under the process working
/// directory, PLAN §1.5.2).
pub const LOCAL_DIR: &str = ".janitor";

/// Format a `SystemTime` as an RFC 3339 UTC timestamp (`…Z`, second
/// precision) — the `ts` field of every log record. No chrono: the civil
/// calendar falls out of the days-since-epoch arithmetic below.
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_seconds(secs)
}

/// [`timestamp`] over a fixed instant (tests; deterministic records).
pub fn format_unix_seconds(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60
    )
}

/// Year/month/day from days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`): proleptic Gregorian, valid for any date the log
/// will ever see.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe as i64 + era * 400 + u64::from(month <= 2) as i64;
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known instants, cross-checked against `date -u -d @<epoch>` — the
    /// civil-calendar algorithm must reproduce them exactly (leap years,
    /// century rules, month lengths all ride through the day count).
    #[test]
    fn unix_seconds_format_known_instants() {
        assert_eq!(format_unix_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_seconds(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_unix_seconds(1_078_056_000), "2004-02-29T12:00:00Z");
        // 2100-03-01 (2100 is NOT a leap year — the century rule).
        assert_eq!(format_unix_seconds(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(format_unix_seconds(1_757_851_254), "2025-09-14T12:00:54Z");
        assert_eq!(format_unix_seconds(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(format_unix_seconds(86_400), "1970-01-02T00:00:00Z");
    }

    /// `timestamp()` is now-shaped: current schema prefix, Z suffix.
    #[test]
    fn timestamp_is_rfc3339_utc_shaped() {
        let ts = timestamp();
        assert!(ts.starts_with("20"), "ts = {ts}");
        assert!(ts.ends_with('Z') && ts.len() == 20, "ts = {ts}");
    }
}
