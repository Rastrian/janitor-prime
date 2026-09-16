//! The daemon client (F10a, ADR-0018 D7): one request line in, one
//! envelope line out — plus the `scan --daemon` routing decision.
//!
//! The CLI never spawns the daemon in v0.2; `--daemon` is opt-in routing:
//! if `$JANITOR_SOCK` (or the default path) answers `ping` AND serves the
//! workspace this scan roots in, the request goes through the socket.
//! Anything else falls back to the in-process path (a stderr note at most,
//! never a stdout side effect) — and the fall is TYPED: no socket, no
//! answer, or a wrong workspace is the silent `NoDaemon` case the fallback
//! was designed for, while a daemon that WAS serving this workspace but
//! could not deliver an intact frame is `Torn` (issue #21's truncation) —
//! the CLI warns on stderr before serving in-process. A daemon that is
//! *reachable* but rejects the scan (bad rule, out-of-workspace path)
//! answers with its own error envelope, which IS routed through: the
//! fallback covers unreachability, not the daemon's honest refusals.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
/// (the socket's own I/O timeouts), not the dial. Both the request write
/// and the response read are the honest loop's to drain (issue #21: a send
/// timeout makes `WouldBlock` on a full buffer ordinary, never fatal), so
/// post-dial failures type as `Protocol` — the conversation broke — not
/// `Connect`, which stays "nothing answered at all".
pub fn request(sock: &Path, req: &Request, timeout: Duration) -> Result<Value, JanitorError> {
    let path_label = sock.display().to_string();
    let connect = |detail: String| DaemonError::Connect {
        path: path_label.clone(),
        detail,
    };
    let protocol_err = |detail: String| DaemonError::Protocol {
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
    protocol::write_all_with_deadline(&mut stream, line.as_bytes(), Instant::now() + timeout)
        .and_then(|()| stream.flush())
        .map_err(|err| protocol_err(format!("request write: {err}")))?;

    let response = protocol::read_line(&mut stream, protocol::MAX_RESPONSE_BYTES)
        .map_err(|err| protocol_err(format!("response read: {err}")))?;
    let value: Value = serde_json::from_slice(&response).map_err(|err| {
        protocol_err(format!(
            "response is not JSON (torn at {} bytes?): {err}",
            response.len()
        ))
    })?;
    if value.get("schema").and_then(Value::as_str) != Some(crate::envelope::SCHEMA) {
        return Err(JanitorError::Daemon(DaemonError::Protocol {
            path: path_label,
            detail: "response does not carry the janitor.v1 schema".to_owned(),
        }));
    }
    Ok(value)
}

/// The `--daemon` routing decision (ADR-0018 D7), typed by WHY the socket
/// path was not taken — the CLI's fallback visibility hangs off the
/// difference (issue #21: a torn frame must not look like "no daemon").
#[derive(Debug)]
pub enum Routed {
    /// The socket answered for this workspace: the envelope to re-emit.
    Served(Value),
    /// No usable daemon — no socket, no ping answer, wrong workspace. The
    /// documented silent in-process fallback.
    NoDaemon,
    /// The daemon answered `ping` for THIS workspace but the response frame
    /// arrived torn/truncated/invalid — the fallback still happens, but it
    /// must be visible (stderr warning before the slow path serves).
    Torn(String),
}

/// Route `req` through the daemon at `sock` when it serves `scan_root`'s
/// workspace (ADR-0018 D7).
pub fn route_request(sock: &Path, scan_root: &Path, req: &Request, timeout: Duration) -> Routed {
    let ping = match request(sock, &Request::ping(), timeout) {
        Ok(ping) => ping,
        Err(_) => return Routed::NoDaemon,
    };
    if ping.get("ok").and_then(Value::as_bool) != Some(true) {
        return Routed::NoDaemon;
    }
    let Some(workspace) = ping
        .pointer("/data/workspace")
        .and_then(Value::as_str)
        .map(PathBuf::from)
    else {
        return Routed::NoDaemon;
    };
    let workspace = super::canonicalize_deepest(&workspace);
    let root = super::canonicalize_deepest(scan_root);
    if !root.starts_with(&workspace) {
        // A daemon serving another tree must never answer for this one.
        tracing::debug!(
            sock = %sock.display(),
            workspace = %workspace.display(),
            "daemon workspace does not cover the scan root; falling back"
        );
        return Routed::NoDaemon;
    }
    match request(sock, req, timeout) {
        Ok(value) => Routed::Served(value),
        Err(err) => Routed::Torn(err.to_string()),
    }
}
