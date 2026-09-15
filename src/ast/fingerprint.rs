//! Merkle fingerprints (PLAN §1.4.3 item 6, ADR-0010 as amended by E2; F8):
//! the per-parse bottom-up structural hash that defines janitor's code
//! equivalence — `Unify`'s O(1) check and `scan --duplicates`' grouping key.
//!
//! **What is hashed (the E2 rule):** every node's fingerprint is a 128-bit
//! SipHash-1-3 over `kind_id || child fingerprints || leaf text hash`, taken
//! over the **named-node skeleton PLUS semantically-significant anonymous
//! tokens** (binary operators, separators, punctuation, keywords) in sibling
//! order — excluding **only** comment/whitespace-class trivia kinds from the
//! per-grammar trivia table (KP-5/KP-6). Excluding *all* anonymous nodes
//! would conflate `a + b` with `a - b`, because operators and separators are
//! anonymous tokens (eval-1 F2); excluding none would make formatting and
//! comments change identity (eval-2 §6.3: byte-strict isomorphism detects
//! almost nothing real). Whitespace between tokens is not a node in any of
//! our grammars, so the trivia table is the whole exclusion list.
//!
//! Consequences, all load-bearing for `scan --duplicates`:
//! - identical subtrees hash equal wherever they sit (no positions hashed);
//! - comments and formatting differences do **not** change a fingerprint;
//! - renamed locals **do** (leaf text is hashed);
//! - `x + y` ≠ `x - y` and `[a, b]` ≠ `[a; b]` (anonymous tokens are hashed,
//!   by kind id and — for the separator itself — leaf text).
//!
//! **128-bit via two keyed SipHash-1-3 legs.** SipHash is a 64-bit PRF and
//! std's `DefaultHasher` exposes no 128-bit form, so a fingerprint is the
//! pair `(SipHash-1-3(k=0, ·), SipHash-1-3(k=K_ALT, ·))` over the same
//! framed fields: leg one keeps the zero-key convention of the repo's
//! `content_sha` (docs/SCHEMA.md), leg two uses a fixed arbitrary key, and
//! the pair is a Carter–Wegman ensemble with 2^-128-scale collision bounds
//! — the property duplicate grouping needs (a 64-bit hash would collide at
//! ~2^32 subtrees, uncomfortably near large-repo node counts). Both legs
//! share one implementation (round counts as const parameters), verified
//! against the SipHash reference vectors in the 2-4 mode below.
//!
//! **Cost discipline (KP-7's fence extends here, KP-8's too):** one pass per
//! parse, O(n) — a single postorder walk with pooled cursors, one hash per
//! node over `O(children)` fixed-width words. Storage is the result map only
//! (per-node digest vector + span lookup); no per-node heap allocations.
//! The pass is iterative, so pathological depth cannot overflow the stack.
//! The table is cacheable on [`crate::ast::cache::ParseEntry`] and computed
//! lazily on first use.

use std::collections::HashMap;

use crate::ast::ffi_shim::{Node, Tree, TreeCursor};

// --- SipHash-1-3 (the std DefaultHasher algorithm, round counts parametric) --

/// One SipHash round (the reference `SIPROUND`).
#[inline(always)]
fn sipround(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    *v0 = v0.wrapping_add(*v1);
    *v1 = v1.rotate_left(13);
    *v1 ^= *v0;
    *v0 = v0.rotate_left(32);
    *v2 = v2.wrapping_add(*v3);
    *v3 = v3.rotate_left(16);
    *v3 ^= *v2;
    *v0 = v0.wrapping_add(*v3);
    *v3 = v3.rotate_left(21);
    *v3 ^= *v0;
    *v2 = v2.wrapping_add(*v1);
    *v1 = v1.rotate_left(17);
    *v1 ^= *v2;
    *v2 = v2.rotate_left(32);
}

/// SipHash with `D` compression and `C` finalization rounds, streaming over
/// an arbitrary byte sequence. `SipHasher<1, 3>` is exactly std's
/// `DefaultHasher` algorithm (zero keys = `DefaultHasher::new()`); the
/// `<2, 4>` instantiation is the reference construction the public test
/// vectors describe. Little-endian throughout, per the reference.
struct SipHasher<const D: u32, const C: u32> {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    /// Buffered partial word (little-endian, low bytes first).
    tail: u64,
    /// Bytes currently buffered in `tail` (0..8).
    ntail: usize,
    /// Total bytes hashed (mod 2^64) — the final block's length byte.
    length: u64,
}

impl<const D: u32, const C: u32> SipHasher<D, C> {
    fn new(k0: u64, k1: u64) -> Self {
        Self {
            v0: k0 ^ 0x736f_6d65_7073_6575,
            v1: k1 ^ 0x646f_7261_6e64_6f6d,
            v2: k0 ^ 0x6c79_6765_6e65_7261,
            v3: k1 ^ 0x7465_6462_7974_6573,
            tail: 0,
            ntail: 0,
            length: 0,
        }
    }

    #[inline]
    fn block(&mut self, m: u64) {
        self.v3 ^= m;
        for _ in 0..D {
            sipround(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        }
        self.v0 ^= m;
    }

    #[inline]
    fn write_u8(&mut self, byte: u8) {
        self.tail |= (byte as u64) << (8 * self.ntail);
        self.ntail += 1;
        self.length += 1;
        if self.ntail == 8 {
            let m = self.tail;
            self.tail = 0;
            self.ntail = 0;
            self.block(m);
        }
    }

    /// Write a fixed-width unsigned integer little-endian (the framing
    /// convention of node hashing: kind ids, counts, child words).
    fn write_le(&mut self, mut value: u64, bytes: usize) {
        for _ in 0..bytes {
            self.write_u8(value as u8);
            value >>= 8;
        }
    }

    #[inline]
    fn write_u16(&mut self, value: u16) {
        self.write_le(value as u64, 2);
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.write_le(value as u64, 4);
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        if self.ntail == 0 {
            self.length += 8;
            self.block(value);
        } else {
            self.write_le(value, 8);
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u8(byte);
        }
    }

    fn finish(mut self) -> u64 {
        // The SipHash final block: buffered bytes, zero-padded, with the
        // total length in the top byte.
        let b = ((self.length & 0xff) << 56) | self.tail;
        self.v3 ^= b;
        for _ in 0..D {
            sipround(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        }
        self.v0 ^= b;
        self.v2 ^= 0xff;
        for _ in 0..C {
            sipround(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        }
        self.v0 ^ self.v1 ^ self.v2 ^ self.v3
    }
}

/// Leg-two key: an arbitrary fixed pair. Any constant distinct from the
/// zero key works; this one is pinned here so fingerprints are stable
/// across releases (they are identity, not a secret — the value just has
/// to never change).
const FP_ALT_K0: u64 = 0x6a6e_6974_6f72_2d66;
const FP_ALT_K1: u64 = 0x7072_696d_652d_6638;

/// A 128-bit structural fingerprint: `(zero-key leg, alt-key leg)`.
pub type Fp = u128;

/// Render a fingerprint as its canonical 32-hex-char form (the wire shape;
/// matches `content_sha`'s lowercase-hex convention).
pub fn fp_hex(fp: Fp) -> String {
    format!("{fp:032x}")
}

/// The per-node hasher: both SipHash-1-3 legs fed the same framed fields.
/// Framing is explicit (tag byte, fixed-width counts, fixed-width child
/// words) so no two field sequences can collide by concatenation.
struct NodeHasher {
    a: SipHasher<1, 3>,
    b: SipHasher<1, 3>,
}

impl NodeHasher {
    fn new() -> Self {
        Self {
            a: SipHasher::new(0, 0),
            b: SipHasher::new(FP_ALT_K0, FP_ALT_K1),
        }
    }

    /// Begin an internal node: tag, kind id, child count. Children follow
    /// via [`Self::child`].
    fn begin_internal(&mut self, kind_id: u16, child_count: u32) {
        self.write2_u8(1);
        self.write2_u16(kind_id);
        self.write2_u32(child_count);
    }

    /// Feed one child's fingerprint (two little-endian words).
    #[inline]
    fn child(&mut self, fp: Fp) {
        self.a.write_u64(fp as u64);
        self.b.write_u64(fp as u64);
        self.a.write_u64((fp >> 64) as u64);
        self.b.write_u64((fp >> 64) as u64);
    }

    /// Begin a leaf: tag, kind id, then the token's text (length-framed).
    fn begin_leaf(&mut self, kind_id: u16, text: &[u8]) {
        self.write2_u8(2);
        self.write2_u16(kind_id);
        self.write2_u32(text.len() as u32);
        self.a.write(text);
        self.b.write(text);
    }

    fn finish(self) -> Fp {
        ((self.a.finish() as Fp) << 64) | self.b.finish() as Fp
    }

    fn write2_u8(&mut self, v: u8) {
        self.a.write_u8(v);
        self.b.write_u8(v);
    }

    fn write2_u16(&mut self, v: u16) {
        self.a.write_u16(v);
        self.b.write_u16(v);
    }

    fn write2_u32(&mut self, v: u32) {
        self.a.write_u32(v);
        self.b.write_u32(v);
    }
}

/// Resolve a language's trivia (comment-class) kind ids — the per-grammar
/// table from [`crate::ast::lang`], the same source `Seq` adjacency skips
/// (ADR-0010), resolved once per scan/bench and shared by every fingerprint
/// pass over that language's files.
pub fn trivia_kind_ids(
    registry: &crate::ast::lang::LangRegistry,
    lang: crate::ast::lang::LangId,
) -> Vec<u16> {
    registry
        .spec(lang)
        .map(|spec| spec.trivia_kinds())
        .unwrap_or_default()
        .iter()
        .filter_map(|kind| registry.kind_id(lang, kind))
        .collect()
}

/// One participating node's fingerprint and coordinates, as plain data
/// (Rule T3). `size` is the subtree's **participating node count** —
/// formatting- and comment-invariant, unlike the byte span — and is the
/// unit `scan --duplicates`' minimum-size gate uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanDigest {
    pub fp: Fp,
    pub start: u32,
    pub end: u32,
    pub kind_id: u16,
    pub size: u32,
}

impl SpanDigest {
    /// The node's byte-span length (formatting-sensitive; reporting only).
    pub fn span_bytes(&self) -> u32 {
        self.end - self.start
    }
}

/// The fingerprint table for one parse: every participating node's digest
/// plus the span→fingerprint lookup `Unify`'s post-match filter needs.
/// Digests appear in bottom-up completion order (children before parents);
/// output-side consumers never rely on this table's iteration order — they
/// sort canonically (ADR-0016 discipline).
#[derive(Debug)]
pub struct FingerprintTable {
    digests: Vec<SpanDigest>,
    by_span: HashMap<(u32, u32, u16), Fp>,
}

impl FingerprintTable {
    /// The one-pass Merkle walk. `trivia` is the language's resolved
    /// comment-class kind ids ([`trivia_kind_ids`]); a node with a trivia
    /// kind — and its whole subtree — is invisible to fingerprinting
    /// (comments are leaves in every current grammar; the exclusion is
    /// written subtree-wise regardless).
    pub fn build(tree: &Tree, bytes: &[u8], trivia: &[u16]) -> Self {
        // Preorder node collection with sibling-linked children (u32 index
        // arrays — no per-node allocation), then one reverse pass that
        // finalizes each node's hash from its children's already-final
        // hashes. The work stack is explicit: no recursion, so depth is
        // bounded by memory, not the call stack.
        let mut nodes: Vec<Node<'_>> = Vec::new();
        let mut fps: Vec<Fp> = Vec::new();
        let mut sizes: Vec<u32> = Vec::new();
        let mut first_child: Vec<u32> = Vec::new();
        let mut next_sibling: Vec<u32> = Vec::new();
        let mut last_child: Vec<u32> = Vec::new();
        let mut child_buf: Vec<Node<'_>> = Vec::new();
        let mut cursors: Vec<TreeCursor<'_>> = Vec::new();

        enum Visit<'t> {
            Pre(Node<'t>),
            Post(usize),
        }
        let mut stack: Vec<Visit<'_>> = Vec::new();
        // Path of open node indexes (top = the parent new children link to).
        let mut open: Vec<u32> = Vec::new();
        stack.push(Visit::Pre(tree.root_node()));

        let mut digests: Vec<SpanDigest> = Vec::new();
        let mut by_span: HashMap<(u32, u32, u16), Fp> = HashMap::new();

        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Pre(node) => {
                    let idx = nodes.len() as u32;
                    nodes.push(node);
                    fps.push(0);
                    sizes.push(0);
                    first_child.push(u32::MAX);
                    next_sibling.push(u32::MAX);
                    last_child.push(u32::MAX);
                    if let Some(&parent) = open.last() {
                        let parent = parent as usize;
                        if first_child[parent] == u32::MAX {
                            first_child[parent] = idx;
                        } else {
                            let prev = last_child[parent];
                            next_sibling[prev as usize] = idx;
                        }
                        last_child[parent] = idx;
                    }

                    // Enumerate participating children through a pooled
                    // cursor (Rule T5; the KP-8 discipline this pass keeps).
                    child_buf.clear();
                    let mut cursor = match cursors.pop() {
                        Some(mut spare) => {
                            spare.reset(node);
                            spare
                        }
                        None => node.walk(),
                    };
                    for child in node.children(&mut cursor) {
                        if !trivia.contains(&child.kind_id()) {
                            child_buf.push(child);
                        }
                    }
                    cursors.push(cursor);

                    stack.push(Visit::Post(idx as usize));
                    for child in child_buf.iter().rev() {
                        stack.push(Visit::Pre(*child));
                    }
                    open.push(idx);
                }
                Visit::Post(idx) => {
                    let _ = open.pop(); // this node's subtree is complete
                    let node = nodes[idx];
                    let kind_id = node.kind_id();
                    let start = node.start_byte() as u32;
                    let end = node.end_byte() as u32;
                    let fc = first_child[idx];

                    let mut hasher = NodeHasher::new();
                    let size = if fc == u32::MAX {
                        hasher.begin_leaf(kind_id, &bytes[node.start_byte()..node.end_byte()]);
                        1
                    } else {
                        // Count first (the framing carries the child count),
                        // then feed each child's fingerprint in sibling
                        // order — order is semantic: `a + b` ≠ `b + a`.
                        let mut count = 0_u32;
                        let mut total = 1_u32;
                        let mut c = fc;
                        while c != u32::MAX {
                            count += 1;
                            total += sizes[c as usize];
                            c = next_sibling[c as usize];
                        }
                        hasher.begin_internal(kind_id, count);
                        let mut c = fc;
                        while c != u32::MAX {
                            hasher.child(fps[c as usize]);
                            c = next_sibling[c as usize];
                        }
                        total
                    };
                    let fp = hasher.finish();
                    fps[idx] = fp;
                    sizes[idx] = size;
                    digests.push(SpanDigest {
                        fp,
                        start,
                        end,
                        kind_id,
                        size,
                    });
                    // Captures address nodes by (span, kind); zero-width
                    // same-kind nodes are degenerate and intentionally
                    // collapse (last writer wins — such spans carry no
                    // structure to unify).
                    by_span.insert((start, end, kind_id), fp);
                }
            }
        }

        Self { digests, by_span }
    }

    /// Every participating node's digest, bottom-up (children before
    /// parents). Deterministic for a given parse.
    pub fn digests(&self) -> &[SpanDigest] {
        &self.digests
    }

    /// The fingerprint of the node at `(start, end, kind_id)`, if the parse
    /// has one — the `Unify` filter's lookup over capture coordinates.
    pub fn fp_of(&self, start: u32, end: u32, kind_id: u16) -> Option<Fp> {
        self.by_span.get(&(start, end, kind_id)).copied()
    }

    /// Number of participating nodes (the fingerprinted skeleton).
    pub fn participating(&self) -> usize {
        self.digests.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::LangRegistry;
    use crate::ast::parse::parse_snapshot;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    use std::sync::Arc;

    /// The SipHash reference vectors (veorq/SipHash test set): key bytes
    /// `00 01 … 0f`, little-endian word pair, under the 2-4 round counts.
    /// Anchor the core before trusting the faster 1-3 production mode.
    #[test]
    fn siphash_core_matches_the_reference_vectors() {
        let (k0, k1) = (0x0706_0504_0302_0100_u64, 0x0f0e_0d0c_0b0a_0908_u64);
        let mut h = SipHasher::<2, 4>::new(k0, k1);
        h.write(b"");
        assert_eq!(h.finish(), 0x726f_db47_dd0e_0e31);
        let mut h = SipHasher::<2, 4>::new(k0, k1);
        h.write(&[0]);
        assert_eq!(h.finish(), 0x74f8_39c5_93dc_67fd);
    }

    /// The 1-3 mode over zero keys is byte-for-byte std's `DefaultHasher`
    /// write path (the repo's documented content_sha convention) — the
    /// streaming implementation agrees with std's on every alignment of the
    /// 8-byte word boundary, including the length-terminated final block.
    #[test]
    fn siphash_1_3_zero_key_agrees_with_std_default_hasher() {
        let cases: [&[u8]; 7] = [
            b"",
            b"a",
            b"janitor",
            b"01234567",
            b"012345678",
            b"0123456789abcde",
            b"the quick brown fox jumps over the lazy dog, twice over",
        ];
        for case in cases {
            let mut std_hasher = DefaultHasher::new();
            std_hasher.write(case);
            let mut ours = SipHasher::<1, 3>::new(0, 0);
            ours.write(case);
            assert_eq!(ours.finish(), std_hasher.finish(), "{case:?}");
        }
    }

    /// Fixed-width writes are little-endian byte writes (the framing the
    /// reference and std share): interleaved u16/u32/u64 feeds agree with
    /// the same bytes fed through `write`.
    #[test]
    fn fixed_width_writes_are_little_endian_bytes() {
        let mut composed = SipHasher::<1, 3>::new(0, 0);
        composed.write_u16(0xabcd);
        composed.write_u32(0xdead_beef);
        composed.write_u64(0x0123_4567_89ab_cdef);
        let mut by_bytes = SipHasher::<1, 3>::new(0, 0);
        by_bytes.write(&[0xcd, 0xab, 0xef, 0xbe, 0xad, 0xde, 0xef, 0xcd]);
        by_bytes.write(&[0xab, 0x89, 0x67, 0x45, 0x23, 0x01]);
        assert_eq!(composed.finish(), by_bytes.finish());
    }

    // --- structural semantics (the T1 contract) ------------------------------

    #[cfg(feature = "lang-rust")]
    fn built(src: &str) -> (String, FingerprintTable) {
        let registry = LangRegistry::new();
        let lang = registry
            .for_extension("rs")
            .expect("lang-rust is the default feature set");
        let mut parser = registry.parser(lang).expect("ABI-verified pair");
        let bytes: Arc<[u8]> = Arc::from(src.as_bytes());
        let tree = parse_snapshot(&mut parser, &bytes).expect("test source parses");
        let trivia = trivia_kind_ids(&registry, lang);
        (
            src.to_owned(),
            FingerprintTable::build(&tree, &bytes, &trivia),
        )
    }

    /// The digest of the largest proper subtree starting exactly at
    /// `needle`'s first occurrence (test addressing into real parse shapes
    /// — several nodes share a start offset; the max-size proper one is the
    /// subtree the needle names, excluding the root, which spans the whole
    /// file and would otherwise win any `find` at offset 0).
    #[cfg(feature = "lang-rust")]
    fn digest_starting_at(
        (src, table): &(String, FingerprintTable),
        needle: &str,
    ) -> Option<SpanDigest> {
        let total = table.participating();
        table
            .digests()
            .iter()
            .filter(|d| {
                d.start as usize == src.find(needle).unwrap_or(usize::MAX)
                    && d.end as usize > d.start as usize
                    && (d.size as usize) < total // the root is not a "subtree"
            })
            .max_by_key(|d| d.size)
            .copied()
    }

    /// Identical subtrees hash equal — the same function text twice in one
    /// file (parse-level identity; rustc would flag the duplicate name, the
    /// grammar does not), and a standalone file against its twin nested in
    /// another file's module.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn identical_subtrees_hash_equal() {
        let twin = "fn pair_one(v: u32) -> u32 {\n    let a = v + 1;\n    a\n}\n";
        let src = format!("{twin}{twin}");
        let starts: Vec<usize> = src.match_indices("fn pair_one").map(|(at, _)| at).collect();
        assert_eq!(starts.len(), 2, "two copies in the source");
        let table = built(&src);
        let total = table.1.participating();
        let digests: Vec<SpanDigest> = starts
            .iter()
            .map(|at| {
                table
                    .1
                    .digests()
                    .iter()
                    .filter(|d| {
                        d.start as usize == *at && d.end as usize > *at && (d.size as usize) < total
                    })
                    .max_by_key(|d| d.size)
                    .copied()
                    .expect("a proper subtree digest at each copy")
            })
            .collect();
        assert_eq!(
            digests[0].fp, digests[1].fp,
            "identical fn subtrees fingerprint equal"
        );
        assert_eq!(digests[0].size, digests[1].size);

        // Cross-file: the same function alone in its own file keeps the
        // fingerprint it has nested among siblings (no positions hashed).
        let solo = built(&format!("mod m {{\n{twin}}}\n"));
        let nested = digest_starting_at(&solo, "fn pair_one").expect("digest for \"fn pair_one\"");
        assert_eq!(nested.fp, digests[0].fp, "nesting does not change identity");
    }

    /// eval-1 F2 acceptance, operator leg: `x + y` vs `x - y` — same named
    /// skeleton (`binary_expression` with two `identifier`s), different
    /// anonymous operator token. NOT duplicates.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn plus_and_minus_binaries_differ() {
        let plus = built("fn f() -> u32 { x + y }\n");
        let minus = built("fn f() -> u32 { x - y }\n");
        let p = digest_starting_at(&plus, "x + y").expect("digest for \"x + y\"");
        let m = digest_starting_at(&minus, "x - y").expect("digest for \"x - y\"");
        assert_ne!(p.fp, m.fp, "`x + y` and `x - y` must not unify");
    }

    /// eval-1 F2 acceptance, separator leg: `[a, b]` vs `[a; b]` — same
    /// `array_expression` kind, same named children, different anonymous
    /// separator (`,` vs `;`). NOT duplicates. (This is the `foo(a, b)` vs
    /// `foo(a; b)` pair in rust-parsable form.)
    #[test]
    #[cfg(feature = "lang-rust")]
    fn comma_and_semicolon_separators_differ() {
        let comma = built("fn f() { let v = [a, b]; }\n");
        let semi = built("fn f() { let v = [a; b]; }\n");
        let c = digest_starting_at(&comma, "[a, b]").expect("digest for \"[a, b]\"");
        let s = digest_starting_at(&semi, "[a; b]").expect("digest for \"[a; b]\"");
        assert_ne!(c.fp, s.fp, "`[a, b]` and `[a; b]` must not unify");
    }

    /// Comments, whitespace, and formatting do NOT change the fingerprint;
    /// renamed locals DO (leaf text is hashed).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn trivia_is_invisible_but_leaf_text_is_not() {
        let plain =
            built("fn calc(input: u32) -> u32 {\n    let double = input * 2;\n    double\n}\n");
        let commented = built(
            "fn calc(input: u32) -> u32 {\n    // double the input\n    let double = input * 2;\n\n    double\n}\n",
        );
        let reformatted = built("fn calc(input: u32) -> u32 { let double = input*2; double }\n");
        let renamed =
            built("fn calc(value: u32) -> u32 {\n    let double = value * 2;\n    double\n}\n");
        let base = digest_starting_at(&plain, "fn calc").expect("digest for \"fn calc\"");
        assert_eq!(
            digest_starting_at(&commented, "fn calc")
                .expect("digest for \"fn calc\"")
                .fp,
            base.fp,
            "a comment and spacing differences are invisible"
        );
        assert_eq!(
            digest_starting_at(&reformatted, "fn calc")
                .expect("digest for \"fn calc\"")
                .fp,
            base.fp,
            "formatting is invisible"
        );
        assert_ne!(
            digest_starting_at(&renamed, "fn calc")
                .expect("digest for \"fn calc\"")
                .fp,
            base.fp,
            "renamed locals change identity (leaf text is hashed)"
        );
    }

    /// `size` counts participating nodes only: a comment inside a subtree
    /// leaves the size untouched (and the byte span grows — the contrast is
    /// the point of the node-count unit).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn size_counts_participating_nodes_not_bytes() {
        let plain = built("fn s() -> u32 {\n    let a = 1;\n    a\n}\n");
        let commented = built("fn s() -> u32 {\n    let a = 1; // trailing\n    a\n}\n");
        let p = digest_starting_at(&plain, "fn s").expect("digest for \"fn s\"");
        let c = digest_starting_at(&commented, "fn s").expect("digest for \"fn s\"");
        assert_eq!(p.size, c.size, "comments are not participating nodes");
        assert!(
            c.span_bytes() > p.span_bytes(),
            "byte spans DO grow — size is the invariant unit"
        );
        // The root digests in: source_file + everything below it.
        let root = plain.1.digests().last().expect("root digests last");
        assert_eq!(root.size as usize, plain.1.participating());
    }

    /// The pass is iterative: a pathologically deep tree (the eval.rs
    /// 1500-brace chain) fingerprints without touching the call stack.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn deep_trees_fingerprint_without_recursion() {
        let mut src = String::from("fn f() {\n");
        for _ in 0..1500 {
            src.push_str("{\n");
        }
        for _ in 0..1500 {
            src.push_str("}\n");
        }
        src.push_str("}\n");
        let table = built(&src);
        assert!(
            table.1.participating() > 3000,
            "the deep tree fingerprinted"
        );
    }

    /// The lazy cache on ParseEntry computes once and shares across clones
    /// (F8's "cacheable on ParseEntry" contract).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn parse_entry_caches_the_table_lazily() {
        use crate::ast::cache::ParseEntry;
        use crate::vfs::Generation;

        let registry = LangRegistry::new();
        let lang = registry
            .for_extension("rs")
            .expect("lang-rust is the default feature set");
        let mut parser = registry.parser(lang).expect("ABI-verified pair");
        let bytes: Arc<[u8]> = Arc::from(b"fn a() -> u32 { 1 }\n".as_slice());
        let tree = parse_snapshot(&mut parser, &bytes).expect("parses");
        let entry = ParseEntry::new(Generation::default(), bytes, tree);
        let trivia = trivia_kind_ids(&registry, lang);
        let first = entry.fingerprints(&trivia);
        let second = entry.fingerprints(&trivia);
        assert!(
            Arc::ptr_eq(&first, &second),
            "the table is computed once and shared"
        );
        assert!(!first.digests().is_empty());
    }
}
