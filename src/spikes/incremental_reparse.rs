//! KP-1 (PLAN §5 KP-1, §1.2.1, risk R3): is incremental `Tree::edit` +
//! re-parse over an overlay snapshot actually worth it versus a full
//! re-parse — per edit size — and is the edit path sound only under
//! exclusive ownership?
//!
//! Two legs:
//!
//! 1. **Measurement** (`run_measurement`, `#[ignore]`d test): a ~3 MB
//!    deterministic synthetic Rust corpus, edits at six sizes from a tiny
//!    identifier rename to a whole-block insertion. Per size, path A is a
//!    full re-parse of the new bytes; path B is the §1.2.1 choreography
//!    steps 5–6 (`Tree::edit` + `parse_with_options(old_tree)`) on an
//!    exclusively owned old tree. N ≥ 20 samples per path per size, reported
//!    as p50/p90, plus a correctness diff (root `kind_id`, root
//!    `byte_range`, `has_error`) and an informational whole-tree
//!    S-expression hash comparison. Results land in
//!    `benches/spike-kp1.json`.
//!
//! 2. **Shared-clone guard leg** (fast test): with another `Arc<Tree>`
//!    handle alive, the `strong_count == 1` guard must refuse the edit with
//!    a typed error, `Arc::make_mut` must never run, and no mutation may be
//!    observable — the aliasing rule of A.3.2 made mechanical (eval-2
//!    amendment 1).

use super::{SpikeError, Splice, input_edit_for, splice};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Target corpus size: "~3 MB synthetic Rust file" (PLAN §5 KP-1).
const TARGET_BYTES: usize = 3 * 1024 * 1024;
/// Samples per path per edit size (brief mandates N ≥ 20).
const SAMPLES: usize = 20;
/// Untimed warmup iterations per timed loop.
const WARMUP: usize = 3;

// --- corpus generation -----------------------------------------------------

/// A generated corpus plus the facts the edit anchors need.
pub struct Corpus {
    pub bytes: Vec<u8>,
    /// Number of generated units (identifiers are `unit_{i}_{alpha,beta,gamma}`).
    pub units: usize,
}

/// Deterministic synthetic Rust: varied item shapes (fns, struct, impl,
/// match, if/else, doc comments, free comments) so the tree is not a
/// uniform fan of one kind. Never compiled — parsed only.
pub fn generate_corpus(target_bytes: usize) -> Corpus {
    let mut out = String::from(
        "// KP-1 synthetic corpus - generated deterministically; parse-only.\n\
         use std::collections::HashMap;\n\
         \n\
         #[derive(Debug, Clone)]\n\
         pub struct Header {\n\
             pub generation: u32,\n\
         }\n\
         \n\
         fn header_util(map: &mut HashMap<u32, Header>) -> Option<Header> {\n\
             map.remove(&0)\n\
         }\n\
         \n\
         fn main() {\n\
             let mut map = HashMap::new();\n\
             map.insert(0, Header { generation: 1 });\n\
             let _ = header_util(&mut map);\n\
         }\n\
         \n",
    );
    let mut units = 0_usize;
    while out.len() < target_bytes {
        out.push_str(&unit(units));
        units += 1;
    }
    Corpus {
        bytes: out.into_bytes(),
        units,
    }
}

/// One ~1.2 KB unit of synthetic Rust with unique identifiers.
fn unit(i: usize) -> String {
    format!(
        r#"/// Doc: alpha for unit {i}.
fn unit_{i}_alpha(count: u32) -> u32 {{
    let base = {i} * count;
    let acc = base + count * 2;
    acc
}}

/// Doc: beta for unit {i}.
fn unit_{i}_beta(flag: bool) -> bool {{
    if flag && ({i} % 2 == 0) {{
        true
    }} else {{
        false
    }}
}}

/// Doc: data for unit {i}.
struct Unit{i}Data {{
    id: u32,
    tag: &'static str,
}}

impl Unit{i}Data {{
    const NAME: &'static str = "unit_{i}";
    fn new(id: u32) -> Self {{
        Self {{ id, tag: Self::NAME }}
    }}
    fn id(&self) -> u32 {{
        self.id
    }}
}}

// free-floating comment between items in unit {i}
fn unit_{i}_gamma(data: &Unit{i}Data) -> u32 {{
    match data.id() {{
        0 => 0,
        n if n % 3 == 0 => n * 3,
        _ => {i} + 1,
    }}
}}
// end unit {i}
"#,
        i = i
    )
}

/// A block of valid top-level items sized to ~`target_bytes`, with names
/// unique under `kp1_{tag}_`.
fn synthetic_block(tag: &str, target_bytes: usize) -> String {
    let mut out = format!("// kp1 synthetic block {tag} start\n");
    let mut j = 0_usize;
    while out.len() < target_bytes {
        out.push_str(&format!(
            "/// Doc for kp1 {tag} item {j}.\nfn kp1_{tag}_{j}(x: u32) -> u32 {{ let y = x + {j}; y * 2 }}\n"
        ));
        j += 1;
    }
    out.push_str(&format!("// kp1 synthetic block {tag} end\n"));
    out
}

// --- edit cases ------------------------------------------------------------

/// One named edit experiment: a splice applied to the pristine corpus.
pub struct EditCase {
    pub name: String,
    pub splice: Splice,
}

/// The six mandated sizes. Sizes 3–6 share one anchor (the mid unit's end
/// marker) so block size is the only variable; the tiny rename and the
/// in-body statement are their own shapes.
pub fn edit_cases(corpus: &Corpus) -> Result<Vec<EditCase>, SpikeError> {
    let old = &corpus.bytes[..];
    let mid = corpus.units / 2;
    let mut cases = Vec::with_capacity(6);

    // 1. tiny: rename one mid-file identifier (~ +8 bytes).
    let needle = format!("unit_{mid}_beta");
    let at = find(old, needle.as_bytes())?;
    cases.push(EditCase {
        name: "tiny-rename".into(),
        splice: Splice {
            start_byte: at,
            old_end_byte: at + needle.len(),
            new_text: format!("{needle}_renamed").into_bytes(),
        },
    });

    // 2. small: one let statement inside the mid unit's first fn (~30 B).
    let fn_at = find(old, format!("fn unit_{mid}_alpha").as_bytes())?;
    let body_line_end = old[fn_at..]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(old.len(), |p| fn_at + p + 1);
    cases.push(EditCase {
        name: "small-statement".into(),
        splice: Splice {
            start_byte: body_line_end,
            old_end_byte: body_line_end,
            new_text: format!("    let kp1_probe_small = {mid};\n").into_bytes(),
        },
    });

    // 3–6. same anchor, growing blocks: ~256 B, ~1 KB, ~16 KB, ~128 KB.
    let anchor = find(old, format!("// end unit {mid}\n").as_bytes())?;
    for (name, tag, size) in [
        ("medium-fn", "medium", 256_usize),
        ("large-block", "large", 1024),
        ("xlarge-block", "xlarge", 16 * 1024),
        ("whole-block", "whole", 128 * 1024),
    ] {
        cases.push(EditCase {
            name: name.into(),
            splice: Splice {
                start_byte: anchor,
                old_end_byte: anchor,
                new_text: synthetic_block(tag, size).into_bytes(),
            },
        });
    }
    Ok(cases)
}

fn find(haystack: &[u8], needle: &[u8]) -> Result<usize, SpikeError> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .ok_or_else(|| SpikeError::AnchorMissing {
            needle: String::from_utf8_lossy(needle).into_owned(),
        })
}

fn parse(
    parser: &mut tree_sitter::Parser,
    bytes: &[u8],
    old_tree: Option<&tree_sitter::Tree>,
) -> Result<tree_sitter::Tree, SpikeError> {
    crate::ast::ffi_shim::parse_bytes(parser, bytes, old_tree).ok_or_else(|| SpikeError::NoTree {
        what: format!("{}-byte source", bytes.len()),
    })
}

// --- the exclusivity guard (§1.2.1 step 5 / eval-2 amendment 1) -------------

/// Edit `tree` only when this handle is the ONLY one. `Tree::clone` is a
/// shallow refcount-incrementing alias (A.3.2), so `Arc::make_mut`'s
/// clone-on-write does **not** isolate the C tree — with any other handle
/// alive, `make_mut` would hand back a handle aliasing the same `TSTree`
/// being mutated. The guard refuses first, with a typed error; nothing
/// outside this function ever calls `Tree::edit`.
pub fn edit_exclusive(
    tree: &mut Arc<tree_sitter::Tree>,
    edit: &tree_sitter::InputEdit,
) -> Result<(), SpikeError> {
    if Arc::strong_count(tree) != 1 {
        return Err(SpikeError::SharedTree {
            count: Arc::strong_count(tree),
        });
    }
    Arc::make_mut(tree).edit(edit);
    Ok(())
}

// --- measurement ------------------------------------------------------------

#[derive(Clone, Copy, Serialize)]
pub struct Percentiles {
    min_us: u64,
    p50_us: u64,
    p90_us: u64,
    max_us: u64,
}

fn percentiles(samples: &[Duration]) -> Percentiles {
    let mut micros: Vec<u64> = samples.iter().map(|d| d.as_micros() as u64).collect();
    micros.sort_unstable();
    if micros.is_empty() {
        return Percentiles {
            min_us: 0,
            p50_us: 0,
            p90_us: 0,
            max_us: 0,
        };
    }
    let pick = |q: f64| -> u64 {
        let idx = ((micros.len() - 1) as f64 * q).round() as usize;
        micros[idx.min(micros.len() - 1)]
    };
    Percentiles {
        min_us: micros[0],
        p50_us: pick(0.5),
        p90_us: pick(0.9),
        max_us: micros[micros.len() - 1],
    }
}

/// Path A: full re-parse of the new bytes, no old tree.
fn timed_full(parser: &mut tree_sitter::Parser, bytes: &[u8]) -> Result<Vec<Duration>, SpikeError> {
    for _ in 0..WARMUP {
        parse(parser, bytes, None)?;
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let tree = parse(parser, bytes, None)?;
        samples.push(start.elapsed());
        drop(tree);
    }
    Ok(samples)
}

/// Path B: the §1.2.1 steps 5–6 fast path. The fresh parse of the OLD bytes
/// before each timed window is the ParseCache stand-in — untimed, because
/// in production `T_old` comes from the cache at zero parse cost. The timed
/// window is exactly `Tree::edit` + `parse_with_options(old_tree)`.
fn timed_incremental(
    parser: &mut tree_sitter::Parser,
    old: &[u8],
    new: &[u8],
    edit: &tree_sitter::InputEdit,
) -> Result<Vec<Duration>, SpikeError> {
    for _ in 0..WARMUP {
        let mut tree = parse(parser, old, None)?;
        tree.edit(edit);
        parse(parser, new, Some(&tree))?;
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let mut tree = parse(parser, old, None)?;
        let start = Instant::now();
        tree.edit(edit);
        let reparsed = parse(parser, new, Some(&tree))?;
        let elapsed = start.elapsed();
        drop(reparsed);
        samples.push(elapsed);
    }
    Ok(samples)
}

/// Whole-tree structural fingerprint (informational: incremental node reuse
/// *should* still land on the same shape as a fresh parse; if not, we want
/// to know, not to assume). Hashed one tree at a time to bound peak memory.
fn sexp_hash(root: tree_sitter::Node<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.to_sexp().hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Serialize)]
pub struct CorrectnessOutcome {
    kind_id_equal: bool,
    byte_range_equal: bool,
    has_error_equal: bool,
    has_error: bool,
    sexp_equal: bool,
}

#[derive(Serialize)]
pub struct EditOutcome {
    name: String,
    old_bytes: usize,
    new_bytes: usize,
    full: Percentiles,
    incremental: Percentiles,
    speedup_p50: f64,
    correctness: CorrectnessOutcome,
}

#[derive(Serialize)]
pub struct Report {
    file_bytes: usize,
    units: usize,
    samples: usize,
    warmup: usize,
    profile: &'static str,
    edits: Vec<EditOutcome>,
}

/// The KP-1 experiment proper. Separate parsers for the two paths so no
/// parser-internal state crosses between them.
pub fn run_measurement() -> Result<Report, SpikeError> {
    let corpus = generate_corpus(TARGET_BYTES);
    let mut full_parser = super::rust_parser()?;
    let mut incr_parser = super::rust_parser()?;

    let baseline = parse(&mut full_parser, &corpus.bytes, None)?;
    if baseline.root_node().has_error() {
        return Err(SpikeError::CorrectnessDivergence {
            detail: "generated corpus does not parse clean".into(),
        });
    }

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let mut edits = Vec::new();
    for case in edit_cases(&corpus)? {
        let new_bytes = splice(&corpus.bytes, &case.splice);
        let input_edit = input_edit_for(&corpus.bytes, &new_bytes, &case.splice);

        // Correctness first (untimed): both paths on the same new bytes.
        let full_tree = parse(&mut full_parser, &new_bytes, None)?;
        let mut edited = parse(&mut incr_parser, &corpus.bytes, None)?;
        edited.edit(&input_edit);
        let incr_tree = parse(&mut incr_parser, &new_bytes, Some(&edited))?;

        let full_root = full_tree.root_node();
        let incr_root = incr_tree.root_node();
        let correctness = CorrectnessOutcome {
            kind_id_equal: full_root.kind_id() == incr_root.kind_id(),
            byte_range_equal: full_root.byte_range() == incr_root.byte_range(),
            has_error_equal: full_root.has_error() == incr_root.has_error(),
            has_error: full_root.has_error() || incr_root.has_error(),
            sexp_equal: sexp_hash(full_root) == sexp_hash(incr_root),
        };

        let full = percentiles(&timed_full(&mut full_parser, &new_bytes)?);
        let incremental = percentiles(&timed_incremental(
            &mut incr_parser,
            &corpus.bytes,
            &new_bytes,
            &input_edit,
        )?);
        let speedup_p50 = full.p50_us as f64 / incremental.p50_us.max(1) as f64;

        edits.push(EditOutcome {
            name: case.name,
            old_bytes: corpus.bytes.len(),
            new_bytes: new_bytes.len(),
            full,
            incremental,
            speedup_p50,
            correctness,
        });
    }

    Ok(Report {
        file_bytes: corpus.bytes.len(),
        units: corpus.units,
        samples: SAMPLES,
        warmup: WARMUP,
        profile,
        edits,
    })
}

// --- entry points -----------------------------------------------------------

/// Fast correctness leg (runs in CI): on a small corpus, every edit size
/// must land both paths on identical root facts with zero error nodes.
#[test]
fn incremental_matches_full_on_small_corpus() {
    let corpus = generate_corpus(16 * 1024);
    let mut full_parser = super::rust_parser().expect("ABI-verified pair (A.3.8)");
    let mut incr_parser = super::rust_parser().expect("ABI-verified pair (A.3.8)");
    let cases = edit_cases(&corpus).expect("small corpus has all anchors");

    for case in cases {
        let new_bytes = splice(&corpus.bytes, &case.splice);
        let input_edit = input_edit_for(&corpus.bytes, &new_bytes, &case.splice);

        let full_tree = parse(&mut full_parser, &new_bytes, None).expect("full parse");
        let mut edited = parse(&mut incr_parser, &corpus.bytes, None).expect("old parse");
        edited.edit(&input_edit);
        let incr_tree =
            parse(&mut incr_parser, &new_bytes, Some(&edited)).expect("incremental parse");

        let full_root = full_tree.root_node();
        let incr_root = incr_tree.root_node();
        assert_eq!(
            full_root.kind_id(),
            incr_root.kind_id(),
            "{}: root kind_id diverged",
            case.name
        );
        assert_eq!(
            full_root.byte_range(),
            incr_root.byte_range(),
            "{}: root byte_range diverged",
            case.name
        );
        assert_eq!(
            full_root.has_error(),
            incr_root.has_error(),
            "{}: has_error diverged",
            case.name
        );
        assert!(
            !full_root.has_error() && !incr_root.has_error(),
            "{}: edit introduced error nodes",
            case.name
        );
        assert!(
            incr_root.byte_range() == (0..new_bytes.len()),
            "{}: incremental root must span the new bytes",
            case.name
        );
    }
}

/// Shared-clone leg (eval-2 amendment 1): the exclusivity guard refuses to
/// edit while another `Arc<Tree>` handle is alive — typed error, no
/// `make_mut`, no observable mutation — and the same edit succeeds once the
/// reader is gone.
#[test]
fn shared_clone_refuses_edit_without_mutation() {
    let corpus = generate_corpus(8 * 1024);
    let mut parser = super::rust_parser().expect("ABI-verified pair (A.3.8)");
    let tree = parse(&mut parser, &corpus.bytes, None).expect("corpus parses");
    let mut owned = Arc::new(tree);

    let case = edit_cases(&corpus)
        .expect("small corpus has all anchors")
        .into_iter()
        .next()
        .expect("at least one edit case");
    let new_bytes = splice(&corpus.bytes, &case.splice);
    let input_edit = input_edit_for(&corpus.bytes, &new_bytes, &case.splice);

    // A second handle alive (a reader mid-traversal): guard must refuse.
    let observer = Arc::clone(&owned);
    let before = owned.root_node().byte_range();
    let err = edit_exclusive(&mut owned, &input_edit).expect_err("guard trips on a shared tree");
    assert!(
        matches!(err, super::SpikeError::SharedTree { count: 2 }),
        "typed shared-tree error, got {err:?}"
    );
    assert_eq!(Arc::strong_count(&owned), 2, "no hidden ts_tree_copy");
    assert_eq!(owned.root_node().byte_range(), before, "tree not mutated");
    assert_eq!(
        observer.root_node().byte_range(),
        before,
        "observer unaffected"
    );

    // Reader gone: the same edit goes through and is observable.
    drop(observer);
    edit_exclusive(&mut owned, &input_edit).expect("exclusive edit succeeds");
    assert_eq!(
        owned.root_node().byte_range(),
        0..new_bytes.len(),
        "edit applied: root spans the new bytes"
    );
}

/// The measurement leg (PLAN F1.1). Heavy: ~3 MB corpus × 6 sizes × 2 paths
/// × (warmup + 20 samples). Run in release for the recorded numbers:
///
/// ```text
/// cargo test --release --features spikes -- --ignored measurement
/// ```
#[test]
#[ignore = "minutes of parsing; run with --release for representative numbers"]
fn measurement_writes_benches_spike_kp1_json() {
    let report = run_measurement().expect("measurement completes");

    for edit in &report.edits {
        let c = &edit.correctness;
        assert!(
            c.kind_id_equal && c.byte_range_equal && c.has_error_equal,
            "{}: paths diverged ({c:?})",
            edit.name
        );
        assert!(!c.has_error, "{}: introduced error nodes", edit.name);
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/spike-kp1.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&report).expect("report serializes"),
    )
    .expect("write benches/spike-kp1.json");
    println!("wrote {}", path.display());
    println!(
        "corpus {} B, {} units, {} samples, profile {}",
        report.file_bytes, report.units, report.samples, report.profile
    );
    for e in &report.edits {
        println!(
            "{:>16}  delta {:+7} B  full p50 {:>9.2} ms  incr p50 {:>9.2} ms  speedup {:>6.2}x  sexp_equal {}",
            e.name,
            e.new_bytes as isize - e.old_bytes as isize,
            e.full.p50_us as f64 / 1000.0,
            e.incremental.p50_us as f64 / 1000.0,
            e.speedup_p50,
            e.correctness.sexp_equal
        );
    }
}
