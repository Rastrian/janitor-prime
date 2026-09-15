# ADR-0018 — The daemon: warm parse cache, watch mode, and a janitor.v1 Unix socket

Status: accepted (F10a, 2026-09-15). Lives in its own file — the first ADR to do
so — because the F10a increment it records is larger than a PLAN §4 paragraph;
PLAN.md §4 carries a one-line pointer. It is executed on `f10a/daemon`.

## Context

KP-10 measured the accepted v0.1 wart: a cold full-repo scan re-parses
everything, every invocation — rust-analyzer's tree (1,484 files / 16.7 MB)
costs 2.21 s serial with a single parser. ADR-0007 named the fix ("the
daemon/LSP story") and reserved it for F10; ADR-0008 kept async runtimes out on
the grounds that the CLI is short-lived; ADR-0017 decoupled *semantic*
validation (external LSP guidance) from this decision. The owner funded the F10
daemon increment (warm cache + watch) with hard rules: no new dependencies, no
network, socket perms 0600, bound to one workspace, holds no secrets, executes
no code.

What a daemon changes fundamentally: one long-lived process owns state the CLI
cannot — a warm `ParseCache` (F3) that survives across invocations, kept fresh
by a filesystem watcher between requests. Everything else (scan/duplicates
pipelines, the janitor.v1 envelope, the rayon pool, Rules T1–T6) is reused
as-is.

## Decision

### D1 — `janitor daemon` owns one warm `ParseCache` behind a Unix domain socket

```
janitor daemon [--workspace DIR] [--sock PATH] [--interval SECS]
               [--max-cache-mb N] [--idle-secs N]
```

- **Socket**: Unix domain socket, default `$JANITOR_SOCK` or
  `/tmp/janitor-$UID.sock` (uid resolved without a libc dependency: the owner
  uid of `/proc/self` on Linux, `$UID` as fallback, plain `janitor.sock` as the
  last resort — resolution order is part of this contract). File permissions
  **0600** at bind time (only the owner uid can connect).
- **Protocol**: the SAME `janitor.v1` envelope, newline-delimited — one JSON
  request line in, one JSON envelope line out, one request per connection
  (simple req/resp; no streaming in this increment). Commands: `scan`,
  `duplicates`, `stats`, `ping` (liveness + cache stats), `shutdown`. `apply`
  is deliberately NOT socket-exposed in v0.2 (see D5).
- **Unix socket ≠ network**: the no-network guarantee is unchanged. A Unix
  domain socket is a kernel-mediated local IPC object addressable only through
  the filesystem (and here only by the owning uid, per 0600); it cannot be
  dialed from another host, performs no DNS, and opens no outbound connection.
  The daemon makes zero network calls of any kind.
- **State**: one `orchestrator::Session` (the composition root, §1.6) over an
  `Os`-backed overlay, memory layer rooted at `<workspace>/.janitor/`,
  long-lived `ParseCache` + `LangRegistry`.

### D2 — Watch mode is POLLING (2 s default, `--interval`), not inotify

Every `--interval` seconds the daemon stats the **warm set** — exactly the
paths the cache holds entries for — collecting `(size, mtime)` via
`std::fs::metadata` and diffing against the previous snapshot (cheaper and
precisely scoped: a file nothing cached is nothing to warm; new files enter
the set when the next scan parses them). Changed warm files are re-parsed
**incrementally**
(KP-1's winning path: common-prefix/suffix diff → one `Splice` →
`ParseCache::take` → `Tree::edit` + re-parse → fresh entry inserted); deleted
files have their entry dropped. Ignored at every level: dot-prefixed entries
(`.git/`, `.janitor/` — same policy as `scan`'s walk) and `target/`.

Why polling, for v0.2: **zero new dependencies** (the `notify`/inotify crates
are the alternative — a new native-dep surface in a supply-chain-audited
static binary for a warm-cache use case where 2 s staleness is fine); fully
portable across the CI/release matrix (linux musl/gnu, macOS — no inotify vs
FSEvents vs kqueue matrix); trivially testable (deterministic: one pass = one
function call, no kernel event queues to fake); and the cost is one metadata
stat per file per interval — ~1.5 k stats/s against rust-analyzer's 1,484
files, negligible against a parse pass. inotify/`notify` remains the documented
upgrade path when watch consumers need sub-second latency (it can replace the
*detection* half without touching the re-parse half — the seam is the poll
function).

### D3 — Freshness keys on Rule T4, not on the watcher; the watcher is an optimization

The overlay's `Generation` counters track **overlay mutations** (`set`/`delete`)
— external on-disk edits cannot and must not bump them (staging external
content into the overlay buffer to force a bump would let the overlay shadow
disk with stale bytes on a subsequent external change: the daemon would serve
worse-than-cold data). Therefore:

- Correctness never depends on the watcher: `ParseCache::get` validates
  `generation ∧ content-hash` (Rule T4's belt-and-suspenders leg); a file
  changed on disk after the last watcher pass is a guaranteed miss and a fresh
  parse. Invalidation stays **never-heuristic** — the poll's (size, mtime)
  diff only decides *when* to re-parse, never *whether* an entry is valid.
- The watcher's proactive re-parse is the warm path: between requests it
  replaces the stale entry with a fresh one (same overlay generation, new
  bytes), so the next scan hits instead of missing. A watcher failure (file
  vanished mid-read, transient I/O) is logged to stderr and the entry is left
  evicted — the next scan re-parses cold; the daemon never crashes on watch
  errors.

### D4 — Memory bound: LRU eviction over accounted entry cost (`--max-cache-mb`, default 512)

`ParseCache` grows a bound (bytes; `None` = unbounded, the CLI's shape). Each
entry's **cost** = `snapshot bytes + node_count × 16 B` (node count computed in
the same walk that precomputes `error_count`; 16 B is the documented
tree-sitter subtree-slot approximation — the budget bounds *accounted* bytes,
an honest estimate, not an RSS promise). `insert` stamps a monotonic tick and
evicts least-recently-used entries until under the bound (LRU sweep on the
insert path, skipped while under bound); `get` refreshes the tick. **Eviction
is free** — no entry is ever "locked": the apply/watch re-parse paths *take*
(exclusively own) before editing (Rule T1), so an evicted handle is just a
shared-read clone whose entry was dropped; a later query re-parses on demand.
Hit/miss counters (atomic, exact) live on the cache and surface through
`ping`/`stats`.

The daemon flips ADR-0007's streaming retention for its own scans
(`retain_cache: true` — entries stay after the report); the CLI keeps the
streaming discipline bit-for-bit. ADR-0007's warning — "retention is pure cost
at a zero hit rate" — is exactly why retention is opt-in per caller: only a
long-lived process can profit.

### D5 — Concurrency: thread-per-connection over the existing rayon pool; mutations stay take-then-own

- The accept loop is plain `std::os::unix::net` + one thread per connection
  (capped; over cap → typed `E_DAEMON_BUSY`). Request bodies run on the
  **existing rayon pool** — `scan`'s internal `par_iter` — unchanged.
- **Locking story** (guard discipline §1.1 + T4, restated for the daemon):
  dashmap shard guards are held only to clone/tick an entry — never across a
  parse or traversal; byte snapshots are `Arc<[u8]>` clones, so N concurrent
  scanners never serialize on a lock; `ParseCache::take` is the one-way
  ownership door every mutating path goes through before `Tree::edit`
  (exclusivity guard asserts `Arc::strong_count == 1`, Rule T1).
- **The only mutating paths in v0.2** are the watcher's re-parses (D3) and LRU
  eviction (D4). `apply` is deliberately CLI-only in this increment: routing
  the one *destructive* operation (flush writes files) through a second,
  concurrent front door would need a cross-process write-serialization story
  (the daemon and a CLI apply can both flush) that v0.2 has no budget for —
  the CLI's one-writer-per-invocation discipline stays intact. Socket-side
  `apply` is the natural F10b candidate, with a mutation queue or a
  daemon-owned flush as the design question.
- Consequence for races: a scan racing a watcher re-parse sees either the old
  entry (validated hit — still a *consistent* (bytes, tree) pair) or a miss;
  a scan racing eviction sees a miss; nobody ever observes a torn entry.

### D6 — Security posture (hard rules, restated as implemented)

1. Socket permissions 0600 at bind (verified by test).
2. Workspace-bound: the daemon pins its workspace at start
   (`--workspace`, default cwd); every request path is resolved and
   **canonicalized** and must fall inside the workspace root — anything else
   is the typed `E_DAEMON_PATH_OUTSIDE_WORKSPACE` (no `..`, no symlink escape:
   canonicalization resolves symlinks before the prefix check). The default
   request path is the workspace itself.
3. No network (D1); no secrets held; no code executed — the daemon parses
   bytes and runs the same pure matcher the CLI does. It accepts exactly five
   commands and rejects everything else with typed errors.
4. Request line capped (1 MiB — a rule plus flags fits comfortably; oversized
   → typed `E_DAEMON_REQUEST_TOO_LARGE`), connection count capped (D5).

### D7 — Lifecycle: idle death, stale-socket recovery, JSON-only stdout

- **Idle timeout**: with no connections for `--idle-secs` (default 1800), the
  daemon exits — stale daemons die naturally. `ping` exists precisely so
  clients can check before spawning.
- **The CLI never spawns the daemon in v0.2** (explicit `janitor daemon`
  start; documented). `janitor scan --daemon` is opt-in client routing: if the
  socket answers `ping` AND the daemon's workspace matches the scan root's
  workspace, route through it; otherwise fall back silently to the in-process
  path (stderr note only). Workspace mismatch also falls back — a daemon
  serving another tree must never answer for this one.
- **Crash safety** (`kill -9` mid-anything): a dead daemon leaves its socket
  file behind (the kernel does not unlink it). On bind collision the daemon
  *probes* the socket: if it answers, another daemon lives → typed
  `E_DAEMON_ALREADY_RUNNING`; if it is dead (connect refused/ENOENT), the file
  is stale → removed, bind retried. Normal exits (shutdown, idle) unlink the
  socket themselves.
- **stdout stays 100% JSON**: the daemon emits exactly one `janitor.v1`
  envelope line on startup (`listening`: socket, workspace, pid, config) and
  one on exit; everything else is the socket or stderr (`tracing`).

### D8 — ADR-0008 is PARTIALLY REVERSED; ADR-0017 is untouched

ADR-0008 ("no async runtime in v0.1") rested on "the CLI is short-lived". The
daemon removes that premise — a long-lived process is now a supported shape —
so the ADR is **partially reversed by owner approval**: long-lived processes
are in scope. What is NOT reversed: *tokio stays out*. The daemon is
`std::os::unix::net` + one thread per connection + rayon — the connection
count is per-user-small (0600 socket), requests are one-shot JSON lines, and
the async-shaped consumer (LSP, JSON-RPC lifecycle) remains unfunded. The
distinction is recorded so "no async" is not misread as "no daemon" or vice
versa. ADR-0017 needs no change: this daemon is a *performance* layer (warm
cache), not a semantic-validation layer — tree-sitter re-parse remains
janitor's only gate, external LSPs remain the semantic guidance.

## Alternatives rejected

- **tokio/tower + a real RPC framework** — the full ADR-0008 reversal. Rejected:
  one-shot req/resp over a local socket has no concurrency shape that needs an
  async runtime; the dependency cost (supply-chain surface, binary size, the
  audit story the release pipeline advertises) buys nothing here. Revisit with
  LSP (the genuinely async consumer), if funded.
- **`notify`/inotify for watching** — rejected for v0.2 (D2): new native
  dependency + per-OS backend matrix vs. a 2 s poll that is `stat`-bound and
  portable. Seam documented for a later swap.
- **Disk cache (`.janitor/cache/parse/`, the ADR-0007 reserved path)** — rejected:
  the warm problem is *process* death-per-invocation, not machine death; a disk
  format (versioned, crash-safe, ABI-tied to tree-sitter) is a maintenance
  contract the socket daemon makes unnecessary. Reservation stays reserved.
- **Stale-socket removal without probing** (blind unlink on EADDRINUSE) —
  rejected: it would let a second daemon steal the socket of a live one.
  Probe-then-remove is two syscalls and is race-free for our purposes (the
  window where two daemons race the probe ends with one of the two binds
  failing loudly).
- **`apply` through the socket in this increment** — rejected (D5): the
  cross-process mutation story deserves its own design; shipping half of it
  would create the exact corruption surface the guard discipline exists to
  prevent.
- **Generation-bump-on-external-change** (staging watched bytes into the
  overlay to force T4 invalidation) — rejected (D3): it would let the overlay
  buffer shadow disk with stale content between polls, degrading the daemon
  below cold-CLI freshness guarantees.

## Consequences

- Warm-cache proof and the incremental/eviction/concurrency/hygiene/crash
  fences are integration tests over the real socket (`tests/daemon_e2e.rs`) —
  the daemon is production code: deny lints, no `unwrap`/`expect`/`panic`,
  100% JSON on stdout.
- `ParseCache` grows hit/miss counters, LRU ticks, cost accounting, and an
  optional bound — additive; the CLI path (unbounded + streaming) is
  byte-identical in behavior and covered by the existing fences.
- `scan`/`duplicates` gain `retain_cache` (default false) — the daemon sets it
  true; nothing else in the pipeline changes (reports stay byte-identical
  modulo `stats.wall_ms`).
- New error family `E_DAEMON_*` (append-only, per the F0 frozen-surface rule):
  `E_DAEMON_BIND`, `E_DAEMON_ALREADY_RUNNING`, `E_DAEMON_PATH_OUTSIDE_WORKSPACE`,
  `E_DAEMON_REQUEST_MALFORMED`, `E_DAEMON_REQUEST_TOO_LARGE`, `E_DAEMON_BUSY`,
  `E_DAEMON_CONNECT`, `E_DAEMON_PROTOCOL`.
- `docs/SCHEMA.md` documents the request/response shapes (append-only);
  CHANGELOG carries the measured warm-vs-cold numbers.
- Later, when MCP (the parallel lane) wants warm parses, it speaks to this
  socket — one warm-cache implementation, two front ends.
