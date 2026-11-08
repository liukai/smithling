# HTTP API contract (master): retired

This page was the planning draft of the whole API, written before M1. The API was then specified and built one
milestone at a time, and each milestone's contract is the source of truth for what it added. Read them in order:

1. [M1](../../001-m1-see-agent-trace-live/contracts/http-api.md): LangSmith uploads (`/info`, `/runs/batch`,
   `POST/PATCH /runs`), the native upload, trace and run reads, live tail.
2. [M2](../../002-m2-durable-storage-query-builder/contracts/http-api.md): `/api/query`, the field catalog, filter
   errors, `/healthz` storage fields.
3. [M3](../../003-m3-search-aggregates-speed/contracts/http-api.md): multipart + zstd, feedback, search, aggregates
   and their stream, the M3 `/healthz` fields.

The routes the server actually registers are in [crates/server/src/lib.rs](../../../crates/server/src/lib.rs).
