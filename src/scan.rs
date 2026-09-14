//! `scan` — the vertical slice's orchestrator (PLAN §3 F4.6, §1.6; ADR-0016,
//! ADR-0007's streaming discipline): rule in, match coordinates out,
//! rayon-parallel, deterministic.
//!
//! Pipeline: walk → classify → compile-per-language → rayon match →
//! canonical assembly.
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
    // name this build does not embed).
    let allowed: Option<LangId> = match &params.lang_filter {
        Some(name) => Some(find_lang(registry, name)?),
        None => None,
    };

    // Walk (base read_dir only) + optional virtual union, canonicalized to
    // the absolute root so the union dedups and display paths can be
    // root-relative. A file root scans just that file — the natural
    // single-file invocation.
    let root_abs = std::fs::canonicalize(&params.root).unwrap_or_else(|_| params.root.clone());
    let single_file = matches!(overlay.stat(&root_abs), Ok(stat) if stat.is_file);
    let mut files = if single_file {
        BTreeSet::from([root_abs.clone()])
    } else {
        walk_files(overlay, &root_abs)?
    };
    let virtual_set: HashSet<PathBuf> = if params.include_virtual {
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
            params.root.display().to_string()
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
        if size > params.max_file_size {
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
                let bytes = overlay.read_file(&candidate.path)?;
                let sniff_len = bytes.len().min(BINARY_SNIFF_BYTES);
                if bytes[..sniff_len].contains(&0) {
                    let display = candidate.display.clone();
                    return Ok((
                        candidate,
                        Outcome::Skip(SkipRecord {
                            path: display,
                            reason: "binary",
                        }),
                    ));
                }

                let key = crate::vfs::VfsPath::new(&candidate.path)?;
                let generation = overlay.current_gen(&candidate.path)?;
                let entry = match cache.get(&key, generation, &bytes) {
                    Some(hit) => hit,
                    None => {
                        // Parse with this worker's parser, materialized
                        // lazily — one per language per worker (§1.2).
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

                let mut reports = Vec::new();
                let display = SmolStr::from(candidate.display.as_str());
                Matcher::new(program, &entry.tree, 0).matches(&display, &mut reports)?;
                matchset::flag_overlaps(&mut reports);

                // Collection is the only cache-touching moment; streaming
                // eviction keeps retention at the working set (ADR-0007).
                cache.insert(key.clone(), entry.clone());
                cache.take(&key);

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
}
