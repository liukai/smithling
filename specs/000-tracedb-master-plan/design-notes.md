# tracedb — design notes (v0.1, 2026-11-05)

> Draft design input for `/speckit-plan`. Requirements and scenario: [spec.md](spec.md). Where they disagree, spec.md wins.

**Decisions**: kept in [roadmap.md](roadmap.md) (plan of record).

A local-first, single-binary trace store + viewer for AI agents, reverse-engineered from
[LangSmith's SmithDB](https://www.langchain.com/blog/introducing-smithdb). It runs on a laptop today,
and its storage layout already works on object storage, so moving to the cloud means swapping config, not rewriting.

Requirements and the driving scenario: [spec.md](spec.md). This file is the design.

## 1. Problem

Local agent runs (LangGraph, custom loops, Claude Code, eval scripts) produce traces that are:
- **Deep and wide**: hundreds of nested spans (LLM → tool → sub-agent → tool …).
- **Arrive in pieces**: a span's `start` lands minutes or hours before its `end`, and long-running agents patch spans many times.
- **Heavy**: prompts, tool outputs and images are KB–MB each, while the filter columns are tiny.
- **Queried in odd ways**: "show the trace tree", "find runs whose *any descendant* errored", "full-text search outputs",
  "rebuild this conversation thread across 40 traces", "p95 latency / tokens by tool".

Today, local options are print logs, JSONL files you grep, or sending everything to a cloud SaaS.
Jaeger and Tempo-style tools don't understand LLM payloads, threads, or late-arriving patches.

**Goal:** `tracedb serve`, then point an existing agent at `localhost`, then search, browse and aggregate traces in a browser, with data in `./.tracedb/`.

### Non-goals (v0)
Multi-tenant auth, HA/replication, evaluation runners, prompt playground, horizontal scale-out.

## 2. What SmithDB does, and what tracedb copies

| SmithDB (from the post) | tracedb v0 (local) | Cloud path (later) |
|---|---|---|
| Rust + Apache DataFusion | Rust + DataFusion | same |
| Vortex columnar files | **Parquet** behind a `SegmentFormat` interface; Vortex is backlog | Vortex from backlog |
| Object storage for durable data | `object_store::LocalFileSystem` at `./.tracedb/data` | `object_store` S3/GCS/Azure, same code |
| Small Postgres metastore (segment location, time bounds, row counts) | **SQLite** via `sqlx` | Postgres via `sqlx`, same queries |
| LSM: memtable → immutable sorted segments → compaction | same, in one process | split into separate services |
| Ingestion / query / compaction as stateless services | **three services with explicit interfaces, one process** (§2.1) | separate deployments |
| Cluster manager + sticky routing by key range | n/a (single node) | hash `project_id` into slices; route queries to the node that owns the slice's cache |
| Query fresh data from ingestion node's SSD before it's compacted | query planner unions the **memtable + unflushed WAL** with segments | same, over RPC |
| Deletion/upgrade vectors in metastore; rewrite at compaction | same | same |
| Time-tiered compaction | same (small tiers for recent data, large for old) | same |
| Late materialization of large payloads | core rows + separate payload files with pointers | same |
| Custom inverted index for object storage (term zones, chunked postings) | ~~scan + per-segment bloom~~ per-segment index (term zones, chunked postings, no word positions) behind the `TextIndex` trait, built in M3 ([D-026](decisions.md#d-026), §6) | same index, range reads on S3 |
| Progressive querying (newest segments first, early stop) | same | same |

SmithDB numbers to compare against (prod, cloud scale), at P50 / P99: trace tree 92ms / 595ms, single run 71ms / 358ms,
run filtering 82ms / 434ms, full-text search 400ms / 870ms, threads 131ms / 268ms; ingestion (event to queryable) 630ms / 1.47s.

### 2.1 Services and interfaces (kept simple)

Three services, each owning its own state, started together by `tracedb serve` in one process. They talk only through
the interfaces below, never through shared mutable state, so each can later become its own deployment.

| Service | Owns | Provides |
|---|---|---|
| **Ingest** | WAL, memtable, flush | `accept(batch) -> ack`; `fresh_scan(filter)` for data not yet in segments (as built: a pinned snapshot of the memtables, read by query's own code, [D-017](decisions.md#d-017)) |
| **Query** | planning, merge-on-read, progressive query | runs/traces/threads/search/stats APIs; reads segments + Ingest's `fresh_scan` |
| **Compaction** | tiered merges, segment swaps | background only; reads and writes segments, swaps them atomically in the metastore |

Cloud path: Compaction splits into one **scheduler** plus scalable **workers**, and the cluster manager becomes its own
service. Self-hosted SmithDB runs five services (roadmap R-6). Locally they all stay in one process.

Shared storage interfaces (one simple v0 implementation each). *As built (M3)*: only `TextIndex` is a Rust trait;
`ObjectStore` is the `object_store` crate's trait, wrapped by storage's `Store`; `Metastore` and `Cache` are concrete
structs; there is no `SegmentFormat` trait (Parquet is used directly). The services are crates without service traits.

| Interface | v0 implementation | SmithDB / cloud implementation |
|---|---|---|
| `ObjectStore` | local filesystem | S3/GCS/Azure |
| `Metastore` | SQLite | Postgres |
| `SegmentFormat` | Parquet | Vortex (backlog) |
| `TextIndex` | ~~scan + per-segment bloom~~ per-segment inverted index, no positions (M3, D-026) | same, plus word positions if needed |
| `Cache` | in-memory LRU | memory + local SSD tier |

## 3. Data model

### 3.1 Runs are event streams, not rows
Clients send **run events**. A run is the merge of its events, ordered by `(run_id, event_seq)`.

```
RunEvent {
  run_id: uuid, trace_id: uuid, parent_run_id: uuid?, dotted_order: string,
  project: string, event_kind: start | patch | end,
  event_time: ts, event_seq: u64,            // assigned at ingest
  // any subset of the fields below; later events override earlier ones
  name, run_type: llm|tool|chain|retriever|agent|other,
  start_time, end_time, status: running|ok|error, error: string?,
  tags: [string], metadata: json, thread_id: string?,
  prompt_tokens, completion_tokens, cost_usd, model,
  inputs: json?, outputs: json?, events: json?   // LARGE fields
}
```

- `dotted_order` (LangSmith convention: `<start_ts><run_id>.<child_start_ts><child_id>…`) gives a single sort key that
  returns a whole tree in DFS order, and supports prefix matching for "all descendants of X".
- **Merge-on-read**: queries over uncompacted data merge events per `run_id`. Compaction writes the merged form, so old data
  is cheap to read.

### 3.2 Physical layout
```
.tracedb/
  meta.sqlite                         # segments, field_stats, state, dedup_keys, feedback (§3.3)
  wal/<ts>.wal                        # append-only, fsync per batch
  data/<project>/<yyyy-mm-dd>/
    seg-<ulid>.core.parquet           # small columns, sorted by (trace_id, dotted_order)
    seg-<ulid>.payload.parquet        # run_id, inputs, outputs, events (zstd)
    seg-<ulid>.fts.dict.parquet       # text index: terms, sorted, page index as term zones (M3, D-026)
    seg-<ulid>.fts.post               # text index: chunked postings (payload row numbers)
```
Core rows carry `payload_seg` + `payload_row` pointers. List and filter queries never touch payload files.

### 3.3 Metastore tables
`segments(id, project, path, min_time, max_time, min_trace_id, max_trace_id, row_count, tier, state, created_at)`
`delete_vectors(segment_id, roaring_bitmap)` · `upgrades(segment_id, row, patch_json)` · `projects(name, ttl_days)`

*As built (M3, `crates/storage/src/metastore.rs`)*: `segments(id, project, core_path, payload_path, fts_path, fts_bytes,
min/max_start_key, min/max_trace_id, min/max_seq, row_count, needs_merge_rows, core_bytes, payload_bytes, tier, state,
created_at)` · `field_stats(segment_id, path, type, …)` · `state(schema_version, flushed_seq)` · `dedup_keys(flushed_seq,
newest_at, keys)` · `feedback(id, run_id, …)`. `delete_vectors` and `upgrades` come with M4 compaction / backlog B1;
`projects` with retention (B1).

## 4. Ingestion

Endpoints (all accept batches):
1. **LangSmith-compatible**: `POST /runs/batch` (`{post: [...], patch: [...]}`) and `POST /runs/multipart`. This lets existing
   LangChain/LangGraph apps run unchanged via `LANGSMITH_ENDPOINT=http://localhost:7070`. *This is the headline demo.*
2. **OTLP/HTTP traces** `POST /v1/traces`, mapping the OTel GenAI semantic conventions (`gen_ai.*`) to run fields. Covers
   OpenTelemetry-instrumented agents and any framework with an OTel exporter.
3. **Native** `POST /v1/events` (the RunEvent JSON above), used by the tiny Python/TS client.
4. **Importer** `tracedb import claude-code ~/.claude/projects/...`: turns local agent session JSONL into traces
   (`uuid`/`parentUuid` → tree, `sessionId` → thread, `isSidechain` → sub-agent span, `tool_use`/`tool_result` → tool runs). This solves "what did my local agent do" with zero instrumentation.

**LangSmith wire format (read from `langsmith` Python SDK source, 0.8.18 and 0.14.17, 2026-11-05).** Known, not invented:
- On startup the SDK calls `GET /info` and reads `batch_ingest_config` (incl. `use_multipart_endpoint`, `size_limit`,
  `size_limit_bytes`) and `instance_flags`. If `/info` fails, it logs a warning and falls back to defaults. **Serve `/info`**
  to control which path the SDK takes.
- `POST /runs/batch`: JSON `{post: [...], patch: [...]}`.
- `POST /runs/multipart`: multipart form. Parts are `post.{run_id}` / `patch.{run_id}` (main JSON minus big fields),
  `post.{run_id}.inputs|outputs|events` (big fields, separate parts), `attachment.{run_id}.{name}` (binary), `feedback.{id}`.
  The client may merge a post+patch for the same run before sending. Every run must carry `trace_id` and `dotted_order`.
- Optional zstd-compressed multipart (`Content-Encoding: zstd`). It appears to be opted into via the server's
  info/flags, so we don't advertise it in v0. *Verify.* *Superseded by [D-030](decisions.md#d-030)*: verified, and
  `/info` now sends `instance_flags.zstd_compression_enabled: true` with `use_multipart_endpoint: true`; multipart
  decodes zstd (fixtures `sdk-0.14.17-multipart-zstd`). Attachment parts are rejected per part ([D-031](decisions.md#d-031)).
- Single-run `POST /runs` and `PATCH /runs/{id}` exist as older or fallback paths.
- Run fields: `id, trace_id, parent_run_id, dotted_order, name, run_type, start_time, end_time, inputs, outputs, error,
  events, tags, extra` (metadata lives in `extra.metadata`), `session_name` (= project), `serialized`, `reference_example_id`.
- **New in 0.14.x: `address`.** A run can be sent to an *agent address* instead of a project: wire value
  `lrn:agents/<agent id>/environments/<environment>`, set in code or by `LANGSMITH_AGENT_ID` + `LANGSMITH_AGENT_ENVIRONMENT`.
  When a run is addressed and no project is named, the SDK **removes `session_name`/`session_id`**. tracedb keeps `address`
  as its own field and derives the project from it (spec.md Assumptions).
- Unchanged in 0.14.17: `/info` drives the choice; with `use_multipart_endpoint: false` the SDK sends `POST /runs/batch`;
  zstd is used only for multipart and only if `/info` sets `instance_flags.zstd_compression_enabled`; `RunTree.patch`
  still re-sends full `events`. The SDK retries `408, 425, 429, 500, 502, 503, 504` and honours `Retry-After`.
- Feedback: `POST /feedback` JSON whenever `/info` has an empty `version` (tracedb's), so that is the default; a `feedback.{id}` multipart part only with a versioned `/info` ([D-027](decisions.md#d-027)).
- The SDK also has an OTel mode that skips `/info`. That's another reason OTLP ingest (backlog, outside v0) is a cheap add-on.
- Test approach: run the real SDK against tracedb in CI, recording requests as golden files; one fixture set per pinned SDK version. Pins: Python **0.14.17** (current) and **0.8.18** (older agents still run it). JS: pin when JS fixtures are recorded.

Write path: validate → assign `event_seq` → append to WAL → insert into memtable (Arrow builders keyed by project) → ack.
*As built*: the memtable holds merged run states, not Arrow builders; flush defaults are 64 MB measured as heap or 2 min
(`--memtable`, `--flush-age`; [D-028](decisions.md#d-028); M2 used 128 MB of JSON or 10 min).
Flush when the memtable passes 64 MB or 5 s → sort by `(trace_id, dotted_order, event_seq)` → write core + payload Parquet →
register in metastore → truncate WAL. Recovery on startup = replay WAL.

## 5. Query

### 5.1 API
*Superseded*: the real API is in [003 contracts/http-api.md](../003-m3-search-aggregates-speed/contracts/http-api.md). Routes
(`crates/server/src/lib.rs`): `POST /api/query` (filters, incl. `search(…)`), `POST /api/aggregate`,
`GET /api/aggregate/stream`, `GET /api/traces/{trace_id}`, `GET /api/runs/{run_id}`, `GET /api/fields`,
`GET /api/fields/values`, `GET /api/feedback`, `GET /api/roots`, `GET /api/projects`, `GET /api/tail`. Threads (M4) and
`POST /sql` are not built. The original table, kept for history:

| Endpoint | Pattern |
|---|---|
| `GET /traces/{trace_id}` | tree load: core rows in dotted order; payloads on demand |
| `GET /runs/{run_id}?include=inputs,outputs` | single run with late-materialized payload |
| `POST /runs/query` | filter + sort + limit + cursor |
| `POST /runs/search` | full-text over inputs/outputs |
| `GET /threads/{thread_id}` | all root runs with `thread_id`, time-ordered |
| `POST /runs/stats` | aggregations: count, p50/p95 latency, tokens, cost, error rate, grouped by `name/model/run_type/tag` |
| `POST /sql` (local only) | raw DataFusion SQL over a `runs` view, for power users and debugging |

### 5.2 Filter DSL (a subset of LangSmith's grammar)

This is the `filter` grammar the LangSmith SDK still documents in 0.14.17 (`Client.list_runs`), so it stays canonical for
the API, CLI and agents. A `field:value` search-bar form, if confirmed in LangSmith's UI docs, could be added later as an
alternate text form in the M2 query builder (roadmap R-2).
```
filter:       and(eq(run_type,"tool"), gt(latency_ms,2000), has(tags,"prod"), eq(metadata.user,"kai"))
trace_filter: eq(status,"error")              -- predicate on the ROOT run
tree_filter:  search("rate limit")            -- ANY run in the same trace matches
```
The DSL compiles to a DataFusion `LogicalPlan`. `tree_filter` becomes a semi-join on `trace_id`.
*Superseded by [D-017](decisions.md#d-017)*: DataFusion prunes and scans files; merging, the exact filter and root /
any-run conditions run in query's own code on merged runs. There is no `TableProvider` and no semi-join plan.

### 5.3 Execution techniques (copied from SmithDB)
- **Pruning**: the metastore picks segments by project and time range, then trace_id bounds. Parquet row-group statistics and
  page indexes handle the rest.
- **Progressive querying**: for `ORDER BY start_time DESC LIMIT n`, walk segments newest-first in time windows, stream
  results, and stop once `n` rows are found and no older window can beat them. This replaces sort-everything-then-limit.
- **Fresh data**: the plan is `UNION(memtable_scan, segment_scan)` followed by a merge-by-run_id for uncompacted events
  (as built: a pinned memtable + file set, merged in query's own code, D-017).
- **Late materialization**: project only core columns, then fetch payloads by pointer for the final `n` rows.
- **Delete vectors**: applied as a row filter at scan time.
- **Cache**: a `Cache` interface; v0 implementation is an in-memory LRU for file footers and hot row groups (stands in for SmithDB's SSD cache). A disk tier can be added behind the same interface for cloud.

## 6. Full-text search
- ~~**v0**: substring/token scan of payload files, limited to segments that pass a per-segment bloom filter on tokens.~~
  *Superseded by [D-026](decisions.md#d-026)*: Parquet blooms match whole values, not words, so the scan stayed too slow.
- **v0 (built in M3)**, following SmithDB: each flush writes a per-segment sidecar behind the `TextIndex` trait
  (`crates/storage/src/fts.rs`)
  - `seg-<ulid>.fts.dict.parquet`: one row per `(term, field)` (field = inputs, outputs or error), sorted, small pages
    with the page index as **min/max term zones**, so a lookup reads one small block;
  - `seg-<ulid>.fts.post`: postings (payload row numbers) **chunked apart from the term dictionary**, fetched with range
    reads (S3-friendly, no huge allocations);
  - **no word positions**: a phrase is checked against the payload text of the candidate rows (staged; revisit if
    text-search p99 misses).

## 7. Compaction & lifecycle
- **Tiers**: T0 (memtable flushes, ~seconds of data) → T1 (merge into ~15-minute files) → T2 (daily, ~256 MB files).
  Recent data compacts conservatively because spans are still being patched. Old data compacts aggressively for cheap scans.
- Compaction merges events into final runs, applies delete and upgrade vectors, drops TTL-expired data, rebuilds FTS
  sidecars, and swaps segments atomically in the metastore (new rows `active`, old rows `tombstoned`, GC after grace period).
- `DELETE /runs?filter=…` writes delete vectors only. It never rewrites files synchronously.

## 8. UI (demo surface)
A static SPA embedded in the binary (`rust-embed`) at `http://localhost:7070`:
- **Runs list**: filter bar (DSL with autocomplete), live tail (SSE from memtable), columns for latency, tokens, cost, status.
- **Trace view**: tree plus waterfall, with an input/output side panel (payloads lazy-loaded).
- **Threads view**: chat-style rebuild of a conversation across traces.
- **Stats**: a few charts (latency percentiles, tokens by model, error rate by tool).

## 9. CLI
`tracedb serve [--data ./.tracedb] [--port 7070]` · `tracedb tail [--filter …]` · `tracedb query '<dsl>'` ·
`tracedb sql '<sql>'` · `tracedb import claude-code|jsonl <path>` · `tracedb seed --traces 10000` · `tracedb bench`
*As built (M3)*: `serve`, `seed`, `query`, `demo`, `bench`; no `tail`, `sql` or `import` yet. CLI contract:
[003 contracts/cli.md](../003-m3-search-aggregates-speed/contracts/cli.md).

## 10. Demo script & done-check
`make demo` (*as built*: there is no Makefile; `tracedb demo --seed 7` loads the scenario and checks the counts, and
`tracedb bench --check` checks the speed targets):
1. `tracedb serve` starts a fresh data dir.
2. `tracedb seed --traces 20000` generates synthetic agent traces (depth 3–8, 20–300 spans, ~5% errors, 10% with
   end events delayed 30–120 s, threads of 2–10 traces, payloads 1–50 KB) through `/runs/batch`.
3. A real LangGraph example agent runs with `LANGSMITH_ENDPOINT=http://localhost:7070` and its trace appears live in the UI.
4. `tracedb bench` prints a SmithDB-style P50/P99 table for: tree load, single run, filtering, FTS, threads, ingestion.

**v0 done when** (M3 below), on a laptop with ~2M runs / ~5 GB:

- [ ] ingest ≥ 20k run events/s sustained; ingest-to-queryable ≤ 1 s (memtable visibility)
- [ ] trace tree P50 ≤ 50 ms, filter P50 ≤ 100 ms, FTS P50 ≤ 500 ms
- [ ] kill -9 mid-ingest, restart: zero acknowledged events lost (WAL test)
- [ ] a late `end` event updates the run in the UI and in query results before and after compaction
- [ ] an unmodified LangChain app traces into it via `LANGSMITH_ENDPOINT`

## 11. Milestones

*Superseded*: the phases of record are in [roadmap.md](roadmap.md#phases) (M3 = search, aggregates, multipart; OTLP and
the cloud split are backlog, outside v0). The original table, kept for history:

UI first: every milestone ships its browser view and is demoed in the browser (spec.md FR-025). curl/CLI demos are secondary.

| | Scope | Demo |
|---|---|---|
| **M1** | RunEvent model, WAL + memtable, `/v1/events`, `GET /traces/{id}` from memtable only, **UI shell: runs list (live tail) + trace tree/waterfall + run side panel** | send a trace, watch it appear in the browser, open its tree |
| **M2** | Parquet flush, SQLite metastore, DataFusion scans with pruning, merge-on-read, `/runs/query` + DSL, **UI filter bar (click-to-filter, autocomplete, shareable addresses)** | `tracedb seed`, then triage steps 4–5 in the browser |
| **M3** | `/runs/batch` LangSmith compat, payload split + late materialization, progressive query, **UI search + stats charts + batch compare**, `bench` | **`make demo` (done-check above)** |
| **M4** | Compaction tiers, threads, **UI thread view, export** | thread + export demo in the browser |
| **Backlog** | Delete vectors, TTL, **UI delete + retention settings** (spec.md B1); Vortex `SegmentFormat`; custom `TextIndex` (§6) | delete + retention demo; search and scan benchmarks vs v0 |
| **M5** | OTLP ingest, Claude Code importer | "what did my agent do" with zero instrumentation |
| **Cloud** | S3 `object_store`, Postgres metastore, services as separate deployments, cluster manager + sticky routing | same binary, `--profile cloud` |

## 12. Crates
`tokio`, `axum`, `datafusion`, `arrow`, `parquet`, `object_store`, `sqlx` (sqlite + postgres), `serde_json`, `uuid`, `ulid`,
`roaring`, `moka` (cache), `rust-embed`, `clap`, `opentelemetry-proto` (M5). Backlog: `vortex`.
*As built*: no `roaring` or `opentelemetry-proto` yet (no delete vectors, no OTLP).
Pin `object_store` to the **0.13** line: `vortex-datafusion` 0.91 (DataFusion 55) needs `object_store` 0.13.2, and the
latest is 0.14.2. Staying on 0.13 keeps the Vortex swap open (roadmap R-4).

## 13. Open questions (to iterate on)

spec.md's Assumptions set defaults for 1 (compat first), 2 (importer is P2), 3 (last-write-wins per field) and 6 (verify before the core demo). 4 and 5 are left to planning.

1. **Primary ingest for the demo**: LangSmith-compat (great for the LangChain ecosystem) or OTLP-first (vendor neutral)? Spec currently says compat first, OTLP in M5.
2. **Claude Code importer priority**: pull it into M3 as the "local agent" hook?
3. **Merge semantics**: last-write-wins per field vs. append-only fields (e.g. `events`, streamed tokens)?
4. ~~**FTS**: custom index or tantivy?~~ Decided 2026-11-05: scan + bloom in v0, custom index in backlog; tantivy dropped.
   *Superseded by [D-026](decisions.md#d-026)*: the custom index (no positions) was built in M3.
5. **UI stack**: plain TS + Vite SPA, or reuse an existing OSS trace viewer?
6. ~~**Exact LangSmith wire format**~~ Checked 2026-11-05 against SDK 0.14.17 (see §4); re-check multipart against the pinned SDK when M3 starts.
