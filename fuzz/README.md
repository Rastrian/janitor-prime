# Fuzz targets (F7.1, PLAN §3 F7 / §8 R16)

The four entry points that face **agent-authored bytes** — the DSL rule text,
the `apply` diff payload, `LOG.txt`, and the `TREE/<size>` record files — are
the surfaces an LLM can hand janitor-prime *arbitrary garbage*. Their contract
is: **never panic, refuse only with typed errors** (PLAN §1.1, ADR-0003). This
directory is the fence that makes the contract tested rather than claimed
(`tests/fuzz_targets.rs` is the executable form).

## Decision record: plain proptest targets, not cargo-fuzz

`cargo-fuzz` (libFuzzer + `-Zsanitizer=coverage`) requires a **nightly
toolchain**. This repository pins stable `1.98.0` locally (`rust-toolchain.toml`)
and its CI matrix runs **stable + MSRV 1.90.0 only** (PLAN Appendix A.5, the F0
pin-check record); no nightly leg exists. PLAN §3 F7.1 itself marks cargo-fuzz
`[VERIFY: setup effort; stretch, droppable]`.

Shipping a fuzz target that can only run on a toolchain the repository does not
have would fence nothing. So the targets are **plain `proptest` property tests**
that run under the ordinary `cargo test` gate (default and `--all-features`),
on every leg of CI:

- `proptest =1.11.0` is already a dev-dependency (no new dependencies).
- proptest executes each property body inside `catch_unwind`: **a panic is a
  property failure** with the failing seed and automatic shrinking — so
  "no panic" is exactly "the property holds", witnessed by the default gate.
- Beyond the randomized cases, each target **replays the corpus below** as
  fixed regression seeds, cargo-fuzz style.

**Upgrade path:** when a nightly leg lands in CI, add a `cargo-fuzz` crate at
`fuzz/` reusing these same corpus directories verbatim (the format is already
one-input-per-file); the proptest targets stay regardless, because the corpus
replay and the shrinking properties belong to the default gate, not to a
bespoke job.

## The four targets

| Test (`tests/fuzz_targets.rs`)                    | Entry point                                    | Property                                                                                                                                                                                                                              |
| ------------------------------------------------- | ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `dsl_parser_no_panic_typed_errors_only`           | `ast::dsl::parse_rule(&str)`                   | Ok or a typed `DslError`, never a panic; `Lex`/`Parse` positions are `<= input.len()` (positions index the rule text — a diagnostic pointing past the input is a lie). Includes an arbitrary-**bytes** leg: non-UTF-8 input can never reach `parse_rule` in the product (argument decoding is the guard), and the lossy-decoded simulation must refuse typedly too. |
| `diff_payload_parser_no_panic_typed_errors_only`  | `apply::diff::DiffPayload::from_json_str`      | Ok or exactly `JanitorError::Edit(EditError::Payload{..})`, never a panic — over arbitrary serialized `serde_json::Value`s. Plus `FileEdit::validate` over arbitrary offsets/texts against a Rust and a multibyte source: every Err is `StaleView`/`Mismatch`/`Overlap`/`OutOfRange`, never `Payload`. |
| `log_loader_no_panic_quarantine_never_panics`     | `memory::log::FactLog::load`                   | Never panics on any LOG.txt bytes (including the quarantine writes and the atomic prefix rewrite it performs); `Ok` keeps position-is-identity (`facts[i].id == i`); every `Err` is `E_LOG_UNRECOVERABLE`, `E_NOT_INITIALIZED`, or `E_VFS_IO`. |
| `tree_record_loader_no_panic_typed_errors_only`   | `memory::tree_store::TreeStore::load`, `BlockSlot::parse` | `load` never panics on any `TREE/<size>` bytes; `Ok` records contain no NUL and the file is left a multiple of the 256-byte record width (torn tails trimmed); `Err` is `E_TREE_CORRUPT` or `E_VFS_IO`. `BlockSlot::parse`: Ok ⇒ power-of-two size ≥ 2 and `notation()` round-trips; Err ⇒ `MemoryError::SlotInvalid`. |

## The corpus

`fuzz/corpus/<target>/` — **one input per file**, filename a short slug:

- `dsl/` — the KP-6 worked examples and the full surface (comments, any-of,
  capture, nested `under`, `unify`-in-under) as golden valid seeds, plus the
  adversarial set: unbalanced parens, 500-deep `(` nesting, `@` alone, `#$%`,
  trailing tokens, operator-as-atom, empty input, multibyte `αβγ((`, very long
  identifier.
- `diff/` — the documented payload shape, malformed JSON, wrong types,
  duplicate paths, `u64::MAX` offsets, inverted ranges, rune-cutting offsets,
  deep nesting.
- `log/` — valid JSONL, a torn unterminated tail, a non-UTF-8 sequence,
  garbage mid-file, an empty file, CRLF mix.
- `tree/` — valid 256-byte NUL-padded records, a torn partial record, garbage
  after the NUL padding, a non-UTF-8 record.

**Replay mechanics:** each test enumerates its corpus directory with
`std::fs::read_dir` (rooted at `CARGO_MANIFEST_DIR`), sorts by name for a
deterministic order, and asserts the directory is **non-empty** — a corpus that
silently vanished from the package would otherwise turn replay into a vacuous
pass. Any file dropped into a corpus directory is picked up automatically on
the next run: the intended regression workflow is to copy a failing proptest
seed (or a field capture) into the corpus and commit it alongside the fix.

## Running

```sh
cargo test --test fuzz_targets            # corpus replay + 256-case properties
PROPTEST_CASES=10000 cargo test --test fuzz_targets   # a longer soak, proptest-native
```
