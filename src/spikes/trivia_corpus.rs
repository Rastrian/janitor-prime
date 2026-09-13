//! KP-6 (PLAN §5 KP-6, ADR-0010, §1.4.3 step 4, risk R4): which `Seq`
//! adjacency semantics matches real-world intent — **byte-strict** sibling
//! adjacency (a comment between siblings breaks the sequence) or
//! **comment-skipping named-sibling** adjacency (ADR-0010's default)?
//!
//! Five curated real-world Rust idioms run under both semantics over
//! (a) crafted snippets with hand-computed ground truth — each idiom once
//! clean and once with a comment deliberately interleaved between exactly
//! the pattern siblings — and (b) this repository's own `src/` (real,
//! comment-heavy code; the tree-sitter crate's fixtures are not vendored).
//! The memo decides whether ADR-0010 stands or flips before the F4
//! evaluator fixes the semantics.
//!
//! Trivia class per the rust grammar: `line_comment` / `block_comment`
//! (there is no `comment` kind — verified against tree-sitter-rust 0.24.2
//! `node-types.json`). "Strict" counts adjacency over the container's named
//! children as-is; "skip" removes trivia-class children first. Both
//! operate on named siblings only — that part is shared by both of
//! ADR-0010's modes.

use super::SpikeError;
use serde::Serialize;

/// Trivia-class kinds for the rust grammar (KP-5's per-grammar trivia
/// table starts here).
const TRIVIA_KINDS: &[&str] = &["line_comment", "block_comment"];

fn is_trivia(node: tree_sitter::Node<'_>) -> bool {
    TRIVIA_KINDS.contains(&node.kind())
}

/// One adjacency idiom: a container kind plus the child-kind sequence that
/// must appear consecutively. `"_"` is a wildcard element (any named
/// child), as in the DSL.
struct Pattern {
    name: &'static str,
    container: &'static str,
    elements: &'static [&'static str],
    intent: &'static str,
}

/// The five curated real-world patterns (F1 brief): statement adjacency,
/// match-arm adjacency, attribute-on-item, method-chain siblings, if/else
/// adjacency. The method-chain link is the field_expression shape of a
/// chained call in the rust grammar: base call_expression immediately
/// followed by the next method's field_identifier.
const PATTERNS: &[Pattern] = &[
    Pattern {
        name: "let-statement-adjacency",
        container: "block",
        elements: &["let_declaration", "let_declaration"],
        intent: "two consecutive let statements in a body",
    },
    Pattern {
        name: "match-arm-adjacency",
        container: "match_block",
        elements: &["match_arm", "match_arm"],
        intent: "neighboring match arms",
    },
    Pattern {
        name: "attribute-on-item",
        container: "source_file",
        elements: &["attribute_item", "function_item"],
        intent: "an attribute attached to a function item",
    },
    Pattern {
        name: "method-chain-link",
        container: "field_expression",
        elements: &["call_expression", "field_identifier"],
        intent: "a chained method call: base call, then the next method",
    },
    Pattern {
        name: "if-else-adjacency",
        container: "if_expression",
        elements: &["_", "block", "else_clause"],
        intent: "an if with an else clause",
    },
];

/// Count matches of `pattern` in `node`'s subtree under one adjacency
/// semantics. Recursive walk; at each container node the named-children
/// list is (or is not) trivia-filtered, then scanned for consecutive runs.
fn count_pattern(node: tree_sitter::Node<'_>, pattern: &Pattern, skip_trivia: bool) -> u64 {
    let mut hits = 0_u64;
    if node.kind() == pattern.container {
        let mut cursor = node.walk();
        let children: Vec<tree_sitter::Node<'_>> = node
            .named_children(&mut cursor)
            .filter(|child| !skip_trivia || !is_trivia(*child))
            .collect();
        for start in 0..children.len() {
            if children.len() - start < pattern.elements.len() {
                break;
            }
            let run_matches = pattern
                .elements
                .iter()
                .enumerate()
                .all(|(k, el)| *el == "_" || children[start + k].kind() == *el);
            if run_matches {
                hits += 1;
            }
        }
    }
    let mut cursor = node.walk();
    let mut sub_hits = 0_u64;
    for child in node.children(&mut cursor) {
        sub_hits += count_pattern(child, pattern, skip_trivia);
    }
    hits + sub_hits
}

fn counts(tree: &tree_sitter::Tree, pattern: &Pattern) -> (u64, u64) {
    let root = tree.root_node();
    (
        count_pattern(root, pattern, false),
        count_pattern(root, pattern, true),
    )
}

/// A crafted snippet plus its hand-computed ground truth under both
/// semantics. Adjacency breakers (unrelated statements, extra arms) are
/// placed so the ONLY pattern-adjacent pairs are the clean one and the
/// comment-separated one — making the expected counts unambiguous.
struct Snippet {
    pattern: &'static str,
    source: &'static str,
    strict: u64,
    skip: u64,
    note: &'static str,
}

const SNIPPETS: &[Snippet] = &[
    Snippet {
        pattern: "let-statement-adjacency",
        source: r#"fn curated_let(v: u32) -> u32 {
    let a = v;
    let b = a;
    b;
    let c = 1;
    // comment between lets
    let d = 2;
    d
}
"#,
        strict: 1,
        skip: 2,
        note: "clean (a,b) pair; (c,d) pair separated by a line comment",
    },
    Snippet {
        pattern: "match-arm-adjacency",
        source: r#"fn curated_match(n: u32) -> u32 {
    match n {
        1 => n,
        // comment between arms
        2 => {
            3
        }
        _ => 4,
    }
}
"#,
        strict: 1,
        skip: 2,
        note: "clean (2,_) pair; (1,2) pair separated by a comment",
    },
    Snippet {
        pattern: "attribute-on-item",
        source: r#"#[derive(Debug)]
struct CuratedAttrPair {
    x: u32,
}

#[allow(dead_code)]
// comment between attribute and fn
fn curated_attr_fn() -> u32 {
    1
}
"#,
        strict: 0,
        skip: 1,
        note: "the only attr->fn pair is comment-separated; derive->struct does not match the pattern",
    },
    Snippet {
        pattern: "method-chain-link",
        source: r#"fn curated_chain(s: String) -> String {
    s.trim().to_lowercase().to_string()
        /* block comment inside the chain */
        .clone()
}
"#,
        strict: 2,
        skip: 3,
        note: ".to_lowercase/.to_string links clean; .clone link separated by a block comment",
    },
    Snippet {
        pattern: "if-else-adjacency",
        source: r#"fn curated_if(x: bool) -> u32 {
    let a = if x {
        1
    } else {
        2
    };
    let b = if !x { 3 } /* why */ else { 4 };
    a + b
}
"#,
        strict: 1,
        skip: 2,
        note: "first if/else clean; second separated by a block comment",
    },
];

#[derive(Serialize)]
pub struct PatternOutcome {
    pattern: String,
    strict: u64,
    skip: u64,
}

#[derive(Serialize)]
pub struct Outcome {
    curated: Vec<PatternOutcome>,
    repo_files: usize,
    repo_bytes: usize,
    repo: Vec<PatternOutcome>,
}

/// Collect this repo's `src/**/*.rs` (deterministic order).
fn repo_sources() -> Result<Vec<(String, Vec<u8>)>, SpikeError> {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    collect_rs_files(&manifest.join("src"), &mut paths)?;
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let bytes = std::fs::read(&path).map_err(|err| SpikeError::ParserSetup {
            message: format!("read {}: {err}", path.display()),
        })?;
        let rel = path
            .strip_prefix(&manifest)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        out.push((rel, bytes));
    }
    Ok(out)
}

fn collect_rs_files(
    dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<(), SpikeError> {
    let entries = std::fs::read_dir(dir).map_err(|err| SpikeError::ParserSetup {
        message: format!("read_dir {}: {err}", dir.display()),
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| SpikeError::ParserSetup {
            message: format!("read_dir {}: {err}", dir.display()),
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Run the KP-6 experiment.
pub fn run() -> Result<Outcome, SpikeError> {
    let mut parser = super::rust_parser()?;
    let language = crate::ast::ffi_shim::rust_language();

    // Corpus soundness: every kind the patterns name must exist in the
    // grammar (0 = unknown would silently zero every count).
    let mut kinds: Vec<&str> = TRIVIA_KINDS.to_vec();
    for pattern in PATTERNS {
        kinds.push(pattern.container);
        kinds.extend(pattern.elements.iter().copied());
    }
    for kind in kinds {
        if kind != "_" && language.id_for_node_kind(kind, true) == 0 {
            return Err(SpikeError::AnchorMissing {
                needle: kind.to_string(),
            });
        }
    }

    // (a) curated ground truth.
    let mut curated = Vec::new();
    for snippet in SNIPPETS {
        let pattern = PATTERNS
            .iter()
            .find(|p| p.name == snippet.pattern)
            .ok_or_else(|| SpikeError::AnchorMissing {
                needle: snippet.pattern.to_string(),
            })?;
        let tree = crate::ast::ffi_shim::parse_bytes(&mut parser, snippet.source.as_bytes(), None)
            .ok_or_else(|| SpikeError::NoTree {
                what: snippet.pattern.to_string(),
            })?;
        if tree.root_node().has_error() {
            return Err(SpikeError::CorrectnessDivergence {
                detail: format!("{}: curated snippet does not parse clean", snippet.pattern),
            });
        }
        let (strict, skip) = counts(&tree, pattern);
        curated.push(PatternOutcome {
            pattern: snippet.pattern.to_string(),
            strict,
            skip,
        });
    }

    // (b) the repo's real sources.
    let sources = repo_sources()?;
    let mut repo: Vec<PatternOutcome> = PATTERNS
        .iter()
        .map(|p| PatternOutcome {
            pattern: p.name.to_string(),
            strict: 0,
            skip: 0,
        })
        .collect();
    let mut repo_bytes = 0_usize;
    for (path, bytes) in &sources {
        repo_bytes += bytes.len();
        let Some(tree) = crate::ast::ffi_shim::parse_bytes(&mut parser, bytes, None) else {
            return Err(SpikeError::NoTree { what: path.clone() });
        };
        if tree.root_node().has_error() {
            // Real code that does not parse clean under this grammar would
            // skew adjacency counts; report it rather than skip silently.
            return Err(SpikeError::CorrectnessDivergence {
                detail: format!("{path}: repo source has ERROR/MISSING nodes"),
            });
        }
        for (i, pattern) in PATTERNS.iter().enumerate() {
            let (strict, skip) = counts(&tree, pattern);
            repo[i].strict += strict;
            repo[i].skip += skip;
        }
    }

    Ok(Outcome {
        curated,
        repo_files: sources.len(),
        repo_bytes,
        repo,
    })
}

/// KP-6 entry point. Ground truth is asserted exactly per snippet (the
/// hand-computed strict/skip counts in `SNIPPETS`); the repo run prints
/// the real-world divergence table that the memo's verdict rests on.
#[test]
fn trivia_adjacency_semantics_measured() {
    let outcome = run().expect("corpora parse and measurement completes");

    for (got, want) in outcome.curated.iter().zip(SNIPPETS) {
        assert_eq!(
            got.strict, want.strict,
            "{}: strict count must match hand-computed ground truth ({})",
            got.pattern, want.note
        );
        assert_eq!(
            got.skip, want.skip,
            "{}: comment-skipping count must match ground truth ({})",
            got.pattern, want.note
        );
    }
    for got in &outcome.repo {
        assert!(
            got.skip >= got.strict,
            "{}: skipping comments can only add matches",
            got.pattern
        );
    }

    println!("KP-6 adjacency semantics - curated ground truth:");
    for (got, want) in outcome.curated.iter().zip(SNIPPETS) {
        println!(
            "  {:>26}  strict {}  skip {}   ({})",
            got.pattern, got.strict, got.skip, want.note
        );
    }
    println!(
        "repo corpus: {} files, {} bytes (this repo's src/)",
        outcome.repo_files, outcome.repo_bytes
    );
    for got in &outcome.repo {
        let loss = if got.skip > 0 {
            100.0 * (got.skip - got.strict) as f64 / got.skip as f64
        } else {
            0.0
        };
        println!(
            "  {:>26}  strict {:>5}  skip {:>5}  strict-loss {:.1}%",
            got.pattern, got.strict, got.skip, loss
        );
    }

    let json = serde_json::to_string_pretty(&outcome).expect("outcome serializes");
    // Tests must not dirty the worktree: the committed benches/spike-kp6-outcome.json
    // is the FROZEN F1 evidence (its repo corpus is this repo's src/, which grows every
    // phase). Live runs write to temp; re-measurement updates the artifact deliberately.
    let path = std::env::temp_dir().join("janitor-spike-kp6-outcome.json");
    std::fs::write(&path, json).expect("write outcome json");
    println!("wrote {}", path.display());
}
