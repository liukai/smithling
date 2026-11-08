# Research: M2 — Durable storage + query builder

Phase 0 decisions for M2. Master decisions ([000 research](../000-tracedb-master-plan/research.md)) still hold; these
refine them for M2. Format: decision, why, what else was considered. Versions checked against crates.io and the SDK
source on 2026-11-06.

## M2-R1. Crate versions
- **Decision**: `datafusion` **55.2** (default features off; `parquet`, `nested_expressions`, `datetime_expressions`),
  which brings `arrow` / `parquet` **59.3** and `object_store` **0.13.2**. `sqlx` **0.9** (`sqlite`, `postgres`,
  `runtime-tokio`), `ulid` 3, `moka` 0.12 for `Cache`. Resolved in a scratch crate: DataFusion 55.2 → arrow 59.3.2,
  parquet 59.3.2, object_store 0.13.2. (M2 built no `Cache`, [D-013](../000-tracedb-master-plan/decisions.md#d-013); M3's `Cache` uses moka, [D-028](../000-tracedb-master-plan/decisions.md#d-028).)
- **Why**: DataFusion 55 is the line `vortex-datafusion` 0.91 supports, and it pins `object_store` 0.13, exactly the
  pin backlog R-4 asked for. Taking arrow/parquet through DataFusion avoids two arrow versions in one binary (latest
  standalone arrow is 60.1).
- **Considered**: arrow 60 directly (would duplicate arrow); `rusqlite` 0.41 (sync, no Postgres path; sqlx gives one
  API for both, constitution V).

## M2-R2. What a file row is: a merged run version, not an event
- **Decision**: the flusher writes each memtable run state (already merged by M1's rule) as one row. Reads merge the
  versions of a run across memtables and files with the same rule, by `last_seq`.
- **Why**: the memtable already holds merged states, so the flush is a straight copy; most runs have one version, so
  most reads need no merge. Event-per-row files would make every read a merge.
- **Considered**: storing raw events (SmithDB's ingest log shape; simpler flush, far slower reads until compaction).

## M2-R3. Merge-on-read with a single-version fast path
- **Decision**: rows carry `needs_merge`; an in-memory multi-version set covers older rows. Only those rows are grouped
  and merged; the rest are filtered in the scan (design §4.3).
- **Why**: filtering before merging is wrong for runs whose late update changes the filtered field; merging all rows
  costs a group-by over every run in range. In the scenario ~10% of runs finish late.
- **Considered**: always merge (simplest, too slow at 2M runs for SC-03); merge at flush by reading back older files
  (puts reads in the flush path, and is half of compaction, which is M4).

## M2-R4. Time pruning on `start_key` from `dotted_order`
- **Decision**: every version carries `start_key`, the run's own start timestamp parsed from the last segment of its
  `dotted_order`. Segment min/max and row-group stats use it.
- **Why**: a late update often has no `start_time`, but the SDK always sends `dotted_order` (it's required), so every
  version can be pruned by time without losing late versions.

## M2-R5. Ids are time-ordered (UUIDv7)
- **Finding**: SDK 0.14.17 creates run ids with `uuid7_from_datetime(start_time)` (`run_trees.py`), and trace ids are
  root run ids. So file min/max `trace_id` is a useful pruning bound, and sorting core files by `trace_id` clusters
  them by time.
- **Decision**: keep per-file min/max ids in the metastore and Parquet bloom filters on `trace_id` and `run_id`
  (R-9). Blooms cover SDK 0.8.x agents and others with random ids.

## M2-R6. JSON fields: promote scalars into typed maps
- **Decision**: three map columns in the core file, `attrs_str`, `attrs_num`, `attrs_bool`, keyed by dotted path. The
  flusher promotes all `metadata` scalars (depth ≤ 4) and `inputs`/`outputs` scalars (depth ≤ 3, ≤ 64 per field,
  strings ≤ 256 bytes). Other paths fall back to reading payload rows of the candidates.
- **Why**: filters on JSON fields (FR-009) must not read payloads (they're most of the bytes). Typed maps give
  numeric comparisons (`outputs.status_code >= 400`) and type-mixing warnings for free.
- **Considered**: a JSON text column with a JSON UDF (reads big strings for every filter); one column per discovered
  path (schema changes every flush; DataFusion handles it but the catalog and files get messy); the
  `datafusion-functions-json` crate (still parses text per row).
- **Superseded by [D-012](../000-tracedb-master-plan/decisions.md#d-012):** one JSON text column `attrs`, evaluated by our filter code after the scan (maps get
  no row-group or page pruning anyway). Unpromoted paths don't read payloads: they report `not_indexed`
  ([D-011](../000-tracedb-master-plan/decisions.md#d-011)).

## M2-R7. Filter language
- **Decision**: LangSmith's grammar as the canonical text form: `eq neq gt gte lt lte has and or not`, `null`, the
  `filter` / `trace_filter` / `tree_filter` / `is_root` slots, `latency` in seconds. tracedb adds dotted JSON paths,
  `in(…)`, and relative times. A hand-written recursive-descent parser in `model` with byte-range errors.
- **Why**: SDK 0.14.17 still documents this grammar in `Client.list_runs` (backlog R-2 corrected). Agents and scripts
  that already speak it work unchanged. A hand-written parser gives the precise error messages FR-013 asks for.
- **Considered**: `field:value` search syntax (R-2; unverified offline, left out of M2); SQL `WHERE` text (powerful
  but not what LangSmith users know; master keeps `/sql` as a separate debug feature).

## M2-R8. Query engine shape
- **Decision**: DataFusion does scan, pruning, pushdown, joins and ordering. Two custom pieces: a `TableProvider` over
  a pinned `Version` (files + `fresh_scan` batch) and a `merge_versions` aggregate (latest present value by
  `last_seq`, per field). The filter AST compiles to DataFusion `Expr`; tree filters to semi-joins on `trace_id`.
- **Why**: constitution's architecture constraint (DataFusion + Arrow); the two custom pieces are the SmithDB-specific
  parts (fresh data, merge-on-read).
- **Considered**: hand-rolled scan loops (fast to start, no joins, no path to M3 aggregates).
- **Superseded by [D-017](../000-tracedb-master-plan/decisions.md#d-017):** DataFusion prunes, uses blooms and pushes filters into file scans; merging versions,
  the exact filter, root / any-run conditions and ordering run in our code on the merged runs
  ([scan.rs](../../crates/query/src/scan.rs), [lib.rs](../../crates/query/src/lib.rs)). No `TableProvider`,
  `fresh_scan`, UDAF or semi-joins were built.

## M2-R9. Live views by re-querying changed traces
- **Decision**: a live view collects changed trace ids per 100 ms tick and re-runs its query restricted to them
  (design §4.5). Moves out of `routes/tail.rs` into the query service.
- **Why**: one evaluator, so live and non-live results can't disagree, including tree conditions. Cost is one small,
  mostly in-memory query per tick per tab.
- **Considered**: evaluating predicates on each `Change` header (M1 style): needs a second evaluator and can't do
  tree conditions or JSON paths without full runs.

## M2-R10. Write slowdown instead of refusal
- **Decision**: `429` + `Retry-After: 1` when the active memtable is full while a flush is running, or when unflushed
  WAL ≥ 1 GB. M1's `507 capacity` is removed.
- **Why**: the SDK retries `429` and honours `Retry-After` (M1 recorded traffic and SDK source), so a burst slows the
  agent briefly instead of dropping traces (R-8). Memory stays bounded.

## M2-R11. Field catalog from flush-time stats
- **Decision**: the flusher computes per-segment stats (type, count, top 50 values, other count) for every promoted
  path and writes them to the metastore; the memtable side is computed on demand. Queries sum stats across segments
  in scope.
- **Why**: suggestions must be fast (≤ 500 ms at 2M runs) without scanning data. Top values summed across segments are
  approximate when a value is outside some segments' top 50; exact for low-cardinality fields like `prompt_version`.
- **Considered**: computing on demand by scanning `attrs_*` (exact, too slow at 2M runs); HyperLogLog distinct counts
  (useful later, not needed for M2's browser).

## M2-R12. Cloud check in CI
- **Decision**: a Linux CI job starts MinIO and Postgres containers, then runs the demo checks and the crash test with
  `--storage s3://…` and `--metastore postgres://…`. Credentials come from CI environment variables and are never
  printed.
- **Why**: constitution V requires the same test suite on both. Containers keep CI self-contained (no cloud account).

## M2-R13. UI build outside `cargo build`
- **Decision**: CI builds the UI as its own job and passes `ui/dist` to the Rust jobs; `build.rs` stops running npm
  and embeds `ui/dist` when it exists, else a placeholder page. `tracedb serve --ui-dir PATH` (or `TRACEDB_UI_DIR`)
  serves UI files from disk.
- **Why**: M1 review: the Rust build shouldn't need Node; UI work shouldn't need a server rebuild. Users still get one
  binary: release builds always embed the UI.
- **Considered**: committing `ui/dist` (noisy diffs, stale-build risk).

## M2-R14. Scenario generator for `tracedb demo`
- **Decision**: a deterministic generator for the overnight-eval scenario, separate from `seed`, that plants exact
  counts: batch `2026-11-05-a` with 200 root tasks; 14 roots errored; 23 roots ok with at least one errored step;
  `metadata.prompt_version` on 1,840 runs (v6 928, v7 912); `fetch_url` steps with `outputs.status_code = 429`; late
  finishers (30–120 s, a few 40+ min); sent through `/runs/batch` like an agent. It also writes enough background data
  to force several flushes, so the checks cross memtable and files.
- **Why**: the demo must assert exact numbers (constitution VI); `seed` makes random shapes for exploration.
