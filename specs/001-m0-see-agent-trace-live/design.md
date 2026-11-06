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

- **Group commit**: many requests share one fsync. This is how one writer reaches ≥ 5k events/s in M0, and later 20k.
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

### 4.3 Live tail
The writer publishes `Change{run_header}` to a broadcast channel. Each browser connection (Server-Sent Events) filters
and **coalesces** changes over 100 ms windows, so a burst of 1,000 updates becomes ≤ 10 messages per second per tab.
A slow tab is dropped and reconnects, and never slows the writer.

### 4.4 Start and restart
Open the WAL directory, replay every record through the same `merge` used live, rebuild the indexes, then open the
HTTP port. Partial or corrupt tail records (from a kill mid-write) are detected by their checksum and truncated.
They were never confirmed, because confirmation waits for fsync.

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

**Cursor**: base64 of `{start_time, run_id, filter_hash}`.

## 6. Mapping LangSmith uploads to events

| LangSmith | `RunEvent` |
|---|---|
| `post` item / `POST /runs` | `op = post` |
| `patch` item / `PATCH /runs/{id}` | `op = patch` |
| `session_name` | `project` (default `default`) |
| `extra.metadata` | `metadata` |
| `inputs`, `outputs`, `events`, `error`, `tags`, times, `run_type`, `name`, ids, `dotted_order` | same meaning |
| `serialized`, attachments | dropped in M0, with sizes logged |

`GET /info` answers with `batch_ingest_config.use_multipart_endpoint = false`, so the SDK sends JSON batches
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
| Memory budget reached | new runs rejected with `capacity`; updates to existing runs still accepted |
| Disk full / fsync error | the writer stops confirming; requests get `503`; health shows the error; nothing confirmed is lost |
| Kill mid-write | the unconfirmed tail is truncated on replay |
| Slow browser | its tail stream is dropped; it reconnects and refetches |
| Unsupported upload format (multipart) | `415` with "supported from a later version" |

## 9. How M0 is proven (tests)

| Check | How | Guards |
|---|---|---|
| Merge rule | unit + property tests in `model` (any order of events → same result as sorted by seq) | M0-FR-03 |
| WAL replay | property test: random batches, random truncation point → replay equals the confirmed prefix | M0-FR-04 |
| **Crash test** | test harness spawns `tracedb serve`, uploads continuously while logging confirmed ids, `kill -9` at random times, restarts, checks every confirmed id is present; 10 rounds | SC-04 |
| **SDK contract** | fixtures recorded once by running the real Python SDK (0.8.18) against a recording endpoint; a Rust test replays them and compares resulting runs to golden JSON | SC-06 |
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

1. **WAL encoding**: JSON (debuggable, ~2× bigger) vs binary. *Proposed: JSON in M0.*
2. **Memory budget default**: 2 GB / 500k runs. *Proposed as is; configurable with `--max-memory`.*
3. **UI build**: the Node build runs from `cargo build` through a build script (one command), or is prebuilt and
   committed (no Node needed to build Rust). *Proposed: build script; CI caches it.*
