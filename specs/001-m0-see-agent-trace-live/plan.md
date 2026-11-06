# Implementation Plan: M0 — See agent trace live

**Branch**: `001-m0-see-agent-trace-live` | **Date**: 2026-11-05 | **Spec**: [spec.md](spec.md) | **Design**: [design.md](design.md)

**Input**: Feature specification from `specs/001-m0-see-agent-trace-live/spec.md`. Master: [000 plan](../000-tracedb-master-plan/plan.md).

## Summary

One binary, `tracedb serve`, that accepts LangSmith SDK JSON uploads. A single writer records them in a group-committed
WAL, merges them into an in-memory run store and fans changes out to browsers over SSE. An embedded SPA shows a live
runs list, a trace tree with waterfall, and a step panel. `tracedb seed` generates realistic traces through the same
HTTP path. The full design and its components are in [design.md](design.md).

## Technical Context

**Language/Version**: Rust 1.99 (edition 2024). UI in TypeScript (React + Vite), built by `cargo build` and embedded.

**Primary Dependencies**: tokio, axum, serde/serde_json, uuid, crc32fast, clap, rust-embed, tower-http (limits,
compression), reqwest (used only by `seed`, to localhost). UI: React, Vite, TanStack Virtual.

**Storage**: append-only WAL files under `./.tracedb/wal/`; all run state in memory (design §5).

**Testing**: `cargo test` (unit, property via proptest, integration); crash harness; SDK contract fixtures; strace-based
no-outbound check in CI.

**Target Platform**: macOS and Linux laptops; CI on Linux.

**Project Type**: single binary = web service + embedded SPA + CLI.

**Performance Goals**: confirmed → visible ≤ 1 s (p99); 250-step trace opens < 1 s; ≥ 5k events/s.

**Constraints**: zero outbound; localhost bind; memory budget 2 GB / 500k runs (configurable); nothing confirmed lost.

**Scale/Scope**: 4 user stories (3 P1); ~4 crates + UI; target ~6–8 PRs of ≤ 400 lines.

No NEEDS CLARIFICATION remain; three small design questions are listed in design §11 for review.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.* Constitution v1.1.0.

| Principle | M0 check | Pre | Post |
|---|---|---|---|
| I. Rust, one binary | Rust workspace → one `tracedb` binary; UI embedded; no external services | PASS | PASS |
| II. SmithDB is the reference | Ingest service + WAL + memtable + query service mirror SmithDB. Columnar segments and metastore are deferred to M1 under reason (c), deliberately staged (roadmap). Merge on write is staged until segments exist (design §10) | PASS | PASS |
| III. Local and private | localhost bind; no outbound (CI strace check); all state under `--data` | PASS | PASS |
| IV. UI first | runs list, trace view and step panel ship in M0; the demo is in the browser | PASS | PASS |
| V. Cloud-portable storage | the WAL is local by nature (SmithDB keeps its WAL on the ingestion node's disk); the first object-storage data arrives in M1 | PASS | PASS |
| VI. Done is measured | crash test, contract test, no-outbound and latency tests in CI (design §9) | PASS | PASS |

## Project Structure

### Documentation (this feature)

```text
specs/001-m0-see-agent-trace-live/
├── spec.md          # what and why
├── design.md        # high-level design + core components (review before implementation)
├── plan.md          # this file
├── research.md      # Phase 0 decisions
├── data-model.md    # M0 entities (refers to master)
├── quickstart.md    # how to prove M0 works
├── contracts/       # http-api.md, cli.md (M0 subset of master)
└── tasks.md         # /speckit-tasks (next)
```

### Source Code (repository root)

```text
Cargo.toml                  # workspace
crates/
├── model/                  # RunEvent, Run, merge, dotted_order, validation, derive
├── ingest/                 # Ingest service: writer task, WAL, run store, dedup, Snapshot trait
├── query/                  # Query service: trace, run, roots + cursor
└── tracedb/                # binary: axum routes (LangSmith compat, native, api, tail), embedded UI, CLI (serve, seed)
ui/                         # React + Vite SPA (runs list, trace view, step panel)
tests/
├── crash/                  # kill -9 harness
├── contract/               # recorded SDK traffic fixtures + golden runs
└── e2e/                    # upload → tail latency, trace-open timing
```

**Structure Decision**: the existing `src/main.rs` scaffold moves to `crates/tracedb`. Crate boundaries enforce the
service separation (design §3 boundary rules). `storage` and `compact` crates are not created until M1/M3.

## Delivery order (PR-sized, each ends in something visible)

| # | PR | As a…, I can… after it merges |
|---|---|---|
| 1 | Workspace + `model` (types, merge, validation) + CI | As a maintainer, I can run the merge-rule tests in CI |
| 2 | `ingest`: writer task, WAL with group commit, replay, run store, dedup | As a maintainer, I can run the WAL replay property test and the crash harness against the native endpoint |
| 3 | HTTP: native `/v1/events`, LangSmith `/info`, `/runs/batch`, `POST/PATCH /runs`; contract fixtures | As an agent developer, I can point an unmodified agent at tracedb and get its uploads confirmed |
| 4 | `query` + `/api/traces`, `/api/runs`, `/api/roots`, `/api/tail` (SSE) | As an agent developer, I can fetch a trace tree and watch changes stream |
| 5 | UI: shell, runs list (live, paged), empty state; embedding | As an agent developer, I can watch my runs arrive in the browser |
| 6 | UI: trace view (tree + waterfall) + step panel; URL state | As an agent developer, I can open any trace and read any step |
| 7 | `tracedb seed`; no-outbound check; e2e latency/trace-open tests | As a test user, I can generate traces and explore them; as a maintainer, I can trust the M0 gates |

## Complexity Tracking

No violations.
