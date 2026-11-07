# HTTP API contract (M2)

Extends the [M1 contract](../../001-m1-see-agent-trace-live/contracts/http-api.md). Shapes match the
[master contract](../../000-tracedb-master-plan/contracts/http-api.md) where it defines them.

## Ingest: changes from M1

| Method + path | Change |
|---|---|
| `GET /info`, `POST /runs/batch`, `POST /runs`, `PATCH /runs/{id}`, `POST /v1/events` | unchanged |
| status `507` | **removed**: tracedb no longer refuses data for lack of memory |
| status `429` + `Retry-After: 1` | also sent when the active memtable is full while a flush runs, or unflushed WAL ≥ 1 GB (R-8) |
| `rejected[]` with `identity_conflict` | now stable: retrying the identical item returns the same rejection (M2-FR-23) |

## Query

### `POST /api/query`

Request (data-model §9; every field optional):

```json
{
  "project": "eval-agent",
  "time": {"last": "12h"},
  "filter": "has(tags, \"batch=2026-11-05-a\")",
  "trace_filter": null,
  "tree_filter": "eq(status, \"error\")",
  "is_root": true,
  "order": {"by": "start_time", "dir": "desc"},
  "limit": 50,
  "cursor": null,
  "columns": ["status", "name", "latency", "total_tokens", "metadata.prompt_version"],
  "count": false
}
```

By start time the server reads newest-first (oldest-first for `asc`) and stops once the page is full, so
`stats.matched` counts only the runs read and `stats.complete` is `false`; `"count": true` reads everything and
makes `matched` the total ([D-021](../../000-tracedb-master-plan/decisions.md#d-021)). `files_version_lookup` and
`merge_rows` are the files searched for other versions of multi-version runs and the rows that search read (also in
`rows_read`; [D-024](../../000-tracedb-master-plan/decisions.md#d-024)).

Response `200`:

```json
{
  "runs": [{"run_id": "…", "trace_id": "…", "name": "research_task", "status": "ok", "start_time": "…",
            "latency_ms": 41000, "total_tokens": 1840, "tags": ["batch=2026-11-05-a"], "last_seq": 88123,
            "columns": {"metadata.prompt_version": "v7"}}],
  "next_cursor": "eyJ…",
  "warnings": [{"kind": "mixed_types", "path": "outputs.status_code", "counts": {"number": 4120, "string": 3}}],
  "stats": {"files_total": 65, "files_scanned": 4, "rows_read": 3120, "rows_merged": 812,
            "files_version_lookup": 65, "merge_rows": 1624, "matched": 51, "complete": false, "ms": 74}
}
```

Errors:

| Status | Body | When |
|---|---|---|
| `400` | `{error: {code: "bad_filter", message, slot: "filter"\|"trace_filter"\|"tree_filter", start, end, expected}}` | parse or type error; `start`/`end` are byte offsets in that slot's text |
| `400` | `{error: {code: "bad_cursor"}}` | cursor is malformed or belongs to another query |
| `400` | `{error: {code: "unsupported", message: "search() is supported from M3"}}` | features of later phases |

### Lookups (unchanged shapes, now over memtables ∪ files)

| Method + path | Response |
|---|---|
| `GET /api/traces/{trace_id}` | `{root, runs: [RunHeader]}` sorted by `dotted_order` |
| `GET /api/runs/{run_id}?include=inputs,outputs,events` | `Run` (+ `warnings`) |
| `GET /api/roots?project=&status=&cursor=&limit=` | kept for M1 clients; same as `POST /api/query` with `is_root: true` |
| `GET /api/projects` | `[{name, runs}]` |

### Field catalog

| Method + path | Response |
|---|---|
| `GET /api/fields?project=&time=12h&prefix=meta` | `{fields: [{path, types: [{type, count}], complete}], complete}` sorted by count |
| `GET /api/fields/values?project=&time=12h&path=metadata.prompt_version&prefix=v` | `{values: [{value, count}], other_count, approximate}` (≤ 50) |

### Live views

`GET /api/tail?q=<url-encoded query JSON>` (SSE; `q` is the `POST /api/query` body). Also accepts M1's `?project=`
and `?trace=`, alone or narrowing `q`.

| Event | Data |
|---|---|
| `hello` | `{server_seq}`, first; the browser loads its view after it |
| `runs` | `[RunHeader]`: runs that match now, newest version each (≤ 10 messages/s) |
| `left` | `[run_id]`: runs that may be in the view (sent earlier, or there when it loaded) and no longer match; ids the view doesn't show can be ignored |
| `resync` | `{}`: reload the view |

### Health

`GET /healthz` → `{ok, wal_ok, flush_ok, memtable_bytes, frozen: bool, unflushed_wal_bytes, segments, last_flush_at,
corrupt_segments: [id]}`.

## UI routes

| Route | View |
|---|---|
| `/?project=&time=&filter=&trace=&tree=&root=1&order=&cols=&view=samples\|table&live=1` | runs view with query (readable parameters, same names as the API) |
| `/?project=&status=` | M1 link; translated on load |
| `/trace/{id}?run={id}` | unchanged |

Served from the embedded copy, or from `--ui-dir` / `TRACEDB_UI_DIR` when set.
