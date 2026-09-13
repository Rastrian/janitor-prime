# janitor-prime

A single statically-linked Rust binary (`janitor`) that gives autonomous LLM
agents three fused capabilities over a target codebase:

1. **`src/vfs`** — a transactional virtual filesystem: an `Overlay` over `Os`
   that simulates edits in memory, with a strict anti-phantom invariant and
   commit/rollback boundaries around multi-file changes.
2. **`src/ast`** — a structural search/rewrite engine over
   [tree-sitter](https://tree-sitter.github.io/), driven by a small
   s-expression DSL with explicit scope semantics and a proven linearity
   bound.
3. **`src/memory`** — a persistent, append-only memory with anti-hallucination
   syntax pointers and a synchronous `nap` compression protocol. No LLM call
   ever happens inside the binary.

**Status:** pre-alpha scaffold (phase F0). Only `janitor --version` exists;
the domain modules are typed skeletons. The authoritative implementation plan
lives in [`docs/planning/PLAN.md`](docs/planning/PLAN.md).

## The stdout contract

`stdout` is 100% JSON, from day one. Every user-facing message flows through
a versioned envelope:

```json
{"schema":"janitor.v1","ok":true,"data":{"version":"0.1.0","grammars":["rust"]}}
```

Failures use the same envelope with an `error` body carrying a stable `E_*`
code. Diagnostics go to `stderr` via `tracing` (env-gated, `RUST_LOG`).
Exit codes: `0` success, `1` operational error, `2` usage error.

## Building

```sh
cargo build                  # default: rust grammar only (ADR-0015)
cargo build --features lang-all
cargo build --no-default-features
```

Language grammars are cargo features: `lang-rust` (default), `lang-python`,
`lang-go`, `lang-javascript`, `lang-typescript`, `lang-java`, `lang-c`,
`lang-all`.

## License

Dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
Credits and provenance in [NOTICE](NOTICE).
