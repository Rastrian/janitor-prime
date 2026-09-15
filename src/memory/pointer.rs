//! `SyntaxPointer` — the anti-hallucination anchor (ADR-0012, §1.5.3).
//!
//! Raw tree node ids dangle after any re-parse by design, so facts anchor
//! to *content*, not identity:
//!
//! - [`SyntaxPointer::Node`] carries `{path, span, kind_name,
//!   content_sha}` — a half-open UTF-8 byte span into the file, the node
//!   kind it named, and the hash of exactly those bytes;
//! - [`SyntaxPointer::Commit`] carries a commit sha validated through
//!   `git rev-parse --verify <sha>^{commit}` as a subprocess (no git2/gix
//!   in v0.1 — ADR-0013).
//!
//! Resolution of a `Node` pointer against the current tree is three-way:
//!
//! 1. **exact** — a node at the same span whose kind still matches and
//!    whose bytes still hash to `content_sha` ⇒ anchored, `reanchored:
//!    false`;
//! 2. **re-anchor** — no exact hit, but a same-kind node with the same
//!    content hash exists near the old span (within
//!    [`REANCHOR_MAX_DRIFT`] bytes) ⇒ anchored at the new span,
//!    `reanchored: true` with the drift distance; the stored pointer is
//!    updated to the new coordinates so the next resolution is exact;
//! 3. otherwise **dangling** — the typed [`MemoryError::PointerDangling`]
//!    refusal. Facts that cannot be anchored are never silently accepted
//!    (OptMem has no anchors at all; this is our addition, A.2).
//!
//! The CLI notation is `<path>:<start>-<end>:<kind_name>` (byte offsets,
//! e.g. `src/lib.rs:120-480:function_item`) or `--commit <sha>`;
//! `docs/SCHEMA.md` freezes it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::ast::ffi_shim::Node;
use crate::ast::lang::LangRegistry;
use crate::ast::parse::parse_snapshot;
use crate::error::{JanitorError, MemoryError};

/// How far a re-anchor search may drift from the old span (bytes). Bounds
/// the "search the current tree near the old span" cost of ADR-0012 —
/// beyond this, the pointer is treated as dangling.
pub const REANCHOR_MAX_DRIFT: u64 = 4096;

/// The pointer a fact carries (ADR-0012). Serialized untagged: the two
/// variants have disjoint field sets, and the log's `ptr` object is the
/// PLAN §1.5.1 shape verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SyntaxPointer {
    /// A structural anchor, re-anchorable across edits that move it.
    Node {
        /// Repo-relative path, as spelled at `note` time.
        path: String,
        /// Half-open `[start, end)` UTF-8 byte span at `note` time.
        span: (u64, u64),
        /// tree-sitter kind name of the anchored node.
        kind_name: String,
        /// Hash of the anchored node's bytes (SipHash-1-3, 16 lowercase
        /// hex — the same convention as apply's `expect_sha`).
        content_sha: String,
    },
    /// A commit anchor, validated via the `git` subprocess.
    Commit { sha: String },
}

impl SyntaxPointer {
    /// Human/CLI rendering (the notation [`parse_notation`] accepts).
    pub fn to_notation(&self) -> String {
        match self {
            Self::Node {
                path,
                span,
                kind_name,
                ..
            } => format!("{path}:{}-{}:{kind_name}", span.0, span.1),
            Self::Commit { sha } => sha.clone(),
        }
    }
}

/// Parse the CLI pointer notation `<path>:<start>-<end>:<kind>` (split from
/// the right, so the path may itself contain `:`). Malformed input is the
/// typed [`MemoryError::PointerNotation`] — distinct from dangling, which
/// means the notation parsed but will not anchor.
pub fn parse_notation(notation: &str) -> Result<SyntaxPointer, MemoryError> {
    let bad = |detail: String| MemoryError::PointerNotation { detail };
    let (head, kind) = notation
        .rsplit_once(':')
        .ok_or_else(|| bad("expected <path>:<start>-<end>:<kind>".to_owned()))?;
    let (path, span) = head
        .rsplit_once(':')
        .ok_or_else(|| bad("expected <path>:<start>-<end>:<kind>".to_owned()))?;
    if path.is_empty() || kind.is_empty() {
        return Err(bad("path and kind_name must be non-empty".to_owned()));
    }
    let (start, end) = span
        .split_once('-')
        .ok_or_else(|| bad(format!("span {span:?} is not <start>-<end>")))?;
    let start: u64 = start
        .parse()
        .map_err(|_| bad(format!("span start {start:?} is not a byte offset")))?;
    let end: u64 = end
        .parse()
        .map_err(|_| bad(format!("span end {end:?} is not a byte offset")))?;
    if start >= end {
        return Err(bad(format!("span [{start}, {end}) is empty or inverted")));
    }
    Ok(SyntaxPointer::Node {
        path: path.to_owned(),
        span: (start, end),
        kind_name: kind.to_owned(),
        content_sha: String::new(),
    })
}

/// The result of anchoring a `Node` pointer to the current tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorOutcome {
    /// Whether the pointer moved (re-anchor) or sits exactly where it did.
    pub reanchored: bool,
    /// `|new_start - old_start|` in bytes (0 on an exact hit).
    pub drift: u64,
    /// The pointer to store: exact hit ⇒ unchanged; re-anchor ⇒ updated
    /// span (content hash and kind are what matched, so they carry over).
    pub anchored: SyntaxPointer,
}

/// What a `note --ptr` argument can be (the CLI accepts both, SCHEMA.md):
/// the compact notation (stamped fresh from the current tree — the
/// anti-hallucination gate is "this span+kind must be a real node *now*"),
/// or a full stored pointer as JSON (the re-anchorable form — spans may
/// have moved, the content hash is what tracks them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliPointer {
    /// `<path>:<start>-<end>:<kind>` — stamped at note time.
    Fresh {
        path: String,
        span: (u64, u64),
        kind_name: String,
    },
    /// A serialized [`SyntaxPointer`] (typically echoed from an earlier
    /// note report) — resolved through the full three-way logic.
    Stored(SyntaxPointer),
}

/// Parse a `--ptr` argument: JSON object ⇒ [`CliPointer::Stored`],
/// otherwise the compact notation ⇒ [`CliPointer::Fresh`].
pub fn parse_cli_pointer(arg: &str) -> Result<CliPointer, MemoryError> {
    if arg.trim_start().starts_with('{') {
        return serde_json::from_str::<SyntaxPointer>(arg)
            .map(CliPointer::Stored)
            .map_err(|err| MemoryError::PointerNotation {
                detail: format!("not a valid pointer JSON object ({err})"),
            });
    }
    match parse_notation(arg)? {
        SyntaxPointer::Node {
            path,
            span,
            kind_name,
            ..
        } => Ok(CliPointer::Fresh {
            path,
            span,
            kind_name,
        }),
        SyntaxPointer::Commit { .. } => Err(MemoryError::PointerNotation {
            detail: "use --commit <sha> for commit pointers".to_owned(),
        }),
    }
}

/// Stamp a fresh pointer from the current tree (the notation flow): the
/// span+kind must hit a real node *exactly* — spans come from janitor's
/// own reports (scan matches, note echoes), never from memory — and the
/// content hash is written from that node's bytes. No hit ⇒ the typed
/// dangling refusal: a hallucinated span is refused, not repaired.
pub fn stamp_node_pointer(
    registry: &LangRegistry,
    path: &str,
    span: (u64, u64),
    kind_name: &str,
    bytes: &Arc<[u8]>,
) -> Result<SyntaxPointer, JanitorError> {
    let lang = registry
        .for_path(Path::new(path))
        .ok_or_else(|| MemoryError::PointerNotation {
            detail: format!(
                "no enabled grammar parses {path:?}; a node pointer must target a \
                 parseable file"
            ),
        })?;
    let mut parser = registry.parser(lang)?;
    let tree = parse_snapshot(&mut parser, bytes)?;

    for node in Preorder::new(tree.root_node()) {
        let range = node.byte_range();
        if (range.start as u64, range.end as u64) == span && node.kind() == kind_name {
            return Ok(SyntaxPointer::Node {
                path: path.to_owned(),
                span,
                kind_name: kind_name.to_owned(),
                content_sha: content_sha(&bytes[range]),
            });
        }
    }
    Err(MemoryError::PointerDangling {
        path: path.to_owned(),
        span,
    }
    .into())
}

/// Content hash of a node's bytes — identical convention to apply's
/// `expect_sha`/`content_sha` (SipHash-1-3 over the slice, 16 lowercase
/// hex): identity check, not a security property.
pub fn content_sha(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Resolve a `Node` pointer against the current tree of `path`'s bytes:
/// exact span+hash hit, re-anchor by content hash near the old span, or
/// the typed dangling refusal (ADR-0012). The file's language must have an
/// enabled grammar — a pointer into an unparseable file cannot be anchored
/// and is refused as invalid, not dangling.
pub fn resolve_node_pointer(
    registry: &LangRegistry,
    path: &str,
    span: (u64, u64),
    kind_name: &str,
    content_sha_expected: &str,
    bytes: &Arc<[u8]>,
) -> Result<AnchorOutcome, JanitorError> {
    let lang = registry
        .for_path(Path::new(path))
        .ok_or_else(|| MemoryError::PointerNotation {
            detail: format!(
                "no enabled grammar parses {path:?}; a node pointer must target a \
                 parseable file"
            ),
        })?;
    let mut parser = registry.parser(lang)?;
    let tree = parse_snapshot(&mut parser, bytes)?;

    let mut best: Option<(u64, (u64, u64))> = None; // (drift, new span)
    for node in Preorder::new(tree.root_node()) {
        if node.kind() != kind_name {
            continue;
        }
        let range = node.byte_range();
        if content_sha(&bytes[range.clone()]) != content_sha_expected {
            continue;
        }
        if (range.start as u64, range.end as u64) == span {
            // Exact span + kind + hash: anchored, no drift, nothing moves.
            return Ok(AnchorOutcome {
                reanchored: false,
                drift: 0,
                anchored: SyntaxPointer::Node {
                    path: path.to_owned(),
                    span,
                    kind_name: kind_name.to_owned(),
                    content_sha: content_sha_expected.to_owned(),
                },
            });
        }
        let drift = (range.start as i64 - span.0 as i64).unsigned_abs();
        if best.is_none_or(|(best_drift, _)| drift < best_drift) {
            best = Some((drift, (range.start as u64, range.end as u64)));
        }
    }

    match best {
        Some((drift, new_span)) if drift <= REANCHOR_MAX_DRIFT => Ok(AnchorOutcome {
            reanchored: true,
            drift,
            anchored: SyntaxPointer::Node {
                path: path.to_owned(),
                span: new_span,
                kind_name: kind_name.to_owned(),
                content_sha: content_sha_expected.to_owned(),
            },
        }),
        // No same-content node at all, or only one farther than the search
        // bound: the fact's target is gone as-written — refuse it.
        _ => Err(MemoryError::PointerDangling {
            path: path.to_owned(),
            span,
        }
        .into()),
    }
}

/// A preorder walk over every node (named and anonymous) — pointer
/// resolution visits one file per `note`, not the hot loop, so this is a
/// plain cursor walk; the pooled-cursor machinery stays where allocation
/// matters (`scan`).
struct Preorder<'tree> {
    stack: Vec<Node<'tree>>,
}

impl<'tree> Preorder<'tree> {
    fn new(root: Node<'tree>) -> Self {
        Self { stack: vec![root] }
    }
}

impl<'tree> Iterator for Preorder<'tree> {
    type Item = Node<'tree>;

    fn next(&mut self) -> Option<Node<'tree>> {
        let node = self.stack.pop()?;
        // Collect the children through one reused cursor, then push them in
        // reverse so the visit order stays left-to-right.
        let mut children = Vec::new();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                children.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        self.stack.extend(children.into_iter().rev());
        Some(node)
    }
}

/// Validate a commit pointer through `git rev-parse --verify <sha>^{commit}`
/// (subprocess, ADR-0013) and return the resolved full sha to store. The
/// input must be a hex object name (4–64 chars) — anything else is refused
/// before a process spawns, so no user string ever reaches git as a flag.
pub fn validate_commit(sha: &str, cwd: &Path) -> Result<String, MemoryError> {
    let invalid = |detail: String| MemoryError::CommitInvalid {
        sha: sha.to_owned(),
        detail,
    };
    let len = sha.len();
    if !(4..=64).contains(&len) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(
            "expected a hex object name (4-64 chars)".to_owned(),
        ));
    }
    let spec = format!("{}^{{commit}}", sha.to_ascii_lowercase());
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--verify")
        .arg(&spec)
        .output()
        .map_err(|err| invalid(format!("git subprocess failed: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(invalid(format!("git rev-parse rejected it: {stderr}")));
    }
    let resolved = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    if resolved.len() != 40 && resolved.len() != 64 {
        return Err(invalid(format!(
            "git rev-parse returned {resolved:?} (not a commit sha)"
        )));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The E_* code of a memory error, through the umbrella (tests speak
    /// the agent-facing contract).
    fn code(err: MemoryError) -> &'static str {
        JanitorError::from(err).code()
    }

    /// Notation round trip: path (colons tolerated), span, kind; the
    /// malformed shapes are the typed E_POINTER_INVALID.
    #[test]
    fn notation_parses_and_round_trips() {
        let ptr = parse_notation("src/lib.rs:120-480:function_item").expect("valid");
        let SyntaxPointer::Node {
            path,
            span,
            kind_name,
            ..
        } = &ptr
        else {
            unreachable!("node pointer");
        };
        assert_eq!(path, "src/lib.rs");
        assert_eq!(*span, (120, 480));
        assert_eq!(kind_name, "function_item");
        assert_eq!(ptr.to_notation(), "src/lib.rs:120-480:function_item");

        // Colons inside the path survive (split from the right).
        let odd = parse_notation("we:ird/path.rs:0-4:identifier").expect("path with colon");
        assert!(matches!(&odd, SyntaxPointer::Node { path, .. } if path == "we:ird/path.rs"));

        for bad in [
            "no-span-or-kind",
            "src/lib.rs:120:function_item",
            "src/lib.rs:480-120:function_item",
            "src/lib.rs:5-5:function_item",
            "src/lib.rs:abc-9:function_item",
            ":120-480:function_item",
            "src/lib.rs:120-480:",
        ] {
            let err = parse_notation(bad).unwrap_err();
            assert_eq!(code(err), "E_POINTER_INVALID", "input {bad:?}");
        }
    }

    /// Commit validation: non-hex or wrong-length input is refused without
    /// spawning git; hex input reaches git (covered end to end in
    /// `tests/memory_e2e.rs` against a real repository fixture).
    #[test]
    fn commit_validation_guards_before_spawning() {
        for bad in ["", "ab", "zzzz", "g1234"] {
            assert!(validate_commit(bad, Path::new(".")).is_err(), "{bad:?}");
        }
        let too_long = "a".repeat(65);
        assert!(validate_commit(&too_long, Path::new(".")).is_err());
        // 40 hex chars is well-formed: it must reach git (this test runs in
        // a git worktree, where 40 zeros resolves to no object → the typed
        // refusal, never a panic).
        let err = validate_commit(&"0".repeat(40), Path::new(".")).unwrap_err();
        assert_eq!(code(err), "E_COMMIT_INVALID");
    }

    /// The content hash convention matches apply's (one crate-wide
    /// convention, cross-checked against the reference implementation).
    #[test]
    fn content_sha_matches_the_apply_convention() {
        assert_eq!(
            content_sha(b"fn a() {}"),
            crate::apply::diff::content_sha(b"fn a() {}")
        );
        assert_eq!(content_sha(b"").len(), 16);
        assert_ne!(content_sha(b"a"), content_sha(b"b"));
    }

    // --- three-way resolution against real trees -------------------------------

    /// A corpus with two structurally identical, byte-identical functions
    /// (the duplicate-content re-anchor tiebreaker) and one unique one.
    #[cfg(feature = "lang-rust")]
    const CORPUS: &str = "fn twin() -> u32 {\n    7\n}\n\n\
                          fn spacer() -> u32 {\n    let probe = 2;\n    probe + 1\n}\n\n\
                          fn twin() -> u32 {\n    7\n}\n";

    /// The span and content sha of the `n`th function_item in CORPUS.
    #[cfg(feature = "lang-rust")]
    fn nth_function(registry: &LangRegistry, source: &Arc<[u8]>, n: usize) -> ((u64, u64), String) {
        let lang = registry.for_extension("rs").expect("rust grammar");
        let mut parser = registry.parser(lang).expect("parser");
        let tree = parse_snapshot(&mut parser, source).expect("corpus parses");
        let functions: Vec<_> = Preorder::new(tree.root_node())
            .filter(|node| node.kind() == "function_item")
            .collect();
        let node = functions[n];
        (
            (node.start_byte() as u64, node.end_byte() as u64),
            content_sha(&source[node.byte_range()]),
        )
    }

    /// Exact hit: a pointer taken from the live tree resolves unchanged.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn exact_hit_anchors_without_drift() {
        let registry = LangRegistry::new();
        let source: Arc<[u8]> = Arc::from(CORPUS.as_bytes());
        let (span, sha) = nth_function(&registry, &source, 1); // the unique spacer

        let outcome = resolve_node_pointer(&registry, "t.rs", span, "function_item", &sha, &source)
            .expect("exact hit");
        assert!(!outcome.reanchored);
        assert_eq!(outcome.drift, 0);
        assert_eq!(
            outcome.anchored.to_notation(),
            format!("t.rs:{}-{}:function_item", span.0, span.1)
        );
    }

    /// Re-anchor: bytes inserted before the node move it; the pointer with
    /// the old span re-anchors to the new coordinates with the drift
    /// reported, and the stored pointer is updated so the next resolution
    /// is exact.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn moved_content_reanchors_with_drift() {
        let registry = LangRegistry::new();
        let old: Arc<[u8]> = Arc::from(CORPUS.as_bytes());
        let (span, sha) = nth_function(&registry, &old, 1);

        let inserted = "fn newcomer() -> u32 {\n    11\n}\n\n";
        let mut moved = Vec::with_capacity(old.len() + inserted.len());
        moved.extend_from_slice(inserted.as_bytes());
        moved.extend_from_slice(&old);
        let moved: Arc<[u8]> = Arc::from(moved);

        let outcome = resolve_node_pointer(&registry, "t.rs", span, "function_item", &sha, &moved)
            .expect("re-anchor");
        assert!(outcome.reanchored);
        assert_eq!(outcome.drift, inserted.len() as u64);
        let SyntaxPointer::Node { span: new_span, .. } = outcome.anchored.clone() else {
            unreachable!("node pointer");
        };
        assert_eq!(new_span.0, span.0 + inserted.len() as u64);

        // The updated pointer is exact against the same bytes.
        let again =
            resolve_node_pointer(&registry, "t.rs", new_span, "function_item", &sha, &moved)
                .expect("updated pointer is exact");
        assert!(!again.reanchored && again.drift == 0);
    }

    /// Dangling: the anchored content no longer exists anywhere in the
    /// file ⇒ the typed E_POINTER_DANGLING refusal, never silent acceptance.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn changed_content_dangles_loudly() {
        let registry = LangRegistry::new();
        let old: Arc<[u8]> = Arc::from(CORPUS.as_bytes());
        let (span, sha) = nth_function(&registry, &old, 1);

        // The spacer's body is rewritten: no node hashes to `sha` anymore.
        let edited = CORPUS.replace("probe + 1", "probe * 3");
        let edited: Arc<[u8]> = Arc::from(edited.as_bytes());
        let err = resolve_node_pointer(&registry, "t.rs", span, "function_item", &sha, &edited)
            .expect_err("content gone");
        assert_eq!(err.code(), "E_POINTER_DANGLING");
        assert_eq!(
            err.details(),
            Some(serde_json::json!({ "path": "t.rs", "span": [span.0, span.1] }))
        );
    }

    /// Duplicate content re-anchors to the *nearest* same-hash node (the
    /// twins are byte-identical; the pointer keeps the closer one).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn duplicate_content_reanchors_to_the_nearest() {
        let registry = LangRegistry::new();
        let old: Arc<[u8]> = Arc::from(CORPUS.as_bytes());
        let (first_span, sha) = nth_function(&registry, &old, 0);
        let (second_span, _) = nth_function(&registry, &old, 2);
        assert_eq!(
            sha,
            content_sha(&old[first_span.0 as usize..first_span.1 as usize])
        );

        // Pointer at the first twin, resolved against the same bytes with a
        // stale span shifted past it: nearest twin wins.
        let outcome = resolve_node_pointer(
            &registry,
            "t.rs",
            (first_span.0 + 4, first_span.1 + 4),
            "function_item",
            &sha,
            &old,
        )
        .expect("re-anchor among duplicates");
        assert!(outcome.reanchored);
        let SyntaxPointer::Node { span, .. } = outcome.anchored else {
            unreachable!("node pointer");
        };
        let dist_first = span.0.abs_diff(first_span.0);
        let dist_second = span.0.abs_diff(second_span.0);
        assert!(dist_first <= dist_second, "nearest twin must win");
        assert_eq!(span, first_span, "the stale span is nearer the first twin");
    }

    /// A pointer into a file whose language has no enabled grammar is a
    /// notation-level refusal (E_POINTER_INVALID), not a dangling anchor.
    #[test]
    fn unparseable_target_is_invalid_not_dangling() {
        let registry = LangRegistry::new();
        let bytes: Arc<[u8]> = Arc::from(b"# just markdown\n".to_vec().into_boxed_slice());
        let err = resolve_node_pointer(&registry, "README.md", (0, 4), "heading", "x", &bytes)
            .expect_err("no grammar for .md");
        assert_eq!(err.code(), "E_POINTER_INVALID");
    }

    /// The CLI accepts both forms: compact notation (fresh stamp) and a
    /// full stored pointer as JSON (re-anchorable).
    #[test]
    fn cli_pointer_parses_notation_and_stored_json() {
        let fresh = parse_cli_pointer("src/lib.rs:120-480:function_item").expect("notation");
        assert_eq!(
            fresh,
            CliPointer::Fresh {
                path: "src/lib.rs".to_owned(),
                span: (120, 480),
                kind_name: "function_item".to_owned(),
            }
        );

        let stored_json = r#"{"path":"src/lib.rs","span":[120,480],"kind_name":"function_item","content_sha":"0f3ac1d8e5b24719"}"#;
        let stored = parse_cli_pointer(stored_json).expect("stored json");
        assert_eq!(
            stored,
            CliPointer::Stored(SyntaxPointer::Node {
                path: "src/lib.rs".to_owned(),
                span: (120, 480),
                kind_name: "function_item".to_owned(),
                content_sha: "0f3ac1d8e5b24719".to_owned(),
            })
        );
        let commit_json = r#"{"sha":"0123456789abcdef0123456789abcdef01234567"}"#;
        assert!(matches!(
            parse_cli_pointer(commit_json).expect("commit json"),
            CliPointer::Stored(SyntaxPointer::Commit { .. })
        ));

        // Broken JSON and bare notation mistakes stay typed.
        assert_eq!(
            code(parse_cli_pointer("{not json").unwrap_err()),
            "E_POINTER_INVALID"
        );
    }

    /// The stamp flow: an exact span+kind hit is stamped with the hash of
    /// the real bytes; a span that is not a node boundary dangles — a
    /// hallucinated span is refused, never repaired.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn stamping_takes_exact_hits_and_refuses_hallucinated_spans() {
        let registry = LangRegistry::new();
        let source: Arc<[u8]> = Arc::from(CORPUS.as_bytes());
        let (span, sha) = nth_function(&registry, &source, 1);

        let stamped =
            stamp_node_pointer(&registry, "t.rs", span, "function_item", &source).expect("stamp");
        assert_eq!(
            stamped,
            SyntaxPointer::Node {
                path: "t.rs".to_owned(),
                span,
                kind_name: "function_item".to_owned(),
                content_sha: sha,
            }
        );

        // One byte off is not a node boundary: dangling.
        let off = (span.0 + 1, span.1);
        let err = stamp_node_pointer(&registry, "t.rs", off, "function_item", &source)
            .expect_err("off-boundary span");
        assert_eq!(err.code(), "E_POINTER_DANGLING");

        // Wrong kind at the right span: dangling too.
        let err = stamp_node_pointer(&registry, "t.rs", span, "impl_item", &source)
            .expect_err("wrong kind");
        assert_eq!(err.code(), "E_POINTER_DANGLING");
    }
}
