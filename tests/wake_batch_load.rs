//! C3 acceptance (PLAN-C2C §2, item C3): the seeded synthetic settled-store
//! generator and the wake read-path wall-time targets.
//!
//! The generator is deterministic and seeded (xorshift64*, std-only): the
//! same seed and fact count always produce byte-identical `LOG.txt` and
//! `TREE/*` files, so the wall-time numbers and the golden payload
//! fixtures (`tests/fixtures/wake/`) are reproducible. Stores
//! are written directly — LOG lines plus every level's fixed-width records
//! in rank order (the dense prefix complete) — because settling 20k facts
//! through the nap protocol would be the measurement, not the fixture.
//! This file is a `tests/` helper on purpose: `benches/` is frozen F1
//! evidence and stays untouched.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use janitor_prime::memory::log::FactRecord;
use janitor_prime::memory::tree_store::{RECORD_WIDTH, TreeStore};
use janitor_prime::memory::wake::{self, WakeReport};

/// The lane-fixed seed: every store this file builds (any size) draws its
/// texts from the xorshift64* stream seeded with `SEED ^ n`.
const SEED: u64 = 0xC3;

/// The budgets the goldens and timings pin: 300 exercises the tight
/// merge/drop/truncate shaping, 8192 the default-budget layout.
const GOLDEN_BUDGETS: [u64; 2] = [300, 8192];

/// xorshift64* (Marsaglia) — deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1)) // the zero state is a fixed point
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A pick below `n` (modulo bias is irrelevant: this shapes text, not
    /// probabilities anyone asserts on).
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const VOCAB: &[&str] = &[
    "overlay", "txn", "flush", "atomic", "rename", "fsync", "budget", "tiling", "summary", "lock",
    "span", "anchor", "drift", "gate", "nap", "wake", "log", "tree", "cache", "janitor", "daemon",
    "socket", "probe", "token", "bytes", "level", "rank", "slot", "record",
];

fn words(rng: &mut Rng, min: usize, spread: u64) -> String {
    let picks = min + rng.below(spread) as usize;
    (0..picks)
        .map(|i| VOCAB[(rng.below(VOCAB.len() as u64) + i as u64) as usize % VOCAB.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

/// The deterministic fact corpus: lock-free one-liners of varied length
/// (~60–200 bytes, the real store's spread — PLAN-C2C §5) so the budget
/// math sees a corpus that actually needs compression at every size.
fn synth_facts(n: u64) -> Vec<FactRecord> {
    let mut rng = Rng::new(SEED ^ n);
    (0..n)
        .map(|id| FactRecord {
            v: 1,
            id,
            ts: "2026-09-18T00:00:00Z".to_owned(),
            ptr: None,
            text: format!("fact {id}: {}", words(&mut rng, 8, 20)),
            flags: Vec::new(),
        })
        .collect()
}

/// The deterministic agent line for one block (≤ 256 bytes, single line —
/// the discipline the store enforces).
fn synth_summary(rng: &mut Rng, lo: u64, hi: u64) -> String {
    format!("block {lo}-{hi}: {}", words(rng, 6, 14))
}

/// Write one fully-settled synthetic store under `<dir>/.janitor`: LOG.txt
/// plus one TREE file per layout level, records in rank order (lock-free
/// facts ⇒ slots are the aligned multiples, ascending). Returns the facts
/// and the store handle. Same `(SEED, n)` ⇒ same bytes on disk.
fn settled_store(dir: &Path, n: u64) -> (Vec<FactRecord>, TreeStore) {
    let facts = synth_facts(n);
    let root = dir.join(".janitor");
    std::fs::create_dir_all(root.join("TREE")).expect("TREE dir");

    let mut log = String::new();
    for fact in &facts {
        log.push_str(&serde_json::to_string(fact).expect("fact json"));
        log.push('\n');
    }
    std::fs::write(root.join("LOG.txt"), log).expect("write LOG.txt");

    let mut rng = Rng::new(SEED ^ n);
    let mut size = 2u64;
    while size <= n {
        let mut file = Vec::new();
        let mut lo = 0u64;
        while lo + size <= n {
            let text = synth_summary(&mut rng, lo, lo + size);
            let mut record = [0u8; RECORD_WIDTH];
            record[..text.len()].copy_from_slice(text.as_bytes());
            file.extend_from_slice(&record);
            lo += size;
        }
        std::fs::write(root.join("TREE").join(size.to_string()), file).expect("write level file");
        size *= 2;
    }
    (facts, TreeStore::new(&root))
}

/// One `janitor wake` through the real binary (env-isolated like
/// memory_e2e so the global layer stays the embedded one).
fn janitor_wake(dir: &Path, budget: u64) -> (Option<i32>, serde_json::Value, std::time::Duration) {
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_janitor"))
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg-absent"))
        .args(["wake", "--budget", &budget.to_string()])
        .output()
        .expect("binary runs");
    let elapsed = started.elapsed();
    (
        out.status.code(),
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null),
        elapsed,
    )
}

/// The wake core over the generated store, in-process (what C3 owns: no
/// binary startup, no environment probes ride on this clock). `task`
/// switches to the C1 ranked pass; `None` is the positional pass.
fn lib_wake(dir: &Path, n: u64, budget: u64, task: Option<&str>) -> WakeReport {
    let (facts, store) = settled_store(dir, n);
    wake::wake(&facts, &store, task, budget, Vec::new()).expect("settled store wakes")
}

/// C3 acceptance (3): wall times on the settled synthetic generator.
/// `wake --budget 8192` through the binary at 439 / 5,000 / 20,000 facts —
/// the plan's baseline for the same generator shape was 139 ms / 977 ms /
/// 2.89 s, with 1,241 TREE opens at 5k. The 20k target (< 700 ms) is
/// asserted; all three numbers print for the lane report.
#[test]
fn settled_wake_wall_times_at_439_5k_20k() {
    for n in [439u64, 5_000, 20_000] {
        let tmp = tempfile::tempdir().expect("tempdir");
        settled_store(tmp.path(), n);
        let (code, payload, elapsed) = janitor_wake(tmp.path(), 8192);
        assert_eq!(code, Some(0), "wake at {n} facts: {payload}");
        assert_eq!(payload["data"]["awake"], true, "{n} facts");
        let items = payload["data"]["items"].as_array().expect("items");
        assert!(
            items.iter().any(|item| item["kind"] == "summary")
                && items.iter().any(|item| item["kind"] == "fact"),
            "{n} facts: a settled store at budget 8192 serves both shapes"
        );
        println!(
            "wake --budget 8192, {n:>6} facts: {:>8} ms (binary wall time)",
            elapsed.as_millis()
        );
        if n == 20_000 {
            assert!(
                elapsed.as_millis() < 700,
                "C3 target: 20k-fact wake under 700 ms, measured {} ms",
                elapsed.as_millis()
            );
        }
    }
}

/// C3 acceptance (1): the payload is byte-identical pre/post — the goldens
/// were captured from the pre-change binary over this exact generator
/// (seed 0xC3, 439 facts), and every future run must reproduce them, field
/// for field, at both the tight and the default budget. They live under
/// `tests/fixtures/wake/` (not the DSL corpus root — fixtures.rs walks
/// every top-level `*.json` as a scan fixture).
#[test]
fn wake_payload_is_byte_identical_to_the_goldens() {
    for budget in GOLDEN_BUDGETS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = lib_wake(tmp.path(), 439, budget, None);
        let golden = std::fs::read_to_string(format!(
            "{}/tests/fixtures/wake/wake_golden_439_b{budget}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("golden fixture");
        assert_eq!(
            serde_json::to_string(&report).expect("report json"),
            golden.trim(),
            "budget {budget}: the wake payload must stay byte-identical"
        );
    }
}

/// Audit helper (ignored): writes one settled store per size under
/// /tmp/c3-audit/ for `strace -e openat -c` open-counting — the unit tests
/// prove one read per level in-process; this reproduces it at the syscall
/// level on the real binary.
#[test]
#[ignore = "audit helper: materializes stores under /tmp/c3-audit for strace"]
fn write_stores_for_strace() {
    for n in [439u64, 5_000, 20_000] {
        let dir = std::path::PathBuf::from(format!("/tmp/c3-audit/store-{n}"));
        std::fs::remove_dir_all(&dir).ok();
        settled_store(&dir, n);
        println!("wrote {dir:?}");
    }
}

/// Capture helper (ignored): prints the pre/post-comparable golden payload
/// for one budget — how the committed fixtures were produced (and how they
/// can be honestly regenerated if the contract ever deliberately changes).
#[test]
#[ignore = "capture helper: prints the golden WakeReport JSON for a budget"]
fn print_golden_payloads() {
    for budget in GOLDEN_BUDGETS {
        let tmp = tempfile::tempdir().expect("tempdir");
        let report = lib_wake(tmp.path(), 439, budget, None);
        println!(
            "--- budget {budget} ---\n{}",
            serde_json::to_string(&report).expect("report json")
        );
    }
}
