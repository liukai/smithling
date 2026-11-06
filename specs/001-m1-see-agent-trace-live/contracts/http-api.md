# HTTP API contract (M1 subset)

This is a subset of the [master contract](../../000-tracedb-master-plan/contracts/http-api.md), and the shapes match it.

## Ingest
| Method + path | Notes |
|---|---|
| `GET /info` | `{version: "", tracedb_version, batch_ingest_config: {use_multipart_endpoint: false, scale_up_qsize_trigger: 200, scale_up_nthreads_limit: 32, scale_down_nempty_trigger: 4, size_limit: 100, size_limit_bytes: 20971520}, instance_flags: {}}`. **All `batch_ingest_config` keys are required**: the SDK reads them without defaults, and a missing one crashes its upload thread (found by T030) |
| `POST /runs/batch` | `{post: [Run], patch: [Run]}` → `202 {accepted, rejected: [{id, reason}]}` |
| `POST /runs` | one Run → `202` |
| `PATCH /runs/{run_id}` | partial Run → `202` |
| `POST /v1/events` | `{events: [RunEvent]}` → same as batch |
| `POST /runs/multipart` | `415 {error: {code: "unsupported", message: "multipart is supported from M3"}}` |

| `POST /feedback` | `501 {error: {code: "unsupported", message: "feedback is supported from M3"}}` (the SDK sends feedback here when multipart is off) |

`202` is sent only after fsync. Per-item validation failures go in `rejected[]` (the SDK ignores this body, so it holds only
items that could never succeed). Whole-request failures use status codes the SDK acts on:

| Status | When | SDK behaviour (0.14.17) |
|---|---|---|
| `413` | body over 50 MB | not retried |
| `429` + `Retry-After: 1` | writer queue full (> 10,000 events in flight) | retried, honouring `Retry-After` |
| `507` | memory budget reached and the batch contains a **new** run; nothing in the batch is applied | not retried; the SDK logs the error |
| `503` + `Retry-After: 5` | storage error (fsync/write failed, disk full) | retried with backoff |

`version` is empty on purpose: the SDK only uses it for minimum-version warnings on non-upload features, and an empty
value skips them. `tracedb_version` carries our own version.

## Query
| Method + path | Response |
|---|---|
| `GET /api/traces/{trace_id}` | `{root, runs: [RunHeader]}` sorted by `dotted_order` |
| `GET /api/runs/{run_id}?include=inputs,outputs,events` | `Run` |
| `GET /api/roots?project=&status=&cursor=&limit=` | `{runs: [RunHeader], next_cursor?}` newest first, `limit ≤ 200` |
| `GET /api/projects` | `[{name, runs}]` |
| `GET /api/tail?project=` or `?trace=` | SSE: `hello {server_seq}` first, then `runs [RunHeader…]` (newest version per run, ≤ 10/s), `resync {}` when the tab must reload (design §4.3) |
| `GET /healthz` | `{ok, wal_ok, runs, memory_bytes, capacity_bytes}` |

## UI
`/` and `/trace/{id}?run={id}`, served from the embedded assets.
