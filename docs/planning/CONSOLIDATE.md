# CONSOLIDATION BRIEF — Janitor Prime (post-eval reconciliation)

You are reconciling `docs/planning/PLAN.md` (the implementation plan) against three hostile
eval reviews in `docs/evals/`. Edit PLAN.md directly (surgical edits, keep its structure,
voice, and formatting conventions). Do NOT rewrite sections that are already correct.
Work in ENGLISH. Do not commit, do not run cargo. Preserve every anchor other sections cite
(section numbers, ADR numbers, KP ids, R ids, Q ids, phase letters F0–F10 must remain
stable — when adding, append; never renumber existing ids).

Inputs:
- `docs/planning/PLAN.md` — the plan (edit target)
- `docs/evals/eval1-feasibility.md` — GO-with-conditions (9 conditions, findings F1–F8)
- `docs/evals/eval2-scope.md` — aprovado-com-reordenacao (9 amendments, §VERDICT)
- `docs/evals/eval3-strategy.md` — HYBRID verdict with conditions C1–C6

Independent verifications already done by the orchestrator (treat as ground truth):
- crates.io API: `tree-sitter 0.27.0` => `rust_version: 1.90`; `0.26.13` => `1.77`. Not yanked.
- Local toolchain: rustc 1.98.0 — can build against either pin.
- `ts_tree_copy` (lib/src/tree.c, v0.26.13 tag) calls `ts_subtree_retain` => Tree::clone is a
  SHALLOW refcount-incrementing copy. PLAN Rule T1 is correct; eval1's "deep copy" phrasing
  is wrong — BUT eval2's amendment #1 still stands on its own correct basis: A.3.2's aliasing
  rule ("must not be mutated while clones exist"). Fix the discipline, keep T1's fact.
- GitHub API 2026-09-13: `VictorTaelin/OptMem` license = NONE, not archived;
  `lucasew/refactree` = HTTP 404 (repo DELETED — confirmed, not "possibly private");
  `ast-grep/ast-grep` = MIT, active.

## EDIT LIST (apply all; each is small)

E1. §2 Cargo.toml: `rust-version = "1.85"` → `"1.90"`; fix the comment: tree-sitter 0.27.0
    requires rustc 1.90 (crates.io rust_version, verified 2026-09-13); clap 4.6.6 (1.85) and
    edition 2024 (1.85) sit below the binding floor. After the toml block add one sentence:
    the alternative (pin =0.26.13, MSRV 1.77) was considered and rejected — A.3 facts are
    verified against 0.27.0 and the toolchain is 1.98.

E2. ADR-0010 + §1.4.3 item 6 (eval1 F2 — blocking for F8 correctness): the `Unify` Merkle
    equivalence MUST hash semantically-significant anonymous tokens (binary operators,
    separators, punctuation) and exclude ONLY comment/whitespace-class trivia kinds
    (per-grammar trivia table, KP-5). Excluding all anonymous nodes conflates `a + b` with
    `a - b` (operators are anonymous tokens). Add to F8 acceptance tests: `x + y` vs `x - y`
    NOT duplicates; `foo(a, b)` vs `foo(a; b)` NOT duplicates.

E3. ADR-0007 + §1.2.1 step 5 + §1.4.4 (eval1 F1/F5 + eval2 #1 — the blocking amendment):
    (a) v0.1 memory discipline: scan processes files streaming — each ParseEntry is dropped
    after that file's report is collected; never retain O(repo) bytes+trees (single-rule cold
    scan has zero cache hit rate; retention is pure cost and an OOM vector). The dashmap
    cache pays off only for multi-rule scans / warm processes (F10). Update R15's mitigation.
    (b) apply-path ownership: `apply` REMOVES the ParseEntry from the cache and takes
    ownership BEFORE `Tree::edit`; assert `Arc::strong_count(&tree) == 1` at the edit point
    (fail to a typed error if not — never proceed on a shared tree, per A.3.2's aliasing
    rule); fresh entry inserted after re-parse. Remove the "cheap and safe" phrasing from
    §1.2.1 step 5 — replace with this exclusivity guard. Extend KP-1 with a shared-clone leg
    (attempt the edit path while a clone exists; assert typed-error, no mutation). F5 test:
    counting-allocator assertion that no tree copy happens on the apply path.

E4. New ADR-0016 (eval1 F3 + eval2 #9 partially) — Match-set semantics: matches = all local
    maxima per pivot; overlapping matches from different pivots both reported with
    `"overlap": true`; canonical order `(start_byte, end_byte, rule_id)`; determinism across
    rayon thread counts enforced by a test asserting byte-identical JSON under
    RAYON_NUM_THREADS=1 and =8. `--include-virtual` defaults to OFF (anti-phantom philosophy:
    scan reports disk reality; flag opts into overlay visibility). Reference from
    §1.4.4/§1.6 MatchReport text; mention docs/SCHEMA.md. Also fold eval2 #9's related gap:
    capture priority when multiple captures compete (document: earliest-position wins),
    `Unify` explicitly stub-or-omit in v0.1 (it already is F8/v0.2 — state it in ADR-0005),
    dashmap×rayon insertion rule (phase-split snapshot-then-parse per §1.1; inserts only at
    collection), STATE.json writes via tmp+rename atomic replace.

E5. §1.4.2 (eval1 F4): resolve UnknownKind wording into ONE behavior: kind unknown in EVERY
    enabled language at compile time => loud typed error; otherwise per-file resolution
    failure => skip + count in `skipped_for_language`. Kind tables seeded from bundled
    per-grammar fixture corpora (grammar crates' own test fixtures) at first use — not from
    whichever file the scan hits first (removes corpus bias).

E6. F4 walk policy (eval1 F6): NUL-byte sniff on first 8 KB => skip binary; size cap
    (default 4 MB, CLI-overridable) => skip oversized; both surface as
    `"skipped": {"reason": ...}` in the report. `set_string` validates UTF-8 => typed error;
    `utf8_text`'s `Err(Utf8Error)` path handled (typed error) in every text-touching DSL op
    including Unify leaf hashing.

E7. ADR-0015: append — `tree-sitter-language` declares `links = "tree-sitter-language"`:
    a future semver-incompatible second version in the graph is a hard cargo error; constraint
    noted for grammar forks/vendorings (eval1 F7).

E8. eval3 C5 fact-fix: §10 credits + A.4 — refactree repo CONFIRMED DELETED (GitHub API 404
    on 2026-09-13). Replace "possibly renamed/private" hedging in A.4 with the verified fact;
    keep pkg.go.dev / proxy.golang.org citations. OptMem no-license already correct — keep.

E9. New KP rows in §5 (numbering: eval2's amendments take KP-9/KP-10; eval3's dogfood gate
    is KP-11 — eval2's verdict landed first):
    - KP-9 (eval2 #2): LLM DSL-authorship gate at F4 entry — before building evaluator
      polish, have a real LLM author N rules from docs alone; measure syntax-error and
      retry rates (typed errors + suggestions are the product surface). F4 entry.
    - KP-10 (eval2 #3): real-repo scan-latency spike in F1 — scan a large real repo
      (e.g. tree-sitter itself or rust-analyzer checkout), cold parse, measure wall time and
      RAM ceiling; converts ADR-0007's accepted wart into a measured decision. F1.
    - KP-11 (eval3 C4): post-v0.1 dogfood gate (~1 wk) — agent runs
      `wake -> scan -> apply -> note` on a real repo vs git-worktree + ast-grep baseline;
      measurable reduction in broken edits or context loss, or the project stops at v0.1.
      Phase column: "post-v0.1 (funding gate)". Reference from §7's cut-to-zero paragraph.

E10. eval3 C2 disposition (do NOT add the pre-F4 spike): append to ADR-0002: eval3's C2
     two-way spike (hand evaluator vs ast-grep-core bridge, ~2–3 d) is deliberately NOT
     scheduled before F4 — at v0.1 one-language scope the net saving is 1.5–2.5 lane-weeks
     against permanent 0.x coupling and fixed matcher semantics; the existing revisit trigger
     (F4 slips >50%) remains the flip point, `ffi_shim.rs` is the seam. Decision: trigger-based.

E11. Phase amendments from eval2 (anchor-stable):
     (a) Move `docs/DSL.md` drafting from F7 to F4 (artifact of KP-9; prevents post-freeze
         surface churn).
     (b) Add the end-to-end agent-loop acceptance test (`wake → scan → apply → note → wake`
         through the real binary) to F7 — v0.1's definition of done.
     (c) v0.2 execution ORDER: F9 (multi-language, python) runs BEFORE F8 (Unify) — validate
         ADR-0002's architecture on a second language before feature depth on one. Keep the
         phase letter ids (F8/F9) unchanged everywhere; record the order swap in §3 (a line
         under the F8 section) so existing cross-references stay valid.
     (d) Re-anchor KP-8's allocation fence from F3 walk helpers onto the F4 matcher (update
         KP-8's Phase cell and §5 text; keep KP-8 id).
     (e) Adopt eval2's corrected estimates: §0 executive summary and §6 table — v0.1
         40–66 focused days (p50–p90), full BRIEF 48–81; delta concentrated in F4 (10–16)
         and F6 (6–10). Update the cross-check paragraph to note eval2's model was adopted
         after reconciliation. Update calendar lines (~2.5–4 months solo v0.1).

E12. New "## Appendix B — Eval reconciliation record" at the end: compact table mapping
     every condition to disposition:
     - eval1 conditions 1–9 (F1–F8 findings): accepted via E1–E7; F8.1–F8.4 (Send/Sync facts,
       parse_with_options shape, clone semantics) SUPERSEDED — final plan was already correct
       (A.3.1–A.3.3, Rules T1/T3); eval1 reviewed an intermediate draft on those points, but
       its F5 exclusivity concern survives via E3(b) on eval2's basis.
     - eval2 amendments 1–9: all accepted via E3, E4, E9 (KP-9/KP-10), E11(a)–(e).
     - eval3 C1–C6: C1 already in plan (ADR-0013, stateless overlay); C2 declined with
       rationale (E10); C3 already in plan (revisit trigger); C4 accepted as KP-11 (E9);
       C5 accepted (E8); C6 accepted — reframe the DSL motivation from "defeats catastrophic
       backtracking" to "explicit scope semantics with a proven linearity bound" (edit §0
       executive summary wording and §1.4.3 intro; KP-7 stays as the falsifiability fence).
     One-line verdict summary per eval at the bottom.

E13. C6 wording fix (from E12, listed separately so it is not missed): §0 and §1.4.3 — the
     founding-motivation claim "designed to resolve catastrophic backtracking" becomes
     "explicit scope semantics with a proven linearity bound (KP-7)"; tree-sitter matching is
     already compiled/linear — the DSL's value is the scoped state machine + adjacência
     estrita, not backtracking rescue. Keep the memoized-NFA mechanism as-is.

## Output
Print to stdout (<= 30 lines): edits applied (E-numbers), anything intentionally NOT applied
(with reason), and confirmation that all anchors (ADR/KP/R/Q/F ids, section numbers) remained
stable or were appended without renumbering.
