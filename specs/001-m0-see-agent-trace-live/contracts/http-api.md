# HTTP API contract (M0 subset)

This is a subset of the [master contract](../../000-tracedb-master-plan/contracts/http-api.md), and the shapes match it.

## Ingest
| Method + path | Notes |
|---|---|
| `GET /info` | `{version, batch_ingest_config: {use_multipart_endpoint: false, size_limit: 100, size_limit_bytes: 20971520}, instance_flags: {}}` |
| `POST /runs/batch` | `{post: [Run], patch: [Run]}` → `202 {accepted, rejected: [{id, reason}]}` |
| `POST /runs` | one Run → `202` |
| `PATCH /runs/{run_id}` | partial Run → `202` |
| `POST /v1/events` | `{events: [RunEvent]}` → same as batch |
| `POST /runs/multipart` | `415 {error: {code: "unsupported", message: "multipart is supported from M2"}}` |

`202` is sent only after fsync. Other responses: `413` (too large), `503` (storage error or capacity reached, with a reason).

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
