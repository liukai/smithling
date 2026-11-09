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
- [ ] T014 Single-run payload read from the row's source file (`query/src/scan.rs`) — *answers already match after compaction (T015); kept open to measure in U5's bench and change only if it's slower*
- [X] T015 Test (`query/tests/compaction.rs`): lists, trace, run + payload, search, field counts, aggregates equal before and after a forced compaction; and after a late update to a compacted run
- [X] T016 Test: every run with versions in several files has a marked row once all are flushed (M4-FR-04) — *covered by T015: unmarked rows would change the field counts and aggregates it compares*

## U4 Background, limits, deletes, crashes

- [ ] T017 Timer in the writer; skip while write slowdown is on or a flush is queued; one job at a time; config flags (`crates/smithling/src/main.rs`, `IngestConfig`)
- [ ] T018 Delete loop: `replaced_at + grace ≤ now` and no pinned `Version` holds the file; files first, then the row
- [ ] T019 Crash points `compact_after_put`, `compact_after_commit`, `compact_mid_delete`; harness rounds
- [ ] T020 Test: a query held across 100 swaps returns the same answer; no missing-file error

## U5 Surface

- [ ] T021 `POST /api/compact` (`force`), `smithling compact [--url]`
- [ ] T022 `/healthz`: `files`, `compaction`; UI storage line (`ui/src`) with a component test
- [ ] T023 `smithling demo --compact`; bench reports files before/after and single-run P50

## U6 CI and docs

- [ ] T024 Cloud CI job runs `demo --compact` and the swap crash points (`.github/workflows/ci.yml`, `ci/cloud.yml`)
- [ ] T025 Roadmap STATE/Progress, CHANGELOG, STARTER_GUIDE M4 section, contracts in USAGE.md
