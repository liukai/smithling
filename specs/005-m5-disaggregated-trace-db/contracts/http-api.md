# HTTP contract: M5 additions

Proposed; follows the design register ([design.md](../design.md)). Every M1–M4 endpoint keeps its shape.

## On a reader

| | Endpoint | Behaviour | Decision |
|---|---|---|---|
| 🟢 | Write endpoints (`/runs/batch`, `/runs/multipart`, `POST/PATCH /runs`, `/v1/events`, `POST/PATCH /feedback`) | `409` with `{"error": "read_only", "role": "reader", "writer": "<address if known>"}` | KD-1 |
| 🟢 | `GET /info` | As the writer's, plus `role: "reader"` | KD-1 |
| 🟠 | Every read endpoint | Answers from one catalog version per request; responses' `stats` add `catalog_version` and `data_up_to` (newest flushed data time) | KD-2 |
| 🟠 | Live view (`/api/tail`, SSE) | Proposed: serves updates as new catalog versions arrive, each event carrying `data_up_to`, and a `writer` link for the live view (KD-9 Open) | KD-9 |
| 🟢 | A request past the query time limit | `503` with `{"error": "query_time_limit", "limit_s": 3600}` and `Retry-After` | KD-6 |

## On every role

| | Endpoint | Fields | Decision |
|---|---|---|---|
| 🟠 | `GET /healthz` | adds `role` (`all`, `writer`, `reader`), `catalog_version`, `data_up_to`, `current` (bool), `behind_s`; on the writer, `lease: {holder, number, expires_at}` and `files_pending_delete` | KD-2, KD-7, KD-10 |
| 🟢 | `GET /api/status` | `{role, catalog_version, data_up_to, current, readers: [{id, address, catalog_version, data_up_to, seen_at}]}`; `readers` listed on the writer from check-ins in the last minute | KD-10 |

## UI

| | Route | View | Decision |
|---|---|---|---|
| 🟢 | status panel (header badge, opens a panel) | role, catalog version, data up to, current/behind; on the writer, the reader list | KD-10 |
| 🟠 | runs list and trace view on a reader | a "data up to <time>" label and a link to the writer's live view | KD-9 |
