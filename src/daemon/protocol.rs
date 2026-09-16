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
//! {"schema":"janitor.v1","cmd":"duplicates","path":"src","min_size":8,"min_members":3,"summary":true}
//! {"schema":"janitor.v1","cmd":"stats"}
//! {"schema":"janitor.v1","cmd":"shutdown"}
//! ```
//!
//! Response: the standard envelope (see `crate::envelope`), one line.

use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ast::cache::CacheStats;
use crate::envelope::SCHEMA;
use crate::error::{DaemonError, JanitorError, OutputError};

/// The request-line cap (ADR-0018 D6): a rule plus flags fits comfortably;
/// anything bigger is refused with the typed error, never buffered.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// The response-line read cap for CLIENTS: a scan of a large tree routinely
/// produces multi-megabyte envelopes (issue #21 saw ~2 MiB for a 7378-match
/// repo), so the client's bound must sit well above the request cap the
/// daemon enforces — 64 MiB is far past any honest envelope while still a
/// real fence against a runaway peer.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Per-connection read/write timeout (both sides): a request must arrive
/// (and be answered) within this, or the connection is dropped. Also the
/// global drain deadline of [`write_line`]'s honest write loop.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the honest write loop yields between `WouldBlock` retries —
/// each blocked `write` already waited the socket's send timeout, so this
/// only keeps a non-timeout (nonblocking) caller from hot-spinning.
const WRITE_RETRY_PAUSE: Duration = Duration::from_millis(1);

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
    /// `duplicates` mode: only groups with at least N members survive
    /// (v0.3 B3; default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_members: Option<u32>,
    /// `duplicates` mode: prepend the per-kind aggregate block (v0.3 B3;
    /// default off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
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
            min_members: None,
            summary: None,
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
/// stripped. The bound is checked between chunk reads (4 KiB each), so a
/// line may overshoot `max` by less than one chunk before this returns —
/// the caller decides what an over-cap line means (the daemon types it
/// `E_DAEMON_REQUEST_TOO_LARGE`; clients see truncated JSON as a protocol
/// break). Raw I/O errors are the caller's to type.
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

/// Write one line (JSON value + newline) and flush. The write is honest:
/// every byte leaves the process or the call fails (issue #21 — the old
/// `write_all` aborted the whole response on the first `WouldBlock` a send
/// timeout produced once the send buffer filled, truncating every response
/// over the socket's `SO_SNDBUF` at exactly that boundary).
pub fn write_line(
    stream: &mut impl Write,
    value: &serde_json::Value,
) -> Result<(), std::io::Error> {
    let mut text =
        serde_json::to_string(value).map_err(|err| std::io::Error::other(err.to_string()))?;
    text.push('\n');
    write_all_with_deadline(stream, text.as_bytes(), Instant::now() + IO_TIMEOUT)?;
    stream.flush()
}

/// `write_all`'s all-or-nothing contract, honest under a send timeout: a
/// `write` on a socket with `SO_SNDTIMEO` returns `ErrorKind::WouldBlock`
/// when the send buffer is full (a slow peer, not a dead one), so those are
/// RETRIED — as are `Interrupted`s — until `deadline`. A deadline overrun
/// becomes a typed `TimedOut` carrying the partial-progress count; any
/// other error aborts the write as before.
pub fn write_all_with_deadline(
    stream: &mut impl Write,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), std::io::Error> {
    let mut written = 0_usize;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(std::io::Error::from(ErrorKind::WriteZero));
            }
            Ok(n) => written += n,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        format!(
                            "send buffer never drained: {written}/{} bytes written before the deadline",
                            bytes.len()
                        ),
                    ));
                }
                std::thread::sleep(WRITE_RETRY_PAUSE);
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
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

        // v0.3 B3: the duplicates knobs ride the wire (append-only,
        // optional — old request shapes are unchanged).
        let summarized = Request {
            min_members: Some(3),
            summary: Some(true),
            ..Request::bare("duplicates")
        };
        summarized
            .validate()
            .expect("duplicates with the v0.3 knobs");
        let line = summarized.to_line().expect("serializes");
        let parsed: Request = serde_json::from_str(&line).expect("round-trips");
        assert_eq!(parsed.min_members, Some(3));
        assert_eq!(parsed.summary, Some(true));
        assert!(
            !line.contains("\"summary\":false"),
            "default-off knobs are skipped, not zeroed: {line}"
        );

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

    /// Issue #21's regression fence: with a send timeout armed and the send
    /// buffer full, `write` returns `ErrorKind::WouldBlock` — and the OLD
    /// `write_all` read that as a dead peer, truncating every response over
    /// the buffer size (8 KiB `SO_SNDBUF` for AF_UNIX on macOS; ~208 KiB on
    /// Linux). The honest write loop must retry until a slow-but-alive peer
    /// drains, preserving `write_all`'s all-or-nothing contract.
    #[test]
    fn write_line_drains_a_slow_peer_past_the_send_buffer() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let (mut writer, mut reader) = UnixStream::pair().expect("socketpair");
        // A 1 ms send timeout: the buffer fills and WouldBlock fires almost
        // immediately — the macOS condition compressed into test time.
        writer
            .set_write_timeout(Some(Duration::from_millis(1)))
            .expect("write timeout");

        // A response line well past any AF_UNIX SO_SNDBUF.
        let payload = serde_json::json!({ "pad": "x".repeat(512 * 1024) });
        let expected = serde_json::to_string(&payload).expect("serializes").len() + 1;

        // The peer stalls first (the buffer fills, WouldBlock fires), then
        // drains everything down to EOF.
        let drainer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let mut drained = 0_usize;
            let mut chunk = [0_u8; 64 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => drained += n,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => return Err(format!("drain failed: {err}")),
                }
            }
            Ok(drained)
        });

        write_line(&mut writer, &payload).expect("every byte reaches the slow peer");
        drop(writer); // EOF for the drainer.
        let drained = drainer.join().expect("drainer exits");
        let drained = drained.expect("the drain completes");
        assert_eq!(drained, expected, "the whole line crossed the socket");
    }

    /// The honest loop's fence the other way: a peer that NEVER drains must
    /// fail with a typed `TimedOut` at the deadline — the drain guarantee
    /// the socket write timeout used to carry alone (issue #21 kept the
    /// timeout; the loop must not outlive it).
    #[test]
    fn write_all_with_deadline_times_out_when_the_peer_never_drains() {
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};

        let (mut writer, reader) = UnixStream::pair().expect("socketpair");
        writer
            .set_write_timeout(Some(Duration::from_millis(1)))
            .expect("write timeout");

        let started = Instant::now();
        let err = write_all_with_deadline(
            &mut writer,
            &vec![b'x'; 512 * 1024],
            Instant::now() + Duration::from_millis(50),
        )
        .expect_err("a stalling peer exhausts the deadline");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline fires, it does not hang: {:?}",
            started.elapsed()
        );
        drop(reader); // release the peer side cleanly.
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
