# Design: M0 — See agent trace live

**Status**: Draft for review (before implementation) · **Spec**: [spec.md](spec.md) · **Plan**: [plan.md](plan.md) ·
**Master design**: [design-notes.md](../000-tracedb-master-plan/design-notes.md)

## 1. What this design has to achieve

From the [spec](spec.md#end-state), M0 must:
- take uploads from an unchanged LangSmith SDK client;
- confirm an upload only after it is durably recorded;
- show it in the browser within 1 s and keep open screens live;
- open a 250-step trace with tree and waterfall in under 1 s;
- survive `kill -9` with nothing confirmed lost.

All of this runs as one binary with no outbound traffic. It should be the **simplest** design that does this while
keeping the **service boundaries** that M1+ build on (roadmap decisions, constitution II).

## 2. Big picture

```
                         tracedb (one process)
 ┌───────────┐   HTTP   ┌──────────────────────────────────────────────────────────────┐
 │ LangSmith │ ───────▶ │  HTTP layer                                                  │
 │ SDK agent │          │   LangSmith compat · native events · query API · tail · UI   │
 └───────────┘          └───────┬───────────────────────┬──────────────────────┬───────┘
 ┌───────────┐                  │ accept(batch)         │ get/list             │ subscribe
 │  Browser  │ ◀── UI + SSE ──  ▼                       ▼                      ▼
 └───────────┘          ┌───────────────┐       ┌───────────────┐      ┌──────────────┐
                        │ Ingest service│       │ Query service │      │ Tail hub     │
 ┌───────────┐          │  validate     │       │  trace tree   │      │  fan-out of  │
 │ tracedb   │ ─HTTP──▶ │  dedup        │       │  single run   │      │  run changes │
 │ seed      │          │  number (seq) │       │  root list    │      └──────▲───────┘
 └───────────┘          │  ┌─────────┐  │       └───────┬───────┘             │
                        │  │  WAL    │  │ apply         │ read snapshot       │ publish
                        │  └─────────┘  │──────▶ ┌──────▼───────┐ ────────────┘
                        └───────────────┘        │  Run store   │  (in memory = memtable)
                                                 └──────────────┘
                       ./.tracedb/wal/*.wal  (the only thing on disk in M0)
```

**The one idea to hold on to:** a single **writer** owns the WAL and the run store. Every upload goes through it, in
order. It writes to the WAL, fsyncs, applies to the store, publishes the change, and only then confirms. Readers
(query, tail) never write. This gives durability, ordering and consistency with almost no locking logic.

## 3. Core components

| # | Component | Responsibility | Interface (shape) | SmithDB counterpart | Changes in later phases |
|---|---|---|---|---|---|
| 1 | **`model`** (crate) | Types and rules, no I/O: `RunEvent`, `Run`, the merge rule, `dotted_order` parsing, derived `status`/`latency`, validation | pure functions: `merge(&mut Run, &RunEvent)`, `validate(&RunEvent)`, `derive(&Run)` | run schema | unchanged; reused by every service |
| 2 | **HTTP layer** (in the `tracedb` binary) | Translate wire formats to `RunEvent`s and back; routes; body limits; serve the UI | routes in [contracts/http-api.md](contracts/http-api.md) | API servers | M2 adds multipart; M1 adds `/api/query` |
| 3 | **Ingest service** (crate) | Validate, dedup, assign `event_seq`, write the WAL with group commit, apply to the store, publish, confirm | `Ingest::accept(Vec<RunEvent>) -> AcceptResult` (async; resolves after fsync) | ingestion service | M1: flushes the store to disk segments, then trims the WAL |
| 4 | **WAL** (inside ingest) | Append-only log of accepted events; replay on start | `append(batch) -> Offset`, `sync()`, `replay() -> iter<RunEvent>` | WAL on ingestion node | M1: trimmed after each flush |
| 5 | **Run store** (inside ingest, read by query) | Current merged state of every run in memory, plus indexes for the three M0 reads | `apply(&RunEvent) -> Change`; read-only `Snapshot` | memtable | M1: becomes the "fresh data" side of every query (`fresh_scan`) |
| 6 | **Query service** (crate) | Answer reads from a store snapshot: trace tree, one run, root list with cursor | `trace(id)`, `run(id, include)`, `roots(project, status, cursor, limit)` | query service | M1: same API over DataFusion (segments ∪ store) |
| 7 | **Tail hub** | Fan out run changes to open browsers; coalesce bursts | `subscribe(filter) -> Stream<RunHeader>` | live tail | M1: filter by any query |
| 8 | **UI** (`ui/`, embedded) | Runs list (live, paged), trace view (tree + waterfall), step panel, empty state, URL state | consumes the HTTP API | LangSmith UI | M1: query builder, field browser |
| 9 | **CLI** (`tracedb` binary) | `serve`, `seed` | [contracts/cli.md](contracts/cli.md) | — | grows per phase |

Crates in M0: `model`, `ingest`, `query`, `tracedb` (binary), plus `ui/`. The `storage` and `compact` crates arrive with
M1 and M3, when there is something to put in them (keep it simple).

**Boundary rules** (these keep the services separable later):
- `query` depends on `model` and on a read-only `Snapshot` trait exported by `ingest`, never on ingest internals.
- `ingest` knows nothing about HTTP, `query` or the UI.
- The HTTP layer is the only place that knows LangSmith field names. Everything inside uses `RunEvent`/`Run`.

### 3.1 What each component can and cannot do in M0

Every component lists what it **can** do, what it explicitly **cannot** do in M0 (non-goals), its **limits** in
numbers, and when a non-goal is lifted. A "cannot" here is a promise, not an oversight: no code for it is written in M0.

#### 1. `model`
- **Can**: parse and validate events; merge them (latest present value wins; identity fields immutable); derive
  status, latency, depth, tokens and model; check `dotted_order`.
- **Cannot**: compute cost (no price table); append fields; resolve conflicting clocks (it trusts arrival order, not
  client timestamps); support schema versions other than the LangSmith run shape plus native events.
- **Limits**: field sizes in [data-model §6](data-model.md#6-validation-and-rejection-reasons).
- **Later**: cost (M2); feedback fields (M2/M3).

#### 2. HTTP layer
- **Can**: answer `/info`; accept `/runs/batch`, `POST /runs`, `PATCH /runs/{id}`, `/v1/events`; serve the query
  API, live tail and embedded UI; enforce body limits.
- **Cannot**: accept multipart uploads (`415`), attachments (sizes noted, content dropped), feedback, compressed
  bodies (zstd), OpenTelemetry; check API keys (any key is accepted, since it's localhost only); serve TLS; accept
  connections from other machines by default.
- **Limits**: body ≤ 50 MB; `/info` advertises `size_limit` 100 items and 20 MB per batch.
- **Later**: multipart + feedback (M2); OTLP (backlog).

#### 3. Ingest service (the writer)
- **Can**: accept batches from many clients at once; dedupe retries; assign a global order; make every event durable
  before confirming; apply it to the run store; publish the change; report per-item rejections.
- **Cannot**: write anything except the WAL (no columnar files, no metastore); scale past one writer; trim the WAL;
  apply back-pressure per client (all clients share one queue); reorder events by client time.
- **Limits**: ≥ 5,000 events/s target (SC-07); in-flight queue 10,000 events, beyond which requests get `429` + `Retry-After: 1`;
  group commit every ~5 ms or 1 MB.
- **Later**: flush to disk segments and trim the WAL (M1); 20,000 events/s (M2).

#### 4. WAL
- **Can**: append records with checksums; fsync in groups; roll files at 64 MB; replay in order; truncate a torn tail.
- **Cannot**: be trimmed or compacted (it grows for the whole life of the data folder); be read by anything except
  replay; be copied safely while tracedb runs; survive disk corruption in the middle of a file (replay stops at the
  first bad record and reports it).
- **Limits**: disk use ≈ the raw upload size (every update is kept, including superseded ones). Restart time grows
  with WAL size (open question §11.4).
- **Later**: trimmed after each flush (M1), so restart replays only the short unflushed tail.

#### 5. Run store
- **Can**: hold every run merged in memory; answer the three M0 reads from memory; detect duplicates for 24 h; count
  its own size against the budget; report each change.
- **Cannot**: **be persisted**: it is never written to disk, and it is rebuilt from the WAL on every start. It also
  can't evict or age out runs, hold more than the memory budget, answer filters other than project and status, sort
  by anything other than newest root first, or search text.
- **Limits**: default 2 GB or 500k runs (`--max-memory`); new runs are refused past it, and updates to known runs
  are still accepted.
- **Later**: becomes a true memtable that is flushed and emptied (M1); the limit then applies only to unflushed data.

#### 6. Query service
- **Can**: return a trace tree (headers only, depth-first order), one run with chosen payload fields, and root runs
  newest first with a cursor, filtered by project and status.
- **Cannot**: filter by tags, metadata, latency, time range, name or any JSON field; apply tree-aware filters; do
  text search, aggregates, stats, threads or export; query across projects in one call; run SQL.
- **Limits**: page size ≤ 200; a trace response holds all its runs (practical to ~10,000 runs per trace).
- **Later**: query builder, filters, tree-aware filters (M1); search and stats (M2); threads and export (M3).

#### 7. Tail hub
- **Can**: push changes to every open tab within 1 s, filtered by project (runs list) or trace (trace view);
  collapse bursts; tell slow tabs to resync instead of slowing the writer.
- **Cannot**: filter by anything except project or trace; replay history ("send me what I missed since X"); deliver
  every intermediate version of a run (only the latest per 100 ms window); guarantee delivery (a tab can always resync);
  push payloads (headers only).
- **Limits**: ring of 4,096 changes; ≤ 5,000 pending runs per tab; ≤ 10 messages/s per tab; heartbeat every 15 s.
- **Later**: subscribe by any query (M1).

#### 8. UI
- **Can**: show the live runs list (project and status pickers, paging); open a trace as tree + waterfall; show a step's
  full inputs, outputs, events and error; keep all state in the URL; show an empty state with setup instructions.
- **Cannot**: build queries or filter by anything else; search; show charts or stats; compare runs; show threads;
  export; edit, delete or annotate; show `serialized` or attachments; work offline from a saved file; give
  multi-user views or log in.
- **Limits**: trees stay smooth to ~10,000 steps (virtualized); payloads over 2 MB render as collapsed raw text first.
- **Later**: query builder and field browser (M1); search, charts, compare (M2); threads, export, feedback, ranking (M3).

#### 9. CLI
- **Can**: `serve` (data folder, port, bind address, memory budget); `seed` (N traces with realistic shapes, a fixed
  random seed, sent through `/runs/batch` like an agent).
- **Cannot**: `query`, `tail`, `stats`, `export`, `import`, `demo`, `bench`, `mcp`; run as a background service or
  daemon; migrate or repair a data folder.
- **Later**: `query`, `seed --scenario`, `demo` (M1); `stats`, `bench` (M2); `export` (M3); `import`, `mcp` (M4).

## 4. Key flows

### 4.1 Write path (one upload)

```
SDK ──POST /runs/batch──▶ HTTP: parse → map LangSmith fields → Vec<RunEvent>
                           │   (bad items → rejected list; never fails the batch)
                           ▼
                Ingest.accept(events) ──channel──▶ writer task
                                                    1. dedup: drop events already seen
                                                    2. assign event_seq (monotonic)
                                                    3. WAL append (buffered)
                                         ┌───────── 4. group commit: fsync every ~5 ms or 1 MB
                                         │          5. apply to run store (merge)
                                         │          6. publish changes to the tail hub
                                         └────────▶ 7. resolve every waiting request
HTTP ◀── 202 {accepted, rejected[]} ─────┘
```

- **Group commit**: many requests share one fsync. The writer (a dedicated thread) takes everything that queued up while
  the previous fsync ran (up to 1 MB), appends it, and fsyncs once, so there is no fixed wait. This is how one writer
  reaches ≥ 5k events/s in M0, and later 20k.
- **Confirm after fsync, apply before confirm**: if the confirmation reached the client, the event is both durable and
  visible. That makes "visible within 1 s" true by construction.

### 4.2 Read paths
- **Trace tree**: from the store's `by_trace` index, return every run's header (no payloads) sorted by `dotted_order`.
  The UI builds the tree and waterfall from this one response.
- **One run**: look it up by id and include the payload fields that were asked for.
- **Root list**: from the store's `roots` index, ordered by `(start_time desc, run_id)`, with a keyset cursor
  (master research R9) and optional project/status filters.
- Reads take a **snapshot**: a short read lock while copying the needed headers out, so readers never block the writer
  for long.

### 4.3 Live tail (fan-out and coalescing)

```
 writer ──publish(Change)──▶ ① broadcast ring ──▶ ② one task per open tab ──SSE──▶ ③ browser
          (never blocks)       last 4,096 changes    filter → coalesce → send       apply by version
```

1. **Publish.** After fsync and before confirming, the writer sends `Change{kind, header: Arc<RunHeader>}` to a
   `tokio::sync::broadcast` ring. `kind` is `new_root`, `updated` or `new_child`. The header has no payloads and is
   shared, not copied. Sending never blocks; when the ring is full, the oldest change is overwritten.
2. **Versioned.** Every header carries `last_seq`. A higher `last_seq` always wins, so duplicates, late arrivals and
   out-of-order changes are harmless.
3. **Per-tab task.** `GET /api/tail?project=P` (runs list: root runs of P) or `?trace=T` (trace view: every run of T).
   The task subscribes **first**, then sends `hello`. The browser loads its view only after `hello`, so nothing
   between load and subscribe is missed.
4. **Coalesce.** The task keeps `pending: run_id → newest header`. Many updates to one run in a window collapse to one
   entry; many runs go out together as one `event: runs` message holding an array.
5. **Timing.** If nothing was sent in the last 100 ms, the first change goes out **immediately** (leading edge); later
   ones wait for the next 100 ms tick. So a quiet system is instant, and a busy one sends at most 10 messages/s per tab.
6. **Overload.** If the tab falls more than 4,096 changes behind (`Lagged`), or `pending` passes 5,000 runs, the task
   drops `pending` and sends `event: resync`; the browser reloads its current view. The writer never waits for a tab.
7. **Keep-alive and reconnect.** A comment every 15 s keeps proxies from closing the connection. On disconnect the
   browser reconnects by itself, gets `hello`, and reloads.

SSE event types: `hello {server_seq}`, `runs [RunHeader…]`, `resync {}`.

### 4.4 Start and restart
Open the WAL directory, replay every record through the same `merge` used live, rebuild the indexes, then open the
HTTP port. Partial or corrupt tail records (from a kill mid-write) are detected by their checksum and truncated.
They were never confirmed, because confirmation waits for fsync.

**What is persisted in M0: only the WAL.** The run store is never written to disk; it is a view of the WAL, rebuilt on
every start. Restart time therefore grows with the WAL (about 1 s per 200–500 MB), and the WAL is never trimmed in M0.
M1 removes both by flushing to segments and trimming the WAL.

## 5. Data structures and formats

**WAL record** (in `./.tracedb/wal/<seq-start>.wal`, rolled every 64 MB):
```
[u32 length][u32 crc32][u64 event_seq][JSON RunEvent bytes]
```
JSON keeps the log debuggable. A binary encoding can replace it behind the same reader if profiling shows a need.

**Run store** (all behind one `RwLock`, mutated only by the writer):
| Structure | Key → value | Serves |
|---|---|---|
| `runs` | `run_id → RunState { run: Run, last_seq, payload_bytes }` | one run, merging |
| `by_trace` | `trace_id → Vec<run_id>` (sorted by `dotted_order` on read) | trace tree |
| `roots` | `BTreeSet<(Reverse(start_time), run_id)>` per project | root list + cursor |
| `seen` | dedup set of `(run_id, op, hash(body))`, last 24 h | duplicate detection |

- **Merged eagerly** on write. M0 keeps one merged `Run` per id, which is the simplest choice. In M1 the flush writes
  these merged rows to disk segments, and later updates merge on read, as SmithDB does.
- **Memory budget**: the store tracks approximate bytes (headers + payloads). Past the budget (default 2 GB) or 500k
  runs, `accept` rejects new runs with `capacity` and tells the user that M1 lifts this limit.

**Example contents** after one research-agent trace, with the `fetch_url` start and update applied:
```
runs
  0b9f…11 → research_task  agent  running  start 02:14:00  children 3   seq 1001..1001
  7c1e…a4 → fetch_url      tool   error    31 076 ms  "429 Too Many…"  seq 1042..1987  inputs 31 B, outputs 19 B
by_trace
  0b9f…11 → [0b9f…11, 3d2a…, 7c1e…a4, 9e44…]
roots
  research-agent → [(02:51:10, a81c…), (02:14:00, 0b9f…11), (02:09:51, 55f0…)]    newest first
seen
  (7c1e…a4, post, 91fa…), (7c1e…a4, patch, c03b…), …                              last 24 h
bytes = 1.3 GB of 2 GB budget
```
Readers take a short read lock, copy out headers, and release it; only the writer takes the write lock.

**Cursor**: base64 of `{start_time, run_id, filter_hash}`.

## 6. Mapping LangSmith uploads to events

| LangSmith | `RunEvent` |
|---|---|
| `post` item / `POST /runs` | `op = post` |
| `patch` item / `PATCH /runs/{id}` | `op = patch` |
| `session_name` | `project` (else `session_id`, else derived from `address`, else `default`) |
| `address` (SDK 0.14+) | `address`; sets `project` to `<agent id>/<environment>` when no project is named |
| `extra.metadata` | `metadata` |
| `inputs`, `outputs`, `events`, `error`, `tags`, times, `run_type`, `name`, ids, `dotted_order` | same meaning |
| `serialized`, attachments | dropped in M0, with sizes logged |

`GET /info` answers with `version: ""` (no compatibility claim; the SDK then skips its version warnings), `tracedb_version`,
and `batch_ingest_config.use_multipart_endpoint = false`, so the SDK sends JSON batches
(master research R2). This is pinned by contract tests built from recorded SDK traffic.

## 7. UI design (M0 screens)

```
┌ tracedb ─ project: research-agent ▾ ─ status: all ▾ ───────────────── ● live ┐
│ status  name           start      latency  tokens   tags                       │
│ ● run   research_task  02:51:10   12m…     18,200   batch=a                    │
│ ✕ err   research_task  02:14:07   41m 3s   48,210   batch=a                    │
│ ✓ ok    research_task  02:09:51   12m 40s  22,980   batch=a      [ older ▸ ]   │
└───────────────────────────────────────────────────────────────────────────────┘
┌ trace 0b9f… ─────────────────────────────────────┬ fetch_url (tool) ✕ 429 ────┐
│ ▾ research_task   ███████████████████████  41m   │ inputs                       │
│   ▸ plan          █                         2s   │  { "url": "https://…" }      │
│   ✕ fetch_url       ████                   31s   │ outputs                      │
│   ▾ summarize          ██████               3m   │  { "status_code": 429, … }   │
│       llm              ██                   4s   │ error                        │
│                                                  │  HTTPError: 429 Too Many …   │
└──────────────────────────────────────────────────┴──────────────────────────────┘
```
- **Routes**: `/` (list), `/trace/:id?run=:runId`. All state lives in the URL (shareable).
- **Large trees**: virtualized rows; the tree and the waterfall share one row model, so collapsing a branch updates
  both.
- **Live**: one SSE stream per tab. New roots are added at the top if you're scrolled there; otherwise a "N new runs"
  pill appears.
- **Payloads**: fetched only when a step is selected; JSON shown collapsible, long strings wrapped.

## 8. Failure handling and limits

| Situation | Behaviour |
|---|---|
| Malformed item | rejected with a reason; rest of batch accepted |
| Body > 50 MB, or field > 20 MB | `413` / per-item rejection with reason |
| Memory budget reached | a batch with any new run gets `507` and nothing in it is applied; batches with only updates to known runs are still accepted |
| Disk full / fsync error | the writer stops confirming; requests get `503` + `Retry-After: 5`; health shows the error; nothing confirmed is lost |
| Kill mid-write | the unconfirmed tail is truncated on replay |
| Slow browser | it gets `resync` and reloads its view; the writer never waits (§4.3) |
| Writer queue full (> 10,000 events in flight) | `429` + `Retry-After: 1`; the SDK retries; nothing is half-accepted |
| SDK sends feedback (`POST /feedback`) | `501` "feedback is supported from M2" (a `404` would be retried by the SDK) |
| Unsupported upload format (multipart) | `415` with "supported from a later version" |

## 9. How M0 is proven (tests)

| Check | How | Guards |
|---|---|---|
| Merge rule | unit + property tests in `model` (any order of events → same result as sorted by seq) | M0-FR-03 |
| WAL replay | property test: random batches, random truncation point → replay equals the confirmed prefix | M0-FR-04 |
| **Crash test** | test harness spawns `tracedb serve`, uploads continuously while logging confirmed ids, `kill -9` at random times, restarts, checks every confirmed id is present; 10 rounds | SC-04 |
| **SDK contract** | fixtures recorded once per pinned Python SDK (0.14.17 and 0.8.18) against a recording endpoint; a Rust test replays them and compares resulting runs to golden JSON | SC-06 |
| **No outbound** | CI runs the demo under a syscall trace (`strace -f -e connect`) and fails on any non-loopback address | SC-05 |
| Live latency | integration test: upload, then time until the event shows on the tail stream (p99 < 1 s) | SC-02 |
| Trace open | integration test with a seeded 250-step trace: API + render under 1 s | SC-03 |
| End-to-end demo | manual + scripted: real LangGraph example agent (quickstart) | SC-01 |

## 10. SmithDB mapping and what M0 deliberately leaves out

| SmithDB | M0 | Why |
|---|---|---|
| WAL + memtable on the ingestion node | same | — |
| Immutable columnar segments on object storage | **none yet** | M1; M0 is RAM-bounded on purpose |
| Metastore | none yet | M1 |
| Merge-on-read across segments | merge on write in the store | nothing to merge across yet; M1 restores merge-on-read |
| Separate ingest/query services | separate crates, one process | constitution II (a) |
| Live query of fresh data | the run store **is** the fresh data | becomes `fresh_scan` in M1 |

## 11. Open design questions (for your review)

*Answered 2026-11-05 with the proposals (user approval to proceed): 1 JSON, 2 2 GB / 500k runs, 3 build script, 4 30 s.*

1. **WAL encoding**: JSON (debuggable, ~2× bigger) vs binary. *Proposed: JSON in M0.*
2. **Memory budget default**: 2 GB / 500k runs. *Proposed as is; configurable with `--max-memory`.*
3. **UI build**: the Node build runs from `cargo build` through a build script (one command), or is prebuilt and
   committed (no Node needed to build Rust). *Proposed: build script; CI caches it.*
4. **Restart target**: the spec says "ready within 10 s at the capacity limit", but replaying a full WAL could take
   5–20 s. Options: (a) correct the target to 30 s and measure it; (b) lower the default budget to ~1 GB; (c) add
   store snapshots in M0 (replaced by M1's flush). *Proposed: (a).*
