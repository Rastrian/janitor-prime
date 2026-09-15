//! The daemon (F10a, ADR-0018): one long-lived process owning a WARM
//! `ParseCache` the CLI cannot have, kept fresh by a polling watcher, and
//! serving the same `janitor.v1` envelope over a local Unix domain socket.
//!
//! ```text
//! janitor daemon ──accept loop── thread-per-connection ── session.scan()
//!       │                (cap, 0600 socket)                 on rayon
//!       └──watcher thread── poll warm set ── incremental re-parse (KP-1)
//! ```
//!
//! What is reused unchanged: the `orchestrator::Session` composition root,
//! the scan/duplicates pipelines (with `retain_cache: true`), the rayon
//! pool, Rules T1–T6. What is new: the socket lifecycle (bind with 0600,
//! probe-and-remove stale files, idle death), the bound LRU cache, the
//! watcher, and the `E_DAEMON_*` error family. What is deliberately NOT
//! here: tokio (ADR-0008's async half stands — thread-per-connection over
//! one-shot JSON lines needs no runtime; see ADR-0018 D8 for the partial
//! reversal), `apply` through the socket (CLI-only in v0.2, D5), semantic
//! validation (ADR-0017).
//!
//! Concurrency/locking story (D5, restated): dashmap shard guards are held
//! only to clone/tick an entry — never across a parse; snapshots are
//! `Arc<[u8]>` clones, so concurrent scanners never serialize on a lock;
//! every mutating path takes the entry out of the cache FIRST
//! (`ParseCache::take`, the Rule T1 exclusivity one-way door) — a scan
//! racing a watcher re-parse sees a valid old entry or a miss, never a
//! torn one.

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

pub mod client;
pub mod protocol;
pub mod watch;

use crate::envelope::Envelope;
use crate::error::{DaemonError, JanitorError, OutputError, VfsError};
use crate::orchestrator::Session;
use crate::scan::{
    DEFAULT_MAX_FILE_SIZE, DEFAULT_MIN_DUPLICATE_SIZE, DuplicatesParams, ScanParams,
};
use crate::vfs::{Os, Overlay};

/// Watch poll interval default (ADR-0018 D2).
pub const DEFAULT_INTERVAL_SECS: u64 = 2;

/// Cache bound default, in MiB (ADR-0018 D4).
pub const DEFAULT_MAX_CACHE_MB: u64 = 512;

/// Idle-exit default: 30 min without connections (ADR-0018 D7).
pub const DEFAULT_IDLE_SECS: u64 = 1800;

/// Concurrent connection cap; above it the daemon answers `E_DAEMON_BUSY`
/// and closes (D6).
pub const MAX_CONNECTIONS: usize = 64;

/// How long the accept loop sleeps between polls (non-blocking accept).
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Per-connection read/write timeout: a request must arrive (and be
/// answered) within this, or the connection is dropped.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// On exit, how long the accept loop waits for in-flight handlers before
/// leaving them to the process teardown (bounded wait, F2.8's pattern).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The watcher sleeps in slices of this size so shutdown stays responsive.
const WATCH_SLICE: Duration = Duration::from_millis(100);

/// Everything the loops and handlers share. `Arc`'d; every connection
/// handler and both loops hold one handle.
struct Shared {
    session: Session,
    /// Canonical workspace root: every request path must resolve inside it
    /// (ADR-0018 D6 — bind to one workspace, no traversal).
    workspace: PathBuf,
    sock: PathBuf,
    started: Instant,
    interval: Duration,
    idle: Duration,
    shutdown: AtomicBool,
    /// Set when the accept loop exited via the idle timeout (run()'s exit
    /// envelope reports the reason).
    exit_idle: AtomicBool,
    active: AtomicUsize,
    watch: watch::WatchCounters,
}

/// Daemon startup configuration (ADR-0018 D1/D2/D4/D7).
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// Workspace root; canonicalized at start and pinned thereafter.
    pub workspace: PathBuf,
    /// Socket path (create; 0600; unlinked on clean exit).
    pub sock: PathBuf,
    /// Watch poll interval.
    pub interval: Duration,
    /// Cache bound in accounted bytes (LRU eviction, ADR-0018 D4).
    pub max_cache_bytes: u64,
    /// Exit after this long without connections.
    pub idle: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from("."),
            sock: protocol::default_sock_path(),
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            max_cache_bytes: DEFAULT_MAX_CACHE_MB * 1024 * 1024,
            idle: Duration::from_secs(DEFAULT_IDLE_SECS),
        }
    }
}

/// A running daemon: the accept loop and the watcher are live; the socket
/// file exists. Call [`Daemon::wait`] to block until the daemon exits
/// (shutdown command or idle), or [`Daemon::request_shutdown`] first.
pub struct Daemon {
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
    watcher: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Bind the socket (0600, stale-file recovery) and start the accept
    /// and watcher loops. Returns once the daemon is serving (connections
    /// queue in the socket backlog immediately after).
    pub fn start(config: DaemonConfig) -> Result<Self, JanitorError> {
        let workspace = std::fs::canonicalize(&config.workspace).map_err(|err| VfsError::Io {
            path: config.workspace.display().to_string(),
            message: format!("workspace: {err}"),
        })?;
        if !workspace.is_dir() {
            return Err(JanitorError::Vfs(VfsError::Io {
                path: workspace.display().to_string(),
                message: "workspace is not a directory".to_owned(),
            }));
        }

        let cache = Arc::new(crate::ast::cache::ParseCache::bounded(
            config.max_cache_bytes,
        ));
        let session = Session::with_overlay_cache_root(
            Arc::new(Overlay::new(Arc::new(Os))),
            cache,
            workspace.join(".janitor"),
        );
        let listener = bind_socket(&config.sock)?;
        let shared = Arc::new(Shared {
            session,
            workspace,
            sock: config.sock.clone(),
            started: Instant::now(),
            interval: config.interval,
            idle: config.idle,
            shutdown: AtomicBool::new(false),
            exit_idle: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            watch: watch::WatchCounters::default(),
        });

        let accept_shared = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("janitor-daemon-accept".to_owned())
            .spawn(move || accept_loop(accept_shared, listener))
            .map_err(|err| {
                // The listener died with the closure; the socket file did
                // not — remove it or the next start sees a stale one.
                shared.shutdown.store(true, Ordering::SeqCst);
                drop(std::fs::remove_file(&config.sock));
                DaemonError::Bind {
                    path: config.sock.display().to_string(),
                    detail: format!("accept loop spawn: {err}"),
                }
            })?;

        let watch_shared = Arc::clone(&shared);
        let watcher = std::thread::Builder::new()
            .name("janitor-daemon-watch".to_owned())
            .spawn(move || watch_loop(watch_shared))
            .map_err(|err| {
                // The accept loop is already live and will clean the socket
                // up on its way out; just signal it.
                shared.shutdown.store(true, Ordering::SeqCst);
                DaemonError::Bind {
                    path: config.sock.display().to_string(),
                    detail: format!("watcher spawn: {err}"),
                }
            })?;

        Ok(Self {
            shared,
            accept: Some(accept),
            watcher: Some(watcher),
        })
    }

    /// The socket path this daemon serves on.
    pub fn sock(&self) -> &Path {
        &self.shared.sock
    }

    /// The canonical workspace this daemon serves.
    pub fn workspace(&self) -> &Path {
        &self.shared.workspace
    }

    /// The status snapshot (`ping`/`stats` payload) — the test seam for
    /// cache and watcher counters without going through the socket.
    pub fn status(&self) -> protocol::DaemonStatus {
        let cache = self.shared.session.cache().stats();
        protocol::DaemonStatus {
            cmd: "ping",
            version: env!("CARGO_PKG_VERSION"),
            pid: std::process::id(),
            uptime_secs: self.shared.started.elapsed().as_secs(),
            workspace: self.shared.workspace.display().to_string(),
            sock: self.shared.sock.display().to_string(),
            cache,
            watch: protocol::WatchStatus {
                interval_secs: self.shared.interval.as_secs(),
                passes: self.shared.watch.passes.load(Ordering::Relaxed),
                reparses: self.shared.watch.reparses.load(Ordering::Relaxed),
                warm: cache.entries,
            },
        }
    }

    /// Ask the loops to stop (the `shutdown` command's effect).
    pub fn request_shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
    }

    /// Block until both loops have exited and the socket file is gone.
    /// `&mut self`: the daemon stays inspectable afterwards (run()'s exit
    /// envelope reads the reason).
    pub fn wait(&mut self) {
        if let Some(handle) = self.accept.take() {
            drop(handle.join());
        }
        if let Some(handle) = self.watcher.take() {
            drop(handle.join());
        }
    }
}

/// The CLI entry: start, emit the startup envelope on stdout (100% JSON —
/// exactly two envelope lines per daemon lifetime, D7), serve until
/// shutdown or idle, emit the exit envelope.
pub fn run(config: DaemonConfig, pretty: bool) -> Result<(), JanitorError> {
    let mut daemon = Daemon::start(config)?;
    let max_cache_bytes = daemon
        .shared
        .session
        .cache()
        .stats()
        .bound_bytes
        .unwrap_or_default();
    let listening = serde_json::json!({
        "cmd": "daemon",
        "listening": true,
        "sock": daemon.shared.sock.display().to_string(),
        "workspace": daemon.shared.workspace.display().to_string(),
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "interval_secs": daemon.shared.interval.as_secs(),
        "max_cache_bytes": max_cache_bytes,
        "idle_secs": daemon.shared.idle.as_secs(),
    });
    crate::envelope::emit_stdout(&Envelope::ok(listening), pretty)?;

    daemon.wait();

    let reason = if daemon.shared.exit_idle.load(Ordering::SeqCst) {
        "idle"
    } else {
        "shutdown"
    };
    let stopped = serde_json::json!({
        "cmd": "daemon",
        "listening": false,
        "reason": reason,
        "uptime_secs": daemon.shared.started.elapsed().as_secs(),
    });
    crate::envelope::emit_stdout(&Envelope::ok(stopped), pretty)?;
    Ok(())
}

/// Bind the socket with 0600 permissions; on `EADDRINUSE`, probe whether a
/// live daemon owns it (typed `E_DAEMON_ALREADY_RUNNING` — a blind unlink
/// would let a second daemon steal a live one's socket) and only remove
/// the file when it is stale (a `kill -9` leftover: connect fails) —
/// ADR-0018 D7's crash-safety path.
fn bind_socket(sock: &Path) -> Result<UnixListener, JanitorError> {
    let path = sock.display().to_string();
    let bind_err = |detail: String| DaemonError::Bind {
        path: path.clone(),
        detail,
    };

    match UnixListener::bind(sock) {
        Ok(listener) => {
            std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))
                .map_err(|err| bind_err(format!("chmod 0600: {err}")))?;
            Ok(listener)
        }
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            // AF_UNIX connect is kernel-local: immediate answer from a live
            // listener, immediate refusal on a dead one's leftover file.
            let answers = UnixStream::connect(sock).is_ok();
            if answers {
                return Err(JanitorError::Daemon(DaemonError::AlreadyRunning { path }));
            }
            std::fs::remove_file(sock)
                .map_err(|err| bind_err(format!("stale socket removal: {err}")))?;
            UnixListener::bind(sock).map_err(|err| JanitorError::Daemon(bind_err(err.to_string())))
        }
        Err(err) => Err(JanitorError::Daemon(bind_err(err.to_string()))),
    }
}

/// The accept loop: non-blocking accept + poll (no async — ADR-0018 D8),
/// one thread per connection under [`MAX_CONNECTIONS`], idle death, and on
/// every exit path: stop the watcher, drain in-flight handlers (bounded),
/// unlink the socket file.
fn accept_loop(shared: Arc<Shared>, listener: UnixListener) {
    if let Err(err) = listener.set_nonblocking(true) {
        tracing::error!(%err, "cannot set the listener non-blocking; exiting");
        shared.shutdown.store(true, Ordering::SeqCst);
    }
    let mut last_accept = Instant::now();

    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                last_accept = Instant::now();
                let active = shared.active.fetch_add(1, Ordering::SeqCst) + 1;
                if active > MAX_CONNECTIONS {
                    shared.active.fetch_sub(1, Ordering::SeqCst);
                    let err: JanitorError = DaemonError::Busy {
                        active,
                        max: MAX_CONNECTIONS,
                    }
                    .into();
                    write_envelope(&stream, &err.into());
                    continue;
                }
                let conn_shared = Arc::clone(&shared);
                let spawned = std::thread::Builder::new()
                    .name("janitor-daemon-conn".to_owned())
                    .spawn(move || {
                        handle_connection(&conn_shared, stream);
                        conn_shared.active.fetch_sub(1, Ordering::SeqCst);
                    });
                if let Err(err) = spawned {
                    shared.active.fetch_sub(1, Ordering::SeqCst);
                    tracing::warn!(%err, "connection handler spawn failed");
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => {
                tracing::error!(%err, "accept failed; exiting");
                break;
            }
        }
        // Idle death (ADR-0018 D7): no connections at all since `idle` ago.
        if shared.active.load(Ordering::SeqCst) == 0 && last_accept.elapsed() >= shared.idle {
            tracing::info!(
                idle_secs = shared.idle.as_secs(),
                "idle timeout reached; exiting"
            );
            shared.exit_idle.store(true, Ordering::SeqCst);
            break;
        }
        std::thread::sleep(ACCEPT_POLL);
    }

    shared.shutdown.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while shared.active.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Err(err) = std::fs::remove_file(&shared.sock) {
        // ENOENT races with test-side cleanup; anything else leaves a file
        // the next start's stale-socket probe handles (D7).
        tracing::debug!(%err, "socket file cleanup");
    }
}

/// The watcher loop: poll the warm set every interval, sleep sliced so
/// shutdown stays responsive, never fatal (ADR-0018 D3).
fn watch_loop(shared: Arc<Shared>) {
    let mut state = watch::WatchState::default();
    loop {
        let deadline = Instant::now() + shared.interval;
        while Instant::now() < deadline {
            if shared.shutdown.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(WATCH_SLICE);
        }
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match watch::pass(&shared.session, &shared.workspace, &mut state) {
            Ok(reparsed) => {
                shared.watch.passes.fetch_add(1, Ordering::Relaxed);
                shared.watch.reparses.fetch_add(reparsed, Ordering::Relaxed);
            }
            Err(err) => tracing::warn!(code = err.code(), "watch pass failed: {err}"),
        }
    }
}

/// One connection: one request line in, one envelope line out (D1).
fn handle_connection(shared: &Shared, stream: UnixStream) {
    let mut stream = stream;
    if let Err(err) = stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
    {
        tracing::debug!(%err, "connection timeouts unavailable");
    }

    let (response, shutdown_requested) =
        match protocol::read_line(&mut stream, protocol::MAX_REQUEST_BYTES) {
            Err(err) => (
                error_envelope(
                    DaemonError::RequestMalformed {
                        detail: format!("read: {err}"),
                    }
                    .into(),
                ),
                false,
            ),
            Ok(line) if line.len() > protocol::MAX_REQUEST_BYTES => (
                error_envelope(
                    DaemonError::RequestTooLarge {
                        len: line.len(),
                        max: protocol::MAX_REQUEST_BYTES,
                    }
                    .into(),
                ),
                false,
            ),
            Ok(line) => dispatch(shared, &line),
        };
    write_envelope(&stream, &response);
    if shutdown_requested {
        shared.shutdown.store(true, Ordering::SeqCst);
    }
}

/// Parse + validate + execute one request line. The bool flags a shutdown
/// request (the caller writes the response BEFORE flipping the flag — the
/// client must receive its ack).
fn dispatch(shared: &Shared, line: &[u8]) -> (Envelope<Value>, bool) {
    let request: protocol::Request = match serde_json::from_slice(line) {
        Ok(request) => request,
        Err(err) => {
            return (
                error_envelope(
                    DaemonError::RequestMalformed {
                        detail: format!("not a request: {err}"),
                    }
                    .into(),
                ),
                false,
            );
        }
    };
    if let Err(err) = request.validate() {
        return (error_envelope(err.into()), false);
    }
    match execute(shared, &request) {
        Ok(envelope) => (envelope, request.cmd == "shutdown"),
        Err(err) => (error_envelope(err), false),
    }
}

/// Run one validated request against the session.
fn execute(shared: &Shared, request: &protocol::Request) -> Result<Envelope<Value>, JanitorError> {
    match request.cmd.as_str() {
        "ping" | "stats" => status_envelope(shared, request.cmd == "stats"),
        "scan" => {
            let root = resolve_root(shared, request.path.as_deref())?;
            let params = ScanParams {
                rule: request.rule.clone().unwrap_or_default(),
                root,
                lang_filter: request.lang.clone(),
                include_virtual: request.include_virtual,
                strict_trivia: request.strict_trivia,
                max_file_size: request.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE),
                // The whole point: entries stay warm across requests
                // (ADR-0007's streaming discipline is the CLI's shape).
                retain_cache: true,
            };
            let report = shared.session.scan(&params)?;
            ok_envelope(&report)
        }
        "duplicates" => {
            let root = resolve_root(shared, request.path.as_deref())?;
            let params = DuplicatesParams {
                root,
                lang_filter: request.lang.clone(),
                include_virtual: request.include_virtual,
                max_file_size: request.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE),
                min_size: request.min_size.unwrap_or(DEFAULT_MIN_DUPLICATE_SIZE),
                retain_cache: true,
            };
            let report = shared.session.duplicates(&params)?;
            ok_envelope(&report)
        }
        "shutdown" => ok_envelope(&serde_json::json!({
            "cmd": "shutdown",
            "shutting_down": true,
        })),
        // Unreachable past validate(); kept total for future callers.
        _ => Err(DaemonError::RequestMalformed {
            detail: format!("unknown cmd {:?}", request.cmd),
        }
        .into()),
    }
}

/// The `ping`/`stats` payload envelope: liveness, identity, cache stats,
/// watcher counters (ADR-0018 D1/D7).
fn status_envelope(shared: &Shared, stats_cmd: bool) -> Result<Envelope<Value>, JanitorError> {
    let cache = shared.session.cache().stats();
    let status = protocol::DaemonStatus {
        cmd: if stats_cmd { "stats" } else { "ping" },
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        uptime_secs: shared.started.elapsed().as_secs(),
        workspace: shared.workspace.display().to_string(),
        sock: shared.sock.display().to_string(),
        cache,
        watch: protocol::WatchStatus {
            interval_secs: shared.interval.as_secs(),
            passes: shared.watch.passes.load(Ordering::Relaxed),
            reparses: shared.watch.reparses.load(Ordering::Relaxed),
            warm: cache.entries,
        },
    };
    ok_envelope(&status)
}

/// Resolve a request's scan root against the workspace and enforce the
/// boundary (ADR-0018 D6): relative paths join the workspace, absolute
/// paths must already fall inside it, and the canonicalized result (the
/// deepest existing ancestor + tail — symlinks resolved) must start with
/// the workspace root. No `..`, no symlink escape.
fn resolve_root(shared: &Shared, requested: Option<&str>) -> Result<PathBuf, JanitorError> {
    let workspace = &shared.workspace;
    let candidate = match requested {
        None => workspace.clone(),
        Some(path) => {
            let path = Path::new(path);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                workspace.join(path)
            }
        }
    };
    let canonical = canonicalize_deepest(&candidate);
    if canonical.starts_with(workspace) {
        Ok(candidate)
    } else {
        Err(DaemonError::PathOutsideWorkspace {
            path: candidate.display().to_string(),
            workspace: workspace.display().to_string(),
        }
        .into())
    }
}

/// Canonicalize `path`, or — when it does not exist yet — its deepest
/// existing ancestor with the nonexistent tail re-joined. Symlinks are
/// resolved wherever they exist, so a link pointing outside the workspace
/// canonicalizes outside (the boundary check catches it).
pub(crate) fn canonicalize_deepest(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            let mut out = canonical;
            for segment in tail.iter().rev() {
                out.push(segment);
            }
            return out;
        }
        match (current.file_name(), current.parent()) {
            (Some(segment), Some(parent)) => {
                tail.push(segment.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// A success envelope around any serializable payload.
fn ok_envelope<T: Serialize>(data: &T) -> Result<Envelope<Value>, JanitorError> {
    let value = serde_json::to_value(data)
        .map_err(|err| JanitorError::Output(OutputError::Serialization(err.to_string())))?;
    Ok(Envelope::ok(value))
}

/// An error envelope (the one `From<JanitorError>` mapping, PLAN §1.7).
fn error_envelope(err: JanitorError) -> Envelope<Value> {
    err.into()
}

/// Best-effort envelope write to a connection (a dead client is logged,
/// never fatal).
fn write_envelope(stream: &UnixStream, envelope: &Envelope<Value>) {
    let value = match serde_json::to_value(envelope) {
        Ok(value) => value,
        Err(err) => {
            tracing::error!(%err, "envelope serialization failed");
            return;
        }
    };
    let mut stream = stream;
    if let Err(err) = protocol::write_line(&mut stream, &value) {
        tracing::debug!(%err, "response write failed (client gone?)");
    }
}
