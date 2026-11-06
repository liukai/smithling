---
description: "Tasks for M2 — Durable storage + query builder"
---

# Tasks: M2 — Durable storage + query builder

**Input**: [spec.md](spec.md), [plan.md](plan.md), [design.md](design.md), [data-model.md](data-model.md),
[contracts/](contracts/), [research.md](research.md), [quickstart.md](quickstart.md), [starter-guide.md](starter-guide.md)

**Tests**: included. The spec requires automated checks (M2-FR-22, SC-01..10) and constitution VI requires the demo,
bench and crash test.

**Assumed answers to design §8** (proposed defaults; change before implementing if you disagree):
1. Identity conflicts for runs only on disk are accepted and flagged (`identity_conflict` warning), not rejected.
2. Dedup window 10 min (configurable); WAL files kept at least that long.
3. `build.rs` stops running npm; it embeds `ui/dist` if present, else a placeholder page.
4. Memtable limit 128 MB; flush age 10 min.
5. No `field:value` short form in M2.

**Organization**: grouped by user story; each story phase ends at a checkpoint that can be shown on its own.

## Format: `[ID] [P?] [Story] Description`
- **[P]**: can run in parallel (different files, no dependency on an unfinished task)
- **[Story]**: US1–US7 from spec.md

## Path conventions
Crates `crates/{model,storage,ingest,query,tracedb}`; UI in `ui/`; integration tests in `crates/<crate>/tests/`;
fixtures in `tests/fixtures/`; CI in `.github/workflows/ci.yml` and `ci/cloud.yml`.

---

## Phase 1: Setup (shared infrastructure)

**Purpose**: dependencies and the new crate.

- [X] T001 Add workspace dependencies in root `Cargo.toml`: `datafusion = { version = "55.2", default-features = false, features = ["parquet", "nested_expressions", "datetime_expressions"] }`, `sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "sqlite", "postgres", "migrate"] }`, `ulid = "3"`, `moka = { version = "0.12", features = ["sync"] }`; take `arrow`, `parquet` and `object_store` (must resolve to 0.13.x, backlog R-4) through DataFusion's re-exports, not as separate versions; verify with `cargo tree -d` that there is one arrow version
- [X] T002 [P] Create `crates/storage/` (`Cargo.toml`, `src/lib.rs` with empty modules `object`, `metastore`, `segment`, `cache`, `version`) and add it to the workspace members
- [ ] T003 [P] Add `ci/cloud.yml` (docker compose: MinIO with a `tracedb` bucket, Postgres 17 with database `tracedb`; credentials from environment variables, never echoed)

**Checkpoint**: `cargo build` succeeds with the new dependencies; `cargo tree -d` shows no duplicate arrow/parquet.

---

## Phase 2: Foundational (blocking prerequisites)

**Purpose**: the filter language, the storage traits, and the memtable split that every story uses.

**⚠️ CRITICAL**: no story can start until this phase is complete.

### Tests for the foundation (write first; they should fail)

- [X] T004 [P] Filter golden tests `crates/model/tests/filter.rs`: every example in `contracts/filter-language.md` (SDK docstring examples, the three scenario queries, the four error rows with exact byte ranges and messages)
- [X] T005 [P] Filter property test `crates/model/tests/filter_roundtrip.rs`: random ASTs → `print` → `parse` gives the same AST; `print` is canonical (printing twice is identical)
- [X] T006 [P] `merge_versions` property test `crates/model/tests/merge_versions.rs`: for random event sequences split at random points into versions, merging the versions by `last_seq` equals applying all events in order with M1 `merge` (incl. `tags: []`/`metadata: {}` set via the `present` bitmask)
- [X] T007 [P] *(local SQLite + local files in `crates/storage/tests/segment.rs`; S3/Postgres runs are T068)* Storage trait tests `crates/storage/tests/traits.rs`, generic over implementations: object put/get/range-read/list/delete; a put to an existing name fails (write-once); metastore migrations, flush transaction atomicity (insert segments + field_stats + `flushed_seq` all or nothing), `active_segments`, `flushed_seq`

### Implementation

- [X] T008 [P] Filter AST in `crates/model/src/filter/ast.rs` per data-model §9: `Expr = And | Or | Not | Cmp{op: eq|neq|gt|gte|lt|lte, path, value} | Has{path, value} | In{path, values}`; `Path` = built-in field or `metadata.<dotted>` / `inputs.<dotted>` / `outputs.<dotted>`; `Value` = string | number | bool | null | time
- [X] T009 Filter parser in `crates/model/src/filter/parse.rs`: hand-written recursive descent for the grammar in `contracts/filter-language.md`; errors are `{start, end, expected}` byte ranges; unknown fields suggest the closest built-in (edit distance ≤ 2); `latency` in seconds; relative times `"-12h"`, `"-30m"`, `"-7d"`; `status` values limited to `"running" | "ok" | "error"`; `feedback_key`/`feedback_score` → `unsupported: feedback is supported from M4`; `search(…)` → `unsupported: search() is supported from M3`
- [X] T010 Canonical printer in `crates/model/src/filter/print.rs` (`print(&Expr) -> String`), and `crates/model/src/filter/mod.rs` exporting `parse_filter`, `print`, `Expr`
- [X] T011 [P] `merge_versions(older: &RunVersion, newer: &RunVersion) -> RunVersion` in `crates/model/src/merge.rs`: per field, the newer present value wins (by `last_seq`); identity fields keep the **first** version's value and add `Warning::IdentityConflict` on mismatch (design §8 q1); recompute derived fields with `derive`
- [X] T012 [P] *(done as a `present` bitmask + `to_fields()` on `RunState` in `crates/model/src/run.rs`: a version is a `RunState`, so no separate type)* `RunVersion` type in `crates/model/src/version.rs` with every column of data-model §2, incl. `start_key` ("from the run's own `dotted_order` segment; **never null**"), `present: u32` bitmask, `needs_merge`, `first_seq`/`last_seq`; `start_key` derived via `DottedOrder` (own segment timestamp)
- [X] T013 [P] `ObjectStore` trait + local impl in `crates/storage/src/object.rs` over `object_store::local::LocalFileSystem`; S3 impl over `object_store::aws::AmazonS3` (endpoint, bucket, prefix from `--storage s3://bucket/prefix` + `AWS_*` env); write-once (`PutMode::Create`)
- [X] T014 [P] `Metastore` trait in `crates/storage/src/metastore.rs` with SQLite and Postgres impls via `sqlx`; migrations in `crates/storage/migrations/0001_init.sql` creating exactly the `state`, `segments`, `field_stats` tables and the `segments_scope` index of data-model §5 (SQL that runs on both; `BLOB` vs `BYTEA` via `sqlx` type mapping)
- [X] T015 [P] *(replaced by DataFusion's built-in Parquet metadata cache, decision D-013)* `Cache` trait + moka LRU impl in `crates/storage/src/cache.rs` (keys: file footer, bloom filter, row group; weight by bytes; default 256 MB)
- [ ] T016 `Version` + pinning in `crates/storage/src/version.rs`: `Arc<Version{id, segments, frozen}>`; `VersionSet::pin()`, `publish(new)`; a segment's files may be deleted only when no live `Arc<Version>` references it (R-7); test-only `swap(remove, add)`
- [ ] T017 Split the M1 run store into memtables in `crates/ingest/src/memtable.rs`: `Active` (M1 `RunStore` with `by_trace`, `roots`, `children`, byte count, oldest-event time) and `Frozen` (`Arc`, immutable); `freeze()` swaps in O(1); at most one frozen
- [ ] T018 `Fresh` trait in `crates/ingest/src/lib.rs`: `fresh_scan(project: Option<&str>, time: TimeRange) -> RecordBatch` over active + frozen (core columns only, no payloads) and `fresh_trace(trace_id)`, `fresh_run(run_id, include)`; replaces M1's `Snapshot` (keep `Snapshot` as a thin adapter until query moves over in US1)
- [ ] T019 [P] Multi-version set in `crates/ingest/src/mvset.rs`: `RwLock<HashSet<RunId>>`; the writer inserts when it creates a state from a patch (`created_by_patch`); exported read-only to query

**Checkpoint**: all Phase 2 tests pass; M1's test suite still passes with memtables (no flush yet).

---

## Phase 3: User Story 3 — Keep all data on disk, restart fast (Priority: P1)

**Goal**: data moves to immutable files in the background, nothing confirmed is lost, restarts are fast.

**Independent test**: load 2M runs, kill during uploads and flushes 10 times, restart, compare against every confirmed
upload (spec Story 3).

### Tests for User Story 3

- [X] T020 [P] [US3] Segment round-trip test `crates/storage/tests/segment.rs`: write run versions → read back equal; sorted by `(trace_id, dotted_order)`; bloom filters answer `may_contain` for every written `trace_id` and `run_id`; promoted paths per data-model §4 ("metadata depth ≤ 4, strings ≤ 256 B"; "inputs/outputs depth ≤ 3, ≤ 64 leaves per field")
- [ ] T021 [P] [US3] Flush test `crates/ingest/tests/flush.rs`: after a flush the catalog lists the segment, `flushed_seq = S`, WAL files with last seq ≤ S and older than the dedup window are gone, field stats rows exist
- [ ] T022 [P] [US3] Recovery test `crates/ingest/tests/recover.rs`: orphan files (written, not registered) are deleted on start; replay skips seq ≤ `flushed_seq`; multi-version set is rebuilt from `needs_merge` rows; an M1 data folder (fixture in `tests/fixtures/m1-folder/`) opens with no manual step (M2-FR-20)
- [ ] T023 [P] [US3] Extend the crash harness `crates/tracedb/tests/crash.rs`: kill points during upload, during file put, between put and COMMIT, after COMMIT before trim, during trim (via a `TRACEDB_TEST_CRASH_AT` hook); 10 rounds; assert 0 missing and 0 duplicated (SC-05)
- [ ] T024 [P] [US3] Slowdown test `crates/tracedb/tests/slowdown.rs`: with a 4 MB memtable and a slowed flusher, a burst gets `429` + `Retry-After: 1`, the client retries, nothing is lost, memtable bytes stay ≤ 2 × limit (R-8)
- [ ] T025 [P] [US3] Restart test `crates/tracedb/tests/restart.rs` (`#[ignore]`): 2M runs on disk → ready ≤ 10 s (SC-06)
- [ ] T026 [P] [US3] Flush-latency test `crates/tracedb/tests/throughput.rs` (`#[ignore]`): ≥ 5,000 events/s while flushes run, p99 upload latency ≤ 1 s (SC-07)

### Implementation for User Story 3

- [ ] T027 [P] [US3] Promotion in `crates/ingest/src/flush/promote.rs`: copy scalar leaves into `attrs_str` / `attrs_num` / `attrs_bool` per data-model §4; a path seen with two types goes into both maps
- [ ] T028 [P] [US3] Field stats in `crates/ingest/src/flush/stats.rs`: per path and type: `count`, top 50 values with counts, `other_count`; cap 10,000 paths per segment and mark it incomplete
- [X] T029 [US3] Parquet writer/reader in `crates/storage/src/segment.rs` (`SegmentFormat` trait): core file with the columns of data-model §2, row groups ~64k, page index on, zstd, bloom filters on `trace_id` and `run_id`; payload file with data-model §3 columns; `payload_row` pointer
- [ ] T030 [US3] Flusher in `crates/ingest/src/flush/mod.rs`: background thread; per project in the frozen memtable: sort, set `start_key`, set `needs_merge` (= `created_by_patch` or a bloom hit in an older segment of the same project), promote, stats, write files under `<project>/<yyyy-mm-dd>/seg-<ulid>.{core,payload}.parquet` (`_unknown/` when project is unknown); one metastore transaction (segments + field_stats + `flushed_seq = S`); `VersionSet::publish`; drop frozen; on error keep frozen and retry with backoff
- [ ] T031 [US3] WAL changes in `crates/ingest/src/wal.rs`: `replay_from(seq)`, `trim(through_seq, keep_younger_than)` deleting whole files only, `unflushed_bytes()`
- [ ] T032 [US3] Writer changes in `crates/ingest/src/writer.rs`: freeze when active ≥ `--memtable` (default 128 MB) or oldest event ≥ `--flush-age` (default 10 min); reply `429` + `Retry-After: 1` to the whole group when active is full and a frozen memtable is still flushing, or when unflushed WAL ≥ 1 GB; remove the M1 budget and `507`; dedup window from `--dedup-window` (default 10 min)
- [ ] T033 [US3] Recovery in `crates/ingest/src/recover.rs`: read `flushed_seq` and active segments; delete orphans; build the multi-version set from the `run_id` column where `needs_merge` (cached); replay WAL > `flushed_seq`; rebuild dedup from WAL records inside the window; create a fresh metastore with `flushed_seq = 0` for an M1 folder
- [ ] T034 [US3] Wire storage into `Ingest::open` and `tracedb serve` in `crates/ingest/src/lib.rs` and `crates/tracedb/src/main.rs`: flags `--storage`, `--metastore`, `--memtable`, `--flush-age`, `--dedup-window`, `--cache` per `contracts/cli.md`; `--max-memory`/`--max-runs` accepted with a "no longer applies" warning
- [ ] T035 [US3] `/healthz` in `crates/tracedb/src/lib.rs`: `{ok, wal_ok, flush_ok, memtable_bytes, frozen, unflushed_wal_bytes, segments, last_flush_at, corrupt_segments}` per `contracts/http-api.md`
- [ ] T036 [US3] Keep M1 reads working over memtables ∪ files until US1's planner lands: trace and run lookups in `crates/query/src/lookup.rs` (min/max `trace_id` + bloom pruning, merge multi-version rows, sort by `dotted_order`; payload read via `payload_row`)

**Checkpoint (demo)**: `tracedb seed --traces 20000` → `/healthz` shows segments and memtable ≤ 128 MB; `kill -9`,
restart ≤ 10 s; the M1 runs list and trace view still work; crash harness 10/10.

---

## Phase 4: User Story 1 — Find the failures with the query builder (Priority: P1) 🎯 MVP

**Goal**: any filter (fields, JSON paths, conditions on the root and on any run in the trace), by clicking, over
memtables ∪ files.

**Independent test**: on the scenario data, build each query in the panel without typing syntax → exactly 14 and 23,
newest first across pages (spec Story 1).

### Tests for User Story 1

- [ ] T037 [P] [US1] Merge-on-read property test `crates/query/tests/merge_on_read.rs`: random runs, random late updates, random flush points → `query(Q)` equals a naive filter over fully merged runs, for random `Q` incl. `trace_filter` and `tree_filter` (SC-09)
- [ ] T038 [P] [US1] Cursor property test `crates/query/tests/cursor.rs`: paging with page size 10 while new runs arrive returns no repeats and no gaps among runs that existed at the first page (M2-FR-04); cursor `query_hash` excludes `cursor`, `view`, `live`
- [ ] T039 [P] [US1] API tests `crates/tracedb/tests/query_api.rs`: `POST /api/query` shapes and `400 bad_filter {slot, start, end, expected}` / `bad_cursor` / `unsupported` per `contracts/http-api.md`; `GET /api/roots` still answers M1 clients
- [ ] T040 [P] [US1] UI tests `ui/src/query/Panel.test.tsx`: chips ⇄ text round trip; negate/disable/remove a chip updates results; click a tag/status/name adds a chip, Alt-click adds its negation; inline error underline from a `bad_filter` response; M1 `?project=&status=` links translate on load

### Implementation for User Story 1

- [ ] T041 [US1] `TableProvider` over a pinned `Version` + `fresh_scan` batch in `crates/query/src/provider.rs`: prune segments by project and `start_key` range from the version's segment list; scan core columns with pushdown `Q.filter OR needs_merge OR run_id ∈ multi-version set` (design §4.3); identity predicates pushed down unconditionally
- [ ] T042 [US1] `merge_versions` aggregate UDAF in `crates/query/src/merge.rs` grouping rows that have `needs_merge` or whose `run_id` is in the multi-version set; uses `model::merge_versions`
- [ ] T043 [US1] Planner in `crates/query/src/plan.rs`: compile `Expr` to DataFusion `Expr` (built-in fields → columns; `metadata.*`/`inputs.*`/`outputs.*` → `attrs_*` map lookups by type; comparisons only on the matching type, with a `mixed_types` warning carrying counts); non-promoted paths → read payload rows of candidates with a `not_indexed` warning; `is_root`; order by `start_time` or `latency` with the keyset cursor `{order_value, run_id, query_hash}`; limit `1..=200`, default 50
- [ ] T044 [US1] Tree filters in `crates/query/src/plan.rs`: `trace_filter` = semi-join with roots matching it on `trace_id`; `tree_filter` = semi-join with distinct `trace_id`s of any run matching it (including the run itself; contract note)
- [ ] T045 [US1] `Query` service API in `crates/query/src/lib.rs`: `query(Query) -> Page{runs, next_cursor, warnings, stats{files_scanned, files_pruned, rows_merged, ms}}`; move M1 `roots`, `trace`, `run` onto it; drop the `Snapshot` adapter from T018
- [ ] T046 [US1] Routes in `crates/tracedb/src/routes/api.rs`: `POST /api/query` (request/response per `contracts/http-api.md`), errors with slot and byte range; `GET /api/roots` mapped onto `query` with `is_root: true`
- [ ] T047 [P] [US1] `tracedb query` in `crates/tracedb/src/main.rs` + `crates/tracedb/src/cli_query.rs`: flags per `contracts/cli.md`; table output, `--json` for JSON lines
- [ ] T048 [P] [US1] UI query model in `ui/src/query/ast.ts`, `ui/src/query/print.ts`, `ui/src/query/parse.ts` mirroring the Rust grammar; a shared golden file `tests/fixtures/filter-golden.json` used by both T004 and the UI tests so the two parsers can't drift
- [ ] T049 [US1] Query panel in `ui/src/query/Panel.tsx`, `ui/src/query/Chip.tsx`, `ui/src/query/TextEditor.tsx`: project, time range, three where-rows (this run / its root / any run in its trace), order, limit; text ⇄ panel toggle; deep expressions shown as one expression chip; inline errors
- [ ] T050 [US1] Samples view in `ui/src/views/Samples.tsx` (from M1 `RunsList`, now driven by `POST /api/query`, infinite paging by cursor); click-to-filter on status badges, tags, names
- [ ] T051 [US1] URL state in `ui/src/query/url.ts` + `ui/src/route.ts`: `?project=&time=&filter=&trace=&tree=&root=1&order=&cols=&view=&live=1`; translate M1 links on load (M2-FR-11)

**Checkpoint (demo)**: quickstart step 2.1–2.2 and 2.4–2.5: 14 and 23 by clicking; text ⇄ panel; a link reopens the
view. **This is the M2 MVP.**

---

## Phase 5: User Story 2 — Explore fields and filter by clicking values (Priority: P1)

**Goal**: a field browser with types and top values; any value anywhere is clickable; a Table view with chosen columns.

**Independent test**: open the field browser → `prompt_version` with `v6 (928)`, `v7 (912)`; click `v7`, a tag and a
status → three chips; results match (spec Story 2).

### Tests for User Story 2

- [ ] T052 [P] [US2] Field catalog tests `crates/query/tests/fields.rs`: stats summed across segments in scope plus memtable stats; a metadata key first seen today is listed; `complete = false` past 10,000 paths; values endpoint returns ≤ 50 with `approximate` when any segment's top 50 cut it off
- [ ] T053 [P] [US2] Suggestion latency check in `crates/tracedb/tests/bench_m2.rs` (`#[ignore]`): p50 ≤ 500 ms for fields and values at 2M runs (SC-03)
- [ ] T054 [P] [US2] UI tests `ui/src/fields/FieldBrowser.test.tsx` and `ui/src/views/Table.test.tsx`: suggestions with counts as you type; add a column; click a cell value → chip

### Implementation for User Story 2

- [ ] T055 [US2] Field catalog reads in `crates/query/src/fields.rs`: `fields(scope)` and `values(scope, path, prefix)` from `field_stats` rows of segments in scope (one SQL `GROUP BY path, type`; merge `top_values` in Rust) + `ingest.memtable_stats(scope)` computed on demand
- [ ] T056 [US2] Routes `GET /api/fields` and `GET /api/fields/values` in `crates/tracedb/src/routes/api.rs` per `contracts/http-api.md`
- [ ] T057 [P] [US2] Field browser in `ui/src/fields/FieldBrowser.tsx` + `ui/src/fields/ValueList.tsx`: paths by count with types, values with counts, prefix search, click → chip in "This run"
- [ ] T058 [P] [US2] Table view in `ui/src/views/Table.tsx`: user-chosen columns sent as `columns` in the query; cells clickable to filter; columns kept in the URL (`cols=`)

**Checkpoint (demo)**: quickstart step 2.3.

---

## Phase 6: User Story 4 — Live views for any query (Priority: P2)

**Goal**: any query view can stay live, including tree conditions; runs that stop matching are marked.

**Independent test**: a live view for `status = error AND name = fetch_url` shows only matching runs within 1 s
(spec Story 4).

### Tests for User Story 4

- [ ] T059 [P] [US4] Live tests `crates/tracedb/tests/e2e_live.rs`: matching runs within 1 s, non-matching ignored; `left` when a run stops matching; quiet-failures live view gets the root when a child errors after the root finished; `resync` past 5,000 changed traces; M1 `?project=`/`?trace=` tails still work (reuse M1 `e2e_tail.rs` assertions)

### Implementation for User Story 4

- [ ] T060 [US4] `query::tail(Query) -> Stream<TailMsg>` in `crates/query/src/tail.rs` per design §4.5: subscribe first, then `Hello{server_seq}`; collect changed `trace_id`s; first change after a quiet spell at once, then every 100 ms run `query(Q ∧ trace_id ∈ changed)`; emit `Runs`, `Left` (last 5,000 sent ids), `Resync` on lag or > 5,000 changed traces
- [ ] T061 [US4] `Change` gains `trace_id` in `crates/ingest/src/hub.rs` / `crates/ingest/src/memtable.rs`
- [ ] T062 [US4] Reduce `crates/tracedb/src/routes/tail.rs` to SSE framing of `TailMsg` (`hello`, `runs`, `left`, `resync`; 15 s keep-alive); parse `?q=` (URL-encoded query JSON) and M1 `?project=` / `?trace=` (M2-FR-26)
- [ ] T063 [US4] UI live toggle in `ui/src/live.ts` + `ui/src/views/Samples.tsx`: subscribe with the current query; mark `left` runs "no longer matching"; reload on `resync`

**Checkpoint (demo)**: quickstart step 5.

---

## Phase 7: User Story 5 — Reproduce the scenario numbers with one command (Priority: P2)

**Goal**: `tracedb demo` generates the overnight-eval scenario and checks 14, 23 and the `prompt_version` top values.

**Independent test**: on an empty folder, `tracedb demo` passes all checks and exits 0 (spec Story 5).

### Tests for User Story 5

- [ ] T064 [P] [US5] Scenario generator test `crates/tracedb/tests/scenario.rs`: same seed → identical data; planted counts exactly as research M2-R14 (200 roots tagged `batch=2026-11-05-a`; 14 roots error; 23 roots ok with ≥ 1 errored step; `metadata.prompt_version` on 1,840 runs: v6 928, v7 912); background volume forces ≥ 2 flushes

### Implementation for User Story 5

- [ ] T065 [US5] Scenario generator in `crates/tracedb/src/scenario.rs` per research M2-R14, sent through `/runs/batch`; late finishers 30–120 s and a few 40+ min (sent immediately with correct timestamps under `--no-delay` semantics)
- [ ] T066 [US5] `tracedb demo` in `crates/tracedb/src/demo.rs` + `main.rs`: flags per `contracts/cli.md` (`--seed`, `--url` or `--data`, `serve` storage flags, `--checks m2`); run its own server on a temp folder when no `--url`; checks: 14 failing roots, 23 quiet failures, `prompt_version` top values, the planted v7 ∧ `status_code = 429` count, and "checks across memtable and files" (at least one counted run has versions in both); print the pass/fail table with timings; exit 1 naming expected vs actual on failure
- [ ] T067 [US5] Run `tracedb demo` in CI (`.github/workflows/ci.yml`, local job)

**Checkpoint (demo)**: quickstart step 1.

---

## Phase 8: User Story 6 — Same results on cloud storage (Priority: P2)

**Goal**: S3-compatible storage + Postgres by configuration only, same results.

**Independent test**: demo checks and crash test on MinIO + Postgres give the same numbers (spec Story 6).

- [ ] T068 [US6] Run the storage trait tests (T007) against S3 (MinIO) and Postgres when `TRACEDB_TEST_S3_URL` / `TRACEDB_TEST_PG_URL` are set, in `crates/storage/tests/traits.rs`
- [ ] T069 [US6] Cloud CI job in `.github/workflows/ci.yml`: start `ci/cloud.yml`, run storage trait tests, `tracedb demo --storage s3://tracedb/ci --metastore postgres://…`, and the crash harness with the same flags; compare the demo table with the local job's
- [ ] T070 [US6] Ensure credentials never reach logs: redact `postgres://user:pass@` and `AWS_*` values in `crates/tracedb/src/main.rs` startup logging; test in `crates/tracedb/tests/redact.rs`

**Checkpoint**: cloud CI job green with the same demo numbers (SC-08).

---

## Phase 9: User Story 7 — Maintainer workflow and M1 follow-ups (Priority: P3)

**Goal**: honest retries, UI work without server rebuilds, Rust builds without Node. Independent of US1–US6; can be
done right after Phase 2 (plan PR 2).

**Independent test**: each acceptance scenario of spec Story 7.

- [X] T071 [P] [US7] Writer test `crates/ingest/tests/writer.rs`: an identity conflict is rejected; the identical retry is rejected again with the same reason; it never gets a seq, WAL record or dedup key (M2-FR-23)
- [X] T072 [US7] Identity check before WAL append in `crates/ingest/src/writer.rs`: check against the active, then frozen state with `model::check_identity`; rejected items are not numbered, logged or added to dedup
- [X] T073 [P] [US7] `--ui-dir PATH` / `TRACEDB_UI_DIR` in `crates/tracedb/src/routes/ui.rs`: serve files from that folder instead of the embedded copy (same index fallback); test in `crates/tracedb/tests/ui_dir.rs` (M2-FR-24)
- [X] T074 [US7] `crates/tracedb/build.rs`: stop running npm; embed `ui/dist` if present, else a one-page placeholder ("UI not built — run `npm run build` in ui/") (M2-FR-25)
- [X] T075 [US7] CI in `.github/workflows/ci.yml`: a `ui` job (npm ci, test, build, upload `ui/dist` artifact); Rust jobs download it; one Rust job runs on an image without Node; the release/clean-container job always embeds the built UI

**Checkpoint**: `cargo build` without Node; `serve --ui-dir ui/dist` serves an edited UI; retry rejection is stable.

---

## Phase 10: Polish and cross-cutting

- [ ] T076 [P] Pinned-version test `crates/query/tests/pinned.rs`: a long scan across a test-only `swap` returns the same rows; swapped-out files are deleted only after the scan drops its `Arc<Version>` (R-7)
- [ ] T077 [P] Bloom bench in `crates/tracedb/tests/bench_m2.rs` (`#[ignore]`): files opened per trace lookup with and without blooms at 2M runs; report (R-9)
- [ ] T078 [P] M2 bench in `crates/tracedb/tests/bench_m2.rs` (`#[ignore]`): P50/P99 for filtered list, trace, single run, suggestions at 2M runs; assert SC-03 (filter p50 ≤ 500 ms; trace/run p50 ≤ 200 ms; suggestions p50 ≤ 500 ms); report memory and disk vs raw JSON (SC-04)
- [ ] T079 [P] No-outbound check `scripts/no-outbound.sh`: add a flush and a `tracedb demo` run in local mode (M1 check extended)
- [ ] T080 [P] `README.md`: storage flags, `tracedb demo`, `tracedb query`, the query panel, cloud mode, M2 limits (no compaction, no text search)
- [ ] T081 Run [quickstart.md](quickstart.md) end to end and record results under "Last verified"
- [ ] T082 Update the M2 row and STATE/NEXT in `specs/000-tracedb-master-plan/roadmap.md`
- [ ] T083 **Code starter guide** (M2-FR-27, plan PR 12): rewrite `specs/002-m2-durable-storage-query-builder/starter-guide.md` from pre-code to a guide for reading the built code, as M1's: reading order of the real files; each scene (write + identity check, freeze, flush write → register → trim, late update, query split/merge/semi-join, trace lookup, live view, field browser, restart, panel ⇄ text) mapped to real functions with links and line numbers; vertical diagrams with short labels; rough edges found while writing it

---

## Dependencies and execution order

```mermaid
flowchart TB
    P1["Phase 1 Setup"]
    P2["Phase 2 Foundational<br/>filter language · storage traits · memtables"]
    US7["US7 Maintainer + M1 follow-ups (P3)<br/>any time after Phase 2"]
    US3["US3 Disk storage (P1)"]
    US1["US1 Query builder (P1) — MVP"]
    US2["US2 Fields + Table (P1)"]
    US4["US4 Live views (P2)"]
    US5["US5 tracedb demo (P2)"]
    US6["US6 Cloud (P2)"]
    PL["Phase 10 Polish<br/>ends with the code starter guide (T083)"]
    P1 --> P2
    P2 --> US7
    P2 --> US3
    US3 --> US1
    US1 --> US2
    US1 --> US4
    US1 --> US5
    US5 --> US6
    US2 --> PL
    US4 --> PL
    US6 --> PL
    US7 --> PL
```

- **US1 after US3**: the planner reads files, so it needs the segment format and flusher. The UI part of US1
  (T048–T051) can start right after Phase 2 against a stub `/api/query`.
- **US6 after US5**: the cloud job runs `tracedb demo`.
- **Within each story**: tests first (they should fail), then implementation, then the checkpoint.
- **T083 is last**: it reads the merged code; M2 isn't done until it lands.

## Parallel opportunities
- **Phase 1**: T002, T003.
- **Phase 2**: tests T004–T007 together; T008, T011, T012 (model) alongside T013, T014, T015 (storage); T019 alongside T017.
- **US3**: tests T020–T026 together; T027, T028 together before T030.
- **US1**: tests T037–T040 together; UI T048–T051 alongside the Rust planner T041–T046; T047 (CLI) alongside T046.
- **US2**: T057, T058 (UI) alongside T055, T056.
- **US7**: whole phase alongside US3.
- **Polish**: T076–T080 together.

## Implementation strategy
1. **Foundation** (Phases 1–2): the language and storage traits; M1 tests stay green.
2. **US3**: data on disk, crash-safe, fast restart. Demo: 2M runs, `kill -9`, back in ≤ 10 s.
3. **MVP = US1**: the 14 and 23 by clicking. Stop and demo.
4. **US2**: field browser and Table.
5. **US4, US5, US6**: live views, `tracedb demo`, cloud CI.
6. **US7** whenever convenient after Phase 2 (small, independent).
7. **Polish**, then the **code starter guide** (T083) closes M2.

PR mapping (plan.md "Delivery order"): PR1 = T004–T006, T008–T012 · PR2 = T071–T075 · PR3 = T001–T003, T007, T013–T016 ·
PR4 = T020, T027–T029 · PR5 = T017–T019, T021, T024, T030–T032, T034, T035 · PR6 = T022, T023, T025, T033, T036 ·
PR7 = T037–T039, T041–T043, T045, T046 · PR8 = T044, T047, T052, T055, T056 · PR9 = T059–T063 ·
PR10 = T040, T048–T051, T054, T057, T058 · PR11 = T026, T053, T064–T070, T076–T082 · PR12 = T083.
