# Research: M3 — Search, aggregates and speed

Phase 0 decisions for M3. Master decisions ([000 research](../000-tracedb-master-plan/research.md)) and M2's
([M2 research](../002-m2-durable-storage-query-builder/research.md)) still hold unless a topic says it changes them.
Format: decision, why, what else was considered. Code references are to branch `002-m2-durable-storage-query-builder`
at `6e77f12`; crate versions were checked against the offline registry on 2026-11-06; SDK behaviour against the
source of `langsmith` 0.14.17 and 0.8.18.

**Delta key** (as in [spec.md](spec.md)):

| Marker | Fill | Meaning |
|---|---|---|
| 🟢 **New** | green | first built in M3 |
| 🟠 **Improved** | orange | exists from M1 or M2; M3 extends or changes it |
| ⚪ **Existing** | grey | built in M1 or M2; M3 uses it as is |

**Settled by the user (2026-11-06)**: multipart + zstd advertised in `/info` (R9); attachment parts rejected per
part (R9); text index without word positions, SmithDB difference (c) (R1); M3 on branch
`003-m3-search-aggregates-speed`, cut from the 002 tip.

## 🟢 M3-R1. Text index: a per-file sidecar with a term dictionary and postings, no positions
- **Decision**: each flush writes, next to a segment's core and payload files, an index sidecar:
  - `seg-<id>.fts.dict.parquet`: one row per `(term, field)` with `field ∈ {inputs, outputs, error}`, `df`
    (rows containing it), `post_off`, `post_len`. Sorted by `(term, field)`, small pages, page index on. The page
    index's min/max per page **is the term zone**: a lookup reads the footer and one page.
  - `seg-<id>.fts.post`: one blob of postings, one list per dictionary row: sorted payload row numbers, delta
    encoded and bit-packed in blocks of 128, lists laid out in ~64 KB chunks so a lookup is one range read.
  - No word positions. A phrase intersects its words' postings, then the candidate rows' text is checked for the
    words in a row (R3).
  - Behind a `TextIndex` trait in `storage` (`write(rows) -> Sidecar`, `lookup(seg, terms, field) -> RowSet`).
- **Why**:
  - SmithDB's index is per segment with term zones and postings chunked apart from the dictionary
    ([design-notes §6](../000-tracedb-master-plan/design-notes.md#6-full-text-search)); this is that shape on top of
    the Parquet reader we already have (parquet 59.3.2 via DataFusion 55.2.0), so it works the same on S3 with range
    reads (constitution V).
  - The roadmap's "scan + per-segment bloom" plan can't meet the 500 ms target: Parquet blooms match whole column
    values, not words, so we'd need our own token bloom anyway, and a passing file still has to be scanned. Scanning
    payloads at 2M runs takes about 1–3 s.
  - Positions would roughly double the index for one feature (phrases), and the verify step needs the text anyway
    for the excerpt. Settled 2026-11-06 as SmithDB difference **(c)**: revisit if text-search p99 misses its target
    or a phrase's candidate set passes ~50k rows.
  - Size estimate: about +12% of today's files, so disk goes from 19% to about 31–35% of the raw JSON (target ≤ 50%).
    Phrase latency estimate: 50–200 ms median at 2M runs.
- **Considered**: tantivy (not in the offline registry; no `object_store` directory, so it can't range-read S3);
  `fst` dictionaries (the whole file must be fetched before a lookup); Parquet bloom filters on payload columns (whole
  values only); scan only (1–3 s); roaring bitmaps for postings (not in the offline registry; delta + bit-pack in
  ~100 lines is enough at segment scale).
- **Changes a recorded decision**: roadmap 2026-11-05 "custom search index is backlog; v0 uses scan + per-segment
  bloom". Needs a decision-log entry (D-024 proposed) when this plan is approved.

## 🟢 M3-R2. Words: Unicode word boundaries, lower case, JSON string and scalar leaves
- **Decision**: tokenize every string and scalar leaf of `inputs`, `outputs` (JSON) and the `error` text with
  `unicode-segmentation` 1.13.3 `split_word_bounds`, keep the segments that contain a letter or digit, and
  lower-case them (`to_lowercase`, no NFKC, no stemming). Keys are not indexed. A phrase is the same tokenization
  of the query text; it matches when its words appear consecutively **within one leaf value**.
- **Why**: spec assumptions (case-insensitive words, phrases within one text value, no fuzzy matching). Word bounds
  handle non-English text and emoji (edge case). `unicode-segmentation` is already in the lockfile.
- **Considered**: whitespace split (breaks on `429,`, CJK); NFKC folding (larger change than the spec asks; revisit
  if users report misses).

## 🟢 M3-R3. Search: the index gives a superset, the merged run decides
- **Decision**:
  1. Files in scope with a sidecar: look up each word, intersect postings → candidate payload rows → their run ids.
  2. Files without a sidecar (written by M2): scan their payload and `error` text (`not_indexed` warning, as M2's
     payload-path fallback).
  3. Memtable runs: searched directly from the memtable's full states (today's `pin` copies runs **without**
     payloads, [scan.rs:4](../../crates/query/src/scan.rs#L4); search adds a read of the text fields under the
     same pin).
  4. Candidates are merged like any query (multi-version runs fetch every version), then `eval` checks the phrase
     on the **merged** text. Late updates can't leave a stale hit: a replaced output no longer matches.
  5. `search` is evaluated last (it needs payload text), after every cheaper condition.
- `not(search(…))` and `or(…, search(…))` can't use the index; they fall back to D-021's start-time windows with
  early stop. Very common words (df > 50% of a file's rows) skip the index for that file.
- Excerpt: the field path, and ±80 characters around the first match, cut on character boundaries.
- Old M2 folders: `segments.fts_path` is nullable; a background backfill writes sidecars for M2 files when idle
  (one file at a time, write-once object, then catalog update).
- **Why**: the index stays a pruning device; the one evaluator ([D-017](../000-tracedb-master-plan/decisions.md#d-017))
  still decides, so results can't differ by where data lives (FR-03: "no false matches").
- **Considered**: answering from postings alone (wrong for multi-version runs and phrases); indexing the memtable
  (rebuilt per write; the memtable is small enough to scan).

## 🟢 M3-R4. Aggregates: a clean side and a dirty side feeding mergeable accumulators
- **Decision**: a new query path in `query::aggregate`:
  - **Clean side**: file rows with `needs_merge = false` whose run id is not in the multi-version set or the
    memtable. DataFusion scans only the columns the metrics and groups need, with the filter pushed down. When the
    filter compiles exactly (`pushdown.rs` gains an `exact` flag), rows go straight into accumulators; otherwise
    the needed columns are decoded and `eval` runs per row.
  - **Dirty side**: the multi-version set, the memtable and anything else, through today's `collect_pinned` →
    merge → `eval`.
  - Each run is on exactly one side, so nothing is counted twice. Trace and tree conditions become a trace-id set
    first (as lists do), checked per row.
  - Accumulators are ours (count, errors, finished, sums, latency values, histogram), mergeable across sides and
    files.
- **Prerequisite fix (unit 1)**: a second *start* for a run already on disk isn't marked multi-version
  ([writer.rs:250](../../crates/ingest/src/writer.rs#L250), [flush.rs:101](../../crates/ingest/src/flush.rs#L101),
  [D-016](../000-tracedb-master-plan/decisions.md#d-016)). Lists tolerate it; the clean side would count the run
  twice. The writer keeps an in-memory bloom filter of every flushed run id (≈2.4 MB for 2M ids at 1% false
  positives, rebuilt at start from the files' `run_id` columns); a start that creates a new memtable state and hits
  it joins the multi-version set. A false positive only sends one run to the slower side.
- **Why**: full read + merge + eval costs ~7.8 µs per run (≈15 s at 2M runs) against a 1 s target. Most runs have one
  version, so most rows need no merge (M2-R3's fast path, taken further).
- **Risks**: group by a metadata key reads `attrs` JSON text per row (no JSON functions in DataFusion 55); the
  dirty side grows with late finishers (~10% → ~200k runs at 2M, ~1.5 s). Mitigations: reuse D-023's cached merged
  states for multi-version runs; the first task of unit 4 is a **2M-row projected group-by spike** that measures
  both before the engine is built.
- **Spike result (T052, 2026-11-07, release, 2.15M rows in 38 files, 202,773 multi-version runs, 50k in memory)**:
  - Clean side, projected scan (`run_id`, `name`, `status`, `latency_ms`, `total_tokens`) into our accumulators:
    96 ms for 1.8M rows; 26 ms with an exact `has(tags, …)` pushdown (400k rows). Our own accumulators stay (exact
    percentiles need the values; DataFusion's `GROUP BY` has no exact nearest-rank).
  - Group by `metadata.<k>`: a full `attrs` JSON parse per row costs 4 s; reading only the key's value from the
    `attrs` text costs 322 ms. **q5 decided**: targeted per-row lookup, no promoted `group_keys` column.
  - Dirty side: `collect → merge → eval` of the 202,773 multi-version runs costs 9 s; walking D-023's cached
    merged states costs 17 ms. **q6 decided**: the dirty side reuses D-023's cache (file rows merged once and
    kept), plus memtable versions. Its first fill is the 9 s (once per server, then per new id/file).
  - Engine as built (2M runs, warm, machine shared with a build, load ~6/18 cores): by name 440 ms p50; with an
    exact filter 297 ms; by `metadata.<k>` 640 ms; with an inexact filter (only the columns it reads, then `eval`)
    475 ms (3 s with a full row decode). Cold first call 9–14 s (cache fill).
- **Considered**: DataFusion `GROUP BY` over merged runs (needs the merge first: same 15 s); materialized rollups at
  flush (wrong after late updates until M4 compaction).

## 🟢 M3-R5. Metric definitions
- **Decision**:
  - `count` = runs; `error_rate` = runs with status `error` / finished runs (running runs are excluded and counted
    separately as `running`).
  - Percentiles are exact, **nearest rank** (⌈p·N⌉ over finished runs' latencies): keep each group's latency values
    (2M × i64 = 16 MB) and select the nth. Past a cap (default 4M values per query), fall back to the Distribution
    histogram (within ~4.5% at 8 bins per doubling) and mark the figure `approximate`.
  - Every percentile is shown with its `n`.
  - Missing groups are `null`, never `0`.
- **Why**: the demo asserts p95 = 31 s exactly; an estimate could miss it (R11 places 22 runs at exactly 31 s so the
  rank lands inside a flat run).
- **Considered**: t-digest (DataFusion's approx percentile) as the default: can return 30.97 s instead of 31 s.

## 🟢 M3-R6. Progressive answers: a sampled estimate, then the exact figures over SSE
- **Decision**: `GET /api/aggregate/stream` sends an `estimate` event (when the exact answer will take more than
  ~300 ms), then a `result` event. The estimate runs the same engine over an evenly spread sample of files (row
  counts from `SegmentMeta`) and the same fraction of dirty runs, until ~10% of rows or 300 ms. Counts and sums are
  scaled by `rows_total / rows_read`; rates and percentiles are not scaled. The response carries `partial: true,
  sampled_pct`. For a group-by on a path with field stats, the first estimate can come straight from the catalog's
  `field_stats` counts. Closing the stream cancels the computation; the UI drops events for an old query id.
- **Why**: constitution II (progressive querying), FR-11, SC-06 (first estimate ≤ 1 s). SSE (axum 0.8.9) is how
  `/api/tail` already streams ([routes/tail.rs](../../crates/tracedb/src/routes/tail.rs)).
- **Considered**: newest-first windows with partial sums (SmithDB's shape for lists; biased for "group batch a by
  name", since the newest files hold the late finishers); a WebSocket (no need for two-way traffic).

## 🟢 M3-R7. Groups, series, distribution and comparison
- **Decision**:
  - Group by built-ins (`name`, `run_type`, `model`, `status`), `tag`, or `metadata.<key>`, up to 2 keys.
  - Top N groups by count (default 10) plus an `other` row, with `cut: true`.
  - A run without the key goes to `(none)`.
  - Tags are unnested: a run with 3 tags counts in 3 groups, so the total is computed separately and the response
    says groups overlap.
  - **Time series**: ~100 `start_time` buckets on a round step (1 m, 5 m, 15 m, 1 h, …).
  - **Distribution**: a fixed log-latency histogram (10 ms … 1 h, 8 bins per doubling).
  - **Comparison**: two specs run at once (`compare.filter`) or the same spec shifted (`compare.shift: "1d"`). Series
    are aligned by offset from each side's first run (or window start, for a shift). A group missing on one side is
    `null` on that side.
  - **Click-through**: each group, bucket or bin carries its condition as AST. `(none)` = `eq(k, null)`;
    `other` = `and(exists(k), not(in(k, [top…])))`; a tag group = `has(tags, v)`; a bucket adds
    `gte/lt(start_time, …)`; a bin adds `gte/lt(latency, …)`.
- **Why**: spec Story 2–3 and edge cases (many values, few runs, different spans). `exists(path)` is in the
  [master filter contract](../000-tracedb-master-plan/contracts/filter-dsl.md) but not in today's AST
  ([ast.rs:5-24](../../crates/model/src/filter/ast.rs#L5)).

## 🟢 M3-R8. Cost: reported cost first, else tokens × a local price list
- **Decision**:
  - Per run, the source order is:
    1. reported cost: `usage_metadata.total_cost`, or `input_cost + output_cost`, or the run's `total_cost`;
    2. else `input_tokens × in_price + output_tokens × out_price` for its `model`;
    3. else "no price".
  - Costs are integer **picodollars**, rounded half-up to cents for display.
  - No rollup to parents (the SDK may put usage on chains too; summing up the tree would count twice).
  - Price list: `prices.toml` embedded in the binary (USD per 1M tokens, with aliases such as dated model names). A
    user copy at `<data>/prices.toml` overrides it, read at start, no network (constitution III).
  - Token × price is computed at query time, so an edited price applies to old runs.
  - A new nullable core column `reported_cost_pico` is derived at apply time like the token counts. M2 files
    without it derive it from their `outputs_usage` / `metadata` text.
- **Why**: FR-09, edge case "unknown model → no price, never zero"; integers make the $4.12 check exact.
- **Considered**: computing cost at flush (stale after a price edit); f64 dollars (rounding drift in sums).

## 🟢 M3-R9. Multipart upload (settled: advertise multipart + zstd)
- **Finding**: tracedb's `/info` decides the SDK's path. Today it returns `use_multipart_endpoint: false`
  ([routes/ingest.rs:26](../../crates/tracedb/src/routes/ingest.rs#L26)), so the SDK never uses multipart; if `/info`
  fails, the SDK defaults to multipart. Compression happens only if `instance_flags.zstd_compression_enabled`.
- **Format** (SDK 0.14.17): `POST /runs/multipart`, boundary = uuid4 hex. Parts:
  - `post.{id}` / `patch.{id}`: the run minus its large fields;
  - `{op}.{id}.{inputs|outputs|events|extra|error|serialized}`: `application/json` with `Content-Length`;
  - `attachment.{id}.{name}`: raw bytes (`<ct>; length=N` when streamed);
  - `feedback.{id}`: JSON.

  With zstd the whole body is compressed (`Content-Encoding: zstd`, `X-Pre-Compressed-Size` /
  `X-Post-Compressed-Size`). Inline parts are ≤ 20 MB; bigger ones are streamed.
- **Decision**:
  - `/info` advertises `use_multipart_endpoint: true` and `instance_flags.zstd_compression_enabled: true`, and keeps
    `version: ""`.
  - A small streaming multipart parser of our own (headers via `httparse` 1.10.1, already locked), zstd via `zstd`
    0.13.4 (already locked through parquet).
  - Parts are reassembled per `(op, id)` and go through the **same** `incoming()` mapping and `submit` as
    `/runs/batch`, so a run is stored identically (FR-14).
  - Attachment parts are drained without buffering and rejected by name (`attachment_not_stored`); the rest of the
    run is stored. Unknown part names are rejected by name too.
  - `/runs/batch` stays; the 0.8.18 fixtures and a "multipart off" `/info` mode keep exercising it.
- **SDK reactions to errors** (shape our status codes):
  - `415` on multipart: the batch is dropped, no fallback;
  - only `404` falls back to `/runs/batch` (and drops feedback);
  - `429` is retried 3× honouring `Retry-After`, then dropped;
  - `409` counts as success.

  So malformed bodies get `400`, never `404`.
- **Considered**: the `multer` crate (not in the offline registry); keeping multipart off (spec Story 5 asks for
  defaults unchanged).
- **Risks**: part order isn't guaranteed (buffer a request's parts, then assemble); flipping `/info` changes every
  client's path at once (both fixture sets replayed in CI).

## 🟢 M3-R10. Feedback: its own table, joined at read time
- **Finding**: the SDK's default is `POST /feedback` with JSON (client.py:8141). Multipart `feedback.{id}` parts
  are sent only with multipart on, `extend_trace_retention`, `info.version ≥ 0.8.10`, the tracing queue and a
  `trace_id`. tracedb sends `version: ""`, so feedback always comes as `POST /feedback`. This corrects
  [design-notes §4](../000-tracedb-master-plan/design-notes.md#4-ingestion) ("multipart part by default"). Today
  `/feedback` returns `501`, which the SDK **re-raises into agent code** (a crash risk).
- **Fields**: `id, run_id, trace_id, key, score, value, comment, correction, feedback_source{type, metadata},
  session_id, created_at, modified_at`, and others.
- **Decision**:
  - A `feedback` table in the metastore (same SQL on SQLite and Postgres). Unknown fields are kept verbatim in
    `raw`. The `200` reply comes after the commit.
  - Rows are stored by `run_id` whether or not the run exists; the join with runs happens at read time, so feedback
    that arrives first attaches when its run arrives (Story 5 scenario 4).
  - An in-memory `run_id → [(key, score)]` map is loaded at start.
  - Orphans (no such run in scope) are listed by `GET /api/feedback?orphaned=true` and counted in the UI.
  - `PATCH /feedback/{id}` updates a row, so `update_feedback` doesn't crash agents. Filters: `feedback.<key>` (the
    run's mean score for that key), plus LangSmith's `and(eq(feedback_key, k), gt(feedback_score, x))`, which maps
    onto it. Metric: `avg(feedback.<key>)`.
  - multipart `feedback.{id}` parts go to the same store, in case a future SDK sends them.
- **Considered**: feedback through the WAL and memtable (feedback isn't a run version; merge rules don't fit);
  storing on the run (feedback can precede the run).

## 🟠 M3-R11. Scenario plant: batch a re-planted, batch b added
- **Finding**: today all 37 errored batch steps are `fetch_url` 429s with `error = "HTTPError: 429 Too Many
  Requests"`, the `fetch_url` count is random, late steps push `end_time`, the background `batch=nightly-smoke` uses
  the same error text, there is no cost field, and llm runs have no model
  ([scenario.rs](../../crates/tracedb/src/scenario.rs)).
- **Decision, batch a** (`batch=2026-11-05-a`, unchanged M2 counts):

  | What | Planted |
  |---|---|
  | `fetch_url` runs | 300 (fixed assignment) |
  | errored | 42 = 31 × 429 (14 failing roots + 17 quiet) + 11 × `ReadTimeout` (6 quiet + a retry step in 5 failing roots) → **14.0%** |
  | quiet failures | 23 = 17 + 6 (M2 check unchanged) |
  | phrase `429 Too Many Requests` | in `outputs.body` and `error` of the 31 only → **31 hits** |
  | `status_code = 429` | on the 31 (19 v7, 12 v6), so M2's v7 ∧ 429 = 19 holds |
  | latencies | 258 ok 0.2–4 s (never late); 11 timeouts 30.000 s; 22 of the 429s 31.000 s (ranks 270–291); 9 at 35–95 s → p95 rank 285 = **31 s** |
  | llm models (`ls_model_name`) | gpt-4o-mini 800 runs, 3.2M in / 0.6M out → $0.84; gpt-4o 200 runs, 0.8M / 0.1M → $3.00; claude-haiku 100 runs reporting `total_cost` $0.0028 each → $0.28 (tokens would give ~$0.52: tests "reported wins"); local-llama 12 runs → no price |
  | batch cost | **$4.12**, "12 runs had no price" |
| feedback | 2 items `correctness = 0` on two failing roots via `POST /feedback`, one sent **before** its run is uploaded → filter `lt(feedback.correctness, 0.5)` = 2 runs, avg 0.00, 0 orphans |

- **Batch b** (`batch=2026-11-06-b`, after the retry fix): 200 roots, 300 `fetch_url`, 6 errors, all timeouts
  (**2.0%**), zero 429s, p95 **9 s**, cost **$4.07**, no phrase, no `prompt_version`.
- **Timing**: batch a ends at now − 30 h, batch b at now − 6 h. M2 checks switch from "last 12 h" to the batch tag.
  The background error text becomes `HTTPError: 503 Service Unavailable`.
- **Why**: exact, assertable numbers (constitution VI); every metric has one planted cause. Late finishers end before
  the checks run, so percentiles don't move between runs of the demo.
- **Risks**: SDK-style usage on chains (cost rollup) is excluded by R8; an approximate p95 could miss 31 s (R5).

## 🟠 M3-R12. Speed and footprint (measured 2026-11-06, release build, vmmap physical footprint)
- **SC-03 gaps**:

  | Query | Measured | Target | Gap |
  |---|---|---|---|
  | trace tree | p50 6.1 / p99 7.1 ms | 50 ms | none |
  | single run + payload | p50 42.9 / p99 78 ms at 0.48M runs (97 files) | 50 ms | **at risk** |
  | filtered list (varied queries) | p50 61 / p99 128 ms | 100 ms | close |
  | suggestions | ≈3 ms | 200 ms | none |
  | text search, grouped | not built | 500 ms, 1 s | new |

  The run lookup is slow because `scan::payload` probes core + payload in every file of an unbounded scope
  ([scan.rs:391](../../crates/query/src/scan.rs#L391)), and `Query::run` loads the whole trace to count children
  ([lib.rs:510](../../crates/query/src/lib.rs#L510)). **Decision**: carry `segment_id` out of `collect`, read
  payloads only from files that hold a version, and count children from the trace's header rows. M2's bench sent
  one body 21× warm; M3's bench varies values and windows, n ≥ 100.
- **Cache**: no data cache today, only DataFusion's metadata cache
  ([storage/lib.rs:7](../../crates/storage/src/lib.rs#L7), [D-013](../000-tracedb-master-plan/decisions.md#d-013)).
  **Decision**: a bounded memory `Cache` in `storage` (`moka` 0.12.16, in the offline registry) for Parquet footers,
  page indexes, index pages, postings chunks and payload ranges, default **32 MB** and counted inside the 200 MB.
  A disk tier (e.g. foyer) is later, behind the same trait. This revisits D-013 (needs a decision-log entry).
- **Idle memory**:

  | State | Footprint |
  |---|---|
  | empty folder | 7.4 MB |
  | after seeding 3,000 traces (483k steps) | 1.6 GB (≈1.2 GB freed but kept by malloc) |
  | restarted, empty memtable | 295 MB |
  | restarted, after 3 queries | 348 MB |
  | after the 10-min dedup window drained | 170 MB |

  Dedup keys take ~300 B each in memory (33 B packed). The 128 MB memtable limit counts JSON bytes; the real heap
  is much larger. Today's defaults don't fit 200 MB.
  **Definition**: idle = no uploads for one flush age, memtable flushed, dedup drained; the median footprint over
  30 s.
  **Proposed defaults (need the user, see plan)**: memtable 64 MB measured as heap; flush age 2 min; dedup as 16 B
  fingerprints with a cap; cache 32 MB; jemalloc (`tikv-jemallocator` 0.7.0 is in the offline registry; mimalloc isn't) so freed memory goes
  back to the OS.
  **Unverified**: what the remaining ~170 MB of live heap is (no profiler run). A heap-profile spike is the first
  task of unit 12.
- **`tracedb bench`** `[--runs 2M] [--data DIR] [--json FILE]`:
  - Moves `bench_m2::run()` into `crates/tracedb/src/bench.rs`.
  - Loads through HTTP `/runs/batch` from 8 clients while a query loop runs (measures SC-04 too;
    `throughput.rs` asserts only 5k ev/s without queries today).
  - Query mix: 101 random traces and runs, 100 varied lists, text searches, grouped queries, suggestions.
  - Prints rows of `query | p50 | p99 | target | budget | PASS/FAIL`; `--json` writes the same; exit 1 names the
    query and value.
  - **Calibration**: a fixed workload (decode one shipped Parquet file 20× + a sort) gives
    `r = runner / reference`, clamped to [1, 3]. `budget = target × r`, and `r` goes in the JSON. A p99 over 5× its
    p50 fails regardless.
  - **CI**: a 2M-run release job on `main` and nightly, with a step summary; a 200k smoke run on PRs. The 2M
    runner's time and disk aren't verified yet.
- **Considered**: criterion (micro-benchmarks, not a P50/P99 table over HTTP); a fixed CI threshold (fails on a
  slow runner for the wrong reason; spec assumption).

## 🟠 M3-R13. M2 follow-ups (Story 7)

| # | Finding | Decision |
|---|---|---|
| a | Start reads only files with `needs_merge` rows ([recover.rs:92](../../crates/ingest/src/recover.rs#L92)); `corrupt` is fixed at start (`lib.rs:400,607`); `read_core` sends every URL through one `read_parquet`, so one bad file → `500 query_failed` ([api.rs:161](../../crates/tracedb/src/routes/api.rs#L161)) | check the footer of every file at start; a shared, growing `damaged` map in `Ingest`; on a scan failure, probe the files, mark the culprit, retry without it, add a `skipped_files` warning; `/healthz` reports the live map |
| b | Double registration: the retry loop re-calls `flush(&job)` ([flush.rs:58](../../crates/ingest/src/flush.rs#L58)); `segment::write` mints a fresh ULID per attempt ([segment.rs:190](../../crates/storage/src/segment.rs#L190)); `commit_flush` accepts `flushed_seq <= ?` ([metastore.rs:290](../../crates/storage/src/metastore.rs#L290)) | write files and ids **once per job**, outside the retry loop; in the transaction, an existing segment id means "already committed" → `Ok`; the job's max seq is the commit token |
| c | `leave()` refetches only the header and keeps old columns ([Samples.tsx:76](../../ui/src/views/Samples.tsx#L76)); "N new" was left out in M2 (T063) | refetch the shown columns; buffer arrivals while scrolled and show an "N new" pill |
| d | `seed` timestamps run from now forward ([seed.rs:84](../../crates/tracedb/src/seed.rs#L84), [main.rs:159](../../crates/tracedb/src/main.rs#L159)) | base = now − n·7 s − the longest duration |
| e | `identity_conflict` when seeding again into a loaded folder: reproduced; the **same** seed reuses run ids (`Rng::new(seed)`) with timestamps from now, so `dotted_order` differs. Not an ingest bug | mix the base time into seed's id stream (demo stays deterministic), or report "already seeded" |
| f | A second start for a run already on disk isn't multi-version (D-016) | fixed first; aggregates depend on it (R4) |

## 🟢 M3-R14. Charts in the UI: hand-drawn SVG, no chart library
- **Decision**: Time series (lines, ≤ 10 series + `other`), Distribution (bars) and the comparison overlay are
  small React components drawing SVG, with the M2 palette, a hover tooltip and click-through. No new runtime
  dependency (as M2).
- **Why**: two chart types, fixed shapes; a library would be most of the bundle and needs offline vetting.
- **Considered**: Recharts or uPlot (more features than needed, new dependency).

## ⚪ M3-R15. Crate versions

| Crate | Version | Status | Use |
|---|---|---|---|
| `unicode-segmentation` | 1.13.3 | already locked (transitive) | tokenizer (R2) |
| `zstd` | 0.13.4 | already locked (via parquet) | multipart bodies (R9) |
| `httparse` | 1.10.1 | already locked | multipart part headers (R9) |
| `moka` | 0.12.16 | offline registry; was M2-R1's pick, dropped by D-013 | `Cache` (R12) |
| `proptest` | 1.12.0 | already used | aggregate == naive, tokenizer, multipart round trip |
| `datafusion` / `parquet` / `object_store` | 55.2 / 59.3.2 / 0.13.2 | unchanged | |
| `tikv-jemallocator` | 0.7.0 | offline registry (`mimalloc` is not) | returns freed memory (R12), if approved |
