# KP-6 — Trivia semantics for `Seq` adjacency

| | |
|---|---|
| **Kill-point** | PLAN §5 KP-6 · ADR-0010 · §1.4.3 step 4 · risk R4 · Q2 |
| **Status** | **MEASURED at F1 (2026-09-13) — ADR-0010 STANDS (comment-skipping default)** |
| **Code** | `src/spikes/trivia_corpus.rs` |
| **Data** | `benches/spike-kp6-outcome.json` |
| **Commit** | F1 work item 5 |

## Hypothesis under attack

Comment-skipping named-sibling adjacency matches real-world intent;
byte-strict adjacency would make most real patterns unmatchable (comments
between statements are idiomatic Rust).

## Setup

Two semantics over a container's **named** children (both modes of
ADR-0010 share the named-sibling part):

- **strict** — adjacency over the list as-is: a trivia-class child between
  two pattern siblings breaks the sequence (`--strict-trivia`);
- **skip** — trivia-class children removed first (ADR-0010's default).

Trivia class for the rust grammar: `line_comment` + `block_comment` —
there is **no** `comment` kind (verified against tree-sitter-rust 0.24.2
`node-types.json`; this seeds KP-5's per-grammar trivia table).

Five curated real-world idioms:

| pattern | container | sequence | intent |
|---|---|---|---|
| let-statement-adjacency | `block` | `let_declaration let_declaration` | consecutive lets |
| match-arm-adjacency | `match_block` | `match_arm match_arm` | neighboring arms |
| attribute-on-item | `source_file` | `attribute_item function_item` | attribute on a fn |
| method-chain-link | `field_expression` | `call_expression field_identifier` | chained call: base call, then next method |
| if-else-adjacency | `if_expression` | `_ block else_clause` | if with else |

Corpora: (a) five crafted snippets, each containing the idiom once clean
and once with a comment deliberately interleaved between exactly the
pattern siblings — hand-computed (strict, skip) asserted exactly;
(b) this repository's own `src/` — 16 files, 109,075 B of real,
comment-heavy Rust (the tree-sitter crate's fixtures are not vendored).

## Results

Curated ground truth — **all five matched exactly** (the assertions are
the experiment: they encode the tree shape the grammar really produces):

| pattern | strict | skip | note |
|---|---|---|---|
| let-statement-adjacency | 1 | 2 | (c,d) pair behind a line comment |
| match-arm-adjacency | 1 | 2 | (1,2) arms behind a comment |
| attribute-on-item | **0** | 1 | the only attr→fn pair is comment-separated |
| method-chain-link | 2 | 3 | `.clone` link behind a block comment mid-chain |
| if-else-adjacency | 1 | 2 | else clause behind a block comment |

Repo corpus (real code):

| pattern | strict | skip | strict-loss |
|---|---|---|---|
| let-statement-adjacency | 114 | 124 | **8.1%** |
| match-arm-adjacency | 37 | 37 | 0% |
| attribute-on-item | 11 | 11 | 0% |
| method-chain-link | 175 | 175 | 0% |
| if-else-adjacency | 11 | 11 | 0% |

## Verdict

**ADR-0010 stands: default `Seq` adjacency = next named sibling skipping
comment-class kinds; `--strict-trivia` restores byte-strict.** The
deciding evidence:

1. Comments are legal between any two tokens of every idiom tested — the
   curated corpus makes each class fail under strict, including a total
   miss (attribute-on-item: strict finds *nothing*).
2. In real code the loss concentrates where comments are idiomatic
   (8.1% of let-pairs in this comment-heavy repo) and vanishes where they
   are rare — but a semantics that silently unmatches real code because a
   commenter wrote a comment is exactly R4.
3. The method-chain result pins a subtlety for F4's evaluator: in the rust
   grammar a mid-chain comment is a child of the `field_expression` link
   node (`value` base call, comment, `field_identifier`), not of the outer
   call — so trivia-skipping must apply at *every* scope the matcher
   enters, not just statement level.

Trivia table for `lang/rust`: `line_comment`, `block_comment` (KP-5 will
grow the per-grammar tables from this seed).
