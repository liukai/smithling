# HTTP API contract (M3)

Only what M3 adds or changes. Everything else is the [M2 contract](../../002-m2-durable-storage-query-builder/contracts/http-api.md)
(which extends M1's). Data shapes are in [data-model.md](../data-model.md).

**Delta key**: 🟢 New · 🟠 Improved · ⚪ Existing (as in [spec.md](../spec.md)).

## Ingest

| | Method + path | Change |
|---|---|---|
| 🟠 | `GET /info` | `batch_ingest_config.use_multipart_endpoint: true`; `instance_flags: {zstd_compression_enabled: true}`; `version` stays `""` (so the SDK sends feedback to `POST /feedback`) |
| 🟢 | `POST /runs/multipart` | the SDK's multipart upload, plain or zstd (below) |
| 🟢 | `POST /feedback` | store one feedback item (below); was `501` |
| 🟢 | `PATCH /feedback/{id}` | update one feedback item; was not routed |
| ⚪ | `POST /runs/batch`, `POST /runs`, `PATCH /runs/{id}`, `POST /v1/events` | unchanged; still used by SDK 0.8.x and by clients that ignore `/info` |

### 🟢 `POST /runs/multipart`

Request headers:

| Header | Value |
|---|---|
| `Content-Type` | `multipart/form-data; boundary=<hex>` |
| `Content-Encoding` | optional `zstd` (whole body) |
| `X-Pre-Compressed-Size`, `X-Post-Compressed-Size` | optional; informational |

Parts, by name:

| Part name | Content | Handling |
|---|---|---|
| `post.{run_id}` / `patch.{run_id}` | JSON: the run minus large fields | base of the item |
| `post.{run_id}.{field}` / `patch.{run_id}.{field}` | JSON, `field ∈ inputs, outputs, events, extra, error, serialized` | merged into the item |
| `attachment.{run_id}.{name}` | raw bytes | **rejected** `attachment_not_stored`; the run is stored without it |
| `feedback.{id}` | JSON feedback | stored as `POST /feedback`; an invalid one (bad JSON included) is **rejected** `bad_feedback` with `detail` |
| anything else | — | **rejected** `unknown_part` |

Parts may come in any order. Each assembled item goes through the same mapping, validation and writer as
`/runs/batch`, so it is stored identically.

Response `202`:

```json
{"accepted": 42, "rejected": [{"part": "attachment.7c1e…a4.screenshot.png", "reason": "attachment_not_stored"}]}
```

Item-level rejections (validation, identity conflict) are reported as in M2's `/runs/batch`, with `id`.

| Status | When |
|---|---|
| `202` | parsed; per-part and per-item rejections listed |
| `400 bad_multipart` | missing boundary, malformed part headers, bad JSON in a run part, zstd decode error |
| `413` | decoded body over 50 MB |
| `429` + `Retry-After` | as M2 (queue or memtable full) |
| `503` + `Retry-After` | as M2 (WAL failure); catalog failure storing feedback parts (`storage`) |

Never `404` or `415` for a multipart request: the SDK falls back to `/runs/batch` (dropping feedback) on `404` and
drops the batch on `415`.

### 🟢 `POST /feedback`

```json
{"id": "5b0c…", "run_id": "7c1e…a4", "trace_id": "0b9f…11", "key": "correctness", "score": 0.0,
 "comment": "gave up on 429", "feedback_source": {"type": "api", "metadata": {}}, "created_at": "2026-11-06T08:10:00Z"}
```

- `run_id` (a UUID) and a non-empty `key` are required; `id` is generated if missing; `score` is a number, a bool
  (1 / 0) or null; `created_at` is kept if given, `modified_at` is the write time.
- Unknown fields are kept verbatim.
- The run doesn't have to exist (stored as orphan until it does).

| Status | Body |
|---|---|
| `200` | the stored feedback (`created_at`, `modified_at` filled) |
| `400 bad_feedback` | missing `run_id` or `key`, or `score` not a number |
| `503 storage` | catalog unavailable (`Retry-After`; the SDK retries) |

A retry with the same `id` replaces the row (idempotent). `PATCH /feedback/{id}` replaces the fields given and
returns `200` with the row, or `404` if no such id. A multipart `feedback.{id}` part is stored the same way (the
part name gives the `id` when the body has none); an invalid one is rejected per part as `bad_feedback`.

## Query

### 🟠 `POST /api/query`: search and feedback in filters

Request: as M2. Filter slots may now use `search(…)`, `exists(…)` and `feedback.<key>`
([filter-language.md](filter-language.md)).

Response additions:

```json
{
  "runs": [{"run_id": "…", "name": "fetch_url", "status": "error", "…": "…",
            "match": {"path": "error", "excerpt": "HTTPError: 429 Too Many Requests", "start": 11, "end": 32}}],
  "warnings": [{"kind": "not_indexed", "files": ["01JA…"]}, {"kind": "skipped_files", "ids": ["01JB…"]}],
  "stats": {"files_total": 65, "files_scanned": 4, "index_files": 63, "scan_files": 2, "candidates": 31,
            "payload_rows": 31, "rows_read": 3120, "matched": 31, "complete": true, "cache_hits": 12, "ms": 84}
}
```

- `match` is present only when the query has a `search` and the run is on the page. `start`/`end` are character
  offsets in `excerpt`.
- `400 unsupported` for `search()` is gone.

### 🟢 `POST /api/aggregate`

Request (`AggSpec`, data-model §7; the query fields are `POST /api/query`'s):

```json
{
  "project": "overnight-eval",
  "filter": "has(tags, \"batch=2026-11-05-a\")",
  "group_by": ["name"],
  "metrics": ["count", "error_rate", "p95(latency)", "sum(cost)"],
  "view": "table",
  "top": 10,
  "compare": {"filter": "has(tags, \"batch=2026-11-06-b\")"}
}
```

Alternatively the stats line: `"stats": "by name | count, error_rate, p95(latency), sum(cost)"` in place of
`group_by` + `metrics`.

Response `200` (exact; `partial: false`):

```json
{
  "query_id": "q-81f2",
  "partial": false,
  "totals": {"runs": 4210, "finished": 4210, "errors": 56, "cost_pico": 4120000000000, "no_price_runs": 12},
  "groups": [
    {"key": ["fetch_url"],
     "cells": {"runs": 300, "finished": 300, "errors": 42, "error_rate": 0.14, "p95": 31000, "n_latency": 300,
               "cost_pico": null, "no_price_runs": 0, "estimated": false, "approximate": false},
     "where": "eq(name, \"fetch_url\")"}
  ],
  "cut": true, "overlap": false,
  "compare": {"side_b": {"groups": [{"key": ["fetch_url"], "cells": {"error_rate": 0.02, "p95": 9000}}]}},
  "stats": {"files_scanned": 12, "clean_rows": 3890, "dirty_runs": 320, "ms": 210}
}
```

- `p50/p95/p99` are in milliseconds; percentiles are nearest rank.
- `feedback_avg` is the mean over the group's runs of each run's mean score for the key (`feedback.<key>`);
  `feedback_n` counts the runs with a score. `by feedback_key`: one group per key a run has a score for
  (`overlap: true`, like tags), `where` = `exists(feedback.<key>)`; `(none)` (no scored feedback) and `(other)`
  have `where: null`.
- `where` is filter text (contracts/filter-language.md) selecting exactly the group's runs when added to `filter`;
  `null` for `(other)` when grouped by tag (a run with several tags can be in a shown group and in `(other)`), or
  when two keys show no group at all.
- `cost_pico` is in picodollars, and `null` when no run in the group had a price.
- `view: "series"` adds `series`; `view: "dist"` adds `dist` (data-model §7).
- `compare.shift: "1d"` compares with the same window one day earlier; series then use offsets.

| Status | Body |
|---|---|
| `400 bad_filter` | as M2, with `slot` |
| `400 bad_stats` | unknown metric or group key, more than 2 keys, bad `bucket`/`shift`, a `metadata.<k>` deeper than promoted paths, more than one `avg(feedback.<key>)` (cells hold one `feedback_avg`); `start`/`end` in the stats line when given |
| `400 bad_request` | body isn't JSON or has an unknown field |
| `500 query_failed` | engine or storage error |

### 🟢 `GET /api/aggregate/stream?q=<url-encoded AggSpec JSON>`

SSE. Closing the connection cancels the computation.

| Event | Data |
|---|---|
| `estimate` | `AggResult` with `partial: true`, `sampled_pct`; every cell `estimated: true`. Sent only when the exact answer is expected to take > ~300 ms |
| `result` | `AggResult` with `partial: false`; last event |
| `error` | `{code, message}` (codes as `POST /api/aggregate`); last event |

Every event carries `query_id`; the client ignores events for another id.

### 🟢 `GET /api/feedback`

| Query | Response |
|---|---|
| `?run_id=…` | `{feedback: [Feedback]}` for that run |
| `?orphaned=true` | `{feedback: [Feedback], count}`: feedback whose run hasn't arrived (not in memory, not in the flushed-ids filter; an orphan has no run, so no project or time) |
| `?keys=true&project=&time=` | `{keys: [{key, count, avg}]}` (field browser's `feedback.` section), over feedback whose run is in scope; `avg` is null when no item has a score |

Anything else is `400 bad_request`.

### 🟠 Field catalog

`GET /api/fields` also lists `feedback.<key>` paths (type `number`) with counts from the feedback index: runs that
have arrived with a score for the key, whatever the project and time (the index doesn't know them). A Table column
`feedback.<key>` gives the run's mean score.

### 🟠 Health

`GET /healthz` adds `damaged_segments: [{id, reason, since}]` (replaces `corrupt_segments`, which stays as an alias
listing the ids), `unindexed_segments` (count without a sidecar), `orphan_feedback` (count), `cache_bytes` (bytes in
the data cache), `dedup_keys` (keys in the dedup window; `0` once it has drained). `memtable_bytes` is now the active
memtable's estimated heap, the measure `--memtable` limits (M2: JSON bytes); `memtable_limit_bytes` (new) is that
limit, so a client such as `tracedb demo --url` can size a load that forces flushes.

## UI routes

| | Route | View |
|---|---|---|
| 🟠 | `/?…M2 params…&q=<search>&view=samples\|table\|stats\|series\|dist&by=name&m=count,error_rate,p95(latency),sum(cost)&top=10&bucket=auto&cmp=<filter>\|shift=1d` | runs view with search, grouping, metrics, chart and comparison |
| ⚪ | `/trace/{id}?run={id}` | unchanged |

`q` is the search box text; it becomes a `search("…")` chip in the `filter` slot.
