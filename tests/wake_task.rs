//! C1 acceptance (PLAN-C2C §2, item C1): `wake --task` ranked recall under
//! budget, through the real binary.
//!
//! The fixture is the plan's retrieval shape: 40 facts, realistic one-line
//! lengths (raw total well over the budget), where the 10 facts carrying
//! the "daemon socket errors" cluster live in the **oldest** half — the
//! positional tiler compresses exactly that half, the ranked pass must not.
//! The store is written settled (LOG lines plus every level's
//! fixed-width records in rank order, locks excluded from compression the
//! way `BlockPlan` says), the same direct-write pattern as
//! `tests/wake_batch_load.rs`: settling through the protocol would test
//! nap, not wake.

use std::path::Path;
use std::process::Command;

use janitor_prime::memory::log::{FactRecord, MemoryFlag};
use janitor_prime::memory::tree_store::{BlockPlan, RECORD_WIDTH};

/// The cluster ids: 0, 2, …, 18 — ten facts, all in the oldest half of 40.
const CLUSTER: [u64; 10] = [0, 2, 4, 6, 8, 10, 12, 14, 16, 18];
/// A lock inside the cluster (score-positive and pinned) and one outside.
const LOCKS: [u64; 2] = [12, 35];
const QUERY: &str = "daemon socket errors";
const BUDGET: u64 = 1500;

fn fact(id: u64, text: &str, locked: bool) -> FactRecord {
    FactRecord {
        v: 1,
        id,
        ts: "2026-09-18T00:00:00Z".to_owned(),
        ptr: None,
        text: text.to_owned(),
        flags: if locked {
            vec![MemoryFlag::ArchitecturalLock]
        } else {
            Vec::new()
        },
    }
}

/// The fixture's deterministic texts: query vocabulary only in the cluster
/// facts, filler vocabulary everywhere, lengths in the real store's spread
/// so the log needs compression at budget 1500 (raw total ≈ 2.1k tokens).
fn cluster_text(id: u64) -> String {
    let filler = if id.is_multiple_of(2) {
        "the overlay txn flush atomic rename fsync budget tiling summary lock span anchor \
         drift gate keeps the invariant honest across runs"
    } else {
        "nap wake log tree cache probe token bytes level rank slot record the payload stays \
         small and the recall stays honest"
    };
    let tail = " with durable decisions worth carrying forward in the log";
    if CLUSTER.contains(&id) {
        format!("fact {id}: {QUERY} under load, backoff exhausted, retry {id}; {filler}{tail}")
    } else {
        format!("fact {id}: {filler}{tail} — run {id}")
    }
}

/// Write the settled 40-fact fixture store under `<dir>/.janitor`.
fn fixture_store(dir: &Path) {
    let facts: Vec<FactRecord> = (0..40)
        .map(|id| fact(id, &cluster_text(id), LOCKS.contains(&id)))
        .collect();
    let root = dir.join(".janitor");
    std::fs::create_dir_all(root.join("TREE")).expect("TREE dir");

    let mut log = String::new();
    for record in &facts {
        log.push_str(&serde_json::to_string(record).expect("fact json"));
        log.push('\n');
    }
    std::fs::write(root.join("LOG.txt"), log).expect("write LOG.txt");

    // Every level fully settled, records in rank order — the compressible
    // slots the log's own BlockPlan names (lock ranges are never slots).
    let plan = BlockPlan::new(&facts);
    for size in plan.levels() {
        let mut file = Vec::new();
        for lo in plan.compressible_slots(&facts, size) {
            let text = format!("block {lo}-{}: the durable decisions of the run", lo + size);
            let mut record = [0u8; RECORD_WIDTH];
            record[..text.len()].copy_from_slice(text.as_bytes());
            file.extend_from_slice(&record);
        }
        std::fs::write(root.join("TREE").join(size.to_string()), file).expect("level file");
    }
}

/// One `janitor wake` through the real binary, env-isolated so the global
/// layer stays the embedded one; returns (code, envelope, stdout bytes).
fn janitor_wake(
    dir: &Path,
    task: Option<&str>,
    budget: u64,
) -> (Option<i32>, serde_json::Value, Vec<u8>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_janitor"));
    cmd.current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg-absent"))
        .args(["wake", "--budget", &budget.to_string()]);
    if let Some(query) = task {
        cmd.args(["--task", query]);
    }
    let out = cmd.output().expect("binary runs");
    (
        out.status.code(),
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null),
        out.stdout,
    )
}

/// The raw fact ids served in a wake payload.
fn raw_ids(payload: &serde_json::Value) -> Vec<u64> {
    payload["data"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| item["kind"] == "fact")
                .filter_map(|item| item["id"].as_u64())
                .collect()
        })
        .unwrap_or_default()
}

/// C1 acceptance (1): the retrieval fixture. `--task` serves ≥ 9 of the 10
/// cluster facts raw at budget 1500; the budget-matched no-task wake serves
/// ≤ 3 (it buys summaries of the old blocks). The task block declares the
/// scorer and the counts, and is absent without the flag.
#[test]
fn ranked_recall_beats_positional_on_the_retrieval_fixture() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture_store(tmp.path());

    let (code, ranked, _) = janitor_wake(tmp.path(), Some(QUERY), BUDGET);
    assert_eq!(code, Some(0), "ranked wake: {ranked}");
    let ranked_raw = raw_ids(&ranked);
    let served = CLUSTER.iter().filter(|id| ranked_raw.contains(id)).count();
    assert!(
        served >= 9,
        "ranked served {served}/10 of the cluster: {ranked_raw:?}"
    );

    let task = &ranked["data"]["task"];
    assert_eq!(task["query"], QUERY);
    assert_eq!(task["scorer"], "tfidf-store");
    assert_eq!(task["served_scored"], served as u64, "counts match items");
    assert_eq!(
        task["dropped_scored"],
        (10 - served) as u64,
        "only cluster facts are score-positive in this fixture"
    );
    assert_eq!(ranked["data"]["truncated"], (10 - served) > 0);
    assert!(
        ranked["data"]["estimate"].as_u64().unwrap_or(u64::MAX) <= BUDGET,
        "budget respected: {}",
        ranked["data"]["estimate"]
    );

    let (code, positional, _) = janitor_wake(tmp.path(), None, BUDGET);
    assert_eq!(code, Some(0), "positional wake: {positional}");
    let positional_raw = raw_ids(&positional);
    let bought = CLUSTER
        .iter()
        .filter(|id| positional_raw.contains(id))
        .count();
    assert!(
        bought <= 3,
        "positional served {bought}/10 of the cluster: {positional_raw:?}"
    );
    assert!(
        positional["data"]["task"].is_null(),
        "append-only: no task key without --task"
    );
    assert!(
        positional["data"]["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["kind"] == "summary")),
        "the positional pass buys summaries of the old blocks"
    );
}

/// C1 acceptance (2): two `--task` runs are byte-identical — the whole
/// envelope, not just the items (scoring order, counts, layout).
#[test]
fn two_ranked_wakes_are_byte_identical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture_store(tmp.path());
    let (code_a, _, bytes_a) = janitor_wake(tmp.path(), Some(QUERY), BUDGET);
    let (code_b, _, bytes_b) = janitor_wake(tmp.path(), Some(QUERY), BUDGET);
    assert_eq!(code_a, Some(0));
    assert_eq!(code_b, Some(0));
    assert_eq!(bytes_a, bytes_b, "byte-identical envelopes");
}

/// C1 acceptance (3): every lock present exactly once, raw, under ranking
/// too — including the lock inside the cluster (score-positive and pinned)
/// and the starved-budget case where nothing else survives.
#[test]
fn locks_are_pinned_raw_exactly_once_under_ranking() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture_store(tmp.path());

    for budget in [BUDGET, 1] {
        let (code, payload, _) = janitor_wake(tmp.path(), Some(QUERY), budget);
        assert_eq!(code, Some(0), "budget {budget}: {payload}");
        let raw = raw_ids(&payload);
        for lock in LOCKS {
            assert_eq!(
                raw.iter().filter(|&&id| id == lock).count(),
                1,
                "budget {budget}: lock {lock} exactly once, raw"
            );
            let covered = payload["data"]["items"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["kind"] == "summary" && {
                        let lo = item["covers"][0].as_u64().unwrap_or(u64::MAX);
                        let hi = item["covers"][1].as_u64().unwrap_or(0);
                        (lo..hi).contains(&lock)
                    }
                })
            });
            assert!(!covered, "budget {budget}: a summary covers lock {lock}");
        }
    }

    // Starvation reports the score-positive drops honestly.
    let (_, starved, _) = janitor_wake(tmp.path(), Some(QUERY), 1);
    assert_eq!(starved["data"]["truncated"], true);
    assert_eq!(
        starved["data"]["task"]["served_scored"]
            .as_u64()
            .unwrap_or(u64::MAX)
            + starved["data"]["task"]["dropped_scored"]
                .as_u64()
                .unwrap_or(u64::MAX),
        10,
        "served + dropped == the score-positive count"
    );
}

/// A degenerate query (no store term matches) is not an error: nothing is
/// served scored, the fill covers everything it can, `awake: true`.
#[test]
fn a_query_the_store_does_not_carry_serves_nothing_scored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture_store(tmp.path());
    let (code, payload, _) = janitor_wake(tmp.path(), Some("zzz qqq nothing"), BUDGET);
    assert_eq!(code, Some(0), "{payload}");
    assert_eq!(payload["data"]["awake"], true);
    assert_eq!(payload["data"]["task"]["served_scored"], 0);
    assert_eq!(payload["data"]["task"]["dropped_scored"], 0);
    assert_eq!(payload["data"]["truncated"], false);
    // The pinned locks still ride raw.
    assert!(raw_ids(&payload).contains(&LOCKS[0]));
}
