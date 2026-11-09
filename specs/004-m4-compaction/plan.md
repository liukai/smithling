# Implementation Plan: M4 — Compaction (core)

**Branch**: `004-m4-compaction` | **Date**: 2026-11-08 | **Spec**: [spec.md](spec.md) | **Tasks**: [tasks.md](tasks.md)

One file on purpose: the design, the data and API changes, and the work units are all here.

## Done when

```sh
cargo test --workspace                      # incl. the compaction property test
smithling demo --seed 7 --compact           # every check passes, same counts as without --compact
SMITHLING_TEST_CRASH_AT=<point> …           # crash harness: 10/10 rounds per compaction point, zero loss
smithling bench --smoke                     # reports files before/after compaction (≤ 10%) and single-run P50
```

Plus the cloud CI job (MinIO + Postgres) running the demo with `--compact`.

## Design

### The rule (spec: [How compaction works](spec.md#how-compaction-works-as-decided))

A job takes **a prefix of the log**: flushed (L0) files in log order, up to the first flush that has a row whose
window is still open. It merges each run's versions into one row, and writes **one L1 file per project per window**
(1 h of run start time, split at 256 MB). One catalog commit swaps L0 out and L1 in. L1 files are never rewritten.

### One job, step by step

| Step | What | Reuses |
|---|---|---|
| 1. Pick | From the pinned `Version`: tier-0 segments ordered by `min_seq`, grouped by flush (same seq range). Take groups while every file's newest window (`max_start_key`) has closed (end + settle age ≤ now). Stop at a damaged file or at the job's size cap (rows / bytes); both keep it a prefix | `Version`, `SegmentMeta` |
| 2. Read | Every core + payload row of the inputs | `segment` readers |
| 3. Merge | Group rows by `run_id`; `merge_versions` in seq order (disjoint ranges hold because it's a prefix) | `model::merge_versions` |
| 4. Mark | An output row is `needs_merge` if its run id is in an existing L1 file of the same window (any project; read only those files' run ids, i.e. windows that got late rows) | `segment::run_ids` |
| 5. Write | Group by (project of the merged run, window); `segment::write` per group with `tier = 1`, names `cmp-<ulid>.*` | `segment::write` (core, payload, text index, stats) |
| 6. Upload | `put_new` each file; crash point `compact_after_put` | flush's upload loop |
| 7. Swap | One transaction: `UPDATE segments SET state='replaced', replaced_at=now WHERE id IN (inputs) AND state='active'` must touch every input, else roll back; insert outputs and their field stats; bump nothing else. Crash point `compact_after_commit` | `Metastore` |
| 8. Publish | `Version::with_compaction(removed, added, stats)`; rebuild the multi-version set (below); reset the query's kept rows | `Mem` lock |
| 9. Delete | Later loop: a replaced file whose `replaced_at + grace ≤ now` and that no pinned `Version` still holds is deleted (files first, then its catalog row). Crash point `compact_mid_delete` | in-process pins (R-7) |

A project-less late update (`""` folder, D-024) merged with its run's start in the same job lands in the run's own
project: K5 for free in that case.

### What changes outside the job

| Part | Change | Why |
|---|---|---|
| Multi-version set (writer memory) | After a swap: rebuild as {run ids of `needs_merge` rows in active files} ∪ {old members that still have a version in a memtable}. Same source as start-up | C2: merged runs leave the set, so their kept copies go |
| Query kept rows (`VersionRows`, D-023/D-029) | Reset when the `Version` lost files; rebuilt lazily on the next request (now small) | They assume files only get added (`query/src/fields.rs:86`) |
| Single-run payload lookup (`query/src/scan.rs:920`) | Keep the source segment id on rows read from files, and read the payload from that file | C5: L1 files have wide seq ranges, so the range filter would match many |
| Start-up (`ingest/src/recover.rs`) | `replaced` rows count as registered (deleted by the loop, not as orphans); unregistered `cmp-*` files are deleted without the WAL-coverage check (their data is still in the active inputs) | Otherwise a crash between upload and swap blocks the restart |
| Flush | Nothing. Test only: every run with versions in several files has at least one marked row once all its versions are flushed | M4-FR-04 holds already (the newer version is always marked) |

### Data and API changes

- Catalog: `segments.replaced_at BIGINT NULL`; `state` gains `'replaced'`; `tier` = 1 for compacted files. One
  `schema_version` step, same SQL on SQLite and Postgres.
- `smithling serve … [--compact on|off] [--compact-window 1h] [--settle 15m] [--compact-target 256MB] [--grace 24h]`.
- `POST /api/compact` (`{force: true}` treats every window as closed; tests and demo) and `smithling compact [--url]`.
- `/healthz`: `files: {l0, l1, replaced}`, `compaction: {last_run_at, last_error, backlog_files, running}`. UI: a new
  small storage line (files, compaction backlog, last run) read from `/healthz`, as the feedback orphan count already is (`ui/src/api.ts:250`).
- `smithling demo --compact` forces compaction between steps.

### When jobs run

A timer in the writer (every 60 s) starts a job if there's a closed window's worth of L0 files, write slowdown is
off and no flush is queued. One job at a time. A job's size cap keeps its memory bounded (default 1M rows); the next
job continues the prefix.

### Choices worth naming

| Choice | Rejected | Why |
|---|---|---|
| Prefix of the log across projects | per-project picks | merge order (C8) |
| Full read of inputs into memory, capped per job | streaming k-way merge | simpler; the cap bounds memory |
| Rebuild the multi-version set from files after a swap | track removals one by one | one code path, the start-up one |
| Reset kept rows on removal | patch them in place | simpler; few multi-version runs remain after compaction |
| Grace period + in-process pins | pins only | M5 readers in other processes need the grace period |

## Constitution Check

| Principle | Check | Pre | Post |
|---|---|---|---|
| I. One binary | compaction is a background job in `serve`; no new service | PASS | PASS |
| II. SmithDB | SmithDB compacts (time-tiered); M4 uses one level, recorded as (c) staged, revisit with K6 | PASS | PASS |
| III. Local, private | no outbound; files under the configured storage | PASS | PASS |
| IV. UI first | compaction status in the UI with `/healthz` and the CLI | PASS | PASS |
| V. Cloud-portable | write-once outputs, catalog migration on both backends, cloud CI job runs it | PASS | PASS |
| VI. Measured | demo with `--compact`, property test, crash points, file-count and P50 in the bench | PASS | PASS |

## Work units

Each unit: tests first, then code; `fmt`, `clippy -D warnings`, `cargo test --workspace`; one local commit with a
CHANGELOG line. Details in [tasks.md](tasks.md).

| Unit | What | Done when |
|---|---|---|
| U1 | Catalog: `replaced_at`, swap transaction with the input check, start-up handling of `replaced` and `cmp-*` | metastore tests on SQLite (Postgres in the cloud job): swap refused if an input changed; restart after a crash before/after the swap is clean |
| U2 | Job core: pick, read, merge, mark, write (no I/O side effects beyond returning files) | unit tests for the prefix rule and windows; property test: merged rows equal merging all versions |
| U3 | Run a job: upload, swap, publish, multi-version rebuild, kept-rows reset, payload lookup by source file | end-to-end test: every query type returns the same before and after a forced compaction, incl. late updates after compaction |
| U4 | Timer, limits, config, delete loop, crash points | crash harness 10/10 per point; a query across 100 swaps returns the same answer |
| U5 | Surface: `/api/compact`, `smithling compact`, `/healthz`, UI line, `demo --compact`, bench file count | done-check commands above pass locally |
| U6 | CI and docs: cloud job with compaction, roadmap STATE, starter-guide section | CI config updated; docs say what was built |
