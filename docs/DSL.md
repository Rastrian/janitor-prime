# The janitor rule DSL — complete contract (v0.1)

This document is the **complete and self-sufficient** contract for writing
structural match rules for `janitor scan`. It is written for an LLM agent
that has never seen the janitor source: everything needed to author, run,
and interpret rules is on this page. If a rule you write fails, the typed
error you get back is documented in [§7](#7-error-codes-the-full-table)
with a fixing recipe.

* Schema version: `janitor.v1`
* Languages: **rust**, **python**, **go**, and **typescript/tsx** are the
  supported, CI-tested languages (see §11–§13). The remaining grammars
  (javascript, java, c) may be enabled at build time; kind-name behavior
  for them follows [§4](#4-kinds-per-language) and their trivia tables are
  seeded but not yet conformance-tested (future F9 waves).
* A rule describes **structure** — node kinds and their adjacency — never
  text. There is no regex, no string matching, no field names in v0.1.

---

## 1. The model in one paragraph

A parse tree is a tree of **nodes**. Every node has a *kind* (a name from
the language's grammar, e.g. `function_item`, `identifier`, `block`) and a
byte span. Some nodes are **named** (they carry a grammar name and appear
in structural views: `function_item`) and some are **anonymous** (punctuation
and operators: `;`, `(`, `+`). A rule says: *find a container node of kind
K (the pivot), then match a sequence of its children, one pattern element
per child, in strict left-to-right adjacency*. The scoping is the point: a
rule never "reaches across" the pivot — the pattern runs only over that
pivot's own child list, so what a rule means is fixed and its worst-case
cost is linear in tree size (no catastrophic backtracking, by construction).

## 2. Grammar

```
rule        := "(" "under" pivot body constraints? ")"
             | seq_form
             | atom
seq_form    := "(" "seq" atom+ ")"
atom        := "(" "kind" IDENTIFIER ")"
             | "(" "capture" SYMBOL atom ")"
             | "(" "any-of" atom atom+ ")"
             | rule                      ; nested (under ...) descends a scope
             | IDENTIFIER                ; sugar for (kind IDENTIFIER)
             | "_"                       ; wildcard: any single node
pivot       := IDENTIFIER | "_"
body        := atom
constraints := "(" "constraints" "(" "unify" SYMBOL SYMBOL ")"* ")"

SYMBOL      := "@" [A-Za-z0-9_-]+        ; e.g. @fn, @lhs
IDENTIFIER  := [A-Za-z_][A-Za-z0-9_-]*   ; a *named* node kind of some grammar
comment     := ";" ... end of line
```

Lexical rules:

* Whitespace separates tokens; `;` starts a comment to end of line.
* `(`, `)`, `_`, `@sym`, identifiers are the only token classes. There are
  no strings, numbers, or operators.
* The reserved words `under`, `seq`, `capture`, `kind`, `any-of`,
  `constraints`, `unify` are reserved **only as list heads**. A bare
  identifier atom is always a kind name.

## 3. Operators

### `(under <kind-or-_> <atom>)` — the structural pivot

Everything below is scoped to nodes of kind `<kind>`: the executor visits
**every** node of that kind in the file (anywhere in the tree, at any
depth) and runs `<atom>` over that node's child list. `_` as the pivot
means every node is a pivot. One rule produces at most one match per
(child-list) start position per pivot — see [§6](#6-match-set-semantics).

```scheme
(under function_item
  (seq (capture @fn identifier)     ; first child: the fn name, captured
       block))                      ; strictly adjacent second child: the body
```

### `(seq <atom>+)` — strict adjacency among siblings

The elements must match **consecutive** children of the current scope, in
order, one child per element. `(seq a b c)` = child of kind `a`
immediately followed by child of kind `b` immediately followed by child of
kind `c`. Comments between them do **not** break adjacency by default
([§5](#5-trivia-semantics-adr-0010)). An empty `(seq)` is a parse error.

### `(capture @sym <atom>)` — bind the matched span

Matches exactly what `<atom>` matches and records the covered byte span
under `@sym`. Captures appear in the report's `captures` list. Each symbol
may be captured **once** per rule (a duplicate is `E_DSL_COMPILE`).

### `(kind <name>)` and bare `<name>` — one node of a kind

`identifier` and `(kind identifier)` are the same atom: matches a single
child whose kind is exactly `name`. Only **named** kinds are addressable —
anonymous punctuation/operators (`;`, `+`) are never pattern targets in
v0.1. Kind resolution and unknown-kind behavior: [§4](#4-kinds-per-language).

### `_` — wildcard

Matches any single node (of any kind). As a pivot (`(under _ …)`) every
node becomes a scope.

### `(any-of <atom> <atom>+)` — alternation

Matches one child that any of the alternatives matches. Alternatives may
consume different numbers of children (e.g. `(any-of identifier (seq
identifier ","))`); the match set rule in [§6](#6-match-set-semantics)
picks the longest. At least two alternatives are required.

### `(constraints (unify @a @b) …)` — isomorphism between captures *(F8)*

A **post-match filter** (ADR-0005): the rule matches exactly as written
first; then every constraint is checked over the match's captures, and a
match survives only if **every** `(unify @a @b)` relates two captures that
are both present in that match and structurally equivalent.

Equivalence is janitor's Merkle fingerprint (ADR-0010, the E2 rule): the
named-node skeleton **plus semantically-significant anonymous tokens**
(operators, separators, punctuation, keywords), with **leaf token texts**
hashed, and **only comment/whitespace-class trivia excluded**. So two
captures unify when they are the same code up to comments and formatting:

* `let a = v + 1;` ≡ `let a = v+1;` ≡ `let a = v + 1; // note` — equal
* `x + y` ≢ `x - y` — the operator is an anonymous token, and it is hashed
* `[a, b]` ≢ `[a; b]` — the separator is an anonymous token, and it is hashed
* `let a = v;` ≢ `let b = v;` — leaf text is hashed, so renamed locals differ

Constraints may only relate **capture symbols of this rule**; a symbol that
is not captured is `E_DSL_COMPILE` (a filter that could never pass would be
a silent no-op — refused loudly). A capture absent on a particular match
(an untaken alternation branch) filters that match, like any inequality.

```scheme
;; consecutive statements that say exactly the same thing
(under block
  (seq (capture @first let_declaration) (capture @second let_declaration))
  (constraints (unify @first @second)))

;; both arms of an if/else that are the same code (a real refactor smell)
(under if_expression
  (seq _ (capture @then block) else_clause (capture @els block))
  (constraints (unify @then @els)))
```

The fingerprint pass is O(n) per file and runs only for rules that carry
constraints; `--strict-trivia` does **not** change equivalence (the
fingerprint's trivia exclusion is the equivalence definition itself).

### Nesting: `(under …)` as a `seq` element

A nested `under` descends one scope level. As an element it consumes one
child that (a) has the pivot kind and (b) whose **own** child list matches
the nested body:

```scheme
; a block whose first statement is an if_expression containing else
(under block
  (seq (under if_expression
         (seq _ block else_clause))))
```

Scoping applies at every level the matcher enters — a comment breaking the
inner adjacency also matters (KP-6).

### Top-level shapes and what they mean

| top-level rule                 | meaning                                                            |
|--------------------------------|--------------------------------------------------------------------|
| `(under K body)`               | every node of kind `K` is a pivot; body runs over its children     |
| `(under _ body)`               | every node is a pivot                                              |
| `(seq …)` (bare at top level)  | same as `(under _ (seq …))`                                        |
| a **single-node** atom at top level (`identifier`, `_`, `(capture @x identifier)`, an `any-of` of single-node atoms) | matches every node satisfying it — **including the root** (a *unit* match; the report span is the node itself) |

A "single-node" atom is one that consumes exactly one child on every
path: `kind`, `_`, `under` (the nested body is a *condition* on the
consumed child, not extra consumption), `capture` of a single-node atom,
`any-of` of single-node atoms. Anything containing a top-level `seq` of
2+ elements runs in every node's child list instead.

## 4. Kinds per language

Node kinds are **per-grammar strings** (`function_item` in rust,
`function_declaration` in python). There is no cross-language kind
vocabulary in v0.1. Resolution follows one behavior:

1. **At rule-compile time**, every kind named in the rule is checked
   against the set of *build-enabled* grammars. A kind unknown to **every**
   enabled language fails loudly with `E_DSL_UNKNOWN_KIND` — the rule is
   wrong everywhere; fix the kind name (the error carries the nearest
   known kind name as a suggestion when one is close).
2. Otherwise, resolution is **per scanned file**: when scanning a file of
   language L, each kind resolves against L. If any kind in the rule is
   unknown in L, the rule provably cannot match in L, so **every file of
   language L is skipped and counted** in `skipped_for_language` — never a
   silent zero-match. Restricting the run with `--lang rust` scopes both
   the scan and this accounting.
3. The `--lang` flag accepts exactly the canonical names (`rust`, `python`,
   `go`, `javascript`, `typescript`, `tsx`, `java`, `c`); a name whose
   grammar is not in the build fails with `E_PARSE_NO_GRAMMAR_FOR_LANG`.
   (`typescript` and `tsx` are two names for the two grammars one cargo
   feature registers — see [§13](#13-typescript--tsx-v02-f9b)).

Kind matching in the hot loop is by integer kind id, but that is invisible
to rule authors: you always write names, reports always carry both `kind`
(name, resolved at serialization) and `kind_id`.

## 5. Trivia semantics (ADR-0010)

Adjacency is defined over the **named** children of a scope, and by
default comment-class kinds are removed from that sibling list before
matching (the KP-6-measured default: byte-strict adjacency silently loses
real matches wherever comments are idiomatic). The filter applies in
**every** scope the matcher enters, not just statement level.

* default (`trivia: "skip"`): trivia kinds are invisible — `(seq a b)`
  matches `a /*c*/ b`.
* `--strict-trivia` (`trivia: "strict"`): trivia kinds stay in the sibling
  list — a comment between two pattern siblings breaks the sequence.
* Trivia kinds per grammar (KP-6-measured table; go/typescript/tsx
  pinned at F9b, the rest seeded and awaiting their F9 wave):

| language    | trivia kinds                      |
|-------------|-----------------------------------|
| rust        | `line_comment`, `block_comment`   |
| python      | `comment`                         |
| go          | `comment`                         |
| javascript  | `comment`                         |
| typescript  | `comment`, `html_comment`         |
| tsx         | `comment`, `html_comment`         |
| java        | `line_comment`, `block_comment`   |
| c           | `comment`                         |
| ruby        | `comment` *(experimental, F9c)*   |
| ocaml       | `comment` *(experimental, F9c)*   |
| ocaml_interface | `comment` *(experimental, F9c)*|

Anonymous nodes (punctuation, operators) are never in the sibling list in
either mode — both modes are defined over named children.

## 6. Match-set semantics

One match is reported per *(pivot, start position)* whose pattern run
accepts, and the report is the **longest** accepting run from that start
(the *local maximum* — alternation can offer shorter runs; the longest
wins). Consequences:

* Overlapping matches from different pivots are **both reported**; any
  match whose byte span intersects another match's span in the same file
  carries `"overlap": true`. Nesting is not deduplicated away.
* Within one file, matches are sorted canonically by
  `(start_byte, end_byte, rule_id)`. Files are ordered by path. Output is
  **byte-identical regardless of thread count** (asserted in CI under
  `RAYON_NUM_THREADS=1` vs `=8`).
* **Capture priority**: when several accepting paths would bind the same
  symbol, the earliest-position binding wins (Pike-VM-style thread
  priority — alternation branches are explored left to right, first path
  to bind a symbol keeps it).
* The reported `kind`/`kind_id` of a match is the **pivot's** kind (the
  container), not the first child's.
* Spans are half-open `[start_byte, end_byte)` UTF-8 byte offsets into the
  file; points are `{row, column}` with `column` measured in **bytes**.

## 7. Error codes — the full table

Every failure is a `janitor.v1` envelope with `ok: false` and a stable
`error.code`. Rule-authoring codes first:

### `E_DSL_LEX` — the rule text is not lexable

`details: {position, expected}` — `position` is the byte offset of the
offending character.

```json
{"schema":"janitor.v1","ok":false,"error":{"code":"E_DSL_LEX",
 "message":"lex error at byte 21: expected one of [\"(\", \")\", \"@symbol\", \"identifier\", \"_\"]",
 "details":{"position":21,"expected":["(",")","@symbol","identifier","_"]}}}
```

Cause: `(under block (seq a #))` — `#` is not a token.

### `E_DSL_PARSE` — the tokens do not form a rule

`details: {position, expected, suggestion}`. `suggestion` is present
whenever the parser recognizes a near-miss and is the fastest fix.

```json
{"schema":"janitor.v1","ok":false,"error":{"code":"E_DSL_PARSE",
 "message":"parse error at byte 6: expected a kind name after (under",
 "details":{"position":6,"expected":["identifier","_"],
            "suggestion":"did you mean: (under block (seq a b))? (sq) is not an operator — wrap multiple atoms in (seq …)"}}}
```

Recurring causes and fixes:

| you wrote                          | error                            | fix                                    |
|------------------------------------|----------------------------------|----------------------------------------|
| `(sq a b)`                         | `E_DSL_PARSE` + did-you-mean `seq`| spell `(seq a b)`                      |
| `(under block (kind a) (kind b))`  | `E_DSL_PARSE` two atoms in body  | `(under block (seq (kind a) (kind b)))`|
| `(capture fn identifier)`          | `E_DSL_PARSE` symbol missing `@` | `(capture @fn identifier)`             |
| `(seq)` / `(any-of a)`             | `E_DSL_PARSE` too few elements   | a `seq` needs ≥1 and an `any-of` ≥2 atoms |
| unclosed `(`                       | `E_DSL_PARSE` at end of input    | balance the parentheses                |
| `unify` outside `constraints`      | `E_DSL_PARSE`                    | unify only appears inside `(constraints …)` |

### `E_DSL_UNKNOWN_KIND` — the kind is unknown to every enabled grammar

`details: {lang, kind}` (`lang` lists the enabled languages it was checked
against; `suggestion`-style nearest-kind help is in the message when
close). Cause: `(under function_itm …)` with `function_itm` unknown
everywhere. Fix: use a kind the grammar really has (the rust grammar's
common ones: `source_file`, `function_item`, `identifier`, `block`,
`let_declaration`, `match_expression`, `match_block`, `match_arm`,
`if_expression`, `else_clause`, `attribute_item`, `call_expression`,
`field_expression`, `field_identifier`, `struct_item`, `impl_item`,
`expression_statement`, `line_comment`, `block_comment`).

### `E_DSL_COMPILE` — the rule parses but cannot compile

`details: {message}`. Causes: the same capture symbol used twice
(`(seq (capture @x a) (capture @x b))` → rename one); an `any-of` or
`seq` that compiled to zero width.

### `E_UNSUPPORTED_VERSION` — parsed, reserved for a later schema

`details` carries `{feature, version}`. No current DSL feature raises it
(`unify` executed from F8 onward); the code is the contract for future
reserved surface — a rule using a not-yet-available feature fails loudly
with it, never a silent no-match.

### Errors you can hit at scan time (not rule-authoring bugs)

| code | meaning |
|------|---------|
| `E_USAGE` (exit 2) | bad CLI usage — clap's message is inside the envelope |
| `E_VFS_NOT_FOUND` / `E_VFS_PERMISSION` / `E_VFS_IO` | the scan root could not be walked |
| `E_PARSE_NO_GRAMMAR_FOR_LANG` | `--lang name` names a grammar this build does not embed |
| `E_PARSE_NO_TREE` | a parse returned no tree (cancelled/unsatisfiable) — operational, retry |

Exit codes: `0` success (zero matches is success), `1` operational error,
`2` usage error.

## 8. Running a scan

```
janitor scan --rule "<rule>" [PATH] [--lang rust] [--strict-trivia]
             [--include-virtual] [--max-file-size BYTES] [--pretty]
janitor scan --duplicates [PATH] [--min-size N] [--lang rust]
             [--include-virtual] [--max-file-size BYTES] [--pretty]
```

Exactly one mode per invocation: `--rule` (structural search, this page)
or `--duplicates` (F8: fingerprint-grouped duplicate detection — the
report shape lives in docs/SCHEMA.md; `--strict-trivia` is rule-mode only,
equivalence is fixed by ADR-0010, and `--min-size` is the participating-
node floor, default 8).

* `PATH` (default `.`) is the scan root; it is walked recursively.
* Files whose extension has no enabled grammar are ignored (not reported).
* **Hidden entries** (name starting with `.`) are never walked — `.git`,
  `.janitor`, `.hidden.rs` are all invisible to scan.
* **Binary skip**: a NUL byte in the first 8 KiB ⇒ `skipped` with
  `reason: "binary"`.
* **Size skip**: files larger than `--max-file-size` (default `4194304` =
  4 MiB) ⇒ `skipped` with `reason: "size"`. Every skip is listed, so
  exclusions are visible, never silent.
* **Virtual files** (staged in the overlay, not on disk) are excluded by
  default — scan reports disk reality (anti-phantom). `--include-virtual`
  unions them in; their matches carry `"virtual": true`.

Report shape (success envelope; `--pretty` reformats, never restructures):

```json
{"schema":"janitor.v1","ok":true,"data":{
  "rule":"(under block (seq let_declaration let_declaration))",
  "trivia":"skip",
  "matches":[
    {"path":"src/main.rs","virtual":false,
     "start_byte":20,"end_byte":88,
     "start_point":{"row":2,"column":4},"end_point":{"row":5,"column":1},
     "kind":"block","kind_id":132,"overlap":false,
     "captures":[
       {"symbol":"@a","start_byte":21,"end_byte":30,
        "start_point":{"row":2,"column":5},"end_point":{"row":2,"column":14},
        "kind":"let_declaration","kind_id":87}]}],
  "skipped_for_language":{"python": 3},
  "skipped":[{"path":"assets/logo.png","reason":"binary"},
             {"path":"big/generated.rs","reason":"size"}],
  "stats":{"files":12,"bytes":340008,"wall_ms":41,
           "files_matched":3,"matches":5,"langs":{"rust":12}}}}
```

`skipped_for_language` maps language name → number of files skipped
because some rule kind does not exist in that language ([§4](#4-kinds-per-language)).
`wall_ms` is wall-clock and may vary between runs; everything else is
deterministic.

## 9. Worked examples — the five KP-6 idioms (copy-pasteable)

Each rule is paired with its measured counts on its curated snippet
(strict = `--strict-trivia`, skip = default). These five are regression
fixtures: `tests/fixtures/` carries them with these exact expectations.

**1. Consecutive `let` statements in a body** — strict 1, skip 2:

```scheme
(under block
  (seq (kind let_declaration) (kind let_declaration)))
```

**2. Neighboring match arms** — strict 1, skip 2:

```scheme
(under match_block
  (seq (kind match_arm) (kind match_arm)))
```

**3. An attribute attached to a function** — strict 0, skip 1 (the only
attr→fn pair in the corpus is comment-separated; this pattern is the
reason comment-skipping is the default):

```scheme
(under source_file
  (seq (kind attribute_item) (kind function_item)))
```

**4. A chained method call link** (base call, then the next method's
field) — strict 2, skip 3:

```scheme
(under field_expression
  (seq (kind call_expression) (kind field_identifier)))
```

**5. An `if` with an `else`** — strict 1, skip 2. Note `_` as the first
element (the condition) and the nested-scope subtlety: the seq runs over
the `if_expression`'s own children:

```scheme
(under if_expression
  (seq _ (kind block) (kind else_clause)))
```

Two more shapes worth internalizing:

```scheme
;; capture the function name of every fn that has a block body
(under function_item
  (seq (capture @name identifier) (kind block)))

;; a block whose first statement is an if that has an else — nested scope
(under block
  (seq (under if_expression
         (seq _ (kind block) (kind else_clause)))))
```

## 10. Authoring checklist

1. One `(under …)` at the top unless you truly mean every-scope matching.
2. Kinds are per-grammar snake_case named kinds — check [§4](#4-kinds-per-language)
   against the language you scan; unknown-in-every-language fails loud.
3. Adjacency is comment-tolerant by default; add `--strict-trivia` only if
   you mean byte-strict.
4. Capture symbols start with `@`, are used at most once each.
5. `unify` relates capture symbols only ([§3](#constraints-unify-ab--isomorphism-between-captures-f8));
   equivalence ignores comments and formatting, nothing else.
6. Read the report's `skipped`/`skipped_for_language` before concluding a
   rule matches nothing — exclusions are always listed.

---

## 11. Python (v0.2, F9a) — kinds, trivia, and worked examples

Python is fully supported and CI-tested alongside rust (this section
supersedes the v0.1 header note that named rust the only supported
language). Everything in §1–§10 applies verbatim; this section gives
the python-specific kind names and two worked examples with measured
counts. Build with `--features lang-python` (release artifacts embed
all grammars).

**Common python kinds** (tree-sitter-python 0.25.0): `module` (the
root), `function_definition`, `decorated_definition` (wraps a decorated
def: its children are the `decorator`s then the def), `class_definition`,
`identifier`, `parameters`, `block` (any indented suite), `if_statement`
(with `elif_clause` and `else_clause` as named siblings),
`expression_statement` (docstrings are `block > expression_statement >
string`), `import_statement` / `import_from_statement`, `comment`,
`string`, `integer`, `call`, `argument_list`, `attribute`.

**Trivia** (§5): python's table is `comment` only. Newlines and the
scanner's INDENT/DEDENT tokens are anonymous, so they never appear in
the named-sibling list — python's indentation discipline lives inside
the grammar and adjacency ports unchanged. Measured (KP-6-python):
comment-skipping is load-bearing — byte-strict loses 28.6% of
method-adjacency pairs and 10.5% of docstring-then-return pairs on real
comment-bearing code.

### Example 1 — decorator pattern (attribute decoration above a def)

A decorator and its def are siblings inside `decorated_definition`, so
the decoration rule scopes to that container:

```scheme
(under decorated_definition
  (seq (kind decorator) (kind function_definition)))
```

Match counts on the KP-6-python curated snippet
(`tests/fixtures/kp6_python/decorated_def.py`): **strict 1, skip 2** —
a comment between the decorator and the def (legal python) hides the
pair from byte-strict, which is exactly why skip is the default.

To capture the decorated function's name:

```scheme
(under function_definition
  (seq (capture @fn identifier) parameters))
```

(the def inside the wrapper is an ordinary `function_definition`).

### Example 2 — refactor rule (find every class method pair to review)

The "methods that are adjacent in a class body" idiom — the shape a
split-class refactor starts from:

```scheme
(under block
  (seq (kind function_definition) (kind function_definition)))
```

Measured on the bundled python corpus (`tests/fixtures/python/`):
**strict 5, skip 7**. The two lost pairs sit behind section-banner
comments between methods — real code, really unmatchable byte-strict.

### Python authoring notes

- Decorated defs are *wrapped*: to match them at module level use
  `decorated_definition` as the pivot; to match the def itself
  (name/parameters/body) use `function_definition` — it works inside
  the wrapper.
- `lambda` is not `function_definition` (it is an expression node);
  the norm table (`lang/python.norm.toml`) maps FUNCTION to
  def-shaped functions only.
- Import groups: `import x` and `from x import y` are *different kinds*
  (`import_statement` vs `import_from_statement`) — a rule over an
  import group that mixes both needs `any-of`.

---

## 12. Go (v0.2, F9b) — kinds, trivia, and worked examples

Go is fully supported and CI-tested alongside rust and python
(`--features lang-go`). Everything in §1–§11 applies verbatim; this
section gives the go-specific kind names and two worked examples with
measured counts.

**Common go kinds** (tree-sitter-go 0.25.0): `source_file` (the root),
`package_clause`, `function_declaration` (plain functions),
`method_declaration` (methods — the name child is a `field_identifier`,
not an `identifier`), `identifier`, `type_identifier` (types),
`field_identifier` (struct fields and method names), `block` (a braced
body — it wraps a single `statement_list`), `statement_list` (**the**
statement-adjacency scope), `if_statement`, `short_var_declaration`
(the `v, ok := …` init inside an if), `field_declaration_list` /
`field_declaration` (struct bodies and fields; tags are
`raw_string_literal` children), `import_declaration` >
`import_spec_list` > `import_spec`, `type_declaration` > `type_spec`,
`comment`, `int_literal` / `float_literal` / `interpreted_string_literal`
/ `raw_string_literal` / `rune_literal` / `imaginary_literal` (and the
named `true`/`false`/`nil` leaves).

**Trivia** (§5): go's table is `comment` only — line and block comments
are the same kind. Two distinctive measured facts: same-line comments
are legal go between declaration children (`func (t T) /* note */ Name()`,
`if ok /* why */ {`) and land as named siblings, exactly like comments
on their own line; and the else-if chain is same-line, so no comment can
ever sit between its blocks. Measured (KP-6-go): comment-skipping is
load-bearing — byte-strict loses 50% of file-level function-adjacency
pairs on the bundled module.

### Example 1 — doc-comment-above-func (the go file shape)

Every exported go function carries a doc comment, which parks between it
and the previous declaration as a named sibling of `source_file`:

```scheme
(under source_file
  (seq (kind function_declaration) (kind function_declaration)))
```

Match counts on the KP-6-go curated snippet
(`tests/fixtures/kp6_go/doc_func_pair.go`): **strict 1, skip 2** — the
documented pair is invisible byte-strict. Over the bundled module
(`tests/fixtures/go/`): **strict 2, skip 4**.

To capture the name of every plain function (methods need
`field_identifier` — see below):

```scheme
(under function_declaration
  (seq (capture @fn identifier) parameter_list))
```

### Example 2 — refactor rule (find methods whose receiver is unused)

The method-with-receiver shape, scoped inside `method_declaration` —
the receiver is the leading `parameter_list`, the name a
`field_identifier`:

```scheme
(under method_declaration
  (seq (kind parameter_list) (capture @name field_identifier) (kind parameter_list)))
```

This matches every method that has a receiver, a name, and a parameter
list (the `(capture @name field_identifier)` binds the method name for
the report). The same-line-comment subtlety: a `/* role */` note between
receiver and name is legal go and a named sibling — strict loses that
method, skip keeps it (measured in `tests/kp6_go.rs`).

### Go authoring notes

- **Statement adjacency scopes to `statement_list`**, not `block`: a go
  `block` has exactly one named child. `(under block …)` matches
  nothing; write `(under statement_list …)`.
- Methods are `method_declaration`, and their names are
  `field_identifier`s — a name-capturing rule over methods must name
  that kind (the norm table's FUNCTION covers both kinds; IDENT does
  not cover `field_identifier`).
- Struct fields and grouped imports are adjacency scopes too:
  `field_declaration_list` and `import_spec_list` are where field and
  import notes park as named siblings.
- `true`/`false`/`nil` are named leaves in go (unlike rust, where they
  are one `boolean_literal`); LITERAL rules see them per-kind.

---

## 13. TypeScript + TSX (v0.2, F9b) — kinds, trivia, and worked examples

TypeScript is fully supported and CI-tested (`--features
lang-typescript`), and the feature registers **two** languages:
`typescript` (`.ts`/`.mts`/`.cts`) and `tsx` (`.tsx`). The split is
measured, not stylistic: the plain typescript grammar does not parse
JSX — a `.tsx` file under it yields `ERROR` nodes — so `.tsx` routes to
the crate's second grammar, with its own kind table. A single scan over
a mixed module compiles the rule per grammar and reports both. The norm
table (`lang/typescript.norm.toml`) is shared, and every kind in it is
conformance-checked against both grammars.

**Common kinds** (tree-sitter-typescript 0.23.2): `program` (the root),
`export_statement` (wraps most top-level declarations; a decorator is a
sibling of the class inside it), `function_declaration`,
`generator_function_declaration`, `method_definition` (class bodies),
`method_signature` (interface members), `abstract_method_signature`,
`function_signature` (ambient/overloads), `arrow_function`,
`lexical_declaration` > `variable_declarator` (with `type_annotation`
as a named sibling of the name), `interface_declaration` >
`interface_body` > `property_signature`, `class_declaration` >
`class_body` > `method_definition`, `import_statement`, `decorator`,
`statement_block` (bodies — there is no separate `block` kind),
`identifier`, `type_identifier`, `property_identifier` (member names),
`comment`, `number`/`string`/`template_string`/`regex`/`true`/`false`/
`null`/`undefined`.

**Trivia** (§5): the table is `comment` **and** `html_comment`, for
both languages. The grammar accepts Annex-B HTML-like comments
(`<!-- … -->`) in plain `.ts` as named siblings between statements, and
inside tsx they also park between `jsx_text` siblings. `hash_bang_line`
is a named program child but only ever at index 0 — never between
siblings — so it is NOT in the table. Measured (KP-6-typescript):
byte-strict loses 50% of exported-declaration adjacency pairs on the
bundled module.

### Example 1 — JSDoc-above-function (the module shape)

```scheme
(under program
  (seq (kind export_statement) (kind export_statement)))
```

Exported-declaration adjacency over the bundled module
(`tests/fixtures/typescript/`): **strict 2, skip 4** — each file's
JSDoc/divider comments hide one pair from byte-strict. To capture
function names instead:

```scheme
(under function_declaration
  (seq (capture @fn identifier) formal_parameters))
```

(this unwraps `export_statement` — the function is an ordinary child
inside it — and works unchanged under the tsx grammar).

### Example 2 — refactor rule (decorated classes to audit)

```scheme
(under export_statement
  (seq (kind decorator) (capture @cls class_declaration)))
```

The decorator and the class are named siblings inside the
`export_statement` — a comment between them is legal typescript and
hides the pair from byte-strict (measured: strict 1, skip 2 on the
KP-6-typescript curated snippet).

### TypeScript authoring notes

- Top-level functions usually live inside `export_statement`: scope
  module-level adjacency rules to `export_statement` pairs, and scope
  name/parameter/body rules to the inner `function_declaration` (it
  works inside the wrapper, like python's defs inside
  `decorated_definition`).
- Interface member style never matters: `;` and newline separators are
  anonymous tokens, equally invisible. Comments between members are
  the only separator that bites (strict loses the pair).
- `arrow_function` and `function_expression` are expression-position
  functions and are NOT the norm table's FUNCTION (the python `lambda`
  precedent); `function_type` is a type, not a function.
- `.d.ts` shapes: `function_signature` and `abstract_method_signature`
  are declaration-position FUNCTIONs; a rename rule that misses them is
  incomplete — the norm table includes them for exactly that reason.
- In `.tsx` files the JSX surface (`jsx_element`, `jsx_self_closing_element`,
  `jsx_text`, `html_comment`) is addressable like any other kind;
  `{/* … */}` is a plain `comment` inside a `jsx_expression`, while a
  bare `<!-- … -->` is an `html_comment` between `jsx_text` siblings.
