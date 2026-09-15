//! The daemon client (F10a, ADR-0018 D7): one request line in, one
//! envelope line out — plus the `scan --daemon` routing decision.
//!
//! The CLI never spawns the daemon in v0.2; `--daemon` is opt-in routing:
//! if `$JANITOR_SOCK` (or the default path) answers `ping` AND serves the
//! workspace this scan roots in, the request goes through the socket;
//! anything else — no socket, no answer, wrong workspace, broken response —
//! falls back to the in-process path silently (a stderr note, never a
//! stdout side effect). A daemon that is *reachable* but rejects the scan
//! (bad rule, out-of-workspace path) answers with its own error envelope,
//! which IS routed through: the fallback covers unreachability, not the
//! daemon's honest refusals.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::daemon::protocol::{self, Request};
use crate::error::{DaemonError, JanitorError};

/// How long `--daemon` routing waits for `ping` before falling back.
pub const PING_TIMEOUT: Duration = Duration::from_millis(500);

/// Send one request, read one response envelope. The response is validated
/// to be a `janitor.v1` object (`schema` key present) — anything else is
/// the typed protocol error, never passed through as if it were ours.
///
/// `connect` on an AF_UNIX socket is kernel-local and fails immediately on
/// a dead/stale file, so the timeout governs the request/response exchange
/// (the socket's own I/O timeouts), not the dial.
pub fn request(sock: &Path, req: &Request, timeout: Duration) -> Result<Value, JanitorError> {
    let path_label = sock.display().to_string();
    let connect = |detail: String| DaemonError::Connect {
        path: path_label.clone(),
        detail,
    };

    let mut stream = UnixStream::connect(sock).map_err(|err| connect(err.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|err| connect(err.to_string()))?;

    let mut line = req.to_line()?;
    line.push('\n');
    use std::io::Write;
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|err| connect(err.to_string()))?;

    let response = protocol::read_line(&mut stream, protocol::MAX_REQUEST_BYTES * 4)
        .map_err(|err| connect(err.to_string()))?;
    let value: Value = serde_json::from_slice(&response).map_err(|err| {
        JanitorError::Daemon(DaemonError::Protocol {
            path: path_label.clone(),
            detail: format!("response is not JSON: {err}"),
        })
    })?;
    if value.get("schema").and_then(Value::as_str) != Some(crate::envelope::SCHEMA) {
        return Err(JanitorError::Daemon(DaemonError::Protocol {
            path: path_label,
            detail: "response does not carry the janitor.v1 schema".to_owned(),
        }));
    }
    Ok(value)
}

/// The `--daemon` routing decision (ADR-0018 D7): `Some(envelope)` when the
/// socket answered for this workspace, `None` to fall back in-process.
pub fn route_request(
    sock: &Path,
    scan_root: &Path,
    req: &Request,
    timeout: Duration,
) -> Option<Value> {
    let ping = request(sock, &Request::ping(), timeout).ok()?;
    if ping.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let workspace = ping
        .pointer("/data/workspace")
        .and_then(Value::as_str)
        .map(PathBuf::from)?;
    let workspace = super::canonicalize_deepest(&workspace);
    let root = super::canonicalize_deepest(scan_root);
    if !root.starts_with(&workspace) {
        // A daemon serving another tree must never answer for this one.
        tracing::debug!(
            sock = %sock.display(),
            workspace = %workspace.display(),
            "daemon workspace does not cover the scan root; falling back"
        );
        return None;
    }
    request(sock, req, timeout).ok()
}
