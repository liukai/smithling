# Tasks: M4 — Compaction (core)

From [plan.md](plan.md). Tests first in each unit. `[X]` when done; one local commit per unit.

## U1 Catalog and start-up

- [X] T001 Migration: `segments.replaced_at`, `state = 'replaced'`, next `schema_version` (`storage/src/metastore.rs`); test on SQLite
- [X] T002 `Metastore::commit_compaction(inputs, outputs, stats)`: one transaction; rolls back unless every input was `active`; test: concurrent flush kept, changed input refused
- [X] T003 `Metastore::replaced_segments()` and `delete_segment(id)` (rows after files)
- [X] T004 Start-up: `replaced` rows count as registered; unregistered `cmp-*` files deleted without the WAL check (`ingest/src/recover.rs`); tests for a crash before and after the swap

## U2 Job core

- [X] T005 `ingest/src/compact.rs`: `pick(version, now, cfg) -> Option<Inputs>`: flush groups in seq order, closed windows only, stop at damaged files and the size cap; unit tests
- [X] T006 Read inputs (core + payload rows) into versions; `segment` helper if missing
- [X] T007 Merge by `run_id` with `merge_versions`; window = start hour; group by (merged project, window)
- [X] T008 Mark `needs_merge` from existing L1 files of the same window (their run ids)
- [X] T009 Write outputs with `tier = 1`, names `cmp-<ulid>`, split at the target size
- [X] T010 Property test (`ingest/tests/compact.rs`): random starts, late updates, retries, project-less patches; merged rows equal merging every version

## U3 Run a job, keep queries right

- [X] T011 Upload + `commit_compaction` + `Version::with_compaction` (`ingest/src/memtable.rs`)
- [X] T012 Rebuild the multi-version set after a swap (start-up's source: `needs_merge` rows, plus members still in a memtable)
- [X] T013 Query: reset kept rows (`VersionRows`) and per-file subtract-stats when the `Version` lost files (`query/src/fields.rs`)
- [~] T014 Single-run payload read from the row's source file (`query/src/scan.rs`) — *deferred (D-033, K11): answers already match after compaction (T015); measure with the full bench*
- [X] T015 Test (`query/tests/compaction.rs`): lists, trace, run + payload, search, field counts, aggregates equal before and after a forced compaction; and after a late update to a compacted run
- [X] T016 Test: every run with versions in several files has a marked row once all are flushed (M4-FR-04) — *covered by T015: unmarked rows would change the field counts and aggregates it compares*

## U4 Background, deletes, crashes

Only what the done-check and M5's M4-REQ-1–4 need. Deferred items are listed at the end.

- [X] T017 Timer thread in the writer: one job at a time, skipped while write slowdown is on; `--compact on|off` plus the stated defaults as flags (`crates/smithling/src/main.rs`, `IngestConfig`). Clean shutdown (stop flag between jobs, bounded wait). A panic in a job is caught and recorded as `last_error`; the thread keeps going; `/healthz` shows `stopped` if it ever exits. `<storage>/LOCK` when the store is a local path (ADR A7). Logs each step; start-up logs the settings — *skips a tick while a flush is running (`frozen`), which covers write slowdown; `/healthz` adds `panics` (jobs that panicked since start); the hidden `--compact-every` sets the interval for the crash harness*
- [X] T018 Delete loop: `replaced_at + grace ≤ now` and the swap's era token is free (keep `(era, files, replaced_at)` per swap); files first, then the row (M4-FR-07) — *eras are `Weak` tokens, so the bookkeeping never counts as a hold; replaced files are deleted as a prefix in swap order; `Ingest::delete_replaced` runs one pass*.
- [X] T019 Crash points `compact_after_put`, `compact_after_commit`, `compact_mid_delete` in the harness; 10/10 rounds each (SC-05, M4-FR-10) — *`crates/smithling/tests/crash.rs`: 30 rounds, jobs every second with grace 0; local run 30/30, 0 missing, 0 duplicated (cloud run in T023)*
- [X] T020 Test: a query held across 100 swaps returns the same answer; no missing-file error (SC-06) — *`query/tests/pinned.rs`: a real scan pinned before the swaps, deletes tried after each with grace 0*

## U5 Surface and demo

- [ ] T021 `POST /api/compact` (`force`); `/healthz` gains `files` and `compaction`; UI storage line reads them (`ui/src`) with a component test
- [ ] T022 `smithling demo --compact`: forces compaction between steps (through `POST /api/compact`, so it works with `--url`), same checks pass, prints files before/after (SC-01, SC-02); output as in [the M4 demo](plan.md#the-m4-demo-what-youll-see)

## U6 CI and close

- [ ] T023 Cloud CI job runs `demo --compact` and the compaction crash points (`.github/workflows/ci.yml`, `ci/cloud.yml`) (M4-FR-10)
- [ ] T024 Close M4: roadmap STATE/Progress, CHANGELOG, STARTER_GUIDE M4 section, contracts in USAGE.md

## Assumption

- One writer per store across machines until M5's lease (M5-FR-10); locally the `LOCK` files enforce it (ADR A7).

## Deferred (not needed to close M4)

- `smithling compact [--url]` CLI: `POST /api/compact` covers tests and the demo
- Settling a job whose swap gave up without a restart: K12; a restart does it
- Skipping a job while a flush is queued: K10, if flushes are seen waiting on compaction
- Bench rows for files before/after and single-run P50 (SC-03, SC-07); T014's payload lookup: K11, with D3-1 (D-033)
