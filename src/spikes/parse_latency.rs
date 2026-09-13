//! KP-10 (PLAN §5 KP-10, ADR-0007, risk R15): what does a **cold-parse**
//! pass over a LARGE real repository actually cost in wall time and RAM?
//! ADR-0007 accepts cold re-parse per CLI invocation as the v0.1 wart;
//! this spike converts that accepted cost into a measured number.
//!
//! NO DSL, NO scan semantics — parse throughput only. The run shallow-
//! clones rust-analyzer (`git clone --depth 1`) into a temp dir **inside
//! the run**, walks every `.rs` file, parses each through the ffi_shim
//! boundary with one parser, and measures: total wall time (read+parse
//! loop), files/sec, MB/s, peak RSS (`VmHWM` from `/proc/self/status` —
//! the streaming discipline of ADR-0007 means RSS must stay flat, not
//! O(repo)), and the five slowest files. The temp clone is removed at the
//! end — on the error paths too (Drop guard).
//!
//! Linux-only (`/proc`), network-required (the clone) — hence `#[ignore]`.

use super::SpikeError;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The corpus repo: large, real, actively maintained Rust.
const REPO_URL: &str = "https://github.com/rust-lang/rust-analyzer";
/// Directories never worth parsing.
const SKIP_DIRS: &[&str] = &[".git", "target"];

#[derive(Serialize)]
pub struct MachineContext {
    cpu_model: String,
    cores: usize,
    mem_total_kib: u64,
}

#[derive(Clone, Serialize)]
pub struct FileTiming {
    path: String,
    bytes: usize,
    micros: u64,
}

#[derive(Serialize)]
pub struct Report {
    repo_url: String,
    machine: MachineContext,
    clone_secs: f64,
    files: usize,
    bytes_total: usize,
    wall_secs: f64,
    files_per_sec: f64,
    mb_per_sec: f64,
    files_with_error_nodes: usize,
    peak_rss_kib: u64,
    slowest: Vec<FileTiming>,
}

/// RAII temp dir: removed on success via [`CloneGuard::clean_up`], and
/// best-effort on any early error via `Drop`.
struct CloneGuard(PathBuf);

impl CloneGuard {
    fn new() -> Result<Self, SpikeError> {
        let path = std::env::temp_dir().join(format!("janitor-kp10-{}", std::process::id()));
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn clean_up(&self) -> Result<(), SpikeError> {
        std::fs::remove_dir_all(self.path()).map_err(|err| SpikeError::ParserSetup {
            message: format!("cleanup {}: {err}", self.path().display()),
        })
    }
}

impl Drop for CloneGuard {
    fn drop(&mut self) {
        if self.0.exists() {
            // Best effort only: the error paths already surfaced their
            // real failure; a stale temp dir is noise, not a panic.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn io_err(context: String, err: std::io::Error) -> SpikeError {
    SpikeError::ParserSetup {
        message: format!("{context}: {err}"),
    }
}

fn machine_context() -> Result<MachineContext, SpikeError> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo")
        .map_err(|err| io_err("read /proc/cpuinfo".into(), err))?;
    let cpu_model = cpuinfo
        .lines()
        .find_map(|line| {
            line.strip_prefix("model name")
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, model)| model.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|err| io_err("read /proc/meminfo".into(), err))?;
    let mem_total_kib = meminfo
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemTotal:")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|kib| kib.parse::<u64>().ok())
        })
        .unwrap_or(0);
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    Ok(MachineContext {
        cpu_model,
        cores,
        mem_total_kib,
    })
}

/// Peak resident set size in KiB (`VmHWM` — the high-water mark, i.e. what
/// the streaming discipline must keep flat as the corpus grows).
fn peak_rss_kib() -> Result<u64, SpikeError> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|err| io_err("read /proc/self/status".into(), err))?;
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|kib| kib.parse::<u64>().ok())
        })
        .ok_or_else(|| SpikeError::ParserSetup {
            message: "VmHWM missing from /proc/self/status".into(),
        })
}

fn clone_shallow(url: &str, dir: &Path) -> Result<(), SpikeError> {
    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(dir)
        .output()
        .map_err(|err| io_err(format!("git clone {url}"), err))?;
    if !output.status.success() {
        return Err(SpikeError::ParserSetup {
            message: format!(
                "git clone failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), SpikeError> {
    let entries =
        std::fs::read_dir(dir).map_err(|err| io_err(format!("read_dir {}", dir.display()), err))?;
    for entry in entries {
        let entry = entry.map_err(|err| io_err(format!("read_dir {}", dir.display()), err))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_rs_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Run the KP-10 experiment: clone, walk, cold-parse everything, measure,
/// clean up.
pub fn run() -> Result<Report, SpikeError> {
    let machine = machine_context()?;
    let clone_dir = CloneGuard::new()?;

    let clone_started = Instant::now();
    clone_shallow(REPO_URL, clone_dir.path())?;
    let clone_secs = clone_started.elapsed().as_secs_f64();

    let mut files = Vec::new();
    collect_rs_files(clone_dir.path(), &mut files)?;
    files.sort();

    let mut parser = super::rust_parser()?;
    let mut timings = Vec::with_capacity(files.len());
    let mut bytes_total = 0_usize;
    let mut files_with_error_nodes = 0_usize;

    let started = Instant::now();
    for path in &files {
        let bytes =
            std::fs::read(path).map_err(|err| io_err(format!("read {}", path.display()), err))?;
        bytes_total += bytes.len();

        let parse_started = Instant::now();
        let Some(tree) = crate::ast::ffi_shim::parse_bytes(&mut parser, &bytes, None) else {
            return Err(SpikeError::NoTree {
                what: path.display().to_string(),
            });
        };
        let micros = parse_started.elapsed().as_micros() as u64;
        if tree.root_node().has_error() {
            files_with_error_nodes += 1;
        }
        // Streaming discipline (ADR-0007): the tree dies with the file's
        // iteration — RSS must stay flat, and VmHWM below proves it.
        drop(tree);
        timings.push(FileTiming {
            path: path.display().to_string(),
            bytes: bytes.len(),
            micros,
        });
    }
    let wall_secs = started.elapsed().as_secs_f64();
    let peak_rss_kib = peak_rss_kib()?;

    // The slowest five files, parse-time only (informational).
    timings.sort_by(|a, b| b.micros.cmp(&a.micros).then(a.path.cmp(&b.path)));
    let slowest = timings.iter().take(5).cloned().collect();
    let files = timings.len();

    clone_dir.clean_up()?;

    Ok(Report {
        repo_url: REPO_URL.into(),
        machine,
        clone_secs,
        files,
        bytes_total,
        wall_secs,
        files_per_sec: if wall_secs > 0.0 {
            files as f64 / wall_secs
        } else {
            0.0
        },
        mb_per_sec: if wall_secs > 0.0 {
            bytes_total as f64 / (1024.0 * 1024.0) / wall_secs
        } else {
            0.0
        },
        files_with_error_nodes,
        peak_rss_kib,
        slowest,
    })
}

/// KP-10 entry point. Heavy + network: shallow-clones rust-analyzer inside
/// the run. Run with:
///
/// ```text
/// cargo test --release --features spikes -- --ignored latency
/// ```
#[test]
#[ignore = "network + minutes; shallow-clones rust-analyzer into /tmp"]
fn cold_parse_latency_on_real_repo() {
    let tmp = std::env::temp_dir().join(format!("janitor-kp10-{}", std::process::id()));
    let report = run().expect("clone + walk + cold-parse completes");

    // A LARGE real repo, by construction: the shallow rust-analyzer clone
    // carries ~1.5k .rs files / ~17 MB of Rust (verified 2026-09-13).
    assert!(report.files > 1000, "expected thousands of files");
    assert!(
        report.bytes_total > 10 * 1024 * 1024,
        "expected a >10 MB corpus"
    );
    assert!(
        !tmp.exists(),
        "the /tmp clone must be cleaned up inside the run"
    );

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/spike-kp10.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&report).expect("report serializes"),
    )
    .expect("write benches/spike-kp10.json");

    println!("wrote {}", path.display());
    println!(
        "repo {} on {} ({} cores)",
        report.repo_url, report.machine.cpu_model, report.machine.cores
    );
    println!(
        "{} files / {:.1} MB in {:.2}s -> {:.0} files/s, {:.2} MB/s (clone {:.1}s excluded)",
        report.files,
        report.bytes_total as f64 / (1024.0 * 1024.0),
        report.wall_secs,
        report.files_per_sec,
        report.mb_per_sec,
        report.clone_secs
    );
    println!(
        "peak RSS {:.1} MiB (streaming discipline: flat vs corpus); {} files with ERROR/MISSING nodes",
        report.peak_rss_kib as f64 / 1024.0,
        report.files_with_error_nodes
    );
    for file in &report.slowest {
        println!(
            "  slowest: {:>8.1} ms  {:>7} B  {}",
            file.micros as f64 / 1000.0,
            file.bytes,
            file.path
        );
    }
}
