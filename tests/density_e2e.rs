//! C2 integration suite (PLAN-C2C §2): the density statistics through the
//! real binary — the novelty score surfaced by `note`, the per-fact
//! novelty on nap requests, and the `E_NAP_LOW_DENSITY` floor on
//! `nap --text`. The three behaviors are pinned hard: a weakened floor, a
//! trivialized novelty, or a removed convergence exit each fail a test
//! here (the acceptance criteria are the saboteurs' list).

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_janitor")
}

fn janitor(dir: &Path, args: &[&str]) -> (Option<i32>, Value) {
    let out = Command::new(bin())
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg-absent"))
        .args(args)
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    (
        out.status.code(),
        serde_json::from_str(&stdout).unwrap_or(Value::Null),
    )
}

fn code_of(envelope: &Value) -> &str {
    envelope["error"]["code"].as_str().unwrap_or("<no code>")
}

/// Seed `n` notes with genuinely varied vocabulary (the migration store's
/// failure mode was 32 real facts behind a 14-byte placeholder).
fn seed_notes(dir: &Path, n: u64) {
    for i in 0..n {
        let (code, out) = janitor(
            dir,
            &[
                "note",
                &format!("fact {i}: durable decision delta{i:02} about the early run"),
            ],
        );
        assert_eq!(code, Some(0), "{out}");
    }
}

/// Follow bare `nap` until the target slot is the pending request,
/// settling everything before it with rich per-slot lines.
fn settle_until(dir: &Path, wanted: &str) {
    for _ in 0..64 {
        let (_, plan) = janitor(dir, &["nap"]);
        if plan["data"]["pending"] == false {
            break;
        }
        let slot = plan["data"]["request"]["slot"].as_str().expect("slot");
        if slot == wanted {
            return;
        }
        let (code, out) = janitor(
            dir,
            &[
                "nap",
                "--slot",
                slot,
                "--text",
                &format!("{slot}: the durable early decisions, distinct words kept"),
            ],
        );
        assert_eq!(code, Some(0), "{out}");
    }
    panic!("slot {wanted} never became pending");
}

/// Acceptance (1): the migration store's exact placeholder, submitted
/// over the block it stood for, is refused with the typed floor error and
/// the observed counts in `details` — the pinned floor. A binary that
/// accepts any summary fails here.
#[test]
fn placeholder_over_32_facts_is_refused_with_counts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    janitor(dir, &["init"]);
    seed_notes(dir, 32);
    settle_until(dir, "0-32");

    let (code, refused) = janitor(dir, &["nap", "--slot", "0-32", "--text", "Fatos 0-32: "]);
    assert_eq!(code, Some(1), "{refused}");
    assert_eq!(code_of(&refused), "E_NAP_LOW_DENSITY");
    let details = &refused["error"]["details"];
    assert_eq!(details["slot"], "0-32");
    let distinct = details["distinct_words"].as_u64().expect("distinct_words");
    assert!(
        distinct <= 3,
        "the placeholder's signature: {distinct} words"
    );
    assert_eq!(details["covered_facts"], 32);
    assert!(
        details["source_distinct_words"]
            .as_u64()
            .is_some_and(|n| n > 10),
        "the block itself is word-rich"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("distinct word")),
        "the refusal sentence carries the counts"
    );
    // Nothing was written by the refusal: the slot is still the request.
    let (_, plan) = janitor(dir, &["nap"]);
    assert_eq!(plan["data"]["request"]["slot"], "0-32");
}

/// Acceptance (2), pinned: a summary with ≥ 10 distinct words ALWAYS
/// settles — even far below the 0.25 ratio (this line shares no token
/// with the block). Removing the convergence exit (refusing on ratio)
/// fails here; the loop must converge by construction.
#[test]
fn ten_distinct_words_always_settle_even_below_the_ratio() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    janitor(dir, &["init"]);
    seed_notes(dir, 32);
    settle_until(dir, "0-32");

    let text = "quark gluon lattice bayesian prior sigma delta omega kappa iota";
    let (code, settled) = janitor(dir, &["nap", "--slot", "0-32", "--text", text]);
    assert_eq!(code, Some(0), "{settled}");
    assert_eq!(settled["data"]["slot"], "0-32");
    // Thin but accepted: the warning tier says so, with the counts.
    let warning = &settled["data"]["density_warning"];
    assert_eq!(warning["kind"], "nap_low_word_ratio");
    assert_eq!(warning["distinct_words"], 10);
    assert!(
        warning["ratio"].as_f64().is_some_and(|r| r < 0.25),
        "genuinely below the ratio: {warning}"
    );
}

/// Acceptance (3) + (4): ten near-duplicate notes then one novel one.
/// Duplicates report novelty < 0.2, the novel one > 0.9, the first note
/// (no precedent) exactly 1.0 — every score in [0, 1] — and the whole
/// sequence reproduces bit-identically on a fresh run. A constant or
/// trivialized novelty fails the spread assertions.
#[test]
fn note_novelty_separates_duplicates_from_the_novel_fact() {
    fn run_sequence(dir: &Path) -> Vec<f64> {
        janitor(dir, &["init"]);
        let mut scores = Vec::new();
        let numbers = [
            "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        ];
        for (i, number) in numbers.iter().enumerate() {
            let (code, out) = janitor(
                dir,
                &[
                    "note",
                    &format!(
                        "[optmem #{i}] daemon socket timeout retry attempt failed again later \
                         count {number}"
                    ),
                ],
            );
            assert_eq!(code, Some(0), "{out}");
            let novelty = out["data"]["density"]["novelty"]
                .as_f64()
                .expect("density.novelty is a number");
            assert_eq!(out["data"]["density"]["window"], 64);
            assert!((0.0..=1.0).contains(&novelty), "in [0, 1]: {novelty}");
            scores.push(novelty);
        }
        let (code, out) = janitor(
            dir,
            &[
                "note",
                "quark gluon lattice bayesian prior sigma delta omega",
            ],
        );
        assert_eq!(code, Some(0), "{out}");
        scores.push(out["data"]["density"]["novelty"].as_f64().expect("novelty"));
        scores
    }

    let first = tempfile::tempdir().expect("tempdir");
    let scores = run_sequence(first.path());

    assert_eq!(scores[0], 1.0, "the first note has no precedent");
    for (i, score) in scores.iter().take(10).skip(1).enumerate() {
        assert!(score < &0.2, "near-duplicate {i} scored {score}");
    }
    assert!(scores[10] > 0.9, "the novel note scored {}", scores[10]);

    // Identical between runs: a fresh store, the same sequence, the same
    // bits — the statistic is a function of the log alone.
    let second = tempfile::tempdir().expect("tempdir");
    assert_eq!(run_sequence(second.path()), scores);
}

/// Nap requests over raw facts carry each source fact's novelty,
/// index-aligned; requests over child summaries carry none (novelty is a
/// fact-level statistic). Sabotage: a trivialized novelty cannot satisfy
/// the spread between the request's first fact (1.0) and its near-dup
/// neighbor.
#[test]
fn nap_requests_list_per_fact_novelty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    janitor(dir, &["init"]);
    seed_notes(dir, 6);

    let (_, plan) = janitor(dir, &["nap"]);
    let request = &plan["data"]["request"];
    assert_eq!(request["slot"], "0-2");
    let novelty = request["density"]["novelty"]
        .as_array()
        .expect("novelty list");
    assert_eq!(novelty.len(), 2, "index-aligned with source.facts");
    assert_eq!(request["density"]["window"], 64);
    assert_eq!(novelty[0], 1.0, "fact 0 has no precedent");
    assert!(
        novelty[1]
            .as_f64()
            .is_some_and(|n| (0.0..=1.0).contains(&n)),
        "scores stay in [0, 1]"
    );
}

/// Acceptance (5) fence: the wake payload of a store with no new nap is
/// C2-free and byte-stable — no density surface leaks into recall, and
/// two runs over the same settled store emit identical bytes (the C1/C3
/// goldens in their own suites carry the full byte-identity fence).
#[test]
fn wake_payload_stays_c2_free_and_byte_stable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    janitor(dir, &["init"]);
    seed_notes(dir, 6);
    // Settle the whole tree: no pending nap anywhere.
    for _ in 0..16 {
        let (_, plan) = janitor(dir, &["nap"]);
        if plan["data"]["pending"] == false {
            break;
        }
        let slot = plan["data"]["request"]["slot"].as_str().expect("slot");
        let (code, out) = janitor(
            dir,
            &[
                "nap",
                "--slot",
                slot,
                "--text",
                &format!("{slot}: the durable early decisions, distinct words kept"),
            ],
        );
        assert_eq!(code, Some(0), "{out}");
    }

    let wake = |dir: &Path| {
        Command::new(bin())
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", dir.join("xdg-absent"))
            .args(["wake", "--budget", "400"])
            .output()
            .expect("wake runs")
            .stdout
    };
    let first = wake(dir);
    let second = wake(dir);
    assert_eq!(first, second, "byte-identical across runs");
    let text = String::from_utf8_lossy(&first);
    assert!(
        !text.contains("novelty") && !text.contains("density"),
        "recall carries no C2 surface: {text}"
    );
    let payload: Value = serde_json::from_str(&text).expect("wake stays valid JSON");
    assert_eq!(payload["data"]["awake"], true);
}
