//! The daemon's wire protocol (F10a, ADR-0018 D1): the SAME `janitor.v1`
//! envelope, newline-delimited — one JSON request line in, one JSON
//! envelope line out, one request per connection. No streaming in this
//! increment.
//!
//! Request shape (all command-specific fields optional-with-defaults;
//! unknown fields are refused — an agent-facing protocol should fail
//! loudly on typos, not silently ignore them):
//!
//! ```json
//! {"schema":"janitor.v1","cmd":"ping"}
//! {"schema":"janitor.v1","cmd":"scan","rule":"(kind function_item)","path":"src"}
//! {"schema":"janitor.v1","cmd":"duplicates","path":"src","min_size":8}
//! {"schema":"janitor.v1","cmd":"stats"}
//! {"schema":"janitor.v1","cmd":"shutdown"}
//! ```
//!
//! Response: the standard envelope (see `crate::envelope`), one line.

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ast::cache::CacheStats;
use crate::envelope::SCHEMA;
use crate::error::{DaemonError, JanitorError, OutputError};

/// The request-line cap (ADR-0018 D6): a rule plus flags fits comfortably;
/// anything bigger is refused with the typed error, never buffered.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// The request envelope's `cmd` set.
pub const CMDS: &[&str] = &["ping", "scan", "duplicates", "stats", "shutdown"];

/// One protocol request. Constructed by the client ([`Request::ping`], …)
/// or deserialized from the wire.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema: String,
    pub cmd: String,
    /// `scan` mode: the DSL rule (required for `scan`).
    #[serde(default)]
    pub rule: Option<String>,
    /// Scan root, workspace-relative or absolute (must fall inside the
    /// daemon's workspace, ADR-0018 D6; default: the workspace itself).
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict to one canonical language name.
    #[serde(default)]
    pub lang: Option<String>,
    /// Union overlay-staged virtual files into the walk (default OFF).
    #[serde(default)]
    pub include_virtual: bool,
    /// Byte-strict adjacency (default: ADR-0010's comment-skipping).
    #[serde(default)]
    pub strict_trivia: bool,
    /// Skip files above this many bytes (default 4 MiB).
    #[serde(default)]
    pub max_file_size: Option<u64>,
    /// `duplicates` mode: minimum participating-node size (default 8).
    #[serde(default)]
    pub min_size: Option<u32>,
}

impl Request {
    /// A `ping` request (liveness + cache stats).
    pub fn ping() -> Self {
        Self::bare("ping")
    }

    /// A `stats` request (the same status payload, `stats`-labeled).
    pub fn stats() -> Self {
        Self::bare("stats")
    }

    /// A `shutdown` request: the daemon answers, then exits.
    pub fn shutdown() -> Self {
        Self::bare("shutdown")
    }

    /// A `scan` request for `rule` at `path` (relative to the daemon's
    /// workspace, or absolute-inside-it).
    pub fn scan(rule: impl Into<String>, path: Option<String>) -> Self {
        Self {
            rule: Some(rule.into()),
            path,
            ..Self::bare("scan")
        }
    }

    /// A `duplicates` request at `path` (no rule — the mode groups
    /// capturable subtrees by fingerprint).
    pub fn duplicates(path: Option<String>) -> Self {
        Self {
            path,
            ..Self::bare("duplicates")
        }
    }

    fn bare(cmd: &str) -> Self {
        Self {
            schema: SCHEMA.to_owned(),
            cmd: cmd.to_owned(),
            rule: None,
            path: None,
            lang: None,
            include_virtual: false,
            strict_trivia: false,
            max_file_size: None,
            min_size: None,
        }
    }

    /// Semantic validation beyond serde's: the schema string is ours, the
    /// command is known, and mode-required fields are present.
    pub fn validate(&self) -> Result<(), DaemonError> {
        if self.schema != SCHEMA {
            return Err(DaemonError::RequestMalformed {
                detail: format!("schema {:?} is not {SCHEMA:?}", self.schema),
            });
        }
        if !CMDS.contains(&self.cmd.as_str()) {
            return Err(DaemonError::RequestMalformed {
                detail: format!("unknown cmd {:?}; expected one of {CMDS:?}", self.cmd),
            });
        }
        if self.cmd == "scan" && self.rule.as_deref().is_none_or(str::is_empty) {
            return Err(DaemonError::RequestMalformed {
                detail: "scan requires a non-empty rule".to_owned(),
            });
        }
        if self.cmd == "duplicates" && self.rule.is_some() {
            return Err(DaemonError::RequestMalformed {
                detail: "duplicates takes no rule".to_owned(),
            });
        }
        Ok(())
    }

    /// Serialize as the request line (no trailing newline).
    pub fn to_line(&self) -> Result<String, JanitorError> {
        serde_json::to_string(self)
            .map_err(|err| JanitorError::Output(OutputError::Serialization(err.to_string())))
    }
}

/// The `ping`/`stats` payload: liveness plus the numbers a client needs to
/// decide whether to spawn a daemon (ADR-0018 D7).
#[derive(Clone, Debug, Serialize)]
pub struct DaemonStatus {
    pub cmd: &'static str,
    pub version: &'static str,
    pub pid: u32,
    pub uptime_secs: u64,
    /// The canonical workspace this daemon serves (requests outside it are
    /// refused; clients serving another tree must not route here).
    pub workspace: String,
    pub sock: String,
    pub cache: CacheStats,
    pub watch: WatchStatus,
}

/// The watcher's counters (polling, ADR-0018 D2).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct WatchStatus {
    pub interval_secs: u64,
    /// Poll passes completed.
    pub passes: u64,
    /// Incremental re-parses performed (changed warm files).
    pub reparses: u64,
    /// Files currently warm in the cache.
    pub warm: usize,
}

/// The default socket path (ADR-0018 D1): `$JANITOR_SOCK`, else
/// `/tmp/janitor-$UID.sock`. The uid is resolved without a libc
/// dependency: the owner of `/proc/self` on Linux, `$UID` as fallback,
/// plain `janitor.sock` as the last resort — resolution order is part of
/// the client/server contract (both sides call this).
pub fn default_sock_path() -> PathBuf {
    sock_path_from(std::env::var("JANITOR_SOCK").ok().as_deref())
}

/// The pure resolution behind [`default_sock_path`] (the test seam).
pub fn sock_path_from(env_sock: Option<&str>) -> PathBuf {
    if let Some(path) = env_sock.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    let uid = current_uid().or_else(|| {
        std::env::var("UID")
            .ok()
            .and_then(|u| u.parse::<u32>().ok())
    });
    match uid {
        Some(uid) => std::env::temp_dir().join(format!("janitor-{uid}.sock")),
        None => std::env::temp_dir().join("janitor.sock"),
    }
}

/// The process uid via `/proc/self`'s owner (std-only; Linux).
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

/// Read one newline-terminated line, bounded by `max` bytes. Tolerant of
/// EOF-terminated final lines (no trailing newline); the newline itself is
/// stripped. Raw I/O errors are the caller's to type.
pub fn read_line(stream: &mut impl Read, max: usize) -> Result<Vec<u8>, std::io::Error> {
    let mut line = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        if line.len() > max {
            return Ok(line);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(line);
        }
        if let Some(pos) = chunk[..read].iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&chunk[..pos]);
            return Ok(line);
        }
        line.extend_from_slice(&chunk[..read]);
        if line.len() > max {
            return Ok(line);
        }
    }
}

/// Write one line (JSON value + newline) and flush.
pub fn write_line(
    stream: &mut impl Write,
    value: &serde_json::Value,
) -> Result<(), std::io::Error> {
    let mut text =
        serde_json::to_string(value).map_err(|err| std::io::Error::other(err.to_string()))?;
    text.push('\n');
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request surface: every command round-trips through a line;
    /// unknown fields, wrong schema, unknown cmd, and mode violations are
    /// all typed `E_DAEMON_REQUEST_MALFORMED` (loud, never silent).
    #[test]
    fn requests_round_trip_and_validate() {
        for request in [
            Request::ping(),
            Request::stats(),
            Request::shutdown(),
            Request::scan("(kind function_item)", Some("src".to_owned())),
        ] {
            request.validate().expect("well-formed request");
            let line = request.to_line().expect("serializes");
            let parsed: Request = serde_json::from_str(&line).expect("round-trips");
            assert_eq!(parsed.cmd, request.cmd);
            assert_eq!(parsed.schema, SCHEMA);
        }

        let duplicates = Request {
            min_size: Some(12),
            ..Request::bare("duplicates")
        };
        duplicates.validate().expect("duplicates with tunables");

        // Wrong schema.
        let mut bad = Request::ping();
        bad.schema = "janitor.v0".to_owned();
        assert!(bad.validate().is_err());

        // Unknown command.
        let mut unknown = Request::ping();
        unknown.cmd = "apply".to_owned();
        assert!(unknown.validate().is_err());

        // scan without a rule.
        assert!(Request::bare("scan").validate().is_err());

        // duplicates with a rule.
        let mut ruled = Request::bare("duplicates");
        ruled.rule = Some("_".to_owned());
        assert!(ruled.validate().is_err());

        // Unknown field refused at the serde boundary.
        assert!(
            serde_json::from_str::<Request>(r#"{"schema":"janitor.v1","cmd":"ping","nope":1}"#)
                .is_err()
        );
    }

    /// The default socket path honors `$JANITOR_SOCK` first and carries the
    /// uid otherwise (ADR-0018 D1's contract).
    #[test]
    fn default_sock_path_is_env_then_uid() {
        assert_eq!(
            sock_path_from(Some("/tmp/janitor-test-explicit.sock")),
            PathBuf::from("/tmp/janitor-test-explicit.sock")
        );
        // Empty and absent env fall through to the uid-shaped default.
        for env_sock in [None, Some("")] {
            let path = sock_path_from(env_sock);
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            assert!(
                (name.starts_with("janitor-") && name.ends_with(".sock")) || name == "janitor.sock",
                "uid-shaped default, got {name:?}"
            );
        }
    }
}
