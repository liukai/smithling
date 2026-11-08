# Implementation Plan: M1 — See agent trace live

**Branch**: `001-m1-see-agent-trace-live` | **Date**: 2026-11-05 | **Spec**: [spec.md](spec.md) | **Design**: [design.md](design.md)

**Input**: Feature specification from `specs/001-m1-see-agent-trace-live/spec.md`. Master: [000 plan](../000-smithling-master-plan/plan.md).

## Summary

One binary, `smithling serve`, that accepts LangSmith SDK JSON uploads. A single writer records them in a group-committed
WAL, merges them into an in-memory run store and fans changes out to browsers over SSE. An embedded SPA shows a live
runs list, a trace tree with waterfall, and a step panel. `smithling seed` generates realistic traces through the same
HTTP path. The full design and its components are in [design.md](design.md).

## End-state map: where you see each requirement

One row per M1 requirement. "Where you see it" is the screen, command or check to look at; "What you should see" is
the pass condition. Steps refer to [quickstart.md](quickstart.md).

### Functional (what you can do)

| Req | As a…, I can… | Where you see it | What you should see | Automated check |
|---|---|---|---|---|
| M1-FR-01 | As an agent developer, I can trace an unmodified LangSmith SDK agent by changing only its endpoint | Browser → runs list, after quickstart step 2 | the agent's root run appears; no code change | contract test (recorded SDK traffic) |
| M1-FR-02 | As an agent developer, I can upload a batch with a bad item and still keep the good ones | upload response (`rejected[]`); runs list | good items listed; the bad one rejected with a reason code (data-model §6) | `ingest` validation tests |
| M1-FR-03 | As an agent developer, I can see one correct state for a step that arrived in pieces | Browser → trace → step panel | after start + update: `error`, 31 s, outputs present | merge unit + property tests |
| M1-FR-04 | As an agent developer, I can kill smithling and restart without losing anything it confirmed | `cargo test --test crash` (step 4); the browser after restart | `0 confirmed events missing` × 10 rounds; the same runs listed | crash harness in CI |
| M1-FR-05 | As an agent developer, I can watch runs arrive and update without refreshing | Browser → runs list with the `● live` indicator | new rows within 1 s; a long root flips `running` → `ok` | e2e tail-latency test (p99 < 1 s) |
| M1-FR-06 | As an agent developer, I can page through root runs newest first and narrow by project and status | Browser → runs list: project/status pickers, `older ▸` | newest first, no repeats across pages | query cursor tests |
| M1-FR-07 | As an agent developer, I can open any trace and see its tree and waterfall at once | Browser → click a root run | 250 steps in tree order with aligned bars, in < 1 s | e2e trace-open timing test |
| M1-FR-08 | As an agent developer, I can read any step's full inputs and outputs | Browser → click a step → right panel | full 40 KB payload, readable JSON | API test for `include=` |
| M1-FR-09 | As an agent developer, I can share a link to a trace or step | copy the URL, open it in a new tab | the same trace and selected step | UI route test |
| M1-FR-10 | As a test user, I can generate realistic traces without an agent | `smithling seed --traces 100` (step 3), then the browser | 100 traces with deep trees, errors, late finishes, big payloads | seed shape test (fixed `--seed`) |

### Non-functional (how well it does it)

| Req | As a…, I can… | Where you see it | What you should see | Automated check |
|---|---|---|---|---|
| M1-FR-11 / QR-001 | As an agent developer, I can trust nothing leaves my laptop | `scripts/no-outbound.sh` (step 5); `--data` folder | 0 non-loopback connections; all files under the data folder | strace check in Linux CI |
| M1-FR-12 / QR-002 | As a test user, I can run it with nothing else installed | a fresh machine: download the binary → `smithling serve` | it starts; no Docker, DB or Node needed at runtime | CI runs the release binary in a clean container |
| QR-003 (via FR-04) | As an agent developer, I can rely on durability | see M1-FR-04 | — | crash harness |
| SC-02 | As an agent developer, I see my data quickly | e2e test output | p99 confirm → visible < 1 s | e2e tail-latency test |
| SC-03 | As an agent developer, big traces open fast | e2e test output | 250-step trace < 1 s | e2e trace-open test |
| SC-07 | As an agent developer, uploads keep up with my agent | `cargo test --test throughput -- --ignored` | ≥ 5,000 events/s sustained | throughput test |
| M1-FR-13 | As a maintainer, I can trust all of the above stays true | CI on every PR | all checks green | CI workflow |

## Technical Context

**Language/Version**: Rust 1.99 (edition 2024). UI in TypeScript (React + Vite), built by `cargo build` and embedded.

**Primary Dependencies**: tokio, axum, serde/serde_json, uuid, crc32fast, clap, rust-embed, tower-http (limits,
compression), reqwest (used only by `seed`, to localhost). UI: React, Vite, TanStack Virtual.

**Storage**: append-only WAL files under `./.smithling/wal/`; all run state in memory (design §5).

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

| Principle | M1 check | Pre | Post |
|---|---|---|---|
| I. Rust, one binary | Rust workspace → one `smithling` binary; UI embedded; no external services | PASS | PASS |
| II. SmithDB is the reference | Ingest service + WAL + memtable + query service mirror SmithDB. Columnar segments and metastore are deferred to M2 under reason (c), deliberately staged (roadmap). Merge on write is staged until segments exist (design §10) | PASS | PASS |
| III. Local and private | localhost bind; no outbound (CI strace check); all state under `--data` | PASS | PASS |
| IV. UI first | runs list, trace view and step panel ship in M1; the demo is in the browser | PASS | PASS |
| V. Cloud-portable storage | the WAL is local by nature (SmithDB keeps its WAL on the ingestion node's disk); the first object-storage data arrives in M2 | PASS | PASS |
| VI. Done is measured | crash test, contract test, no-outbound and latency tests in CI (design §9) | PASS | PASS |

## Project Structure

### Documentation (this feature)

```text
specs/001-m1-see-agent-trace-live/
├── spec.md          # what and why
├── design.md        # high-level design + core components (review before implementation)
├── plan.md          # this file
├── research.md      # Phase 0 decisions
├── data-model.md    # M1 entities (refers to master)
├── quickstart.md    # how to prove M1 works
├── contracts/       # http-api.md, cli.md (M1 subset of master)
└── tasks.md         # /speckit-tasks (next)
```

### Source Code (repository root)

```text
Cargo.toml                  # workspace
crates/
├── model/                  # RunEvent, Run, merge, dotted_order, validation, derive
├── ingest/                 # Ingest service: writer task, WAL, run store, dedup, Snapshot trait
├── query/                  # Query service: trace, run, roots + cursor
└── smithling/                # binary: axum routes (LangSmith compat, native, api, tail), embedded UI, CLI (serve, seed)
ui/                         # React + Vite SPA (runs list, trace view, step panel)
tests/
├── crash/                  # kill -9 harness
├── contract/               # recorded SDK traffic fixtures + golden runs
└── e2e/                    # upload → tail latency, trace-open timing
```

**Structure Decision**: the existing `src/main.rs` scaffold moves to `crates/smithling`. Crate boundaries enforce the
service separation (design §3 boundary rules). `storage` and `compact` crates are not created until M2/M4.

## Delivery order (PR-sized, each ends in something visible)

| # | PR | As a…, I can… after it merges |
|---|---|---|
| 1 | Workspace + `model` (types, merge, validation) + CI | As a maintainer, I can run the merge-rule tests in CI |
| 2 | `ingest`: writer task, WAL with group commit, replay, run store, dedup | As a maintainer, I can run the WAL replay property test and the crash harness against the native endpoint |
| 3 | HTTP: native `/v1/events`, LangSmith `/info`, `/runs/batch`, `POST/PATCH /runs`; contract fixtures | As an agent developer, I can point an unmodified agent at smithling and get its uploads confirmed |
| 4 | `query` + `/api/traces`, `/api/runs`, `/api/roots`, `/api/tail` (SSE) | As an agent developer, I can fetch a trace tree and watch changes stream |
| 5 | UI: shell, runs list (live, paged), empty state; embedding | As an agent developer, I can watch my runs arrive in the browser |
| 6 | UI: trace view (tree + waterfall) + step panel; URL state | As an agent developer, I can open any trace and read any step |
| 7 | `smithling seed`; no-outbound check; e2e latency/trace-open tests | As a test user, I can generate traces and explore them; as a maintainer, I can trust the M1 gates |

## Complexity Tracking

No violations.
