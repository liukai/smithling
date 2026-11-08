# Research: tracedb v0

Phase 0 decisions. Each one is a decision, then why, then what else was considered. The source for SmithDB is the
[SmithDB post](https://www.langchain.com/blog/introducing-smithdb). The source for LangSmith is the `langsmith` Python SDK
0.8.18 and 0.14.17 code (read locally on 2026-11-05).

## R1. Merge rule for run events (FR-002)
- **Decision (confirmed by user 2026-11-05)**: for every field, the latest event wins (ordered by `event_seq`, which is
  assigned at ingest). A missing field never overwrites a value. No field appends.
- **Rationale**: the SDK keeps the full run in memory and sends complete values in each patch. That includes the
  whole `events` list (`RunTree.patch` sends `events=self.events`; checked in SDK 0.8.18 and 0.14.17 source on 2026-11-05), as well
  as `tags` and `extra`. Appending would duplicate events. Replacing matches what the SDK does itself when it folds a
  patch into a post.
- **Alternatives**: appending `events`, which was the earlier default and is wrong for LangSmith clients. It could be
  offered later on the native `/v1/events` API for clients that send only new events. Field-level CRDTs were rejected
  as too complex.

## R2. LangSmith compatibility (FR-001)
- **Decision**: implement `GET /info` (advertise `batch_ingest_config.use_multipart_endpoint=true` and size limits, with
  no zstd flag), `POST /runs/batch`, `POST /runs/multipart`, `POST /runs` and `PATCH /runs/{id}`. Map `session_name` to
  project and `extra.metadata` to metadata. Keep the 0.14 `address` field; when no project is named, derive the project
  from it (`<agent id>/<environment>`). Accept and drop `serialized` and attachments in v0, logging their sizes.
- **Changed in M3**: `/info` now also sends `instance_flags.zstd_compression_enabled: true` and multipart decodes zstd
  ([D-030](decisions.md#d-030)); attachment parts are rejected per part as `attachment_not_stored`, the rest of the
  request accepted ([D-031](decisions.md#d-031)). `serialized` is still dropped.
- **Rationale**: the format is defined by open-source SDKs. Controlling `/info` decides which path clients take.
- **Verification**: golden-file contract tests built by running the real Python and JS SDKs against tracedb, one fixture
  set per pinned SDK version: Python 0.14.17 and 0.8.18.
- **Alternatives**: OTLP-first, which moves to the backlog because the SDK's OTel mode makes it a cheap add-on later.

## R3. UI stack (FR-022, FR-025–027)
- **Decision**: React + Vite + TypeScript, TanStack Virtual for 250+ row trees and long lists, and uPlot for charts. The
  build runs at `cargo build` time (a build script calls Vite) and the output is embedded with rust-embed. The URL holds
  all view state, so views can be shared.
- **As built**: `build.rs` embeds `ui/dist` if present, else a placeholder; CI builds the UI as its own job
  ([D-007](decisions.md#d-007)). Charts are hand-written SVG (`ui/src/stats/chart.tsx`), not uPlot.
- **Rationale**: it's the largest ecosystem for virtualized trees and JSON viewers, and nothing is needed at runtime.
- **Alternatives**: Svelte or Solid, which are smaller but have fewer ready-made components. Plain TS is too slow to
  build. Reusing Jaeger UI was rejected because it has no LLM payloads, threads or late patches.

## R4. Segment format
- **Decision**: Parquet behind `SegmentFormat`. Each segment is split into a `core` file (small columns, sorted by
  `(trace_id, dotted_order)`) and a `payload` file (zstd). Row groups are about 128k rows, with page indexes on.
- **Rationale**: DataFusion supports Parquet natively, it's stable, and its row-group statistics give us pruning.
- **Alternatives**: Vortex (SmithDB's choice), moved to the backlog: it is pre-1.0 and not benchmarked by us. It does
  now support DataFusion 55 (`vortex-datafusion` 0.91, which needs `object_store` 0.13). See the plan's Constitution Check.

## R5. Text search (FR-011)
- **Decision**: `TextIndex` v0 is a per-segment bloom filter over lowercased word tokens from inputs and outputs, stored
  as a small file next to the segment. A search uses the blooms to pick segments, scans their payload files for the
  exact phrase, and returns run ids that join back into the filter.
- **Rationale**: it's the simplest approach that prunes most segments. Benchmark gate: P50 ≤ 500 ms at ~5 GB.
- **Alternatives**: the custom inverted index (backlog; it comes back if the bench fails), and tantivy (dropped).
- **Superseded by [D-026](decisions.md#d-026) (M3)**: Parquet blooms match whole values, not words, so scanning stayed
  too slow. Each flush now writes a per-segment index (`.fts.dict.parquet` term zones + `.fts.post` chunked postings,
  no word positions) behind the `TextIndex` trait (design-notes §6).

## R6. Metastore
- **Decision**: SQLite through sqlx, in WAL mode, with one writer (the compaction or flush path). Queries use the subset
  of SQL that SQLite and Postgres share. The tables are those in design-notes §3.3 (as built: `segments`, `field_stats`,
  `state`, `dedup_keys`, `feedback`).
- **Rationale**: it needs no server (Principle I), and the same queries run on Postgres (Principle V).

## R7. Durability (QR-003)
- **Decision**: append each request batch to the WAL with group commit (fsync about every 5 ms or 1 MB) and acknowledge
  only after fsync. On startup, replay the WAL into the memtable. Delete WAL files only after their flushed segment is
  registered in the metastore.
- **Rationale**: no acknowledged event is lost on a hard kill, and group commit still allows ≥ 20k events/s.

## R8. Fresh data (FR-003)
- **Decision**: the Ingest service exposes `fresh_scan` as a DataFusion `TableProvider` over a memtable snapshot. Query
  plans union it with segment scans, then merge by run id.
- **Rationale**: this is SmithDB's "query the ingestion node before compaction", done in one process. Visibility is
  immediate once the event is acknowledged.
- **As built ([D-017](decisions.md#d-017))**: no `TableProvider`. A query pins the memtables and files in one step
  (D-015); DataFusion scans files with pruning, and merging, the exact filter and trace conditions run in query's own
  code over memtable and file rows alike.

## R9. Pagination (FR-012)
- **Decision**: keyset cursor on `(start_time DESC, run_id)`. The cursor encodes the last key seen plus the filter hash.
- **Rationale**: a new run sorts ahead of the cursor, so pages never repeat. A run that arrives late with an old
  `start_time` can appear on a later page, which is acceptable.

## R10. Tree-aware filters (FR-010)
- **Decision**: `trace_filter` applies to root runs, where `run_id = trace_id`. `tree_filter` becomes a semi-join on
  `trace_id` against runs that match the predicate. Both compile to one DataFusion `LogicalPlan`.
- **As built ([D-017](decisions.md#d-017))**: not a `LogicalPlan`; root and any-run conditions are checked in query's
  own code on merged runs, per batch of candidates in page order ([D-021](decisions.md#d-021)).

## R11. Dedup and validation (FR-004, FR-005)
- **Decision**: deduplicate on `(run_id, op, hash(body))` within a 24 h window held in a memtable-side set. Validate each
  event on its own. A batch response lists the rejected items with reasons, and everything valid is acknowledged.
- **Changed**: the window is 10 min, configurable (`--dedup-window`; [D-008](decisions.md#d-008)); keys are saved per
  flush in the catalog ([D-022](decisions.md#d-022)) and held as 16 B fingerprints, at most 1,048,576 (`--dedup-keys`;
  [D-028](decisions.md#d-028)).

## R12. Tokens, cost, status (derived fields)
- **Decision**: `status` is `running` if there's no `end_time`, `error` if `error` is set, and `ok` otherwise.
  `latency_ms` is `end_time - start_time`. Tokens are taken from the known SDK locations (LLM outputs' usage metadata,
  run-level token fields); the locations are confirmed by the golden files from R2. Cost comes from a price table bundled
  with tracedb that users can override, with no fetching (Principle III).

## R13. Process and services
- **Decision**: there are three services (Ingest, Query, Compaction), each a struct implementing a trait, run as tokio
  tasks inside one process. They talk only through their traits and the storage interfaces (design-notes §2.1). One
  axum router fronts Ingest and Query.
- **Rationale**: the boundaries match SmithDB's services, and keeping them in one process is simpler to run.
- **As built**: the services are crates (`ingest`, `query`; `server` holds the router), called directly, with no
  service traits. Compaction is M4 and not built.
