# Design: M2 — Durable storage + query builder

**Status**: Draft for review (before implementation) · **Spec**: [spec.md](spec.md) · **Plan**: [plan.md](plan.md) ·
**Starter guide (pre-code)**: [starter-guide.md](starter-guide.md) · **Parquet + DataFusion primer**: [parquet-datafusion-primer.md](parquet-datafusion-primer.md) · **Builds on**: [M1 design](../001-m1-see-agent-trace-live/design.md) ·
**Master design**: [design-notes.md](../000-tracedb-master-plan/design-notes.md)

## 1. What this design has to achieve

From the [spec](spec.md#end-state), M2 must:
- keep data on disk, so the memory limit stops being a data limit (2M runs at the reference size);
- never lose or duplicate a confirmed event, including crashes while data is being written to disk;
- answer any filter (fields, JSON paths, tree conditions) with one merged state per run, wherever its pieces live;
- give a point-and-click query panel that is the same query as the LangSmith filter text;
- keep any query view live;
- run unchanged on cloud object storage and a hosted database.

It keeps M1's promises (confirm after fsync, visible within 1 s) and its one-writer design.

## 2. Big picture

```
                                tracedb (one process)
 SDK ──HTTP──▶ ┌─────────────────────────────── HTTP layer ────────────────────────────────┐ ◀── Browser
               │ LangSmith compat · native · /api/query · /api/fields · /api/tail · UI     │
               └──────┬────────────────────────────┬───────────────────────────┬──────────┘
                      │ accept                     │ query / fields / trace    │ tail(query)
               ┌──────▼──────────────┐      ┌──────▼──────────────────────────▼──────┐
               │ Ingest service      │      │ Query service                           │
               │  writer (one thread)│      │  filter text ⇄ AST · planner            │
               │  WAL · dedup · seq  │      │  DataFusion: prune → scan → merge →     │
               │  ┌───────┐ ┌──────┐ │fresh │   filter → order → page                 │
               │  │active │ │frozen│─┼─────▶│  live views: re-query changed traces    │
               │  │memtbl │ │memtbl│ │ scan │  field catalog reads                    │
               │  └───────┘ └──┬───┘ │      └──────▲───────────────────▲──────────────┘
               │     flusher ◀─┘     │             │ pinned Version     │ field stats
               └──────┬──────────────┘             │                    │
                      │ write files, then register │                    │
               ┌──────▼────────────────────────────┴────────────────────┴──────────────┐
               │ storage crate:  ObjectStore (local FS | S3)   Metastore (SQLite | PG)  │
               │                 SegmentFormat (Parquet)       Cache (memory LRU)        │
               └───────────────────────────────────────────────────────────────────────┘
   ./.tracedb/wal/*.wal (local)    ./.tracedb/data/<project>/<day>/seg-*.parquet    ./.tracedb/meta.sqlite
```

**The ideas to hold on to:**
1. **Still one writer.** Uploads go through the M1 writer unchanged: dedup → seq → WAL → fsync → apply → publish → confirm.
2. **Two memtables.** The writer fills the *active* memtable. When it is full it becomes *frozen* (read-only) and a
   new active one starts, so uploads never wait for disk writes (R-10).
3. **Files are immutable; the catalog says which ones count.** The flusher writes files first, then registers them in
   the metastore in one transaction, then trims the WAL. A crash anywhere leaves the data in the WAL, in registered
   files, or both — never neither.
4. **A run can live in several places.** Its start may be in a file and its late update in the memtable. Reads merge
   all versions with the M1 merge rule. Most runs live in exactly one place, and the query takes a fast path for them.
5. **One query shape everywhere.** Panel, text, link, API, live view and the demo checks all compile to the same
   query AST and run through the same planner.

## 3. Core components

| # | Component | Responsibility | Interface (shape) | SmithDB counterpart | Change from M1 |
|---|---|---|---|---|---|
| 1 | **`model`** | Types, merge rule, derive, validation; **filter AST, parser, printer** (no I/O) | `merge`, `validate`, `derive`; `parse_filter(&str) -> Result<Expr, ParseError>`, `print(&Expr)` | run schema, filter language | + filter language |
| 2 | **HTTP layer** | Wire formats ↔ model; routes; serves UI from embedded copy or `--ui-dir` | [contracts/http-api.md](contracts/http-api.md) | API servers | + `/api/query`, `/api/fields`, query-aware `/api/tail`; tail loop moves out |
| 3 | **Ingest service** | Writer (as M1) + memtable swap + flusher + WAL trimming + write slowdown | `accept(items)`, `fresh_scan(scope) -> RecordBatch`, `subscribe()` | ingestion service | + freeze/flush/trim; identity check before WAL |
| 4 | **WAL** | Append-only log; replay from `flushed_seq` | `append`, `sync`, `replay_from(seq)`, `trim(through_seq)` | WAL on ingestion node | + trim, replay-from |
| 5 | **Memtables** | Active (mutable, writer only) and frozen (immutable `Arc`, being flushed) | `apply`, `freeze() -> Arc<Frozen>` | memtable (×2) | was the whole run store |
| 6 | **Flusher** | Frozen memtable → core + payload Parquet per project → register → field stats → trim WAL | background thread; `flush(Arc<Frozen>)` | flush | new |
| 7 | **`storage`** crate | `ObjectStore`, `Metastore`, `SegmentFormat`, `Cache` traits + v0 implementations | [data-model.md §5–6](data-model.md) | object storage, metastore, Vortex files, SSD cache | new |
| 8 | **Query service** | Filter → plan over pinned files ∪ fresh memtable rows; merge-on-read; tree filters; cursor paging; trace and run lookups; field catalog reads; live views | `query(Query) -> Page`, `trace(id)`, `run(id, include)`, `fields(scope)`, `values(scope, path)`, `tail(Query) -> Stream<TailMsg>` | query service | DataFusion; owns live views |
| 9 | **UI** | Query panel ↔ text, chips, field browser, Samples + Table views, click-to-filter, live toggle, links | consumes the HTTP API | LangSmith UI | + query builder |
| 10 | **CLI** | `serve` (+ storage flags), `seed`, `demo`, `query` | [contracts/cli.md](contracts/cli.md) | — | + `demo`, `query` |

Crates in M2: `model`, `storage` (new), `ingest`, `query`, `tracedb`, plus `ui/`. `compact` waits for M4.

**Boundary rules** (unchanged in spirit from M1):
- `query` depends on `model`, `storage`, and two read-only traits exported by `ingest` (`Fresh`, `Changes`); never on
  ingest internals.
- `ingest` depends on `model` and `storage`; it knows nothing about HTTP, DataFusion plans or the UI.
- Only `storage` knows file paths, Parquet, SQL or S3. Everything above sees traits.
- The HTTP layer is the only place that knows LangSmith field names *and* the only place that knows SSE.

### 3.1 What each component can and cannot do in M2

#### 1. `model`
- **Can**: parse the LangSmith filter grammar subset plus tracedb's dotted JSON paths into an AST, with error
  positions; print an AST back to canonical text; merge two run *versions* (not only event onto state).
- **Cannot**: run a query (no I/O); full-text `search()` (M3, parse error says "supported from M3"); feedback fields.

#### 2. HTTP layer
- **Can**: everything M1 does; `POST /api/query`, field endpoints, `GET /api/tail?q=…`; serve UI from `--ui-dir`.
- **Cannot**: multipart, feedback (M3). It holds no live-view logic any more: it only frames `TailMsg` as SSE.

#### 3. Ingest service (writer)
- **Can**: everything M1 does; reject an identity conflict **before** writing it to the WAL, so a retry is rejected
  again (M2-FR-23); answer `429 busy` when memory or unflushed WAL passes its limit (R-8); hand query a consistent
  snapshot of active + frozen memtables.
- **Cannot**: check identity against runs that are only on disk (see §8, open question 1). It never touches files
  except through the flusher.

#### 4. WAL
- **Can**: replay only records with `seq > flushed_seq`; delete whole files whose records are all flushed and older than
  the dedup window.
- **Cannot**: live on object storage. It stays on local disk in every mode, like SmithDB's WAL on the ingestion node's
  SSD. Cloud mode therefore still needs a local data folder for the WAL.

#### 5. Memtables
- **Can**: hold merged run states (with payloads) for recent events, with M1's indexes (by trace, roots by project);
  freeze in O(1) by swapping a pointer.
- **Cannot**: hold everything. Default limit: **128 MB** per memtable (configurable). At most one frozen memtable
  exists; a second freeze waits for the flush, and uploads get `429` meanwhile.

#### 6. Flusher
- **Can**: write one core file and one payload file per project in the frozen memtable; mark rows that need merging;
  compute field stats; register everything plus the new `flushed_seq` in one metastore transaction.
- **Cannot**: merge or rewrite older files (compaction, M4). File count grows with data in M2.

#### 7. `storage`
- **Can**: put/get/range-read immutable objects on local FS or S3-compatible stores; keep the segment catalog and field
  stats in SQLite or Postgres with the same SQL; write and read Parquet with page indexes and bloom filters; cache
  footers, bloom filters and hot row groups in memory.
- **Cannot**: rename or overwrite objects (object-storage semantics, constitution V); Vortex (backlog R-4).

#### 8. Query service
- **Can**: run any M2 query over memtables ∪ files; prune files by project, time and key bloom filters; merge
  multi-version runs; evaluate run, root and any-run-in-trace conditions; page with a stable cursor; serve live
  views by re-querying changed traces.
- **Cannot**: text search, group by, metrics, charts, comparison (M3); progressive early-stop scanning (M3); pick a
  different plan per user hint.

#### 9. UI
- **Can**: build queries with chips (this run / its root / any run in its trace), negate/disable/remove chips, edit as
  text with inline errors, browse fields and values with counts, show Samples or a Table with chosen columns, click any
  value to filter, toggle live, share links; open M1 links.
- **Cannot**: Time series, Distribution, group by, compare (M3); saved queries.

#### 10. CLI
- **Can**: `tracedb serve --storage … --metastore … --ui-dir …`; `tracedb demo` (M2 checks); `tracedb query '<filter>'`.
- **Cannot**: `bench`, `tail`, `import` (M3, M5).

## 4. Key flows

### 4.1 Write path: unchanged, plus one check and one limit

```
writer, per group (M1 order kept):
  for each item:
    if dedup hit            → count accepted, skip
    if identity conflict with ACTIVE or FROZEN memtable state → reject now (no seq, no WAL, no dedup key)   ← new (FR-23)
    seq += 1; WAL append
  fsync once
  apply to ACTIVE memtable; publish Change{trace_id, run_id, last_seq}; reply 202
before taking a group:
  if active ≥ limit and frozen is still flushing → reply 429 Retry-After: 1 to the whole group           ← new (R-8)
  if active ≥ limit and no frozen               → freeze(active), wake flusher, continue                ← new (R-10)
```

- The identity check uses the same `check_identity` as merge, but runs **before** the WAL append. The M1 rough edge
  goes away: a rejected event never gets a dedup key, so the identical retry is checked again and rejected again.
- M1's `507 capacity` goes away. Memory is bounded by `2 × memtable limit` plus indexes; the client is asked to wait
  instead. The SDK retries `429` and honours `Retry-After` (M1 recorded traffic).
- Also `429` when unflushed WAL passes **1 GB** (keeps restart fast), e.g. if flushes keep failing.

### 4.2 Flush: write, register, then trim

```mermaid
sequenceDiagram
    participant W as writer
    participant F as flusher
    participant OS as ObjectStore
    participant MS as Metastore
    participant Q as Query (Version)
    W->>F: freeze: frozen = active (max seq S); new active
    F->>F: per project: sort rows, mark needs_merge, build field stats
    F->>OS: put seg-<ulid>.core.parquet, seg-<ulid>.payload.parquet
    F->>MS: BEGIN; insert segments; insert field_stats; flushed_seq = S; COMMIT
    F->>Q: publish Version{segments + new, frozen = none}
    F->>F: drop frozen (when no reader holds it)
    F->>F: WAL: delete files with last seq ≤ S and older than dedup window
```

Crash at each point:

| Crash after… | On restart | Lost? Duplicated? |
|---|---|---|
| freeze, before files are written | no new files; `flushed_seq` unchanged; WAL replays everything > old `flushed_seq` | no / no |
| files written, before COMMIT | files exist but aren't in the catalog → **orphans**, deleted on start; WAL replays | no / no |
| COMMIT, before WAL trim | catalog has the files and `flushed_seq = S`; replay skips seq ≤ S | no / no |
| WAL trim | normal state | no / no |

**Triggers**: active ≥ 128 MB, or the oldest event in active is ≥ 10 min old, or shutdown. The 10-minute age limit
bounds how much the WAL must replay on restart, without creating thousands of tiny files.

### 4.3 Read path: one query

```
query(Q):
  v      = versions.pin()                                  # Arc<Version>: fixed file set + frozen memtable  (R-7)
  scope  = (Q.project, Q.time_range)
  files  = metastore-cached list in v, pruned by project and start-time range
  fresh  = ingest.fresh_scan(scope)                         # active + frozen rows in scope, as Arrow (no payloads)
  rows   = scan(files, columns = core only, predicate = Q pushed down where safe) ∪ fresh
  single = rows where not needs_merge                       # one version: already the merged run
  multi  = merge_by_run_id(rows where needs_merge)          # M1 merge rule, field by field, by last_seq
  runs   = single ∪ multi
  runs   = apply(Q.filter)                                  # conditions on the run itself
  if Q.trace_filter: runs = runs ⋉ roots(matching trace_filter) on trace_id      # condition on its root
  if Q.tree_filter:  runs = runs ⋉ traces(any run matching tree_filter) on trace_id  # any run in its trace
  if Q.is_root:      runs = runs where parent_run_id is null
  page   = order by (start_time desc, run_id) after cursor, limit + 1
  return page (headers only; payloads on demand)
```

- **Why the split.** Filtering *before* merging is wrong for a run whose late update changed the filtered field (it was
  `running` in a file, `error` in the memtable). Merging *everything* is slow. So rows that might have another version
  carry `needs_merge = true`, and only those are grouped. In the scenario that's about 10% of runs (late finishers).
- **What sets `needs_merge`**: a version started by an update rather than a start (the run's start is elsewhere), or
  the flusher finding the run id in an older file's bloom filter (a run started in one file and updated in the next).
  When the writer creates such a version in the memtable, it adds the run id to the in-memory *multi-version set*,
  and the query treats **every** row whose run id is in that set as `needs_merge`, including the older file row that
  was written as single-version.
- **Pushdown** into the file scan is `Q.filter OR needs_merge OR run_id ∈ multi-version set`. Single-version rows are
  filtered in the scan; rows that may have other versions always come through to the merge, so no version is dropped
  before its run is complete. Only identity predicates (project, time, trace id) are pushed down unconditionally.
- **Time pruning uses `start_key`**, the run's own start time taken from the last segment of its `dotted_order`.
  Every version has it, even a late update with no `start_time`, so time pruning never drops a late version.
- **Tree filters** are semi-joins on `trace_id` (master research R10). `tree_filter` matches *other* runs in the trace,
  as the SDK documents it; M2 includes the run itself too, which matches the spec's "any run in the same trace".

### 4.4 Trace and single-run lookups

```
trace(T):  files with min_trace ≤ T ≤ max_trace (UUIDv7 ids are time-ordered) and bloom(trace_id) hit  (R-9)
           ∪ fresh rows of T → merge multi-version → sort by dotted_order
run(R, include): same with bloom(run_id); payload columns read only from the payload file row the core row points to
```

The SDK makes run and trace ids as UUIDv7 from `start_time` (SDK 0.14.17 `run_trees.py`), so per-file min/max trace id
already narrows a lookup to a few files; the bloom filter handles the rest and older SDKs with random ids.

### 4.5 Live views: re-query the traces that changed

```
tail(Q):                                   # lives in the query service now (M2-FR-26)
  rx = ingest.subscribe(); send Hello{server_seq}
  every 100 ms (first change after a quiet spell: at once):
    T = trace ids changed since the last tick        # from Change{trace_id, run_id, last_seq}
    if |T| > 5,000 or rx lagged → send Resync
    now = query(Q with extra condition trace_id ∈ T)  # same planner, same semantics
    send Runs(now)                                    # runs that match, newest version
    send Left(previously sent ∩ T − now)              # runs that stopped matching (spec Story 4, scenario 2)
```

- **One semantics.** A live view is the same query restricted to the traces that changed, so tree conditions ("root
  ok, some child errored") work live without a second evaluator.
- **Cheap.** Changed traces are almost always in the memtable; their files, if any, are found by the bloom filter.
- **Bounded.** Per tab: at most one small query per 100 ms tick; remembers the last 5,000 sent run ids for `Left`.
- **No repeats.** Without trace or tree conditions, a run unchanged since `Hello` matched when the tab loaded, so it
  isn't resent; `Left` also covers runs the tab loaded itself (changed runs that existed at `Hello`). Filters
  relative to now ("started over 5 min ago") are re-checked only when the trace changes.

### 4.6 Start and restart

```
open:
  metastore.open(); read flushed_seq S and the segment list
  delete orphan files (in storage, not in the catalog)
  build multi-version set = run ids of rows with needs_merge = true (one column read per file, cached)
  replay WAL records with seq > S into the active memtable; rebuild dedup from records inside the window
  open HTTP
```

At 2M runs this reads the catalog, one boolean + id column per file, and at most ~10 minutes / 1 GB of WAL: well
inside the 10 s target (SC-06). An M1 data folder has no metastore: it is created with `flushed_seq = 0`, the WAL replays
in full, and the first flush moves it to files (M2-FR-20).

## 5. Data structures and formats

Details of record: [data-model.md](data-model.md). In short:

**Core file** (`seg-<ulid>.core.parquet`, one per project per flush): one row per run version; small columns only.
Sorted by `(trace_id, dotted_order)`; row groups ~64k rows; page indexes on; bloom filters on `trace_id` and `run_id`.

| Column group | Columns |
|---|---|
| identity | `run_id`, `trace_id`, `parent_run_id`, `dotted_order`, `start_key`, `depth`, `is_root`, `project`, `address` |
| kind/timing | `name`, `run_type`, `start_time`, `end_time`, `status`, `latency_ms`, `error` (first 1 KB) |
| grouping | `tags` (list), `thread_id`, `model`, `input_tokens`, `output_tokens`, `total_tokens` |
| promoted JSON | `attrs_str: map<path, string>`, `attrs_num: map<path, f64>`, `attrs_bool: map<path, bool>` |
| bookkeeping | `first_seq`, `last_seq`, `event_count`, `needs_merge`, `present` (bitmask of fields this version set), `payload_row` |

**Payload file** (`seg-<ulid>.payload.parquet`): `run_id`, `inputs`, `outputs`, `events`, `extra`, `metadata` (JSON
text, zstd). Read only for the step panel and for filters on non-promoted paths.

**Promoted JSON.** The flusher copies scalar leaves into the typed `attrs_*` maps so filters never read payloads:
all of `metadata.*` (depth ≤ 4), and `inputs.*` / `outputs.*` (depth ≤ 3, ≤ 64 leaves per field, strings ≤ 256 bytes).
`outputs.status_code = 429` and `metadata.prompt_version = "v7"` are both promoted. A filter on a path that wasn't
promoted (deeper, longer, an array element) still works by reading payload rows of the candidates, with a
"slow: not indexed" warning.

**Present bitmask.** Parquet can't tell "absent" from "null", and the merge rule needs it ("absent never overwrites").
Because M1 also treats null as absent, the bitmask is only needed for fields where an empty value is meaningful
(`tags: []`, `metadata: {}`); it records which fields a version set.

**Metastore** (same SQL on SQLite and Postgres): `segments`, `field_stats`, `state(flushed_seq, schema_version)`.

**Version (in memory)**: `Arc<Version { id, segments: Vec<SegmentMeta>, frozen: Option<Arc<Frozen>> }>`. Queries pin
one for their whole run. A segment's files may be deleted only when no pinned version references it. M2 never deletes
registered files; the mechanism exists for M4 compaction and is tested with a test-only swap (R-7).

**Multi-version set**: `HashSet<RunId>` of runs with `needs_merge` anywhere; ~16 bytes per entry (~3 MB at 2M runs with
10% late finishers).

**Field stats**: per segment, per path: `type`, `count`, top 50 values with counts, `other_count`. The memtable side
is computed on demand. A project keeps at most 10,000 paths per segment; past that the browser says "incomplete".

## 6. Query language and the panel

Details of record: [contracts/filter-language.md](contracts/filter-language.md).

- **Text form = LangSmith's `filter` grammar** (still documented in SDK 0.14.17 `Client.list_runs`): `eq`, `neq`, `gt`,
  `gte`, `lt`, `lte`, `has`, `and`, `or`, `not`, with `null` for existence (`neq(error, null)`), plus the three slots
  `filter`, `trace_filter`, `tree_filter` and `is_root`.
- **tracedb additions** (marked as such in the contract): dotted JSON paths (`metadata.prompt_version`,
  `outputs.status_code`), `in(field, [..])`, and relative time (`gt(start_time, "-12h")`).
- **One AST.** Panel ⇄ AST ⇄ text. The panel is a view of the AST; printing is canonical, so panel → text → panel is
  lossless. Text the panel can't show as chips (deep nesting) is kept and shown as one "expression" chip.
- **Errors** carry a byte range and an expected-token message: `unknown field "staus" (did you mean "status"?)`.

UI layout (M2):

```
┌ Query ───────────────────────────────────────────────────────────────────────────────┐
│ Project [eval-agent ▾]  Time [last 12h ▾]  ● live                     [ Text ⇄ Panel ] │
│ This run    [tag = batch=2026-11-05-a ×] [root only ×] [+]                             │
│ Its root    [+]                                                                        │
│ Any run in trace  [status = error ×] [+]                                               │
│ Order [start time ▾ newest]   Limit [50]                                               │
├ Fields ──────────────┬ Samples | Table ─────────────────────────────────── 23 results ┤
│ status      3 values │ ● ok  research_task  02:14  41 s  1,840 tok  batch=…-a  v7 ▸    │
│ name       12 values │ ● ok  research_task  02:09  38 s  1,512 tok  batch=…-a  v6 ▸    │
│ tags        4 values │ …                                                               │
│ metadata.            │                                                                 │
│  prompt_version      │                                                                 │
│   v6 (928) v7 (912)  │                                                                 │
│ outputs.status_code  │                                                                 │
└──────────────────────┴─────────────────────────────────────────────────────────────────┘
```

Clicking any value (a field-browser value, a tag in a row, a status badge, a table cell) adds a chip to "This run";
Alt-click adds its negation. Links encode the query as readable URL parameters (`?project=…&time=12h&filter=…&tree=…`).
M1 links (`?project=&status=`) are translated on load.

## 7. Cloud mode

Same binary, different configuration (M2-FR-19, constitution V):

| Setting | Local (default) | Cloud |
|---|---|---|
| `--storage` | `file://<data>/data` | `s3://bucket/prefix` (S3, MinIO, R2 …) |
| `--metastore` | `sqlite://<data>/meta.sqlite` | `postgres://…` (credentials from the environment, never logged) |
| WAL | `<data>/wal` | `<data>/wal` (local disk, by design) |

Files are written once with a unique ULID name and never renamed or overwritten, so object-storage semantics hold
in both modes. Automated checks run the demo checks and the crash test against MinIO + Postgres containers in Linux CI.

## 8. Failure handling, limits and open questions

| Situation | Behaviour |
|---|---|
| Disk or object store full during flush | flush fails, frozen memtable is kept, retried with backoff; uploads get `429` once active is also full; nothing confirmed is lost |
| Metastore unavailable | same as above; queries keep using the last pinned version |
| Unflushed WAL ≥ 1 GB | `429` until a flush succeeds |
| Orphan files after a crash | deleted on start (they were never registered) |
| Corrupt core file (checksum) | the file is skipped by queries and reported in `/healthz`; its data is not in the WAL any more, so this is real loss — flagged loudly (rare; same risk as any database file) |
| Query over a path with mixed types | compares only matching types; response carries a warning with counts |
| Field catalog past 10,000 paths | stats stop growing for that segment; the browser shows "incomplete" |

**Open design questions (for your review):**
1. **Identity conflicts for runs already on disk.** The writer checks identity only against memtables. A late update
   that changes a flushed run's trace id is accepted; reads keep the *first* version's identity and attach an
   `identity_conflict` warning to the run. Checking against disk would put file reads in the write path.
   *Proposed: accept this in M2; revisit with compaction (M4), which sees all versions.*
2. **Dedup window shrinks from 24 h to 10 min.** At 2M+ runs a 24 h key set is too big to keep in memory, and SDK
   retries happen within seconds. WAL files are kept at least as long as the window so a restart rebuilds it.
   *Proposed: 10 min, configurable.*
3. **UI build leaves `cargo build`** (M2-FR-25, reverses M1 decision §11.3). CI builds the UI first and the Rust build
   embeds `ui/dist` if present, else a one-page "UI not built — run `npm run build` in ui/" placeholder. Release
   builds always build the UI first. *Proposed as is.*
4. **Memtable limit 128 MB, flush age 10 min.** Bigger means fewer files (no compaction until M4) but longer replays.
   *Proposed as is; revisit after measuring file counts at 2M runs.*
5. **`field:value` short form (R-2).** Not in M2's exit check; add only if LangSmith's UI docs confirm it (can't be
   checked offline here). *Proposed: leave out of M2.*

## 9. How M2 is proven (tests)

| What | Test | Gate |
|---|---|---|
| Scenario numbers (14, 23, `prompt_version`) | `tracedb demo` checks | CI, local + cloud |
| Same results as run-by-run checking | property test: random runs, random late updates, random flush points → `query` equals a naive filter over fully merged runs | CI |
| Crash safety incl. flush | crash harness extended: kills during upload, flush write, between write and commit, and during trim; 10 rounds | CI |
| Restart ≤ 10 s at 2M runs | restart test (ignored by default, run in CI release job) | CI |
| No pause during flush | latency test: p99 upload latency during a flush ≤ 1 s, ≥ 5k events/s | CI release job |
| Pinned versions | test-only swap during a long scan returns the same rows | CI |
| Bloom filters help | bench: files opened per trace lookup with vs without | reported |
| Filter language | parser/printer round-trip property test; error-position golden tests | CI |
| Live views | e2e: tree-condition live view gets a root when a child errors; `Left` when a run stops matching | CI |
| Retry of a rejected item | ingest test: conflict → rejected; identical retry → rejected again | CI |
| UI | component tests (panel ⇄ text, chips, field browser, table, click-to-filter, M1 link translation) | CI |
| Build without Node | CI job builds the server with no Node installed | CI |

## 10. SmithDB mapping and what M2 deliberately leaves out

| SmithDB | M2 | Why |
|---|---|---|
| WAL + memtable on the ingestion node | WAL + two memtables | same (R-10 adds the frozen one, as RocksDB does) |
| Immutable columnar segments on object storage | Parquet core + payload files on `object_store` | Vortex → Parquet: reason (b), behind `SegmentFormat` |
| Postgres metastore | SQLite locally, Postgres in cloud mode | reason (a), behind `Metastore` |
| Merge-on-read across segments | yes, with a single-version fast path | — |
| Query fresh data on the ingestion node | `fresh_scan` of active + frozen memtables | one process: reason (a) |
| Late materialization of payloads | payload file read only on demand | — |
| Pruning by metastore bounds + file statistics | project/time from metastore; row-group stats; bloom filters on ids (R-9) | — |
| Pinned readers during swaps | `Arc<Version>` (R-7) | RocksDB idea; needed before M4 compaction |
| Compaction | **none yet** | M4 |
| Inverted index / text search | **none yet** | M3 (scan + bloom), custom index in backlog |
| Progressive querying | **none yet** | M3 |
| SSD cache | in-memory `Cache` (footers, blooms, hot row groups) | reason (a) |
