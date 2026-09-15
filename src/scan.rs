//! `scan` — the vertical slice's orchestrator (PLAN §3 F4.6, §1.6; ADR-0016,
//! ADR-0007's streaming discipline): rule in, match coordinates out,
//! rayon-parallel, deterministic. F8 adds the second mode,
//! `scan --duplicates` (§3 F8): no rule — every capturable subtree at or
//! above a minimum size, grouped by Merkle fingerprint
//! (`crate::ast::fingerprint`, ADR-0010's equivalence definition made
//! falsifiable), reported as canonical groups.
//!
//! Pipeline: walk → classify → compile-per-language → rayon match →
//! canonical assembly (duplicates: walk → classify → rayon fingerprint →
//! canonical grouping).
//!
//! - **Walk (E6):** recursive over the Overlay's **base `read_dir` only**
//!   (anti-phantom — the buffer never leaks into listings); hidden entries
//!   (dot-prefixed) are skipped; `--include-virtual` unions
//!   [`Overlay::virtual_paths`] in as a separate, flagged set (ADR-0016 (5),
//!   default OFF). Walked paths are canonicalized to the absolute root so
//!   the union dedups and the report can show root-relative paths.
//! - **Skip policy (E6):** files over `--max-file-size` (default 4 MiB) are
//!   skipped with `reason: "size"`; a NUL byte in the first 8 KiB skips
//!   with `reason: "binary"`; unreadable entries (dangling symlinks,
//!   tombstones) skip with `reason: "unreadable"`. Every skip is listed —
//!   exclusions are never silent. Files whose extension has no enabled
//!   grammar are simply not scan candidates (not code), and `--lang` scopes
//!   other languages out the same way.
//! - **Per-language compilation (E5):** the rule compiles once per language
//!   actually present in the file set; a compile-time `UnknownKind` for
//!   that language skips the language's files and counts them in
//!   `skipped_for_language` — never a silent no-op.
//! - **rayon + streaming (E3a / ADR-0016 (7)):** workers receive owned
//!   snapshots, parse with per-worker parsers (`map_init`), run the
//!   matcher, and collect plain data; the `ParseCache` is touched only at
//!   collection (get → miss parses → insert → evict), so the process never
//!   retains O(repo) bytes+trees.
//! - **Determinism (ADR-0016 (3)):** files sorted by path, matches
//!   canonically ordered per file by [`crate::ast::matchset`]; parallelism
//!   only partitions work — the assembled report is byte-identical under
//!   any thread count (fenced by the RAYON determinism test).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;
use smol_str::SmolStr;

use crate::ast::cache::{ParseCache, ParseEntry};
use crate::ast::compile::{self, Program, TriviaPolicy};
use crate::ast::dsl;
use crate::ast::eval::{CaptureReport, MatchReport, Matcher};
use crate::ast::lang::{LangId, LangRegistry};
use crate::ast::matchset;
use crate::ast::parse::parse_snapshot;
use crate::error::{DslError, JanitorError};
use crate::vfs::Overlay;

/// How far into a file the binary sniff looks (E6: "first 8 KB").
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// The default size cap (4 MiB, docs/DSL.md §8).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 4 * 1024 * 1024;

/// Everything `scan` needs besides the session's own state.
#[derive(Clone, Debug)]
pub struct ScanParams {
    /// The rule text, verbatim (echoed in the report).
    pub rule: String,
    /// Scan root.
    pub root: PathBuf,
    /// Restrict to one canonical language name (`rust`, …).
    pub lang_filter: Option<String>,
    /// Union overlay-staged virtual files into the walk (ADR-0016 (5)).
    pub include_virtual: bool,
    /// Byte-strict adjacency instead of ADR-0010's comment-skipping default.
    pub strict_trivia: bool,
    /// Files above this many bytes are skipped with `reason: "size"`.
    pub max_file_size: u64,
}

impl ScanParams {
    /// Defaults for every field but `rule` and `root` (docs/DSL.md §8).
    pub fn new(rule: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            rule: rule.into(),
            root: root.into(),
            lang_filter: None,
            include_virtual: false,
            strict_trivia: false,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// One listed exclusion (E6: skips are reported, never silent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkipRecord {
    pub path: String,
    /// `"binary"`, `"size"`, or `"unreadable"`.
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PointJson {
    pub row: u32,
    pub column: u32,
}

impl PointJson {
    fn new(p: crate::ast::eval::PointData) -> Self {
        Self {
            row: p.row,
            column: p.column,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CaptureJson {
    pub symbol: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: PointJson,
    pub end_point: PointJson,
    pub kind: Option<String>,
    pub kind_id: u16,
}

/// One match, report-shaped (Rule T3 + ADR-0016's `overlap` + E4's
/// `virtual`): kind names resolved at serialization time, never in the
/// matcher's hot loop (§1.4.2).
#[derive(Clone, Debug, Serialize)]
pub struct ScanMatch {
    pub path: String,
    pub virtual_file: bool,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: PointJson,
    pub end_point: PointJson,
    pub kind: Option<String>,
    pub kind_id: u16,
    pub overlap: bool,
    pub captures: Vec<CaptureJson>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanStats {
    /// Files actually scanned (post skip/filter).
    pub files: u64,
    /// Total bytes of scanned files.
    pub bytes: u64,
    /// Wall-clock scan time — the only non-deterministic field.
    pub wall_ms: u64,
    /// Files with at least one match.
    pub files_matched: u64,
    /// Total matches reported.
    pub matches: u64,
    /// Scanned files per language.
    pub langs: BTreeMap<String, u64>,
}

/// The `janitor scan` report payload (the `data` of the janitor.v1
/// envelope; docs/DSL.md §8 documents the exact shape).
#[derive(Clone, Debug, Serialize)]
pub struct ScanReport {
    pub rule: String,
    /// `"skip"` (ADR-0010 default) or `"strict"`.
    pub trivia: &'static str,
    /// Path-sorted, per-file canonically ordered matches.
    pub matches: Vec<ScanMatch>,
    /// language → files skipped because the rule names a kind that language
    /// does not have (E5).
    pub skipped_for_language: BTreeMap<String, u64>,
    /// Binary/size/unreadable exclusions (E6).
    pub skipped: Vec<SkipRecord>,
    pub stats: ScanStats,
}

/// One walk/classification survivor headed for the parallel phase.
struct Candidate {
    path: PathBuf,
    display: String,
    virtual_file: bool,
    lang: LangId,
}

/// Run a scan. See the module docs for the pipeline; every failure is a
/// typed [`JanitorError`] (rule problems fail before any I/O).
pub fn scan(
    overlay: &Overlay,
    cache: &ParseCache,
    registry: &LangRegistry,
    params: &ScanParams,
) -> Result<ScanReport, JanitorError> {
    let started = Instant::now();

    // Rule front-end: parse, then the loud E5 gate — both before any I/O.
    let rule = dsl::parse_rule(&params.rule)?;
    compile::validate_kinds(&rule, registry)?;
    let trivia = if params.strict_trivia {
        TriviaPolicy::Strict
    } else {
        TriviaPolicy::Skip
    };

    // Language filter resolves to a registry id up front (typed error for a
    // name this build does not embed), then the shared walk/classify phase.
    let walk = collect_candidates(
        overlay,
        registry,
        &params.root,
        params.lang_filter.as_deref(),
        params.include_virtual,
        params.max_file_size,
    )?;
    let allowed = walk.allowed;
    let mut candidates = walk.candidates;
    let mut skipped = walk.skipped;

    // Structural compile checks are file-independent (duplicate capture
    // symbols, unify refusal) and must fire even when the file set is
    // empty — a rule that cannot compile never reports success. Probed
    // against the filter language or the first enabled one; per-language
    // `UnknownKind` is NOT a failure here (that is the E5 skip path).
    let probe_lang = allowed.or_else(|| registry.ids().next());
    if let Some(lang) = probe_lang {
        match compile::compile_rule(&rule, registry, lang, trivia) {
            Ok(_) | Err(DslError::UnknownKind { .. }) => {}
            Err(err) => return Err(err.into()),
        }
    }

    // Compile once per language present; `UnknownKind` for a language
    // means its files are skipped and counted (E5) — before any parsing.
    let mut programs: BTreeMap<LangId, Program> = BTreeMap::new();
    let mut skipped_for_language: BTreeMap<String, u64> = BTreeMap::new();
    let mut langs_present: Vec<LangId> = Vec::new();
    for candidate in &candidates {
        if !langs_present.contains(&candidate.lang) {
            langs_present.push(candidate.lang);
        }
    }
    for lang in langs_present {
        match compile::compile_rule(&rule, registry, lang, trivia) {
            Ok(program) => {
                programs.insert(lang, program);
            }
            Err(DslError::UnknownKind { .. }) => {
                let count = candidates.iter().filter(|c| c.lang == lang).count() as u64;
                skipped_for_language
                    .insert(registry.name(lang).unwrap_or("<unknown>").to_owned(), count);
            }
            Err(err) => return Err(err.into()),
        }
    }
    // Resolved trivia ids per compiled language — the fingerprint tables'
    // only language-dependent input (F8; resolved once, shared read-only
    // by every worker).
    let trivia_by_lang: BTreeMap<LangId, Vec<u16>> = programs
        .keys()
        .map(|&lang| {
            (
                lang,
                crate::ast::fingerprint::trivia_kind_ids(registry, lang),
            )
        })
        .collect();
    candidates.retain(|c| programs.contains_key(&c.lang));

    // The parallel phase: one work item per candidate, parser per worker
    // (§1.2's ownership shape), ParseCache touched only at collection
    // (ADR-0016 (7)) and evicted right after (ADR-0007 streaming).
    struct Scanned {
        bytes: usize,
        reports: Vec<MatchReport>,
    }
    enum Outcome {
        Hit(Scanned),
        Skip(SkipRecord),
    }

    let outcomes: Result<Vec<(Candidate, Outcome)>, JanitorError> = candidates
        .into_par_iter()
        .map_init(
            || None::<crate::ast::ffi_shim::Parser>,
            |worker_parser, candidate| -> Result<(Candidate, Outcome), JanitorError> {
                let Some(program) = programs.get(&candidate.lang) else {
                    // Language skip-classified after this candidate was
                    // queued (defensive: candidates are retained to
                    // compiled languages before the parallel phase).
                    let display = candidate.display.clone();
                    return Ok((
                        candidate,
                        Outcome::Skip(SkipRecord {
                            path: display,
                            reason: "unreadable",
                        }),
                    ));
                };
                let (bytes, key, entry) =
                    match read_parse_entry(overlay, cache, registry, worker_parser, &candidate)? {
                        // Binary content: honest skip (E6).
                        ReadParsed::Binary => {
                            let display = candidate.display.clone();
                            return Ok((
                                candidate,
                                Outcome::Skip(SkipRecord {
                                    path: display,
                                    reason: "binary",
                                }),
                            ));
                        }
                        ReadParsed::Entry { bytes, key, entry } => (bytes, key, entry),
                    };

                let mut reports = Vec::new();
                let display = SmolStr::from(candidate.display.as_str());
                Matcher::new(program, &entry.tree, 0).matches(&display, &mut reports)?;
                // F8: unify constraints are a post-match filter over
                // capture fingerprints — applied to the match set before
                // ADR-0016's overlap flags (they describe what is
                // reported). Constraint-free rules (the common case) skip
                // the whole pass, ParseEntry's lazy field included.
                if !program.unifies.is_empty() {
                    let trivia = trivia_by_lang
                        .get(&candidate.lang)
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    let table = entry.fingerprints(trivia);
                    crate::ast::fingerprint::filter_unify(program, &table, &mut reports);
                }
                matchset::flag_overlaps(&mut reports);

                // Collection is the only cache-touching moment; streaming
                // eviction keeps retention at the working set (ADR-0007).
                evict_after_use(cache, &key, &entry);

                Ok((
                    candidate,
                    Outcome::Hit(Scanned {
                        bytes: bytes.len(),
                        reports,
                    }),
                ))
            },
        )
        .collect();

    // Assemble deterministically: file order is the (sorted) input order —
    // par_iter preserves it — and each file's reports are already in
    // canonical order. Kind names resolve here, at serialization time.
    let mut matches: Vec<ScanMatch> = Vec::new();
    let mut stats = ScanStats::default();
    for (candidate, outcome) in outcomes? {
        match outcome {
            Outcome::Skip(record) => skipped.push(record),
            Outcome::Hit(scanned) => {
                stats.files += 1;
                stats.bytes += scanned.bytes as u64;
                if !scanned.reports.is_empty() {
                    stats.files_matched += 1;
                }
                stats.matches += scanned.reports.len() as u64;
                *stats
                    .langs
                    .entry(
                        registry
                            .name(candidate.lang)
                            .unwrap_or("<unknown>")
                            .to_owned(),
                    )
                    .or_default() += 1;
                for report in scanned.reports {
                    matches.push(to_json_match(
                        report,
                        registry,
                        candidate.lang,
                        candidate.virtual_file,
                    ));
                }
            }
        }
    }
    skipped.sort_by(|a, b| a.path.cmp(&b.path));

    stats.wall_ms = started.elapsed().as_millis() as u64;
    Ok(ScanReport {
        rule: params.rule.clone(),
        trivia: if params.strict_trivia {
            "strict"
        } else {
            "skip"
        },
        matches,
        skipped_for_language,
        skipped,
        stats,
    })
}

/// The default `--min-size` for duplicates mode: 8 participating nodes —
/// roughly one full statement's worth of structure. Below that the groups
/// degenerate into single-token noise (every `i`, every `()`); above it the
/// pass starts missing small real copies. `--min-size` tunes it (F8).
pub const DEFAULT_MIN_DUPLICATE_SIZE: u32 = 8;

/// Everything `scan --duplicates` needs (F8): no rule — the mode groups
/// ALL capturable subtrees above `min_size` by fingerprint.
#[derive(Clone, Debug)]
pub struct DuplicatesParams {
    /// Scan root.
    pub root: PathBuf,
    /// Restrict to one canonical language name (`rust`, …).
    pub lang_filter: Option<String>,
    /// Union overlay-staged virtual files into the walk (ADR-0016 (5)).
    pub include_virtual: bool,
    /// Files above this many bytes are skipped with `reason: "size"`.
    pub max_file_size: u64,
    /// Minimum subtree size, in participating nodes (comment- and
    /// formatting-invariant), for a subtree to be groupable.
    pub min_size: u32,
}

impl DuplicatesParams {
    /// Defaults for every field but `root` (docs/SCHEMA.md § duplicates).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            lang_filter: None,
            include_virtual: false,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            min_size: DEFAULT_MIN_DUPLICATE_SIZE,
        }
    }
}

/// One duplicate member: the full match-report shape (Rule T3 / ADR-0016
/// — `captures` is structurally empty here, kept for shape parity with
/// `ScanMatch` so consumers need one renderer) plus the group's
/// fingerprint and id.
#[derive(Clone, Debug, Serialize)]
pub struct DuplicateMember {
    pub path: String,
    pub virtual_file: bool,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: PointJson,
    pub end_point: PointJson,
    pub kind: Option<String>,
    pub kind_id: u16,
    /// ADR-0016 overlap flag, computed per file across all reported
    /// members (nested duplicates of the same code flag each other).
    pub overlap: bool,
    pub captures: Vec<CaptureJson>,
    /// The group's 128-bit fingerprint, 32 lowercase hex chars.
    pub fingerprint: String,
    /// The group's output-order id (groups are numbered 0, 1, … after the
    /// canonical sort).
    pub group: u32,
}

/// One duplicate group: every reported member shares `fingerprint` (hence
/// `size` — the subtree's participating-node count) and the language
/// (kind ids are per-grammar; equal hashes across grammars are not
/// equivalence).
#[derive(Clone, Debug, Serialize)]
pub struct DuplicateGroup {
    pub id: u32,
    pub fingerprint: String,
    pub lang: String,
    pub size: u32,
    /// Canonically ordered: (path, start_byte, end_byte).
    pub members: Vec<DuplicateMember>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DuplicatesStats {
    /// Files actually scanned (post skip/filter).
    pub files: u64,
    /// Total bytes of scanned files.
    pub bytes: u64,
    /// Participating nodes fingerprinted (the pass's n).
    pub nodes: u64,
    /// Groups with at least two members.
    pub groups: u64,
    /// Total members across reported groups.
    pub members: u64,
    /// Scanned files per language.
    pub langs: BTreeMap<String, u64>,
    /// Wall-clock time — the only non-deterministic field.
    pub wall_ms: u64,
}

/// The `janitor scan --duplicates` payload (the `data` of the janitor.v1
/// envelope, `kind: "duplicates"`; documented in docs/SCHEMA.md).
#[derive(Clone, Debug, Serialize)]
pub struct DuplicatesReport {
    pub kind: &'static str,
    /// The participating-node floor that gated grouping.
    pub min_size: u32,
    /// Groups ordered canonically: by first member's (path, start, end),
    /// then size descending; ids assigned in this order.
    pub groups: Vec<DuplicateGroup>,
    /// Binary/size/unreadable exclusions (E6) — same policy as rule scans.
    pub skipped: Vec<SkipRecord>,
    pub stats: DuplicatesStats,
}

/// The ADR-0016 overlap sweep over sorted half-open spans: member i
/// overlaps iff some earlier member ends after it starts, or the next
/// member starts before it ends. Returns per-member flags.
fn overlap_flags(spans_sorted: &[(u32, u32)]) -> Vec<bool> {
    let mut flags = vec![false; spans_sorted.len()];
    let mut max_end: Option<u32> = None;
    for (i, &(start, end)) in spans_sorted.iter().enumerate() {
        if max_end.is_some_and(|e| e > start) {
            flags[i] = true;
        }
        if max_end.is_none_or(|c| end > c) {
            max_end = Some(end);
        }
    }
    for i in 0..spans_sorted.len().saturating_sub(1) {
        if spans_sorted[i + 1].0 < spans_sorted[i].1 {
            flags[i] = true;
            flags[i + 1] = true;
        }
    }
    flags
}

/// Run a duplicates scan (F8): walk/classify exactly like a rule scan, then
/// per file (rayon, streaming, per-worker parsers — same ownership shape)
/// build the fingerprint table and keep every digest at or above
/// `min_size`; group globally by `(language, fingerprint)`; report groups
/// with ≥ 2 members, canonically ordered — byte-identical under any rayon
/// thread count (parallelism only partitions work; assembly is a pure
/// function of the per-file digests, which are functions of the parses).
pub fn duplicates(
    overlay: &Overlay,
    cache: &ParseCache,
    registry: &LangRegistry,
    params: &DuplicatesParams,
) -> Result<DuplicatesReport, JanitorError> {
    let started = Instant::now();
    let walk = collect_candidates(
        overlay,
        registry,
        &params.root,
        params.lang_filter.as_deref(),
        params.include_virtual,
        params.max_file_size,
    )?;
    let candidates = walk.candidates;
    let mut skipped = walk.skipped;

    // Resolved trivia ids per language present (the tables' only
    // language-dependent input), shared read-only by every worker.
    let mut trivia_by_lang: BTreeMap<LangId, Vec<u16>> = BTreeMap::new();
    for candidate in &candidates {
        let lang = candidate.lang;
        trivia_by_lang
            .entry(lang)
            .or_insert_with(|| crate::ast::fingerprint::trivia_kind_ids(registry, lang));
    }

    // The parallel phase: one work item per candidate — read, sniff,
    // parse-or-cache (ADR-0016 (7) insertion rule), fingerprint, filter by
    // size; plain data out; entry evicted right after (ADR-0007).
    struct Scanned {
        bytes: usize,
        nodes: u64,
        members: Vec<crate::ast::fingerprint::SpanDigest>,
    }
    enum Outcome {
        Hit(Scanned),
        Skip(SkipRecord),
    }

    let outcomes: Result<Vec<(Candidate, Outcome)>, JanitorError> = candidates
        .into_par_iter()
        .map_init(
            || None::<crate::ast::ffi_shim::Parser>,
            |worker_parser, candidate| -> Result<(Candidate, Outcome), JanitorError> {
                let (bytes, key, entry) =
                    match read_parse_entry(overlay, cache, registry, worker_parser, &candidate)? {
                        ReadParsed::Binary => {
                            let display = candidate.display.clone();
                            return Ok((
                                candidate,
                                Outcome::Skip(SkipRecord {
                                    path: display,
                                    reason: "binary",
                                }),
                            ));
                        }
                        ReadParsed::Entry { bytes, key, entry } => (bytes, key, entry),
                    };

                let trivia = trivia_by_lang
                    .get(&candidate.lang)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let table = entry.fingerprints(trivia);
                let members: Vec<crate::ast::fingerprint::SpanDigest> = table
                    .digests()
                    .iter()
                    .filter(|d| d.size >= params.min_size)
                    .copied()
                    .collect();
                let scanned = Scanned {
                    bytes: bytes.len(),
                    nodes: table.participating() as u64,
                    members,
                };

                // Collection is the only cache-touching moment; streaming
                // eviction keeps retention at the working set (ADR-0007).
                evict_after_use(cache, &key, &entry);

                Ok((candidate, Outcome::Hit(scanned)))
            },
        )
        .collect();

    // Group globally by (language, fingerprint): kind ids are per-grammar,
    // so identical hashes in two grammars are not equivalence. Candidates
    // arrive in path order (BTreeSet walk) and par_iter preserves it —
    // group assembly is a pure function of that order.
    let mut groups_map: BTreeMap<
        (LangId, crate::ast::fingerprint::Fp),
        Vec<(usize, crate::ast::fingerprint::SpanDigest)>,
    > = BTreeMap::new();
    let mut scanned_files: Vec<Candidate> = Vec::new();
    let mut stats = DuplicatesStats::default();
    for (candidate, outcome) in outcomes? {
        match outcome {
            Outcome::Skip(record) => skipped.push(record),
            Outcome::Hit(scanned) => {
                stats.files += 1;
                stats.bytes += scanned.bytes as u64;
                stats.nodes += scanned.nodes;
                *stats
                    .langs
                    .entry(
                        registry
                            .name(candidate.lang)
                            .unwrap_or("<unknown>")
                            .to_owned(),
                    )
                    .or_default() += 1;
                let file_idx = scanned_files.len();
                for digest in scanned.members {
                    groups_map
                        .entry((candidate.lang, digest.fp))
                        .or_default()
                        .push((file_idx, digest));
                }
                scanned_files.push(candidate);
            }
        }
    }
    skipped.sort_by(|a, b| a.path.cmp(&b.path));

    /// One group under assembly: the canonical sort key of its first
    /// member (file order, start, end), each member's file index (for the
    /// overlap pass), and the assembled group.
    struct Assembled {
        first: (usize, u32, u32),
        member_files: Vec<usize>,
        group: DuplicateGroup,
    }

    // Canonical member order inside a group: (file order, start, end).
    let mut groups: Vec<Assembled> = groups_map
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|((lang, fp), mut members)| {
            members.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.start.cmp(&b.1.start))
                    .then_with(|| a.1.end.cmp(&b.1.end))
            });
            let size = members[0].1.size;
            let first = (members[0].0, members[0].1.start, members[0].1.end);
            Assembled {
                first,
                member_files: members.iter().map(|(f, _)| *f).collect(),
                group: DuplicateGroup {
                    id: 0, // assigned after the canonical group sort
                    fingerprint: crate::ast::fingerprint::fp_hex(fp),
                    lang: registry.name(lang).unwrap_or("<unknown>").to_owned(),
                    size,
                    members: members
                        .iter()
                        .map(|(file_idx, d)| {
                            let candidate = &scanned_files[*file_idx];
                            DuplicateMember {
                                path: candidate.display.clone(),
                                virtual_file: candidate.virtual_file,
                                start_byte: d.start as usize,
                                end_byte: d.end as usize,
                                start_point: PointJson::new(d.start_point),
                                end_point: PointJson::new(d.end_point),
                                kind: registry.kind_name(lang, d.kind_id).map(|n| n.to_string()),
                                kind_id: d.kind_id,
                                overlap: false,
                                captures: Vec::new(),
                                fingerprint: crate::ast::fingerprint::fp_hex(d.fp),
                                group: 0,
                            }
                        })
                        .collect(),
                },
            }
        })
        .collect();

    // ADR-0016 (3) discipline: groups ordered by canonical first-member
    // span — (file order, start, end) — then size desc (the tie-break for
    // nested groups whose first members coincide); ids assigned in this
    // order. The map's iteration order never leaks: everything is sorted
    // before the first id exists.
    groups.sort_by(|a, b| {
        a.first
            .0
            .cmp(&b.first.0)
            .then_with(|| a.first.1.cmp(&b.first.1))
            .then_with(|| a.first.2.cmp(&b.first.2))
            .then_with(|| b.group.size.cmp(&a.group.size))
    });

    // Overlap flags, per file, across ALL reported members — nested
    // duplicates of the same code flag each other whatever their group
    // (the same span-intersection definition as ADR-0016 match sets).
    let mut by_file: Vec<Vec<(u32, u32, usize, usize)>> = vec![Vec::new(); scanned_files.len()];
    for (gi, assembled) in groups.iter().enumerate() {
        for (mi, member) in assembled.group.members.iter().enumerate() {
            by_file[assembled.member_files[mi]].push((
                member.start_byte as u32,
                member.end_byte as u32,
                gi,
                mi,
            ));
        }
    }
    for entries in &mut by_file {
        entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let spans: Vec<(u32, u32)> = entries.iter().map(|e| (e.0, e.1)).collect();
        let flags = overlap_flags(&spans);
        for (entry, flag) in entries.iter().zip(flags) {
            groups[entry.2].group.members[entry.3].overlap |= flag;
        }
    }

    let mut groups: Vec<DuplicateGroup> = groups.into_iter().map(|a| a.group).collect();
    for (gi, group) in groups.iter_mut().enumerate() {
        group.id = gi as u32;
        for member in &mut group.members {
            member.group = gi as u32;
        }
    }
    stats.groups = groups.len() as u64;
    stats.members = groups.iter().map(|g| g.members.len() as u64).sum();

    stats.wall_ms = started.elapsed().as_millis() as u64;
    Ok(DuplicatesReport {
        kind: "duplicates",
        min_size: params.min_size,
        groups,
        skipped,
        stats,
    })
}

/// Resolve a `--lang` name to a registry id; a name this build does not
/// embed is the typed `E_PARSE_NO_GRAMMAR_FOR_LANG` (docs/DSL.md §4).
fn find_lang(registry: &LangRegistry, name: &str) -> Result<LangId, JanitorError> {
    registry
        .ids()
        .find(|&id| registry.name(id) == Some(name))
        .ok_or_else(|| {
            JanitorError::from(crate::error::ParseError::NoGrammarForLang {
                lang: name.to_owned(),
            })
        })
}

/// The per-candidate read + binary sniff + parse-or-cache step, shared by
/// both scan modes (this very duplication was flagged by `scan
/// --duplicates` over this repo's own src — the F8 dogfood working as
/// intended). `worker_parser` is the worker's lazily-materialized parser
/// (one per language per worker, §1.2).
enum ReadParsed {
    /// A NUL byte in the first 8 KiB (E6).
    Binary,
    /// The snapshot, its cache key, and the validated entry.
    Entry {
        bytes: Arc<[u8]>,
        key: crate::vfs::VfsPath,
        entry: ParseEntry,
    },
}

fn read_parse_entry(
    overlay: &Overlay,
    cache: &ParseCache,
    registry: &LangRegistry,
    worker_parser: &mut Option<crate::ast::ffi_shim::Parser>,
    candidate: &Candidate,
) -> Result<ReadParsed, JanitorError> {
    let bytes = overlay.read_file(&candidate.path)?;
    let sniff_len = bytes.len().min(BINARY_SNIFF_BYTES);
    if bytes[..sniff_len].contains(&0) {
        return Ok(ReadParsed::Binary);
    }
    let key = crate::vfs::VfsPath::new(&candidate.path)?;
    let generation = overlay.current_gen(&candidate.path)?;
    let entry = match cache.get(&key, generation, &bytes) {
        Some(hit) => hit,
        None => {
            // Parse with this worker's parser, materialized lazily — one
            // per language per worker (§1.2).
            if worker_parser.is_none() {
                *worker_parser = Some(registry.parser(candidate.lang)?);
            }
            let parser = match worker_parser.as_mut() {
                Some(parser) => parser,
                None => {
                    return Err(JanitorError::from(crate::error::ParseError::NoTree {
                        detail: "worker parser vanished".to_owned(),
                    }));
                }
            };
            let tree = parse_snapshot(parser, &bytes)?;
            ParseEntry::new(generation, Arc::clone(&bytes), tree)
        }
    };
    Ok(ReadParsed::Entry { bytes, key, entry })
}

/// The streaming-eviction epilogue (ADR-0007 / ADR-0016 (7)): collection
/// is the only cache-touching moment, and the entry is dropped right after
/// its report is collected.
fn evict_after_use(cache: &ParseCache, key: &crate::vfs::VfsPath, entry: &ParseEntry) {
    cache.insert(key.clone(), entry.clone());
    cache.take(key);
}

/// The walk/classification phase shared by rule scans and the duplicates
/// mode (E4/E6): `--lang` resolution, the base-`read_dir` walk with the
/// optional virtual union (canonicalized to the absolute root), extension
/// routing, `--lang` scope, the size cap — and honest skip records for
/// everything excluded on the way.
struct WalkOutcome {
    allowed: Option<LangId>,
    candidates: Vec<Candidate>,
    skipped: Vec<SkipRecord>,
}

fn collect_candidates(
    overlay: &Overlay,
    registry: &LangRegistry,
    root: &Path,
    lang_filter: Option<&str>,
    include_virtual: bool,
    max_file_size: u64,
) -> Result<WalkOutcome, JanitorError> {
    let allowed = match lang_filter {
        Some(name) => Some(find_lang(registry, name)?),
        None => None,
    };

    // Walk (base read_dir only) + optional virtual union, canonicalized to
    // the absolute root so the union dedups and display paths can be
    // root-relative. A file root scans just that file — the natural
    // single-file invocation.
    let root_abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let single_file = matches!(overlay.stat(&root_abs), Ok(stat) if stat.is_file);
    let mut files = if single_file {
        BTreeSet::from([root_abs.clone()])
    } else {
        walk_files(overlay, &root_abs)?
    };
    let virtual_set: HashSet<PathBuf> = if include_virtual {
        let staged: HashSet<PathBuf> = overlay
            .virtual_paths()
            .iter()
            .map(|p| p.as_path().to_path_buf())
            .collect();
        files.extend(staged.iter().cloned());
        staged
    } else {
        HashSet::new()
    };

    // Classify: extension routing, --lang scope, size cap. The binary sniff
    // needs bytes, so it runs in the parallel phase and lands in `skipped`.
    let mut candidates = Vec::new();
    let mut skipped: Vec<SkipRecord> = Vec::new();
    for path in &files {
        // A single-file root displays as the user wrote it; everything
        // else is root-relative (absolute only outside the root).
        let display = if single_file && path == &root_abs {
            root.display().to_string()
        } else {
            display_path(path, &root_abs)
        };
        let Some(lang) = registry.for_path(path) else {
            continue; // no enabled grammar routes this extension: not code
        };
        if allowed.is_some_and(|a| a != lang) {
            continue; // --lang scope: other languages are out of scope, not skipped
        }
        let size = match overlay.stat(path) {
            Ok(stat) => stat.len,
            Err(_) => {
                // Dangling symlinks, tombstones, permission walls: honest
                // skips, never a walk failure.
                skipped.push(SkipRecord {
                    path: display,
                    reason: "unreadable",
                });
                continue;
            }
        };
        if size > max_file_size {
            skipped.push(SkipRecord {
                path: display,
                reason: "size",
            });
            continue;
        }
        candidates.push(Candidate {
            path: path.clone(),
            display,
            virtual_file: virtual_set.contains(path),
            lang,
        });
    }

    Ok(WalkOutcome {
        allowed,
        candidates,
        skipped,
    })
}

/// Recursive file walk over the Overlay's base `read_dir` (anti-phantom:
/// staged files never appear here — the union is the explicit
/// `--include-virtual` path). Hidden entries are skipped at every level;
/// the result is sorted; symlinked directories list as files (lstat-shaped
/// `DirEntry`s, F2.3) and are therefore read, not descended.
fn walk_files(overlay: &Overlay, root: &Path) -> Result<BTreeSet<PathBuf>, JanitorError> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let listing = overlay.read_dir(&dir)?;
        for entry in listing.entries() {
            if entry.name.starts_with('.') {
                continue; // hidden: .git, .janitor, dotfiles — never walked
            }
            let full = dir.join(&entry.name);
            if entry.is_dir {
                stack.push(full);
            } else {
                out.insert(full);
            }
        }
    }
    Ok(out)
}

/// Root-relative display path when the file is under the root, absolute
/// otherwise (virtual files staged outside the root keep their full path).
/// A single-file root is its own display name (never empty).
fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .filter(|rel| !rel.as_os_str().is_empty())
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Plain-data report → JSON shape, resolving kind names through the
/// registry (§1.4.2: names at serialization time only).
fn to_json_match(
    report: MatchReport,
    registry: &LangRegistry,
    lang: LangId,
    virtual_file: bool,
) -> ScanMatch {
    let kind = registry
        .kind_name(lang, report.kind_id)
        .map(|n| n.to_string());
    ScanMatch {
        path: report.path.to_string(),
        virtual_file,
        start_byte: report.start_byte,
        end_byte: report.end_byte,
        start_point: PointJson::new(report.start_point),
        end_point: PointJson::new(report.end_point),
        kind,
        kind_id: report.kind_id,
        overlap: report.overlap,
        captures: report
            .captures
            .into_iter()
            .map(|cap| to_json_capture(cap, registry, lang))
            .collect(),
    }
}

fn to_json_capture(cap: CaptureReport, registry: &LangRegistry, lang: LangId) -> CaptureJson {
    let kind = registry.kind_name(lang, cap.kind_id).map(|n| n.to_string());
    CaptureJson {
        symbol: format!("@{}", cap.symbol),
        start_byte: cap.start_byte,
        end_byte: cap.end_byte,
        start_point: PointJson::new(cap.start_point),
        end_point: PointJson::new(cap.end_point),
        kind,
        kind_id: cap.kind_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::Session;

    /// The KP-6 curated snippet, end to end through the session API:
    /// strict vs skip counts, skip records, stats, and the canonical order.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn scan_reports_kp6_let_adjacency_end_to_end() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("lets.rs"),
            "fn curated_let(v: u32) -> u32 {\n    let a = v;\n    let b = a;\n    b;\n    let c = 1;\n    // comment between lets\n    let d = 2;\n    d\n}\n",
        )
        .expect("write corpus");
        std::fs::write(tmp.path().join("notes.md"), "not code\n").expect("write md");
        let hidden = tmp.path().join(".hidden");
        std::fs::create_dir(&hidden).expect("hidden dir");
        std::fs::write(
            hidden.join("secret.rs"),
            "fn secret() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n",
        )
        .expect("write hidden match");
        std::fs::write(tmp.path().join("binary.rs"), b"\x00fn hidden() {}\n")
            .expect("write binary");

        let session = Session::new();
        let mut params = ScanParams::new(
            "(under block (seq (kind let_declaration) (kind let_declaration)))",
            tmp.path(),
        );
        let report = session.scan(&params).expect("scan runs");
        assert_eq!(report.matches.len(), 2, "skip mode: the (c,d) pair matches");
        assert_eq!(
            report.stats.files, 1,
            "only lets.rs scanned (binary skipped)"
        );
        assert_eq!(report.stats.files_matched, 1);
        assert_eq!(report.stats.matches, 2);
        assert_eq!(report.stats.langs["rust"], 1);
        assert_eq!(
            report.skipped,
            vec![SkipRecord {
                path: "binary.rs".into(),
                reason: "binary"
            }],
            "NUL sniff skips the binary; .hidden never walked; notes.md unrouted"
        );
        assert_eq!(report.trivia, "skip");
        assert!(
            report.matches[0].start_byte < report.matches[1].start_byte,
            "canonical order"
        );
        assert_eq!(report.matches[0].kind.as_deref(), Some("block"));

        params.strict_trivia = true;
        let strict = session.scan(&params).expect("strict scan runs");
        assert_eq!(strict.matches.len(), 1, "byte-strict loses the (c,d) pair");
        assert_eq!(strict.trivia, "strict");
    }

    /// The virtual union (E4/ADR-0016 (5)): staged-only files are invisible
    /// by default and flagged virtual under `--include-virtual`; a staged
    /// override of a disk file reads through and is scanned exactly once.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn virtual_files_union_under_the_flag_and_dedup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("disk.rs"), "fn one() -> u32 { 1 }\n").expect("disk file");

        let session = Session::with_overlay(Arc::new(Overlay::new(Arc::new(crate::vfs::Os))));
        let pair = "fn f() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n";
        session
            .overlay()
            .set(tmp.path().join("staged.rs"), pair)
            .expect("stage virtual");
        session
            .overlay()
            .set(tmp.path().join("disk.rs"), pair)
            .expect("stage override");

        let rule = "(under block (seq (kind let_declaration) (kind let_declaration)))";
        let mut params = ScanParams::new(rule, tmp.path());

        // Default (anti-phantom): the walk sees disk.rs only, but
        // read-through serves the STAGED bytes — the pair matches, the
        // staged-only file is invisible.
        let report = session.scan(&params).expect("default scan");
        assert_eq!(report.stats.files, 1, "disk.rs only (overridden content)");
        assert_eq!(report.matches.len(), 1, "read-through content matches");
        assert!(report.matches.iter().all(|m| !m.virtual_file));

        params.include_virtual = true;
        let with_virtual = session.scan(&params).expect("virtual scan");
        assert_eq!(with_virtual.stats.files, 2, "disk.rs + staged.rs, deduped");
        let staged: Vec<&ScanMatch> = with_virtual
            .matches
            .iter()
            .filter(|m| m.path == "staged.rs")
            .collect();
        assert_eq!(staged.len(), 1);
        assert!(staged[0].virtual_file, "staged-only files are flagged");
        // The overridden disk file reports exactly once (dedup) and is
        // flagged too — its content is overlay-staged, not disk truth.
        let disk: Vec<&ScanMatch> = with_virtual
            .matches
            .iter()
            .filter(|m| m.path == "disk.rs")
            .collect();
        assert_eq!(disk.len(), 1, "the union dedups the staged override");
        assert!(
            disk[0].virtual_file,
            "staged content is flagged, whatever the path"
        );
    }

    /// Size cap, --lang typing, and the streaming discipline's cache shape.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn size_cap_and_lang_filter_are_typed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("huge.rs"), "fn big() -> u32 {\n    1\n}\n")
            .expect("write file");

        let session = Session::new();
        let mut params = ScanParams::new("(under block _)", tmp.path());
        params.max_file_size = 8; // far below the file
        let report = session.scan(&params).expect("scan runs");
        assert_eq!(
            report.skipped,
            vec![SkipRecord {
                path: "huge.rs".into(),
                reason: "size"
            }]
        );
        assert_eq!(report.stats.files, 0);

        params.max_file_size = DEFAULT_MAX_FILE_SIZE;
        params.lang_filter = Some("ruby".into());
        let err = session.scan(&params).expect_err("unknown --lang is typed");
        assert!(
            matches!(
                err,
                JanitorError::Parse(crate::error::ParseError::NoGrammarForLang { .. })
            ),
            "got {err:?}"
        );
    }

    /// Streaming (E3a): after a scan the cache holds nothing — each file's
    /// entry was dropped once its report was collected (ADR-0007).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn scan_streams_and_leaves_the_cache_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            std::fs::write(
                tmp.path().join(format!("f{i}.rs")),
                format!("fn f{i}() -> u32 {{\n    let a = {i};\n    let b = a;\n    b\n}}\n"),
            )
            .expect("write file");
        }
        let session = Session::new();
        let params = ScanParams::new(
            "(under block (seq (kind let_declaration) (kind let_declaration)))",
            tmp.path(),
        );
        let report = session.scan(&params).expect("scan runs");
        assert_eq!(report.matches.len(), 5);
        assert!(session.cache().is_empty(), "streaming: nothing retained");
    }

    /// F8: `unify` end to end through the session API — the same rule with
    /// and without the constraint differs exactly by the fingerprint
    /// filter: the identical pair survives, the differing pair (which the
    /// constraint-free rule reports) is filtered. A comment between the
    /// members does not break unification.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn unify_constraints_filter_the_scan_end_to_end() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("twin.rs"),
            "fn twin() -> u32 {\n    let a = 1;\n    // between the twins\n    let a = 1;\n    a\n}\n",
        )
        .expect("write twin file");
        std::fs::write(
            tmp.path().join("mixed.rs"),
            "fn mixed() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n",
        )
        .expect("write mixed file");

        let pair = "(under block (seq (capture @x let_declaration) (capture @y let_declaration))";
        let with_unify = format!("{pair} (constraints (unify @x @y)))");
        let session = Session::new();

        let plain = session
            .scan(&ScanParams::new(format!("{pair})"), tmp.path()))
            .expect("constraint-free scan runs");
        assert_eq!(
            plain.matches.len(),
            2,
            "both files' adjacent pairs match without the constraint"
        );

        let unified = session
            .scan(&ScanParams::new(&with_unify, tmp.path()))
            .expect("unify scan runs");
        assert_eq!(
            unified.matches.len(),
            1,
            "only the fingerprint-equal pair survives"
        );
        assert_eq!(unified.matches[0].path, "twin.rs");
        assert_eq!(unified.matches[0].captures.len(), 2);
        assert!(
            session.cache().is_empty(),
            "the fingerprint table dies with the entry (streaming, ADR-0007)"
        );
    }

    /// F8 smoke: `scan --duplicates` groups the copied function across
    /// files (renamed locals break the group; comments and formatting do
    /// not), gates on `min_size`, and orders groups canonically. The full
    /// corpus/determinism/perf suite lives in tests/f8_duplicates.rs.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn duplicates_group_copies_across_files_canonically() {
        let twin = "fn helper(v: u32) -> u32 {\n    let a = v + 1;\n    a\n}\n";
        let tmp = tempfile::tempdir().expect("tempdir");
        // a.rs: the original; b.rs: a comment-abused, reformatted copy.
        std::fs::write(tmp.path().join("a.rs"), twin).expect("write a");
        std::fs::write(
            tmp.path().join("b.rs"),
            "// copied from a.rs\nfn helper(v: u32) -> u32 { let a = v+1; a }\n",
        )
        .expect("write b");
        // c.rs: the same shape with renamed locals — NOT a duplicate.
        std::fs::write(
            tmp.path().join("c.rs"),
            "fn helper(input: u32) -> u32 {\n    let acc = input + 1;\n    acc\n}\n",
        )
        .expect("write c");
        std::fs::write(tmp.path().join("notes.md"), "not code\n").expect("write md");

        let session = Session::new();
        let report = session
            .duplicates(&DuplicatesParams::new(tmp.path()))
            .expect("duplicates scan runs");
        assert_eq!(report.kind, "duplicates");
        assert_eq!(report.stats.files, 3, "a, b, c scanned (md unrouted)");
        assert_eq!(report.stats.langs["rust"], 3);

        // The helper-copy group: exactly the a.rs/b.rs function_items.
        let paired: Vec<&DuplicateGroup> = report
            .groups
            .iter()
            .filter(|g| g.members.len() == 2)
            .collect();
        assert!(
            paired.iter().any(|g| g
                .members
                .iter()
                .all(|m| m.kind.as_deref() == Some("function_item"))),
            "the cross-file identical pair groups (comments/formatting ignored)"
        );
        assert!(
            report
                .groups
                .iter()
                .flat_map(|g| &g.members)
                .all(|m| m.path != "c.rs"),
            "renamed locals never group (c.rs absent from every group)"
        );
        // Canonical order + ids: group ids are 0..n in output order and
        // every member carries its group's id and fingerprint.
        for (i, g) in report.groups.iter().enumerate() {
            assert_eq!(g.id, i as u32);
            for m in &g.members {
                assert_eq!(m.group, g.id);
                assert_eq!(m.fingerprint, g.fingerprint);
                assert_eq!(m.fingerprint.len(), 32, "128-bit hex");
                assert!(m.captures.is_empty());
            }
        }
        // The min-size gate bites: cranked past every subtree, no groups.
        let mut huge = DuplicatesParams::new(tmp.path());
        huge.min_size = 10_000;
        let empty = session.duplicates(&huge).expect("scan runs");
        assert!(empty.groups.is_empty());
        assert!(session.cache().is_empty(), "streaming, ADR-0007");
    }
}
