# PROMOTION.md — experimental → stable, as a checklist you can run

> B9 (PLAN-v0.3): "promotion requires full trivia-corpus curation"
> (README, pre-v0.4) was not a measurable gate. This file is the
> replacement: every item below is a command that exits 0 or a file that
> exists, none is a judgment call. Until every box is checked for a
> language, that language stays in `lang-experimental` and `--version`
> reports it under `tiers.experimental`.

**Tier today (v0.4):** stable = `rust, python, go, javascript, typescript,
tsx, java, c` (the `lang-all` fence set); experimental = `ruby, ocaml,
ocaml_interface, elixir`. `--version` is the visible state:

```
cargo build --features lang-experimental && ./target/debug/janitor --version
# {"schema":"janitor.v1",...,"grammars":["rust","ruby","ocaml","ocaml_interface","elixir"],
#  "tiers":{"stable":["rust"],"experimental":["ruby","ocaml","ocaml_interface","elixir"]}}
```

The precedent is F9a/F9b (python, then go and ts/tsx, CHANGELOG v0.2):
each stable language shipped the same measurable ladder — a curated
trivia corpus, a norm table, an e2e acceptance, a determinism leg, a CI
leg. This document is that ladder written down as gates.

## The checklist

### C1 — Curated trivia corpus: ≥ 5 measured fixtures

- `tests/fixtures/kp6_<lang>/` holds **≥ 5 fixture files** of real-world
  shapes (the stable precedent: python 5, go 5, typescript 5 — measured;
  today's experimental tier sits at ruby 3, ocaml 4, elixir 3, which is
  the curating gap this criterion exists to close).
- Every fixture parses **clean** under its own grammar (zero ERROR /
  MISSING nodes — the same fence `bundled_fixtures_parse_clean_under_
  their_grammars` enforces for the registry fixtures).
- ≥ 2 fixtures deliberately interleave a comment between exactly the
  pattern siblings of the language's def idiom, and
  `tests/kp6_<lang>.rs` pins **hand-computed** strict/skip counts per
  fixture (`--strict-trivia` vs default) plus corpus-wide totals — the
  F9a shape: ground truth written from the tree shapes while authoring,
  never from the implementation's own output.

*Check:* `cargo test --features lang-<x> --test kp6_<lang>` exits 0 and
`ls tests/fixtures/kp6_<lang> | wc -l` ≥ 5.

### C2 — Determinism leg: byte-identical across thread counts

- The language's e2e determinism case (the `tests/determinism.rs`
  shape: same scan through the real binary under `RAYON_NUM_THREADS=1`
  and `=8`) is green for a corpus that includes the language's files.
- 3 consecutive runs byte-identical (the determinism suite already
  pins 1-vs-8; the promotion commit records a 3× repetition locally —
  output in the PR description, not a judgment).

*Check:* `cargo test --features lang-<x> --test determinism` exits 0.

### C3 — Apply-gate honesty probes: passing, generalized

The gate is the product's core promise, so promotion requires the
breakage classes pinned **for this language**, not inherited:

1. **Unclosed block comment** (the B9 case, generalized to the
   language's own comment delimiters — `(* *)` for ocaml, `/* */` for
   C-shaped syntax, N/A for line-comment-only languages): an edit that
   opens a comment and never closes it must be refused typedly —
   `E_PARSE_UNCLOSED_COMMENT` with the opening offset where the probe
   owns the class, or `E_PARSE_INVALID` with coordinates where the
   grammar's own recovery emits ERROR/MISSING (the rust/ruby shapes,
   measured). Either way: **zero silent flushes.**
2. **Deleted mandatory token** (MISSING-node class): refused with
   coordinates, whole diff rolled back (the `in`-deletion leg of
   `tests/ocaml_e2e.rs` is the model).
3. **Clean atomic multi-edit rename** through the real binary: applies,
   and a deliberate same-diff breakage rolls the whole transaction
   back.

*Check:* the three probes exist in the language's e2e (or a shared
probe suite) and `cargo test --features lang-<x> --test <lang>_e2e`
exits 0.

### C4 — Norm table + documented caveats

- `lang/<name>.norm.toml` exists for **every LangId** the feature
  registers (ocaml needs two: `.ml` and `.mli`), header flipped from
  EXPERIMENTAL to STABLE in the same promotion commit.
- Every measured grammar-vs-vocabulary divergence has a written caveat
  in `docs/SCHEMA.md` and `share/GLOBAL.md` — the elixir precedent:
  tree-sitter-elixir has **no dedicated def kind**, so FUNCTION=`call`
  also matches ordinary invocations and `defmodule` itself; the caveat
  is documented (SCHEMA.md "Language support — elixir"), not smoothed
  over. A promotion with an undocumented divergence is not a promotion.

*Check:* the toml files exist; `grep -l EXPERIMENTAL lang/<name>.norm.toml`
returns nothing after the flip; SCHEMA.md names the caveat.

### C5 — Dedicated CI leg green

- A `cargo test --features lang-<x>` step runs on every PR (its own
  step in the `gates` matrix, like python/go/typescript today, or the
  merged tier job if the whole tier promotes at once), **on both
  toolchain legs** (1.90.0 MSRV and stable).

*Check:* the workflow step exists; both matrix legs green on the
promotion PR.

### C6 — Dogfood sign-off: one real session, recorded

- One real dogfood session used the language through the production
  surface (`scan` + `apply`, not tests-only) and hit **zero
  false-positive refused edits**.
- The evidence is recorded, not attested: the dogfood repo's janitor
  memory carries the session note(s) — `janitor note --ptr
  path:start-end:kind --commit <sha> "<what was refactored>"` — so the
  sign-off is re-checkable (`wake` in that repo surfaces it).

*Check:* `janitor wake --budget 2000` in the dogfood repo surfaces the
session note with its `--ptr`/`--commit` anchors.

## Flipping the switch (the mechanics, in one commit)

1. `Cargo.toml`: add the feature to `lang-all`; it stays in
   `lang-experimental` (which then simply includes it via lang-all).
2. `build.rs`: flip the grammar's tier column — `--version` moves the
   name from `tiers.experimental` to `tiers.stable` (the partition test
   in `src/cli.rs` enforces the split stays a partition).
3. `.github/workflows/ci.yml`: the dedicated step merges into the
   gates matrix (C5).
4. `.github/workflows/release.yml`: `grammars | length == 8` is a
   **fence** — the bump to 9+ (and the tier-smoke's `stable | length`)
   happens deliberately in this commit, never as drive-by.

## Historical exemptions (the pre-checklist stable set)

`c`, `java` and `javascript` entered `lang-all` in the v0.1 scaffold
wave — before this checklist existed (it is the v0.3 B9 deliverable).
They carry **no kp6 trivia corpus, no norm table, and no dedicated e2e
of their own**; that gap is recorded here as debt, not waived:

- **c**: registered and fixture-fenced, but no `tests/kp6_c*`, no
  `lang/c.norm.toml`, no `c_e2e`. Note the cpp promotion's measured
  `.h → cpp` routing move (docs/SCHEMA.md "Language support — cpp")
  already tightened c's surface to exactly `.c`.
- **java**: same shape — grammar pinned (ABI 14) and bundled-fixture
  fenced, corpus/norm/e2e absent.
- **javascript**: same shape (`.js/.jsx/.mjs/.cjs`).

Settling this debt = running C1-C6 per language (the ladder is
language-generic by construction; cpp proved it end to end as the
first full pass). Until then the three stay in `lang-all` on the
strength of the pin-check ABI fence and the shared engine surfaces —
the tier split reports what IS, not what is owed.

## Non-criteria (explicitly out)

- **Upstream grammar version bumps** are not promotion — they are a pin
  update with the same test ladder re-run.
- **"Feels mature" / coverage percentages** — no subjective bar, ever;
  if a real concern surfaces it becomes a new measurable item here.
- **Semantic validation** of any kind (ADR-0017): the LSP remains the
  client's job; a promotion never widens the gate past syntax.
