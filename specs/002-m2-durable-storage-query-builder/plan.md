# Implementation Plan: M2 — Durable storage + query builder

**Branch**: `002-m2-durable-storage-query-builder` | **Date**: 2026-11-06 | **Spec**: [spec.md](spec.md) | **Design**: [design.md](design.md) | **Appendix A**: [starter guide](starter-guide.md) | **Status**: Built (M2 closed at `c7595c6`)

**Input**: Feature specification from `specs/002-m2-durable-storage-query-builder/spec.md`. Master: [000 plan](../000-smithling-master-plan/plan.md). Builds on: [M1 plan](../001-m1-see-agent-trace-live/plan.md).

## Summary

M2 puts SmithDB's storage half under M1's write path. The writer stays as it is; a full memtable is frozen and a
flusher writes it as immutable Parquet files (small filter columns and large payloads apart) to an `ObjectStore`,
registers them in a SQLite/Postgres metastore, and only then trims the WAL. Queries pin a file set, prune by project,
time and key bloom filters, union the fresh memtables, merge the few multi-version runs, and evaluate LangSmith-style
filters, including conditions on a run's root and on any run in its trace, through DataFusion (as built, DataFusion
prunes and pre-filters; merge, filter and trace conditions run in our code, [D-017](../000-smithling-master-plan/decisions.md#d-017)). The UI gets a
Scuba-style query panel that is the same AST as the filter text, a field browser fed by flush-time stats, Samples and
Table views, and live views for any query. `smithling demo` asserts the scenario numbers on local and cloud-style storage.
Design: [design.md](design.md). How it all flows, as pseudo-code: [Appendix A](starter-guide.md).

## End-state map: where you see each requirement

"Where you see it" is the screen, command or check to look at; "What you should see" is the pass condition. Steps
refer to [quickstart.md](quickstart.md).

### Functional (what you can do)

| Req | As a…, I can… | Where you see it | What you should see | Automated check |
|---|---|---|---|---|
| M2-FR-01 | As an agent developer, I can filter by time, tags, status, latency, type, name, model with AND/OR/NOT | Browser → query panel (step 2.1) | 14 failing roots, by clicking | `demo` check; planner tests |
| M2-FR-02 | As an agent developer, I can filter on any metadata, input or output path | Field browser → `metadata.prompt_version`; Table → `outputs.status_code` (step 2.3) | only runs with both values | `demo` check; `not_indexed` fallback test |
| M2-FR-03 | As an agent developer, I can put conditions on the run, its root and any run in its trace | *Any run in trace* row (step 2.2) | 23 quiet failures | `demo` check; semi-join tests |
| M2-FR-04 | As an agent developer, I can page newest first with no gaps or repeats while data arrives | scroll the 23 with page size 10 while `seed` runs | 23 distinct rows, newest first | cursor property test |
| M2-FR-05 | As an agent developer, I get told exactly where my filter is wrong | *Text* → `staus` (step 2.4) | underline + "did you mean status" | parser golden tests |
| M2-FR-06 | As a script or agent, I can use LangSmith filter text unchanged | `smithling query 'and(…)'` | same rows as the panel | grammar tests from SDK docstring examples |
| M2-FR-07 | As an agent developer, I can switch panel ⇄ text and keep the same query | *Text ⇄ Panel* toggle | identical chips after a round trip | UI round-trip test; print/parse property test |
| M2-FR-08 | As an agent developer, I can click any value to filter, and negate/disable/remove chips | rows, badges, table cells, field browser | chip added; results update | UI component tests |
| M2-FR-09 | As an agent developer, I can see every field with types and top values | field browser | `v6 (928)`, `v7 (912)` | `demo` check; field-stats tests |
| M2-FR-10 | As an agent developer, I can view results as Samples or a Table with my columns | *Samples / Table* tabs | columns persist in the link | UI tests |
| M2-FR-11 | As an agent developer, I can share any view; old links still work | copy URL (step 2.5); an M1 `?project=&status=` link | same query and view | route tests |
| M2-FR-12 | As an agent developer, I can keep any query live | *● live* on the quiet-failures query (step 5) | roots appear when a child errors; *no longer matching* marks | e2e live tree-condition test |
| M2-FR-13 | As an agent developer, I can keep uploading while data moves to disk | `/healthz` shows `frozen: true` during a flush | uploads keep succeeding | throughput + latency test |
| M2-FR-14 | As an agent developer, I can trust the log is trimmed only after data is safe | crash harness (step 3) | 0 missing, 0 duplicated | crash harness with flush kill points |
| M2-FR-15 | As an agent developer, I see one merged run even when its pieces are on disk and in memory | trace view of a late-finishing step | final status, no duplicate rows | merge-on-read property test |
| M2-FR-16 | As an agent developer, my long query never loses files mid-scan | — | — | pinned-version swap test |
| M2-FR-17 | As an agent developer, opening a trace stays fast as files pile up | `stats.files_scanned` in the response | 1–2 files per trace | bloom bench |
| M2-FR-18 | As an agent developer, bursts slow my agent down instead of losing traces | `slowdown` test (step 4) | 429s, nothing lost, memory under cap | slowdown test |
| M2-FR-19 | As a maintainer, I can switch to object storage + hosted DB by config | step 7 | same demo table | cloud CI job |
| M2-FR-20 | As an M1 user, I can open my old data folder | start M2 on an M1 folder | same runs, then files after the first flush | migration test with an M1 fixture folder |
| M2-FR-21 | As a test user, I can reproduce the scenario numbers | `smithling demo` (step 1) | all checks ✓, exit 0 | CI |
| M2-FR-22 | As a maintainer, I can trust the checks run on both storages | CI | local + cloud jobs green | CI workflow |
| M2-FR-23 | As an SDK, I get the same answer when I retry a rejected item | ingest test | rejected twice with the same reason | writer test |
| M2-FR-24 | As a maintainer, I can work on the UI without rebuilding the server | `serve --ui-dir ui/dist` (step 8) | edited UI after a reload | route test |
| M2-FR-25 | As a maintainer, I can build the server without Node | `cargo build` on a Node-less runner | builds; placeholder page if no `ui/dist` | CI job without Node |
| M2-FR-26 | As a maintainer, I can add a live view without touching the web layer | `query::tail` | `routes/tail.rs` only frames SSE | code review + e2e |

### Non-functional (how well it does it)

| Req | As a…, I can… | Where you see it | What you should see | Automated check |
|---|---|---|---|---|
| SC-03 | As an agent developer, queries feel interactive at 2M runs | `stats.ms` in responses; bench | filter p50 ≤ 500 ms; trace/run p50 ≤ 200 ms; suggestions p50 ≤ 500 ms | M2 bench (release CI) |
| SC-04 | As an agent developer, smithling stays light | `/healthz`; `du -sh .smithling` | memory under limit; disk ≤ raw JSON | bench report |
| SC-05 | As an agent developer, nothing confirmed is lost | crash harness | 10/10 rounds, incl. flush kills | CI |
| SC-06 | As an agent developer, restarts are quick | restart test | ≤ 10 s at 2M runs | release CI |
| SC-07 | As an agent developer, uploads don't stall during flushes | throughput test | ≥ 5k events/s; p99 ≤ 1 s (M3 raised the test to 20k events/s while querying) | release CI |
| SC-08 / QR-007 | As a maintainer, cloud gives the same answers | cloud CI job | identical demo table | CI |
| SC-09 | As an agent developer, results are exactly right | merge-on-read property test | 100% agreement | CI |
| SC-10 | As an M1 user, nothing I had got worse | M1 test suite | all green | CI |

## Technical Context

**Language/Version**: Rust 1.99 (edition 2024). UI in TypeScript (React + Vite), built as its own CI step and embedded.

**Primary Dependencies**: M1's (tokio, axum, serde, uuid, crc32fast, clap, rust-embed, tower-http, reqwest for
seed/demo) plus `datafusion` 55.2 (→ `arrow`/`parquet` 59.3, `object_store` 0.13.2, research M2-R1), `sqlx` 0.9
(sqlite + postgres), `ulid` 3, `moka` 0.12. UI adds no new runtime library (chips, panel, table are ours; TanStack
Virtual stays).

**Storage**: WAL on local disk (as M1); Parquet core + payload files on `object_store` (local FS or S3); metastore in
SQLite (`<data>/meta.sqlite`) or Postgres.

**Testing**: `cargo test` (unit, proptest: merge-on-read, filter print/parse, cursor); crash harness with flush kill
points; SDK contract fixtures (unchanged); `smithling demo` checks; M2 bench; cloud CI job with MinIO + Postgres
containers; UI component tests (vitest).

**Target Platform**: macOS and Linux laptops; CI on Linux.

**Project Type**: single binary = web service + embedded SPA + CLI (as M1).

**Performance Goals**: filter p50 ≤ 500 ms, trace/run p50 ≤ 200 ms, suggestions p50 ≤ 500 ms at 2M runs; ≥ 5k
events/s with flushes; restart ≤ 10 s at 2M runs; visible ≤ 1 s (M1).

**Constraints**: zero outbound in local mode; memory bounded by 2 × memtable limit + indexes + cache; nothing
confirmed lost or duplicated; object-storage semantics (write once, never rename).

**Scale/Scope**: 7 user stories (3 P1); 1 new crate (`storage`); ~12 PRs of ≤ 400 lines.

No NEEDS CLARIFICATION remain. Five design questions with proposals are in design §8 for review.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.* Constitution v1.1.0.

| Principle | M2 check | Pre | Post |
|---|---|---|---|
| I. Rust, one binary | still one binary; SQLite in-process; UI embedded in release builds (dev builds may serve a placeholder or `--ui-dir`) | PASS | PASS |
| II. SmithDB is the reference | immutable segments, metastore, merge-on-read, fresh data from memtables, late materialization, pruning (design §10). Differences: Parquet (b), SQLite (a), one process (a), memory cache (a; not built in M2, [D-013](../000-smithling-master-plan/decisions.md#d-013); M3, [D-028](../000-smithling-master-plan/decisions.md#d-028)), no compaction/progressive/text index yet (c, staged to M3/M4 in the roadmap). All behind `SegmentFormat`, `Metastore`, `Cache`, service traits | PASS | PASS |
| III. Local and private | local mode makes no outbound connections; data under `--data` (WAL, files, metastore); the cloud job talks only to its own containers | PASS | PASS |
| IV. UI first | query panel, field browser, Samples/Table, live views ship with the API; CLI `query`/`demo` are equivalents | PASS | PASS |
| V. Cloud-portable storage | all file and catalog access via `ObjectStore` + `Metastore`; write-once objects; same tests on MinIO + Postgres | PASS | PASS |
| VI. Done is measured | `smithling demo` asserts 14/23/top values; bench reports P50/P99; crash test extended to flush kill points, in CI | PASS | PASS |

Architecture constraint "query execution builds on DataFusion and Arrow": met from M2 on (M1 had no DataFusion).

## Project Structure

### Documentation (this feature)

```text
specs/002-m2-durable-storage-query-builder/
├── spec.md            # what and why
├── design.md          # high-level design + components (reviewed before implementation; notes where the code differs)
├── plan.md            # this file
├── starter-guide.md   # Appendix A: reading the built M2 code (line links as of 6e77f12)
├── parquet-datafusion-primer.md  # how Parquet + DataFusion work inside, with real EXPLAIN output
├── research.md        # Phase 0 decisions (M2-R1…R14)
├── data-model.md      # file rows, metastore tables, query AST, lifecycle states
├── quickstart.md      # how to prove M2 works
├── contracts/         # http-api.md, filter-language.md, cli.md
└── tasks.md           # task list (all done)
```

### Source Code (repository root)

Planned layout (as reviewed):

```text
crates/
├── model/          # + filter/{ast,parse,print}.rs, merge_versions
├── storage/        # NEW: object.rs (ObjectStore: local, s3), metastore.rs (sqlite, postgres + migrations),
│                   #      segment.rs (Parquet core/payload writer+reader, blooms), cache.rs, version.rs
├── ingest/         # + memtable.rs (active/frozen), flush.rs (+ stats, promote), recover.rs, mvset.rs;
│                   #   writer.rs: identity check before WAL, freeze, 429; wal.rs: replay_from, trim
├── query/          # + plan.rs (AST → DataFusion), provider.rs (Version + fresh TableProvider),
│                   #   merge.rs (merge_versions UDAF), fields.rs, tail.rs (moved from routes)
└── smithling/        # routes: api.rs (+ /api/query, /api/fields), tail.rs (SSE framing only), ui.rs (--ui-dir);
                    # demo.rs + scenario.rs (overnight-eval generator); main.rs (storage flags, demo, query)
```

As built (M2 at `c7595c6`; paths as after the crate split, which moved `crates/smithling` into three crates):

```text
crates/
├── model/          # + filter/{ast,parse,print,eval}.rs; merge_versions in merge.rs; promote in attrs.rs
├── storage/        # NEW: object.rs, metastore.rs (DDL inline, no migrations folder), segment.rs (Parquet, blooms,
│                   #      field StatsBuilder); no cache.rs (D-013), no version.rs (D-015)
├── ingest/         # + memtable.rs (Mem, Version), flush.rs, recover.rs, projects.rs (D-024), dedup.rs;
│                   #   multi-version set in lib.rs (Ingest::multi_version); writer.rs, wal.rs
├── query/          # + scan.rs (pin, read_core, versions_of, merge_group), pushdown.rs, window.rs (D-021),
│                   #   cursor.rs, fields.rs, tail.rs; select / trace / run in lib.rs (D-017: no plan/provider/UDAF)
├── server/         # smithling-server: lib.rs, routes/{api,ingest,tail,ui,errors}.rs, langsmith_map.rs, build.rs
├── tools/          # smithling-tools: seed.rs, scenario.rs, demo.rs, cli_query.rs (M3 adds bench.rs)
└── smithling/        # the binary: main.rs; tests that start it (crash, restart, throughput, demo, …)
```

M3 adds `storage/src/{cache,fts}.rs`, `ingest/src/{flushed,feedback,backfill}.rs`, `query/src/{search.rs,aggregate/}`;
see the [M3 plan](../003-m3-search-aggregates-speed/plan.md).

**Structure Decision**: one new crate, `storage`, holding every trait that touches files or SQL, so `ingest` and
`query` never see Parquet, paths or SQL. The filter language lives in `model` (pure, shared by query, CLI and tests).
The live-view loop moves into `query` (M1 review). The UI parser mirrors the Rust one; a shared golden-file suite keeps
them in step.

## Delivery order (PR-sized; each ends in something checkable)

| # | PR | As a…, I can… after it merges |
|---|---|---|
| 1 | `model::filter`: AST, parser with positions, canonical printer; `merge_versions` | As a maintainer, I can run the grammar golden tests (incl. SDK docstring examples) and the print/parse property test |
| 2 | M1 follow-ups: identity check before WAL; `build.rs` embeds `ui/dist` or a placeholder; `--ui-dir`; CI UI job | As an SDK, I get the same rejection on retry; as a maintainer, I build without Node |
| 3 | `storage`: `ObjectStore` (local, S3), `Metastore` (SQLite, Postgres, migrations), `Cache` | As a maintainer, I can run the storage trait tests on local and on MinIO + Postgres |
| 4 | `storage::segment`: Parquet core/payload writer and reader, promoted attrs, blooms, page index | As a maintainer, I can round-trip run versions through files and check blooms |
| 5 | `ingest`: active/frozen memtables, flusher (write → register → trim), field stats, `429` slowdown | As an agent developer, I can upload past the old memory limit; `/healthz` shows segments |
| 6 | `ingest::recover` + crash harness kill points in flush; M1-folder migration | As a maintainer, I can run 10/10 crash rounds with flush kills and restart ≤ 10 s |
| 7 | `query`: provider over Version + fresh, prune, split, `merge_versions`, filters, cursor; `/api/query`; trace/run lookups over files | As an agent developer, I can `curl /api/query` and get 14 and 23; merge-on-read property test green |
| 8 | Tree filters (semi-joins), field catalog endpoints, `smithling query` | As an agent developer, I can find quiet failures and list fields from the API and CLI |
| 9 | `query::tail` (re-query changed traces, `left`), SSE framing only in routes | As an agent developer, I can keep any query live |
| 10 | UI: query panel ⇄ text, chips, field browser, click-to-filter, Samples/Table, URL state, M1 link translation | As an agent developer, I can triage the scenario by clicking |
| 11 | `smithling demo` + scenario generator; cloud CI job; M2 bench; quickstart verified | As a test user, I can run `smithling demo`; as a maintainer, CI runs it on both storages |
| 12 | **Code starter guide**: rewrite [starter-guide.md](starter-guide.md) from pre-code to a guide for reading the built M2 code, as M1's: reading order, every scene mapped to real functions with links and line numbers, vertical diagrams, rough edges found while writing it | As a new reader, I can follow one run through the M2 code (flush, recovery, merge-on-read, planner, live views, panel ⇄ text) with line-level links |

As built, PR 3 has no `Cache` ([D-013](../000-smithling-master-plan/decisions.md#d-013)), PR 7 no provider or UDAF and PR 8 no semi-joins ([D-017](../000-smithling-master-plan/decisions.md#d-017)).

PRs 1–2 and 3–4 can proceed in parallel; 5 needs 3–4; 7 needs 1 and 5; 10 needs 7–8 (it can start against a stub API). 12 is last: it reads the merged code, so M2 isn't done until it lands.

## Complexity Tracking

No violations. Two choices add moving parts and are justified in research:

| Choice | Why needed | Simpler alternative rejected because |
|---|---|---|
| Single-version fast path (`needs_merge` + multi-version set) | merge-on-read at 2M runs within SC-03 | always merging every row in range costs a group-by over all runs per query (M2-R3) |
| Promoted typed attribute maps | JSON-path filters without reading payloads | a JSON text column means reading and parsing large strings for every filter (M2-R6). Superseded: a JSON text column `attrs` of promoted leaves shipped ([D-012](../000-smithling-master-plan/decisions.md#d-012)) |

## Appendix A: starter guide

The user-requested walkthrough lives in [starter-guide.md](starter-guide.md). It was pre-code (planned names,
pseudo-flows) until the last PR (12) rewrote it in place into the guide for reading the built M2 code, with real links
and line numbers as of `6e77f12`, as M1's. The plan for it covered:

1. **Goals → functionality → planned home**: every end-state line mapped to the mechanism and module that delivers it.
2. **The system in one picture** and the crate dependency graph.
3. **Reading order** once the code exists, with the critical files marked.
4. **One run, upload to the 23 quiet failures**: a sequence diagram plus ten scenes with pseudo-flows: the write with
   its new identity check; freezing without pausing; the flush (write → register → trim) and why each crash point is
   safe; a late update meeting a flushed run; the query (prune → scan → split → merge → filter → semi-join → page);
   trace lookup with blooms; live views; the field browser; restart; and panel ⇄ text.
5. **Status codes**, **things to try**, and **where M2 is most likely to be wrong**.
