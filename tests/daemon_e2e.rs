//! Daemon end-to-end fences (F10a, ADR-0018) — every test runs against a
//! REAL Unix socket on a REAL daemon (library `Daemon::start` for the
//! protocol/warm-cache fences; the real binary for lifecycle, crash
//! safety, and `scan --daemon` routing). The six mandated areas from the
//! F10a brief, one named test each (plus the surrounding contract):
//!
//! 1. warm-cache proof — second identical scan hits, is faster, and the
//!    report is byte-identical to both the cold scan and an in-process
//!    scan (modulo `stats.wall_ms`);
//! 2. incremental — a changed watched file is re-parsed by the watcher
//!    only, and the next scan is still all hits;
//! 3. eviction — a small `--max-cache-mb` bound LRU-evicts, later queries
//!    re-parse (misses), and accounted bytes never exceed the bound;
//! 4. concurrency — parallel scans + pings + watcher re-parses (the
//!    apply-equivalent mutation, ADR-0018 D5) with a bounded wait: every
//!    response is one intact envelope, no deadlock, daemon still serves;
//! 5. socket hygiene — 0600 permissions; paths outside the workspace
//!    (including a symlink escape) are the typed boundary error;
//! 6. crash safety — `kill -9` leaves the socket file; the next start
//!    probes, removes it, and serves.
//!
//! Plus: the request-validation surface, the already-running probe, the
//! connection cap, idle death, and the CLI `--daemon` routing contract
//! (route when the workspace matches, fall back silently otherwise).
#![cfg(feature = "lang-rust")]

use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use janitor_prime::daemon::protocol::Request;
use janitor_prime::daemon::{Daemon, DaemonConfig, client};
use serde_json::Value;

/// Scan/protocol timeouts: generous for CI, still bounded.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the bounded waits poll before declaring failure.
const WAIT_DEADLINE: Duration = Duration::from_secs(10);

// --- helpers ---------------------------------------------------------------

/// One deterministic rust source file: `fns` functions with varied shapes
/// (doc comments, lets, returns) so the trees are non-trivial.
fn corpus_file_source(file: usize, fns: usize) -> String {
    let mut out = String::from("//! corpus file\n");
    for f in 0..fns {
        out.push_str(&format!(
            "/// doc for f{file}_{f}\n\
             pub fn f{file}_{f}(x: u32) -> u32 {{\n    \
             let y = x + {f};\n    \
             let z = y * {file};\n    \
             z + {f}\n\
             }}\n\n"
        ));
    }
    out
}

/// Seed `files` rust files under `<ws>/src/`; returns the file count.
fn seed_corpus(ws: &Path, files: usize, fns_per_file: usize) {
    std::fs::create_dir_all(ws.join("src")).expect("src dir");
    for i in 0..files {
        std::fs::write(
            ws.join("src").join(format!("m{i:03}.rs")),
            corpus_file_source(i, fns_per_file),
        )
        .expect("corpus file");
    }
}

fn config(ws: &Path, sock: &Path) -> DaemonConfig {
    DaemonConfig {
        workspace: ws.to_path_buf(),
        sock: sock.to_path_buf(),
        interval: Duration::from_millis(150),
        max_cache_bytes: 512 * 1024 * 1024,
        idle: Duration::from_secs(1800),
    }
}

fn start(ws: &Path, sock: &Path) -> Daemon {
    let daemon = Daemon::start(config(ws, sock)).expect("daemon starts");
    // Serve-ability proof: ping answers before the test proceeds.
    let pong = client::request(sock, &Request::ping(), REQUEST_TIMEOUT).expect("ping answers");
    assert_eq!(pong["ok"], true, "ping must be ok: {pong}");
    daemon
}

fn scan_via(sock: &Path, rule: &str, path: Option<&str>) -> Value {
    client::request(
        sock,
        &Request::scan(rule, path.map(str::to_owned)),
        REQUEST_TIMEOUT,
    )
    .expect("scan request answers")
}

fn cache_stats(sock: &Path) -> Value {
    let pong = client::request(sock, &Request::ping(), REQUEST_TIMEOUT).expect("ping");
    pong.pointer("/data/cache")
        .cloned()
        .expect("ping carries cache stats")
}

/// Poll `probe` until it returns `Some`, or the deadline passes.
fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + WAIT_DEADLINE;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Strip the fields that differ between daemon and in-process runs BY
/// DESIGN so the rest compares byte-for-byte: `stats.wall_ms` (the only
/// non-deterministic field) and the v0.3 B4 provenance surface
/// (`served_by` + `stats.cache` — the daemon answers "daemon" with real
/// counters, the in-process report omits both). Works at both shapes
/// compared here: a bare report and a full envelope.
fn without_wall(value: &Value) -> Value {
    let mut value = value.clone();
    for pointer in ["/stats/wall_ms", "/data/stats/wall_ms"] {
        if let Some(wall) = value.pointer_mut(pointer) {
            *wall = Value::Null;
        }
    }
    // Provenance keys are REMOVED, not nulled: the in-process report omits
    // them entirely, and a null key is still a key.
    for pointer in [
        "/served_by",
        "/data/served_by",
        "/stats/cache",
        "/data/stats/cache",
    ] {
        let (parent, key) = match pointer.rsplit_once('/') {
            Some(split) => split,
            None => continue,
        };
        if let Some(object) = value.pointer_mut(parent).and_then(Value::as_object_mut) {
            object.remove(key);
        }
    }
    value
}

fn stop(mut daemon: Daemon) {
    daemon.request_shutdown();
    let sock = daemon.sock().to_path_buf();
    daemon.wait();
    assert!(
        !sock.exists(),
        "clean exit unlinks the socket file (ADR-0018 D7)"
    );
}

/// Spawn the real binary as a daemon; return the child once it answers
/// `ping` (or fail with what it printed).
fn spawn_binary_daemon(ws: &Path, sock: &Path) -> std::process::Child {
    let child = Command::new(env!("CARGO_BIN_EXE_janitor"))
        .arg("daemon")
        .arg("--workspace")
        .arg(ws)
        .arg("--sock")
        .arg(sock)
        .arg("--interval")
        .arg("1")
        .arg("--idle-secs")
        .arg("3600")
        .arg("--max-cache-mb")
        .arg("64")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn janitor daemon");
    let answered = wait_for(|| {
        client::request(sock, &Request::ping(), REQUEST_TIMEOUT)
            .ok()
            .filter(|pong| pong["ok"] == true)
    });
    assert!(answered.is_some(), "binary daemon never answered ping");
    child
}

// --- 1. warm-cache proof ---------------------------------------------------

/// The mandated warm-cache proof: two consecutive identical scans through
/// the socket — the second is all hits, measurably faster (parse
/// dominates the cold pass), and its report is byte-identical to both the
/// cold scan and an in-process CLI-shaped scan (modulo `stats.wall_ms`).
#[test]
fn warm_scan_hits_is_faster_and_stays_identical() {
    let ws = tempfile::tempdir().expect("workspace");
    // ~2 MB of parseable rust: big enough that cold parsing dominates and
    // the warm/cold wall-time margin cannot invert on a fast machine.
    seed_corpus(ws.path(), 250, 33);
    let sock = ws.path().join("warm.sock");
    let daemon = start(ws.path(), &sock);

    let cold = scan_via(&sock, "(kind function_item)", None);
    assert_eq!(cold["ok"], true, "cold scan succeeds: {cold}");
    let cold_stats = cache_stats(&sock);
    assert!(
        cold_stats["misses"].as_u64().unwrap_or(0) >= 250,
        "the cold pass missed every file: {cold_stats}"
    );
    assert_eq!(
        cold_stats["hits"].as_u64().unwrap_or(1),
        0,
        "nothing was warm before the first scan: {cold_stats}"
    );

    let mut warm = scan_via(&sock, "(kind function_item)", None);
    assert_eq!(warm["ok"], true, "warm scan succeeds");
    let warm_stats = cache_stats(&sock);
    assert_eq!(
        warm_stats["misses"].as_u64().unwrap_or(1) - cold_stats["misses"].as_u64().unwrap_or(0),
        0,
        "the warm pass re-parsed nothing: {warm_stats}"
    );
    assert!(
        warm_stats["hits"].as_u64().unwrap_or(0) - cold_stats["hits"].as_u64().unwrap_or(0) >= 250,
        "the warm pass hit every file: {warm_stats}"
    );

    // Lower wall time: the deterministic half is already proven above (the
    // warm pass parsed NOTHING), so no-parse ⇒ faster follows on any
    // unloaded machine. Wall-clock on a shared runner can still starve a
    // 10 ms warm scan past a 150 ms cold one, so the fence allows one
    // retry before failing.
    let cold_wall = cold.pointer("/data/stats/wall_ms").and_then(Value::as_u64);
    if let Some(cold_wall) = cold_wall {
        let mut attempts = 0;
        loop {
            let warm_wall = warm
                .pointer("/data/stats/wall_ms")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            if warm_wall < cold_wall || attempts >= 1 {
                assert!(
                    warm_wall < cold_wall,
                    "warm ({warm_wall} ms) must beat cold ({cold_wall} ms) at this corpus size"
                );
                break;
            }
            attempts += 1;
            warm = scan_via(&sock, "(kind function_item)", None);
        }
    }

    // Correctness: identical matches, byte-for-byte, cold vs warm…
    assert_eq!(without_wall(&cold), without_wall(&warm));
    // …and daemon vs the in-process CLI shape (same pipeline, retain flag
    // aside — it must not leak into the report).
    let session = janitor_prime::orchestrator::Session::new();
    let report = session
        .scan(&janitor_prime::scan::ScanParams::new(
            "(kind function_item)",
            ws.path(),
        ))
        .expect("in-process scan");
    let in_process = serde_json::to_value(&report).expect("serialize");
    assert_eq!(
        without_wall(&cold["data"].clone()),
        without_wall(&in_process),
        "daemon response data equals the in-process report"
    );

    stop(daemon);
}

// --- 2. incremental --------------------------------------------------------

/// The mandated incremental fence: modify ONE watched file between scans —
/// the watcher re-parses exactly that file (incrementally), the next scan
/// is still all cache hits, and the report reflects the edit.
#[test]
fn watcher_reparses_only_the_changed_file() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 4, 8);
    let sock = ws.path().join("incr.sock");
    let daemon = start(ws.path(), &sock);

    let first = scan_via(&sock, "(kind function_item)", None);
    assert_eq!(first["ok"], true);
    let first_matches = first.pointer("/data/stats/matches").and_then(Value::as_u64);
    let baseline = cache_stats(&sock);
    // Let the watcher seed its stamps (one pass over the warm set).
    let seeded = wait_for(|| {
        let passes = daemon.status().watch.passes;
        (passes >= 1).then_some(passes)
    });
    assert!(seeded.is_some(), "watcher never completed a pass");
    assert_eq!(
        daemon.status().watch.reparses,
        0,
        "unchanged files are never re-parsed"
    );

    // One local edit: add one function (content AND length change, so the
    // (size, mtime) stamp cannot miss it).
    let target = ws.path().join("src/m001.rs");
    let mut edited = std::fs::read_to_string(&target).expect("target");
    edited.push_str("pub fn added_late(x: u32) -> u32 { x + 1 }\n");
    std::fs::write(&target, edited).expect("edit");

    let reparsed = wait_for(|| {
        let status = daemon.status();
        (status.watch.reparses == 1).then_some(status.watch.reparses)
    });
    assert!(
        reparsed.is_some(),
        "watcher re-parsed the changed file: {:?}",
        daemon.status()
    );
    assert_eq!(
        daemon.status().watch.reparses,
        1,
        "exactly one file — the others stayed warm"
    );

    // The next scan is all hits (the watcher already warmed the new
    // bytes) and reports the new function.
    let second = scan_via(&sock, "(kind function_item)", None);
    assert_eq!(second["ok"], true);
    let after = cache_stats(&sock);
    assert_eq!(
        after["misses"].as_u64().unwrap_or(1) - baseline["misses"].as_u64().unwrap_or(0),
        0,
        "the scan itself re-parsed nothing — the watcher did it: {after}"
    );
    let second_matches = second
        .pointer("/data/stats/matches")
        .and_then(Value::as_u64);
    assert_eq!(
        second_matches.unwrap_or(0),
        first_matches.unwrap_or(0) + 1,
        "the report reflects the edit"
    );

    stop(daemon);
}

// --- 3. eviction -----------------------------------------------------------

/// The mandated eviction fence: with a small bound the warm cache evicts
/// LRU, accounted bytes never exceed the bound, and a later scan of the
/// evicted paths re-parses (misses) — eviction loses warmth, never data.
#[test]
fn bounded_daemon_evicts_and_reparses() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 4, 30);
    let measure_sock = ws.path().join("measure.sock");

    // Pass 1: unbounded daemon measures the full accounted cost.
    let bound = {
        let daemon = Daemon::start(DaemonConfig {
            max_cache_bytes: u64::MAX,
            ..config(ws.path(), &measure_sock)
        })
        .expect("measuring daemon");
        let scan = scan_via(&measure_sock, "(kind function_item)", None);
        assert_eq!(scan["ok"], true);
        let stats = cache_stats(&measure_sock);
        assert_eq!(stats["entries"].as_u64(), Some(4), "all four stay warm");
        let bytes = stats["bytes"].as_u64().expect("accounted bytes");
        stop(daemon);
        // A bound under the total forces eviction (entries cost more than
        // 60% of the total each? no — but 4 entries cannot all fit under
        // 60% of their sum).
        bytes * 6 / 10
    };

    // Pass 2: bounded daemon on the same corpus.
    let sock = ws.path().join("evict.sock");
    let daemon = Daemon::start(DaemonConfig {
        max_cache_bytes: bound,
        ..config(ws.path(), &sock)
    })
    .expect("bounded daemon");
    let scan = scan_via(&sock, "(kind function_item)", None);
    assert_eq!(scan["ok"], true);

    let stats = cache_stats(&sock);
    assert!(
        stats["bytes"].as_u64().unwrap_or(u64::MAX) <= bound,
        "accounted bytes respect the bound: {stats} (bound {bound})"
    );
    assert!(
        stats["entries"].as_u64().unwrap_or(0) < 4,
        "the bound evicted at least one entry: {stats}"
    );
    assert!(
        stats["evictions"].as_u64().unwrap_or(0) >= 1,
        "the eviction is counted: {stats}"
    );

    // A later scan re-parses what was evicted (misses) and still answers
    // correctly — the report is complete either way.
    let rescanned = scan_via(&sock, "(kind function_item)", None);
    assert_eq!(rescanned["ok"], true);
    let after = cache_stats(&sock);
    assert!(
        after["misses"].as_u64().unwrap_or(0) > stats["misses"].as_u64().unwrap_or(0),
        "evicted paths re-parse on demand: {after}"
    );
    assert_eq!(
        without_wall(&scan["data"].clone()),
        without_wall(&rescanned["data"].clone())
    );

    stop(daemon);
}

// --- 4. concurrency --------------------------------------------------------

/// The mandated concurrency fence: parallel scans + pings + a file writer
/// (driving watcher re-parses — the apply-equivalent mutation, ADR-0018
/// D5) under a bounded wait: every response is one intact envelope, no
/// deadlock, and the daemon still serves afterwards.
#[test]
fn parallel_scans_pings_and_mutations_never_tear() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 12, 12);
    let sock = ws.path().join("conc.sock");
    let daemon = start(ws.path(), &sock);

    let stop_flag = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();

    // The mutation driver: keep editing one file so the watcher's
    // take-edit-reparse choreography races the scans continuously.
    {
        let target = ws.path().join("src/m000.rs");
        let stop_flag = Arc::clone(&stop_flag);
        workers.push(std::thread::spawn(move || {
            let mut round = 0_u32;
            while !stop_flag.load(Ordering::Relaxed) {
                round += 1;
                let source = corpus_file_source(0, 12 + (round as usize % 5));
                std::fs::write(&target, source).expect("writer edit");
                std::thread::sleep(Duration::from_millis(40));
            }
        }));
    }

    // Four scanners and two ping loops, all under a bounded wait.
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    for worker in 0..6 {
        let sock = sock.clone();
        let tx = tx.clone();
        workers.push(std::thread::spawn(move || {
            for round in 0..6 {
                let result = if worker % 3 == 2 {
                    client::request(&sock, &Request::ping(), REQUEST_TIMEOUT)
                } else {
                    client::request(
                        &sock,
                        &Request::scan("(kind function_item)", Some("src".to_owned())),
                        REQUEST_TIMEOUT,
                    )
                };
                match result {
                    Ok(response) => {
                        if response.get("schema").and_then(Value::as_str) != Some("janitor.v1") {
                            tx.send(Err(format!("torn response: {response}")))
                                .expect("report");
                        }
                        if response["ok"] != true {
                            tx.send(Err(format!("failed response: {response}")))
                                .expect("report");
                        }
                        if response.pointer("/data/stats/matches").is_none()
                            && response.pointer("/data/cache").is_none()
                        {
                            tx.send(Err(format!(
                                "response carries neither report nor stats: {response}"
                            )))
                            .expect("report");
                        }
                        tx.send(Ok(())).expect("report");
                    }
                    Err(err) => {
                        tx.send(Err(format!("request {worker}/{round} failed: {err}")))
                            .expect("report");
                    }
                }
            }
        }));
    }
    drop(tx);

    // The bounded wait (F2.8's pattern): every worker reports within the
    // deadline or the test fails — a deadlock cannot hang it.
    let mut failures = Vec::new();
    let mut reports = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    while reports + failures.len() < 36 {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(())) => reports += 1,
            Ok(Err(failure)) => failures.push(failure),
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            Err(err) => {
                failures.push(format!("bounded wait gave up: {err}"));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "concurrent responses: {failures:?}");
    assert_eq!(reports, 36, "every request answered exactly once");

    stop_flag.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.join().expect("worker exits");
    }

    // The daemon survived the storm.
    let pong = client::request(&sock, &Request::ping(), REQUEST_TIMEOUT).expect("still serving");
    assert_eq!(pong["ok"], true);

    stop(daemon);
}

// --- 5. socket hygiene -----------------------------------------------------

/// The mandated hygiene fence: 0600 socket permissions, and the workspace
/// boundary is enforced with the typed error — relative `..`, an absolute
/// foreign path, and a symlink escape are all refused.
#[test]
fn socket_permissions_and_workspace_bounds_hold() {
    let ws = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    seed_corpus(ws.path(), 2, 4);
    std::fs::write(outside.path().join("secret.rs"), "fn s() {}\n").expect("outside file");
    let sock = ws.path().join("hyg.sock");
    let daemon = start(ws.path(), &sock);

    let mode = std::fs::metadata(&sock)
        .expect("socket file exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket is owner-only (ADR-0018 D6)");

    // The ping payload pins the workspace identity.
    let pong = client::request(&sock, &Request::ping(), REQUEST_TIMEOUT).expect("ping");
    assert_eq!(
        pong.pointer("/data/workspace").and_then(Value::as_str),
        Some(ws.path().display().to_string().as_str())
    );

    for (label, path) in [
        ("relative escape", "../../../etc".to_owned()),
        (
            "absolute foreign root",
            outside.path().display().to_string(),
        ),
        (
            "absolute foreign file",
            outside.path().join("secret.rs").display().to_string(),
        ),
    ] {
        let refused = scan_via(&sock, "_", Some(&path));
        assert_eq!(refused["ok"], false, "{label} must be refused: {refused}");
        assert_eq!(
            refused.pointer("/error/code").and_then(Value::as_str),
            Some("E_DAEMON_PATH_OUTSIDE_WORKSPACE"),
            "{label}: {refused}"
        );
    }

    // A symlink inside the workspace pointing outside escapes lexically —
    // canonicalization must catch it.
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), ws.path().join("leak")).expect("symlink");
    let refused = scan_via(&sock, "_", Some("leak"));
    assert_eq!(
        refused.pointer("/error/code").and_then(Value::as_str),
        Some("E_DAEMON_PATH_OUTSIDE_WORKSPACE"),
        "symlink escape: {refused}"
    );

    // In-workspace paths (absolute and relative, subdirectory and file)
    // still serve.
    for path in [
        None,
        Some("src"),
        Some(ws.path().join("src").display().to_string().as_str()),
    ] {
        let ok = scan_via(&sock, "(kind function_item)", path);
        assert_eq!(ok["ok"], true, "in-bounds path serves: {ok}");
    }

    stop(daemon);
}

// --- 6. crash safety -------------------------------------------------------

/// The mandated crash fence: `kill -9` the real binary mid-serve — the
/// socket file is left behind, the next start probes it (dead), removes
/// it, binds, and serves; a clean shutdown afterwards unlinks it. Also
/// proves the binary's stdout contract: a janitor.v1 `listening` envelope.
#[test]
fn kill9_leaves_a_stale_socket_the_next_start_recovers() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 3, 6);
    let sock = ws.path().join("crash.sock");

    let mut child = spawn_binary_daemon(ws.path(), &sock);

    // Prove the startup envelope on stdout is a janitor.v1 envelope
    // (stdout stays 100% JSON, ADR-0018 D7). Read from a cloned pipe so
    // the child keeps its stdout for later kill semantics.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (line_tx, line_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(&mut stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() && !line.is_empty() {
            let _ = line_tx.send(line);
        }
    });
    let startup = line_rx
        .recv_timeout(WAIT_DEADLINE)
        .expect("startup envelope line");
    let envelope: Value = serde_json::from_str(startup.trim()).expect("startup line is JSON");
    assert_eq!(envelope["schema"], "janitor.v1");
    assert_eq!(envelope["data"]["listening"], true);
    assert_eq!(
        envelope["data"]["sock"],
        Value::from(sock.display().to_string())
    );

    // kill -9: no cleanup runs, the socket file survives.
    child.kill().expect("SIGKILL");
    let _ = child.wait();
    assert!(sock.exists(), "kill -9 leaves the socket file behind");

    // In-process restart on the same path: probe → stale → remove → bind.
    let daemon = Daemon::start(config(ws.path(), &sock)).expect("stale socket recovered");
    let pong = client::request(&sock, &Request::ping(), REQUEST_TIMEOUT).expect("serves again");
    assert_eq!(pong["ok"], true);
    stop(daemon);

    // And a second binary daemon binds cleanly too (no stale file left).
    let mut second = spawn_binary_daemon(ws.path(), &sock);
    second.kill().expect("SIGKILL second");
    let _ = second.wait();
    assert!(sock.exists());
    std::fs::remove_file(&sock).expect("test cleanup");
}

/// A LIVE daemon on the path refuses a second start (the probe): typed
/// `E_DAEMON_ALREADY_RUNNING`, and the first daemon keeps serving — the
/// stale-file recovery never steals a live socket (ADR-0018 D7).
#[test]
fn second_start_on_a_live_socket_is_refused() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 2, 4);
    let sock = ws.path().join("live.sock");
    let daemon = start(ws.path(), &sock);

    let refused = Daemon::start(config(ws.path(), &sock));
    let err = match refused {
        Err(err) => err,
        Ok(live) => panic!("a live daemon must refuse, not start: {:?}", live.sock()),
    };
    assert_eq!(err.code(), "E_DAEMON_ALREADY_RUNNING");

    let pong = client::request(&sock, &Request::ping(), REQUEST_TIMEOUT).expect("still serving");
    assert_eq!(pong["ok"], true);
    stop(daemon);
}

// --- protocol surface ------------------------------------------------------

/// The request-validation contract through the real socket: unknown cmd,
/// wrong schema, malformed JSON, scan-without-rule, and oversized lines
/// are all typed `E_DAEMON_*` envelopes — never silent, never a hang.
#[test]
fn malformed_requests_get_typed_errors() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 1, 2);
    let sock = ws.path().join("proto.sock");
    let daemon = start(ws.path(), &sock);

    let expect_code = |label: &str, line: &str, code: &str| {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        use std::io::Write;
        stream.write_all(line.as_bytes()).expect("write");
        let response = janitor_prime::daemon::protocol::read_line(&mut stream, 8 * 1024 * 1024)
            .expect("response line");
        let value: Value = serde_json::from_slice(&response)
            .unwrap_or_else(|err| panic!("{label}: response not JSON ({err})"));
        assert_eq!(value["ok"], false, "{label}: {value}");
        assert_eq!(
            value.pointer("/error/code").and_then(Value::as_str),
            Some(code),
            "{label}: {value}"
        );
    };

    expect_code("not json", "garbage\n", "E_DAEMON_REQUEST_MALFORMED");
    expect_code(
        "wrong schema",
        "{\"schema\":\"other\",\"cmd\":\"ping\"}\n",
        "E_DAEMON_REQUEST_MALFORMED",
    );
    expect_code(
        "unknown cmd",
        "{\"schema\":\"janitor.v1\",\"cmd\":\"nap\"}\n",
        "E_DAEMON_REQUEST_MALFORMED",
    );
    expect_code(
        "unknown field",
        "{\"schema\":\"janitor.v1\",\"cmd\":\"ping\",\"zzz\":1}\n",
        "E_DAEMON_REQUEST_MALFORMED",
    );
    expect_code(
        "scan without rule",
        "{\"schema\":\"janitor.v1\",\"cmd\":\"scan\"}\n",
        "E_DAEMON_REQUEST_MALFORMED",
    );
    expect_code("empty line", "\n", "E_DAEMON_REQUEST_MALFORMED");

    // Oversized: 2 MiB with no newline — refused without buffering it all.
    let mut oversized = vec![b'x'; 2 * 1024 * 1024];
    oversized.push(b'\n');
    let mut stream = UnixStream::connect(&sock).expect("connect");
    // Write in chunks: the server answers as soon as it sees the cap.
    for chunk in oversized.chunks(64 * 1024) {
        use std::io::Write;
        if stream.write_all(chunk).is_err() {
            break; // server closed early — fine, it already answered
        }
    }
    let response = janitor_prime::daemon::protocol::read_line(&mut stream, 8 * 1024 * 1024)
        .expect("oversize response line");
    let value: Value = serde_json::from_slice(&response).expect("oversize response JSON");
    assert_eq!(
        value.pointer("/error/code").and_then(Value::as_str),
        Some("E_DAEMON_REQUEST_TOO_LARGE"),
        "{value}"
    );

    // The daemon is unaffected by all of it.
    let pong = client::request(&sock, &Request::ping(), REQUEST_TIMEOUT).expect("still serving");
    assert_eq!(pong["ok"], true);
    stop(daemon);
}

/// The connection cap: 64 idle connections hold the handlers; the 65th is
/// refused with `E_DAEMON_BUSY` and the daemon recovers once they drop.
#[test]
fn connection_cap_answers_busy() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 1, 2);
    let sock = ws.path().join("cap.sock");
    let daemon = start(ws.path(), &sock);

    // Hold MAX_CONNECTIONS idle sockets (handlers block on read).
    let mut held: Vec<UnixStream> = Vec::new();
    for _ in 0..janitor_prime::daemon::MAX_CONNECTIONS {
        held.push(UnixStream::connect(&sock).expect("idle connect"));
    }
    // Let the accept loop catch up before the probing request.
    std::thread::sleep(Duration::from_millis(500));

    let busy = client::request(&sock, &Request::ping(), REQUEST_TIMEOUT).expect("answers busy");
    assert_eq!(busy["ok"], false, "{busy}");
    assert_eq!(
        busy.pointer("/error/code").and_then(Value::as_str),
        Some("E_DAEMON_BUSY")
    );

    drop(held);
    // The handlers notice EOF and free the slots; ping serves again.
    let recovered = wait_for(|| {
        client::request(&sock, &Request::ping(), REQUEST_TIMEOUT)
            .ok()
            .filter(|pong| pong["ok"] == true)
    });
    assert!(recovered.is_some(), "daemon recovers after the storm");
    stop(daemon);
}

/// Idle death: with a tiny idle timeout and no connections, the daemon
/// exits on its own and unlinks the socket (stale daemons die naturally).
#[test]
fn idle_daemon_dies_and_unlinks() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 1, 2);
    let sock = ws.path().join("idle.sock");
    let mut daemon = Daemon::start(DaemonConfig {
        idle: Duration::from_millis(300),
        ..config(ws.path(), &sock)
    })
    .expect("idle daemon starts");

    let gone = wait_for(|| (!sock.exists()).then_some(()));
    assert!(gone.is_some(), "idle timeout exited the daemon");
    daemon.request_shutdown(); // no-op if already exiting; joins cleanly
    daemon.wait();
    assert!(!sock.exists());
}

// --- CLI routing (`scan --daemon`) -----------------------------------------

/// Run the real binary's scan and return (exit code, stdout envelope).
fn cli_scan(args: &[&str], cwd: &Path, janitor_sock: &str) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_janitor"))
        .args(args)
        .current_dir(cwd)
        .env("JANITOR_SOCK", janitor_sock)
        .stderr(Stdio::null())
        .output()
        .expect("cli scan runs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let value: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("cli stdout is one envelope ({err}): {stdout}"));
    (output.status.code(), value)
}

/// The client contract: `--daemon` routes through a live daemon (identical
/// report to the in-process run, modulo wall time) and silently falls back
/// in-process when the socket is dead or serves another workspace.
#[test]
fn scan_daemon_flag_routes_and_falls_back() {
    let ws = tempfile::tempdir().expect("workspace");
    let other = tempfile::tempdir().expect("other workspace");
    seed_corpus(ws.path(), 6, 6);
    seed_corpus(other.path(), 3, 3);
    let sock = ws.path().join("route.sock");

    // Fallback 1: no daemon at all — the report is the normal one.
    let missing = ws.path().join("missing.sock");
    let (code, cold) = cli_scan(
        &["scan", "--rule", "(kind function_item)", "--daemon"],
        ws.path(),
        &missing.display().to_string(),
    );
    assert_eq!(code, Some(0), "fallback exits 0: {cold}");
    assert_eq!(cold["ok"], true);

    let daemon = start(ws.path(), &sock);

    // Routed: through the live daemon — same data as the cold in-process
    // run (the daemon re-parses nothing the CLI already proved; the point
    // is the report equality, not the warmth).
    let (code, routed) = cli_scan(
        &["scan", "--rule", "(kind function_item)", "--daemon"],
        ws.path(),
        &sock.display().to_string(),
    );
    assert_eq!(code, Some(0), "routed scan exits 0: {routed}");
    assert_eq!(routed["ok"], true);
    assert_eq!(
        without_wall(&routed["data"].clone()),
        without_wall(&cold["data"].clone()),
        "routed and in-process reports are identical modulo wall_ms"
    );

    // The warmth is real: a second routed scan is all hits.
    let stats_before = cache_stats(&sock);
    let (_code, second) = cli_scan(
        &["scan", "--rule", "(kind function_item)", "--daemon"],
        ws.path(),
        &sock.display().to_string(),
    );
    assert_eq!(second["ok"], true);
    let stats_after = cache_stats(&sock);
    assert!(
        stats_after["hits"].as_u64().unwrap_or(0) > stats_before["hits"].as_u64().unwrap_or(0),
        "the routed scan served from the warm cache: {stats_after}"
    );

    // Fallback 2: a live daemon for ANOTHER workspace — the scan roots in
    // `other`, so routing must decline and run in-process locally.
    let (code, foreign) = cli_scan(
        &[
            "scan",
            "--rule",
            "(kind function_item)",
            "--daemon",
            &other.path().display().to_string(),
        ],
        ws.path(),
        &sock.display().to_string(),
    );
    assert_eq!(code, Some(0), "mismatch fallback exits 0: {foreign}");
    assert_eq!(foreign["ok"], true);
    let foreign_files = foreign.pointer("/data/stats/files").and_then(Value::as_u64);
    assert_eq!(
        foreign_files,
        Some(3),
        "the fallback scanned `other` locally"
    );

    stop(daemon);
}

// --- provenance + cache telemetry (v0.3 B4) --------------------------------

/// The mandated honesty fence: a `scan --daemon` response SAYS who served
/// it — `"daemon"` with the per-request warm-cache hit/miss delta through
/// a live socket, `"process"` with `{hits: 0, misses: 0}` on the
/// dead-socket fallback (both modes) — and the `stats` op exposes the
/// counters: nonzero `cache.hits` after a routed scan, no new misses on
/// the warm re-scan.
#[test]
fn scan_daemon_responses_say_who_served_them() {
    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 4, 6);
    let files = 4_u64;
    let sock = ws.path().join("prov.sock");

    // Dead socket, rule mode: the fallback is no longer silent.
    let dead = ws.path().join("dead.sock");
    let (code, cold) = cli_scan(
        &["scan", "--rule", "(kind function_item)", "--daemon"],
        ws.path(),
        &dead.display().to_string(),
    );
    assert_eq!(code, Some(0), "fallback exits 0: {cold}");
    assert_eq!(
        cold.pointer("/data/served_by").and_then(Value::as_str),
        Some("process"),
        "the in-process fallback names itself: {cold}"
    );
    assert_eq!(
        cold.pointer("/data/stats/cache"),
        Some(&serde_json::json!({"hits": 0, "misses": 0})),
        "no warm cache was consulted: {cold}"
    );

    // Dead socket, duplicates mode: same provenance.
    let (code, dup) = cli_scan(
        &["scan", "--duplicates", "--daemon"],
        ws.path(),
        &dead.display().to_string(),
    );
    assert_eq!(code, Some(0));
    assert_eq!(
        dup.pointer("/data/served_by").and_then(Value::as_str),
        Some("process")
    );
    assert_eq!(
        dup.pointer("/data/stats/cache"),
        Some(&serde_json::json!({"hits": 0, "misses": 0}))
    );

    // Live socket: routed responses name the daemon and carry the
    // per-request delta — cold (all misses), then all hits on the re-scan.
    let daemon = start(ws.path(), &sock);
    let (code, routed_cold) = cli_scan(
        &["scan", "--rule", "(kind function_item)", "--daemon"],
        ws.path(),
        &sock.display().to_string(),
    );
    assert_eq!(code, Some(0));
    assert_eq!(
        routed_cold
            .pointer("/data/served_by")
            .and_then(Value::as_str),
        Some("daemon")
    );
    assert_eq!(
        routed_cold
            .pointer("/data/stats/cache/misses")
            .and_then(Value::as_u64),
        Some(files),
        "the cold routed pass re-parsed every file: {routed_cold}"
    );
    assert_eq!(
        routed_cold
            .pointer("/data/stats/cache/hits")
            .and_then(Value::as_u64),
        Some(0)
    );

    let (code, routed_warm) = cli_scan(
        &["scan", "--rule", "(kind function_item)", "--daemon"],
        ws.path(),
        &sock.display().to_string(),
    );
    assert_eq!(code, Some(0));
    assert_eq!(
        routed_warm
            .pointer("/data/served_by")
            .and_then(Value::as_str),
        Some("daemon")
    );
    assert_eq!(
        routed_warm
            .pointer("/data/stats/cache/misses")
            .and_then(Value::as_u64),
        Some(0),
        "the warm re-scan re-parsed nothing: {routed_warm}"
    );
    assert!(
        routed_warm
            .pointer("/data/stats/cache/hits")
            .and_then(Value::as_u64)
            .is_some_and(|hits| hits >= files),
        "the warm re-scan hit every file: {routed_warm}"
    );

    // The `stats` op exposes the accumulated counters (the deltas above
    // are its increments): hits nonzero after the routed scans, misses
    // still just the cold pass's.
    let stats = client::request(&sock, &Request::stats(), REQUEST_TIMEOUT)
        .expect("stats answers")
        .pointer("/data/cache")
        .cloned()
        .expect("stats carries cache");
    assert!(
        stats["hits"].as_u64().unwrap_or(0) >= files,
        "nonzero hits after a routed scan: {stats}"
    );
    assert_eq!(
        stats["misses"].as_u64(),
        Some(files),
        "the warm scan added no misses: {stats}"
    );

    stop(daemon);
}

// --- issue #21: large/torn responses over the socket -----------------------

/// Issue #21's e2e fence (the reporter's exact scenario — a response line
/// over 8192 bytes, macOS's AF_UNIX `SO_SNDBUF`): the daemon serves the
/// WHOLE line. On Linux the ~208 KiB send buffer hides the truncation this
/// exercised on macOS, so the `WouldBlock`-retry behavior itself is pinned
/// by the unit fence in `protocol::tests`; this test pins the end-to-end
/// contract that retry protects — a big envelope crosses the socket
/// intact, complete, and parseable, with every match accounted for.
#[test]
fn responses_over_8192_bytes_arrive_complete() {
    let ws = tempfile::tempdir().expect("workspace");
    // 720 functions: a response line well past 8192 bytes (the reporter's
    // bisected boundary — 8041 served fine, 8192+ truncated).
    seed_corpus(ws.path(), 60, 12);
    let sock = ws.path().join("big.sock");
    let daemon = start(ws.path(), &sock);

    // Raw socket view (bypassing the CLI's fallback): measure the exact
    // line the wire carries, the way the issue's reproduction did.
    let mut stream = UnixStream::connect(&sock).expect("connect");
    use std::io::Write;
    let request = Request::scan("(kind function_item)", None);
    let mut line = request.to_line().expect("request serializes");
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
        .expect("request sent");

    let response = janitor_prime::daemon::protocol::read_line(
        &mut stream,
        janitor_prime::daemon::protocol::MAX_RESPONSE_BYTES,
    )
    .expect("the response line arrives");
    assert!(
        response.len() > 8192,
        "the scenario needs a response past macOS's SO_SNDBUF, got {} bytes",
        response.len()
    );
    let value: Value =
        serde_json::from_slice(&response).expect("the whole line is one intact JSON envelope");
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(
        value.pointer("/data/served_by").and_then(Value::as_str),
        Some("daemon"),
        "the warm path served it: {value}"
    );
    assert_eq!(
        value.pointer("/data/stats/matches").and_then(Value::as_u64),
        Some(60 * 12),
        "every match made it across: {} bytes",
        response.len()
    );

    stop(daemon);
}

/// Issue #21's visibility fence: a daemon that answers `ping` for this
/// workspace but delivers a torn frame (truncated JSON, EOF mid-line) must
/// make `scan --daemon` fall back LOUDLY — one stderr warning BEFORE the
/// in-process scan serves, stdout still exactly one clean envelope naming
/// `served_by: "process"`. The silent fallback stays reserved for "no
/// daemon at all", which the contrast leg asserts.
#[test]
fn torn_daemon_frame_falls_back_with_a_stderr_warning() {
    use std::io::Write;

    let ws = tempfile::tempdir().expect("workspace");
    seed_corpus(ws.path(), 2, 2);
    let ws_canonical = std::fs::canonicalize(ws.path()).expect("canonical workspace");
    let sock = ws.path().join("torn.sock");

    // A fake daemon: an honest `ping` for this workspace, then a scan
    // response cut off mid-frame. Non-blocking accept + bounded wait so a
    // client-side failure fails the test instead of hanging the fake.
    let listener = UnixListener::bind(&sock).expect("fake daemon binds");
    listener
        .set_nonblocking(true)
        .expect("fake daemon polls accepts");
    let fake = std::thread::spawn(move || {
        let mut served = 0_u32;
        let deadline = Instant::now() + WAIT_DEADLINE;
        while served < 2 && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(err) => panic!("fake accept: {err}"),
            };
            let request = janitor_prime::daemon::protocol::read_line(&mut stream, 1024 * 1024)
                .expect("request line");
            let cmd = serde_json::from_slice::<Value>(&request)
                .expect("request is json")
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            assert_eq!(
                cmd,
                if served == 0 { "ping" } else { "scan" },
                "routing probes ping first, then the scan"
            );
            served += 1;
            if cmd == "ping" {
                let pong = serde_json::json!({
                    "schema": "janitor.v1",
                    "ok": true,
                    "data": { "workspace": ws_canonical.display().to_string() }
                });
                let mut pong_line = serde_json::to_string(&pong).expect("pong serializes");
                pong_line.push('\n');
                stream
                    .write_all(pong_line.as_bytes())
                    .and_then(|()| stream.flush())
                    .expect("pong sent");
            } else {
                // The truncation under test: half an envelope, then EOF.
                stream
                    .write_all(br#"{"schema":"janitor.v1","ok":true,"data":{"report":"trunc"#)
                    .and_then(|()| stream.flush())
                    .expect("torn frame sent");
                drop(stream);
            }
        }
    });

    let output = Command::new(env!("CARGO_BIN_EXE_janitor"))
        .args(["scan", "--rule", "(kind function_item)", "--daemon"])
        .current_dir(ws.path())
        .env("JANITOR_SOCK", sock.display().to_string())
        .output()
        .expect("cli scan runs");
    fake.join().expect("fake daemon exits");

    assert_eq!(output.status.code(), Some(0), "the fallback still succeeds");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("falling back to in-process scan"),
        "the torn frame must be visible on stderr, got: {stderr:?}"
    );
    assert!(
        stderr.contains("invalid/torn"),
        "the warning names the condition, got: {stderr:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = stdout.trim().parse().expect("stdout is one envelope");
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(
        value.pointer("/data/served_by").and_then(Value::as_str),
        Some("process"),
        "the in-process path served the fallback: {value}"
    );

    // The contrast leg: no daemon at all stays SILENT — the designed
    // fallback (ADR-0018 D7), unchanged by this fix.
    let silent = Command::new(env!("CARGO_BIN_EXE_janitor"))
        .args(["scan", "--rule", "(kind function_item)", "--daemon"])
        .current_dir(ws.path())
        .env(
            "JANITOR_SOCK",
            ws.path().join("missing.sock").display().to_string(),
        )
        .output()
        .expect("cli scan runs");
    assert_eq!(silent.status.code(), Some(0), "silent fallback exits 0");
    let silent_err = String::from_utf8_lossy(&silent.stderr);
    assert!(
        !silent_err.contains("falling back"),
        "no daemon = the documented silent fallback, got: {silent_err:?}"
    );
}
