# HTTP API contract (v0)

Base: `http://localhost:7070`. All bodies are JSON unless noted. Errors: `{error: {code, message, details?}}`, where
`details` points at the bad field or filter span.

## Ingest: LangSmith-compatible (FR-001, research R2)

| Method + path | Body | Response |
|---|---|---|
| `GET /info` | — | `{version, batch_ingest_config: {use_multipart_endpoint: true, size_limit, size_limit_bytes}, instance_flags: {}}` |
| `POST /runs/batch` | `{post: [Run], patch: [Run]}` | `202 {accepted: n, rejected: [{id, reason}]}` |
| `POST /runs/multipart` | multipart: `post.{id}`, `patch.{id}`, `{op}.{id}.inputs\|outputs\|events`, `attachment.{id}.{name}` (dropped in v0), `feedback.{id}` | same as batch |
| `POST /runs` | one Run | `202` |
| `PATCH /runs/{run_id}` | partial Run | `202` |

Acknowledgment (`202`) is sent only after the WAL fsync (R7). The API key header is accepted and ignored.

## Ingest: native

| `POST /v1/events` | `{events: [RunEvent]}` (data-model.md) | same as batch |
|---|---|---|

## Query (FR-006 to FR-016)

| Method + path | Request | Response |
|---|---|---|
| `GET /api/traces/{trace_id}` | — | `{root, runs: [RunHeader]}` in dotted_order; no payloads |
| `GET /api/runs/{run_id}?include=inputs,outputs,events` | — | `Run` |
| `POST /api/query` | one query shape for every view (FR-029): `{project, start, end, filter?, trace_filter?, tree_filter?, search?, group_by?: [field], metrics?: [count, error_rate, p50, p95, p99, tokens, cost, feedback_avg], view: samples\|table\|timeseries\|distribution, bucket?, compare?: {filter} \| {shift: "-7d"}, order?, limit ≤ 500, cursor?}` | `samples` → `{runs: [RunHeader], next_cursor?}`; `table` → `{groups: [{key, metrics}]}`; `timeseries` → `{series: [{key, points}]}`; `distribution` → `{buckets}`; plus `compare` when requested |
| `GET /api/fields?project=&prefix=` | — | `[{field, type, top_values: [{value, count}]}]` (FR-030) |
| `GET /api/fields/{field}/values?prefix=&filter=` | — | `[{value, count}]` (suggestions, QR-008) |
| `POST /api/query/stands-out` | query + `{selection: {filter}}` | `[{field, score, selection: [{value, pct}], rest: [{value, pct}]}]` (FR-032) |
| `GET /api/threads/{thread_id}` | — | `{turns: [{root: RunHeader, input_preview, output_preview}]}` |
| `POST /api/runs/export` | query fields + `{format: jsonl}` | streamed `application/x-ndjson` |
| `POST /api/feedback` | `{run_id, key, score?, comment?}` | `201 Feedback` |
| `GET /api/tail?filter=…` | — | Server-Sent Events of `RunHeader` (live tail) |

`RunHeader` = every Run field except inputs, outputs and events, plus `has_payload` sizes. Filter strings use the grammar
in [filter-dsl.md](filter-dsl.md).

## Agent tools (FR-028, M4)
`GET /api/traces/{id}/outline`, `GET /api/traces/{id}/error-path`, `GET /api/payload/{handle}?path=&range=` (size cap
16 KB), `POST /api/runs/clusters`, `POST /api/diff`, `POST /api/annotate`. Every response includes `ui_url`. These are
also exposed through `tracedb mcp` (stdio MCP server).

## UI routes (FR-022, FR-027)
`/` runs list · `/trace/{id}?run={id}` · `/thread/{id}` · `/stats` · all filter state lives in query params.
