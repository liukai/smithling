# Master Plan: tracedb — architecture and phases

> **MASTER PLAN.** This folder (000) is the tracedb master: requirements of record, architecture and roadmap. It has
> no code tasks. Each phase is its own spec folder (`specs/001-…`, `specs/002-…`) that delivers requirement IDs from
> here. Phase details live in the phase folder; this folder links to it. Progress: [roadmap.md](roadmap.md).

**Branch**: `000-tracedb-master-plan` | **Date**: 2026-11-05 | **Spec**: [spec.md](spec.md)

**Progress and decisions**: [roadmap.md](roadmap.md) (plan of record).

**Input**: Feature specification from `specs/000-tracedb-master-plan/spec.md`. Design input: [design-notes.md](design-notes.md).

## Summary

One Rust binary, `tracedb serve`, that accepts LangSmith SDK traffic unchanged, stores runs SmithDB-style (WAL →
memtable → immutable columnar segments + small metastore → tiered compaction), answers the seven trace workloads through
DataFusion, and serves an embedded browser UI from day one. Three services (ingest, query, compaction) run in one process
behind explicit interfaces. Five storage interfaces (`ObjectStore`, `Metastore`, `SegmentFormat`, `TextIndex`, `Cache`)
each get the simplest v0 implementation, with the SmithDB/cloud version as a drop-in later.

## Technical Context

**Language/Version**: Rust 1.99 (edition 2024). UI in TypeScript, built at compile time and embedded; no runtime Node.

**Primary Dependencies**: tokio, axum, datafusion + arrow + parquet, object_store, sqlx (sqlite; postgres for cloud
tests), serde_json, uuid, ulid, roaring, moka, rust-embed, clap. UI: React + Vite, TanStack Virtual, uPlot (see research R3).

**Storage**: `./.tracedb/`: WAL files, Parquet segments (core + payload split) on `object_store` (0.13 line, design-notes §12) local FS, SQLite metastore.

**Testing**: `cargo test` (unit + integration), contract tests replaying recorded LangSmith SDK traffic, scenario demo
(`tracedb demo`) asserting spec numbers, crash test (kill -9 loop) and `tracedb bench` in CI.

**Target Platform**: macOS and Linux laptops (x86_64, aarch64). Reference: 8+ cores, 16+ GB RAM, SSD.

**Project Type**: single binary = web service + embedded SPA + CLI.

**Performance Goals**: ≥ 20k events/s ingest; visible ≤ 1 s; P50 tree/run ≤ 50 ms, filter ≤ 100 ms, search ≤ 500 ms,
thread ≤ 150 ms; P99 ≤ 5× P50 (spec QR-004/005).

**Constraints**: zero outbound network; localhost bind; idle RSS ≤ 200 MB; disk ≤ 0.5× raw JSON; zero acked-event loss.

**Scale/Scope**: ~2M runs / ~5 GB per laptop; single user; 11 user stories (5 P1), backlog B1 excluded.

All unknowns resolved in [research.md](research.md); no NEEDS CLARIFICATION remain.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.* Constitution v1.1.0.

| Principle | Check | Pre-design | Post-design |
|---|---|---|---|
| I. Rust, one binary | Rust workspace → one `tracedb` binary; UI embedded; SQLite in-process | PASS | PASS |
| II. SmithDB is the reference | Every component mapped (design-notes §2, §2.1); differences listed below with reasons, each behind an interface | PASS | PASS |
| III. Local and private | No outbound calls; localhost default; all state under `--data` dir; UI assets embedded (no CDN) | PASS | PASS |
| IV. UI first | Each phase below ships its UI view; M1 demo is in the browser | PASS | PASS |
| V. Cloud-portable storage | All I/O via `ObjectStore` + `Metastore`; segments immutable; MinIO + Postgres CI job | PASS | PASS |
| VI. Done is measured | `tracedb demo` asserts scenario numbers; `bench` P50/P99; crash test in CI | PASS | PASS |

Recorded differences from SmithDB (Principle II):

| Difference | Allowed reason | Interface |
|---|---|---|
| Parquet instead of Vortex | (b) maturity: Vortex is pre-1.0 and not benchmarked by us (it supports DataFusion 55 via `vortex-datafusion` 0.91) | `SegmentFormat` |
| SQLite metastore instead of Postgres (locally) | (a) local-first: no database server | `Metastore` |
| Three services in one process | (a) local-first; Architecture Constraints allow it | service traits |
| In-memory cache instead of SSD cache | (a) local-first: data is already on local disk | `Cache` |
| Scan + bloom text search instead of custom inverted index | (c) deliberately staged (decision 2026-11-05). Revisit when search P50 > 500 ms in `bench`, or when cloud work starts | `TextIndex` |

## Project Structure

### Documentation (this feature)

```text
specs/000-tracedb-master-plan/   # master: no tasks.md
├── spec.md · primer.md · design-notes.md
├── plan.md              # this file
├── research.md          # Phase 0
├── data-model.md        # Phase 1
├── quickstart.md        # Phase 1
├── contracts/           # Phase 1: http-api.md, filter-dsl.md, cli.md
```

### Source Code (repository root)

```text
Cargo.toml                    # workspace
crates/
├── model/        # RunEvent, Run, merge rule, dotted_order, filter AST (no I/O)
├── storage/      # ObjectStore, Metastore, SegmentFormat, TextIndex, Cache traits + v0 impls
├── ingest/       # Ingest service: WAL, memtable, flush, fresh_scan
├── query/        # Query service: DSL → DataFusion plan, merge-on-read, progressive query, stats
├── compact/      # Compaction service: tiers, merge, atomic swap
└── tracedb/      # binary: axum server (LangSmith compat + native API), embedded UI, CLI, demo, bench
ui/               # React + Vite SPA, built into crates/tracedb at compile time
tests/
├── contract/     # recorded LangSmith SDK traffic (golden files)
├── scenario/     # overnight-eval demo assertions
└── crash/        # kill -9 durability loop
```

**Structure Decision**: one crate per service plus shared `model` and `storage` crates. Crate boundaries enforce the
service separation (ingest/query/compact may only depend on `model` + `storage` and on each other's traits), while the
`tracedb` binary wires all three into one process. The existing `src/main.rs` scaffold moves into `crates/tracedb`.

## Delivery Phases (the major plan)

Each phase ends with a browser demo and its automated checks. Details: design-notes §11.

| Phase | Delivers (stories) | Demo / exit check |
|---|---|---|
| **M1 See agent trace live** | Native + LangSmith JSON ingest (`/info`, `/runs/batch`, `POST/PATCH /runs`), WAL + memtable, trace tree from memory; UI shell: runs list (live), trace tree + waterfall, run panel (S1 partial, S2) | Send a trace, watch it appear, open its tree. Crash test green. |
| **M2 Durable storage + filtering** | Flush to segments, metastore, DataFusion scans with pruning, merge-on-read, filter DSL, cursor paging, field catalog; UI **query builder** (panel ↔ text, chips), field browser with suggestions, Samples + Table views, click-to-filter, shareable URLs (S3) | `tracedb seed` → 14 failing roots and 23 quiet failures found in the browser |
| **M3 Core demo** | LangSmith `/runs/multipart` + feedback parts; payload split + late materialization; progressive query; text search (scan + bloom); stats; Time series + Distribution views with comparison overlay; `bench` (S1, S4, S5) | `tracedb demo` passes steps 1–9 + QR-004/005 timings; unmodified LangGraph agent traces in |
| **M4 Follow-through** | Compaction tiers; threads; export; feedback write; find-what-stands-out (S6, S7, S9, S12) | demo steps 10, 11, 13 pass; late `end` correct before and after compaction |
| **M5 Zero-instrumentation + agents** | Claude Code importer; agent tools: outline, error path, sliced payloads, clusters, diff, annotate (S10, S11) | agent names the 429 cause with 31 cited runs |
| **Backlog** | Delete + retention (B1), Vortex `SegmentFormat`, custom `TextIndex`, OTLP ingest, cloud deployment split | — |

Cross-cutting from M1: crash test, zero-outbound check, P50/P99 bench harness, MinIO + Postgres CI job from M2.

## Complexity Tracking

No violations. The text-search staging that was listed here is allowed under constitution v1.1.0, Principle II (c).
