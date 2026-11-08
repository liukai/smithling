---
description: "Tasks for M1 — See agent trace live"
---

# Tasks: M1 — See agent trace live

**Input**: [spec.md](spec.md), [plan.md](plan.md), [design.md](design.md), [data-model.md](data-model.md),
[contracts/](contracts/), [research.md](research.md), [quickstart.md](quickstart.md)

**Tests**: included. The spec requires automated checks on every change (M1-FR-13, SC-02..07).

**Assumed answers to design §11** (proposed defaults; change before implementing if you disagree):
1. WAL records are JSON. 2. Memory budget defaults to 2 GB / 500k runs. 3. `cargo build` builds the UI through a build
script. 4. Restart target is ready within 30 s at the budget limit (the spec's 10 s is to be corrected).

**Organization**: grouped by user story; each story phase ends at a checkpoint that can be demoed on its own.

## Format: `[ID] [P?] [Story] Description`
- **[P]**: can run in parallel (different files, no dependency on an unfinished task)
- **[Story]**: US1–US4 from spec.md

## Path conventions
Rust workspace with `crates/{model,ingest,query,tracedb}`; UI in `ui/`; integration tests in `crates/tracedb/tests/`;
fixtures in `tests/fixtures/`; scripts in `scripts/`.

---

## Phase 1: Setup (shared infrastructure)

**Purpose**: workspace, toolchain, CI skeleton.

- [X] T001 Convert the repo to a Cargo workspace: root `Cargo.toml` with members `crates/model`, `crates/ingest`, `crates/query`, `crates/tracedb`; `edition = "2024"`; move `src/main.rs` to `crates/tracedb/src/main.rs` and delete `src/`
- [X] T002 [P] Create empty library crates with `Cargo.toml` and `src/lib.rs` in `crates/model/`, `crates/ingest/`, `crates/query/`; `crates/tracedb/Cargo.toml` as a binary named `tracedb`
- [X] T003 [P] Add workspace dependencies in root `Cargo.toml` (`[workspace.dependencies]`): tokio (full), axum, serde, serde_json, uuid (serde, v4), crc32fast, clap (derive), rust-embed, tower-http (limit, compression), reqwest (json, rustls, default-features off), proptest (dev), anyhow, thiserror, tracing, tracing-subscriber, indexmap, xxhash-rust
- [X] T004 [P] Add `rustfmt.toml`, `clippy.toml`, and `[workspace.lints]` (deny warnings in CI) in the repo root
- [X] T005 [P] Scaffold the UI: `ui/package.json` (React, React DOM, Vite, TypeScript, @tanstack/react-virtual), `ui/vite.config.ts` (output `ui/dist`), `ui/tsconfig.json`, `ui/index.html`, `ui/src/main.tsx` rendering an empty `App`
- [X] T006 Add `crates/server/build.rs`: run `npm ci && npm run build` in `ui/` when `ui/src/**` changes (`cargo:rerun-if-changed`); skip if `TRACEDB_SKIP_UI=1` and `ui/dist` exists
- [X] T007 [P] Add `.github/workflows/ci.yml`: Linux job with Node 24 + Rust 1.99; steps `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`; cache `~/.cargo` and `ui/node_modules`

**Checkpoint**: `cargo build` produces `target/debug/tracedb` and CI is green on an empty program.

---

## Phase 2: Foundational (blocking prerequisites)

**Purpose**: the model, the writer with WAL, the run store and a serving skeleton. Every story needs these.

**⚠️ No user story work starts until this phase is complete.**

### Model (`crates/model`)
- [X] T008 [P] Define `RunEvent` in `crates/model/src/event.rs`: `op: Op {Post, Patch}`, `event_seq: u64`, `received_at: i64` (µs UTC), `source: Source {LsBatch, LsSingle, Native}`, `body_hash: u64`, `fields: RunFields` (all optional; "`null` counts as absent")
- [X] T009 [P] Define `Run`, `RunFields`, `RunType`, `Status` in `crates/model/src/run.rs` with the fields of data-model §3.1–3.6 and bookkeeping of §3.8; `RunType` = `llm, chain, tool, retriever, embedding, prompt, parser, agent, other` with "anything else is stored as `other`, with the original in `run_type_raw`"
- [X] T010 [P] Implement `dotted_order` parsing in `crates/model/src/dotted_order.rs` per data-model §3.7: segments `<YYYYMMDDTHHMMSSffffffZ><UUID>` (22-char timestamp + 36-char UUID), joined by `.`; return `depth`, first UUID, last UUID, second-to-last UUID
- [X] T011 Implement validation in `crates/model/src/validate.rs` returning reason codes `missing_field, invalid_uuid, invalid_timestamp, invalid_dotted_order, too_large` (data-model §6): `post` requires `id, trace_id, dotted_order, name, run_type, start_time`; `patch` requires `id, trace_id, dotted_order`; "first UUID = `trace_id`; the last = `run_id`; the second-to-last = `parent_run_id`"; limits: `inputs`/`outputs`/`events` "≤ 20 MB", `metadata` "≤ 1 MB", `error` "≤ 1 MB", `tags` "≤ 100 tags, each ≤ 256 chars", `name` "1–512 chars", `project` "1–128 chars"; timestamps "ISO-8601 with or without `Z`; naive times are taken as UTC"
- [X] T012 Implement `merge(&mut RunState, &RunEvent) -> Result<Change, Reject>` in `crates/model/src/merge.rs` per data-model §5: present fields replace; absent/`null` never overwrite; identity fields (`trace_id`, `parent_run_id`, `dotted_order`, `project`) immutable → `identity_conflict`; `patch` before `post` sets `has_start = false`; a second `post` acts as a patch; update `first_seq`, `last_seq`, `event_count`, `payload_bytes`
- [X] T013 Implement `derive(&mut Run)` in `crates/model/src/derive.rs`: `status` (error non-empty → `error`; else `end_time` → `ok`; else `running`), `latency_ms` (empty with warning `negative_latency` if end < start), `depth`, `is_root`, `thread_id` (first of `metadata.thread_id`, `session_id`, `conversation_id`), `model` = `metadata.ls_model_name`, tokens from `outputs.usage_metadata` else `metadata.usage_metadata` (`input_tokens`, `output_tokens`, `total_tokens` else sum)
- [X] T014 [P] Define API shapes `RunHeader` (Run minus `inputs/outputs/events` + `payload_sizes` + `child_count`), `TraceView`, `RootsPage` in `crates/model/src/views.rs` (data-model §7)
- [X] T015 [P] Unit + property tests in `crates/model/tests/merge.rs`: any permutation of a run's events applied in `event_seq` order gives the same `Run`; `null` never overwrites; identity conflicts rejected; status transitions of data-model §4
- [X] T016 [P] Unit tests in `crates/model/tests/validate.rs` covering every reason code and every limit in T011

### Ingest (`crates/ingest`)
- [X] T017 Implement the WAL in `crates/ingest/src/wal.rs`: files `<first_seq>.wal` rolled at 64 MB; file header magic `TDBW` + `u32` version 1; record `[u32 length][u32 crc32 of the rest][u64 event_seq][u64 received_at_us][u8 op][u8 source][JSON fields]`; `append`, `sync`, `replay()` that stops at the first bad length/checksum and truncates from there
- [X] T018 [P] Property test in `crates/ingest/tests/wal_replay.rs`: random batches, truncate the last file at a random byte → replay returns exactly the records synced before the cut
- [X] T019 Implement `RunStore` in `crates/ingest/src/store.rs`: `runs: HashMap<RunId, RunState>`, `by_trace: HashMap<TraceId, Vec<RunId>>`, `roots: HashMap<Project, BTreeSet<(Reverse<Ts>, RunId)>>` (only runs with `has_start` and `is_root`), `bytes`; `apply(&RunEvent) -> Result<Change, Reject>` calling `model::merge` + `derive`; `Change { kind: NewRoot|Updated|NewChild, header: Arc<RunHeader> }`
- [X] T020 [P] Implement dedup in `crates/ingest/src/dedup.rs`: key `(run_id, op, body_hash)`, 24 h window with periodic pruning; a duplicate is "dropped silently and counted as accepted"
- [X] T021 [P] Implement the memory budget in `crates/ingest/src/budget.rs`: approximate bytes = headers + `payload_bytes`; default 2 GB and 500k runs; new run over the limit → `capacity`; "updates to known runs are still accepted"
- [X] T022 Implement the writer task in `crates/ingest/src/writer.rs`: bounded mpsc (10,000 events in flight, then `Busy` → HTTP `429` + `Retry-After: 1`); loop: dedup → assign `event_seq` → WAL append → group commit (fsync every ~5 ms or 1 MB) → `store.apply` → publish to the hub → resolve each request's oneshot with `AcceptResult { accepted, rejected: Vec<{id, reason, detail}> }`; on fsync error stop confirming and report unhealthy
- [X] T023 Implement the tail hub publisher in `crates/ingest/src/hub.rs`: `tokio::sync::broadcast` of `Change` with capacity 4,096; `subscribe()`; sending never blocks
- [X] T024 Implement `Ingest` in `crates/ingest/src/lib.rs`: `Ingest::open(data_dir, budget) -> Ingest` (replays the WAL into the store before returning, logging count and duration), `accept(Vec<RunEvent>) -> AcceptResult` (async), `snapshot() -> impl Snapshot`, `subscribe()`, `health()`; export the read-only `Snapshot` trait (`get_run`, `trace_runs`, `roots_page`)
- [X] T025 [P] Integration test in `crates/ingest/tests/writer.rs`: concurrent `accept` calls get confirmations only after fsync; duplicate batches counted once; capacity and identity-conflict rejections reported per item

### Serving skeleton (`crates/tracedb`)
- [X] T026 Implement the CLI in `crates/tracedb/src/cli.rs` (clap): `serve --data ./.tracedb --port 7070 --bind 127.0.0.1 --max-memory 2GB` and `seed` (args filled in US4); `main.rs` dispatches
- [X] T027 Implement the server in `crates/tracedb/src/server.rs`: axum router, body limit 50 MB (`413`), tracing, graceful shutdown; opens `Ingest` before binding the port; `GET /healthz` → `{ok, wal_ok, runs, memory_bytes, capacity_bytes}`
- [X] T028 [P] Serve the embedded UI in `crates/server/src/routes/ui.rs` with rust-embed over `ui/dist`; unknown non-API paths return `index.html` (client routing)
- [X] T029 Implement native ingest `POST /v1/events` in `crates/tracedb/src/routes/native.rs`: `{events: [RunEvent]}` → `202 {accepted, rejected}`; status codes per contracts/http-api.md: `429` + `Retry-After: 1` on `Busy`, `507` when the budget is full and the batch has a new run (nothing applied), `503` + `Retry-After: 5` on storage errors

**Checkpoint**: `tracedb serve` starts, replays its WAL, accepts native events, and `/healthz` reports run count.

---

## Phase 2b: LangSmith SDK 0.14 reconciliation (roadmap R-1, decided 2026-11-05)

**Purpose**: keep M1 correct for the current SDK. T066 runs first (PR1.5, before PR2) because everything builds on the
model; T067–T069 land with the HTTP layer in PR3.

- [X] T066 Add `address` to the model: `RunFields.address: Option<String>`, `Run.address`, `RunHeader.address` in `crates/model/src/run.rs` and `crates/model/src/views.rs`; validation "≤ 512 chars" → `too_large` in `crates/model/src/validate.rs`; immutable once set (`identity_conflict` if a later event differs) in `crates/model/src/merge.rs`; tests in `crates/model/tests/merge.rs` and `crates/model/tests/validate.rs`
- [X] T067 Map overload and storage errors to HTTP in `crates/server/src/routes/errors.rs` per contracts/http-api.md: `Busy` → `429` + `Retry-After: 1`; budget full with a new run in the batch → `507`, nothing applied; storage error → `503` + `Retry-After: 5`; test each in `crates/server/tests/status_codes.rs`
- [X] T068 Add `POST /feedback` → `501 {error: {code: "unsupported", message: "feedback is supported from M3"}}` in `crates/tracedb/src/routes/langsmith.rs`; test in `crates/server/tests/status_codes.rs`
- [X] T069 Serve `/info` with `version: ""` and `tracedb_version` in `crates/tracedb/src/routes/langsmith.rs`; extend `scripts/record-sdk-fixtures.py` (T030) to fail if the SDK logs a "Backend version" warning while tracing

---

## Phase 3: User Story 1 — Trace an unmodified agent and watch it live (P1) 🎯 MVP

**Goal**: an agent using the LangSmith SDK, with only its endpoint changed, shows its runs live in the browser.

**Independent Test**: start tracedb on an empty folder, run the LangGraph example agent with `LANGSMITH_ENDPOINT` set,
and see its runs appear within 1 s and update when they finish.

### Tests for User Story 1
- [X] T030 [P] [US1] Write `scripts/record-sdk-fixtures.py`: runs a LangGraph example agent against a recording HTTP endpoint, once with `langsmith==0.14.17` and once with `langsmith==0.8.18`, and writes each request (method, path, headers, body) to `tests/fixtures/sdk-<version>/requests.jsonl` plus the expected merged runs to `tests/fixtures/sdk-<version>/expected-runs.json`; include one run sent with `LANGSMITH_AGENT_ID`/`LANGSMITH_AGENT_ENVIRONMENT` and no project; commit the recorded fixtures
- [X] T031 [P] [US1] Contract test `crates/server/tests/contract.rs`: replay `tests/fixtures/sdk-*/requests.jsonl` (0.14.17 and 0.8.18) against an in-process server; assert 100% accepted and runs equal `expected-runs.json` (SC-06)
- [X] T032 [P] [US1] E2E test `crates/server/tests/e2e_tail.rs`: subscribe to `/api/tail?project=p`, upload 1,000 events at a steady rate, assert p99 confirm→received < 1 s (SC-02) and that a burst of 1,000 updates to one run yields ≤ 10 messages/s
- [X] T033 [P] [US1] API test `crates/server/tests/roots.rs`: `/api/roots` newest first, cursor paging without repeats while new runs arrive, project and status filters, `limit ≤ 200`

### Implementation for User Story 1
- [X] T034 [US1] Implement LangSmith → `RunEvent` mapping in `crates/server/src/langsmith_map.rs` per design §6: `post`/`POST /runs` → `Post`, `patch`/`PATCH /runs/{id}` → `Patch`; `session_name` → `project`, else `session_id`, else `<agent id>/<environment>` from `address`, else `default`; keep `address` as sent; `extra.metadata` → `metadata`, rest of `extra` → `extra`; drop `serialized` and attachments, recording sizes in `dropped`; compute `body_hash` with xxhash64 over canonical JSON
- [X] T035 [US1] Implement LangSmith routes in `crates/tracedb/src/routes/langsmith.rs`: `GET /info` → `{version: "", tracedb_version, batch_ingest_config: {use_multipart_endpoint: false, size_limit: 100, size_limit_bytes: 20971520}, instance_flags: {}}`; `POST /runs/batch`; `POST /runs`; `PATCH /runs/{run_id}`; `POST /runs/multipart` → `415 {error: {code: "unsupported", message: "multipart is supported from M3"}}`; accept and ignore API-key headers
- [X] T036 [P] [US1] Implement roots in `crates/query/src/roots.rs` and cursor in `crates/query/src/cursor.rs`: keyset `(start_time desc, run_id)`, cursor = base64 of `{start_time, run_id, filter_hash}`, filters `project`, `status`
- [X] T037 [US1] Implement `Query` in `crates/query/src/lib.rs` over `ingest::Snapshot` (no dependency on ingest internals), and routes `GET /api/roots`, `GET /api/projects` in `crates/server/src/routes/api.rs`
- [X] T038 [US1] Implement the per-tab tail task in `crates/server/src/routes/tail.rs` per design §4.3: `GET /api/tail?project=` or `?trace=`; subscribe first, then send `hello {server_seq}`; `pending: IndexMap<RunId, Arc<RunHeader>>` keeping the higher `last_seq`; leading-edge send if nothing was sent in the last 100 ms, otherwise flush on the 100 ms tick as one `runs [..]` event; `resync {}` on `Lagged` or `pending > 5,000`; comment heartbeat every 15 s
- [X] T039 [P] [US1] UI API client and live stream in `ui/src/api.ts` and `ui/src/live.ts`: fetch helpers; EventSource wrapper that waits for `hello` before loading, applies `runs` by `last_seq`, reloads on `resync`, reconnects automatically
- [X] T040 [US1] UI runs list in `ui/src/RunsList.tsx`: columns status, name, start, latency, tokens, tags; project and status pickers; `older ▸` paging; new roots on top when scrolled to the top, otherwise a "N new runs" pill; `● live` indicator; state in the URL query
- [X] T041 [P] [US1] UI empty state in `ui/src/EmptyState.tsx`: shows `LANGSMITH_TRACING=true LANGSMITH_ENDPOINT=http://localhost:7070` setup and a `tracedb seed` hint
- [X] T042 [US1] App shell and routing in `ui/src/App.tsx`: `/` → runs list, `/trace/:id` → trace view placeholder

**Checkpoint (demo)**: the LangGraph example agent streams into the runs list; a long root flips `running` → `ok`.

---

## Phase 4: User Story 2 — Inspect a trace and any step in it (P1)

**Goal**: open any trace as a tree + waterfall and read any step's full content.

**Independent Test**: load a 250-step trace with 40 KB payloads; the tree and waterfall render before any payload
loads; opening a step shows its full payload.

### Tests for User Story 2
- [X] T043 [P] [US2] API test `crates/server/tests/trace.rs`: `/api/traces/{id}` returns headers only, sorted by `dotted_order`, with `run_count` and `has_running`; `/api/runs/{id}?include=inputs,outputs,events` returns only the requested payloads; orphan children appear in their trace
- [X] T044 [P] [US2] E2E timing test `crates/server/tests/e2e_trace_open.rs`: a 250-step trace with 40 KB payloads returns from `/api/traces/{id}` in < 200 ms on CI hardware (UI render budget brings the total under SC-03's 1 s)

### Implementation for User Story 2
- [X] T045 [P] [US2] Implement trace and run reads in `crates/query/src/trace.rs` and `crates/query/src/run.rs`; add routes `GET /api/traces/{trace_id}` and `GET /api/runs/{run_id}` to `crates/server/src/routes/api.rs`
- [X] T046 [P] [US2] UI tree model in `ui/src/treeModel.ts`: build the parent/child structure from headers sorted by `dotted_order`; collapse/expand state; flattened visible rows shared by the tree and the waterfall
- [X] T047 [US2] UI trace view in `ui/src/TraceView.tsx`, `ui/src/Tree.tsx`, `ui/src/Waterfall.tsx`: virtualized rows (@tanstack/react-virtual); bars positioned by start/end relative to the root; running steps drawn open-ended; live updates via `?trace=` subscription
- [X] T048 [P] [US2] UI JSON viewer in `ui/src/JsonView.tsx`: collapsible objects/arrays, wrapped long strings; payloads over 2 MB first render as collapsed raw text
- [X] T049 [US2] UI step panel in `ui/src/StepPanel.tsx`: fetches `include=inputs,outputs,events` on selection; shows error, timing, tokens, model, tags, metadata, and `dropped` sizes; selection stored in the URL as `?run=`

**Checkpoint (demo)**: open a seeded or real 250-step trace, collapse branches, click `fetch_url`, read its 40 KB response; share the URL.

---

## Phase 5: User Story 3 — Never lose acknowledged data (P1)

**Goal**: everything confirmed survives any crash or restart.

**Independent Test**: upload continuously, `kill -9` at random times, restart, compare against every confirmed
upload; 10 rounds.

- [X] T050 [P] [US3] Crash harness `crates/tracedb/tests/crash.rs` (`#[ignore]`, run in CI): spawn `tracedb serve` on a temp folder, upload continuously from N clients logging every confirmed item id, `kill -9` at a random 0.5–5 s, restart, assert every confirmed id is present and merged correctly; 10 rounds (SC-04)
- [X] T051 [P] [US3] Retry-after-crash test in `crates/tracedb/tests/crash.rs`: an unconfirmed batch re-sent after restart is stored exactly once
- [X] T052 [P] [US3] Restart timing test `crates/tracedb/tests/restart.rs` (`#[ignore]`): fill to the default budget, restart, assert ready within 30 s and log replay throughput
- [X] T053 [US3] Disk-error handling in `crates/ingest/src/writer.rs` and `crates/server/src/routes/*`: on fsync/write error stop confirming, return `503` + `Retry-After: 5` with reason, `/healthz` `wal_ok: false`; test with a fault-injecting WAL in `crates/ingest/tests/disk_error.rs`
- [X] T054 [US3] Add the crash and restart tests to `.github/workflows/ci.yml` (`cargo test -- --ignored crash restart`)

**Checkpoint**: crash harness green in CI 10/10.

---

## Phase 6: User Story 4 — Explore realistic traces without an agent (P2)

**Goal**: anyone can generate realistic traces and explore them in a minute.

**Independent Test**: `tracedb seed --traces 100` → browse traces with deep trees, late finishes, errors, large payloads.

- [X] T055 [P] [US4] Seed shape test `crates/tools/tests/seed.rs`: with `--seed 42`, generated traces have depth 3–8, 20–300 steps, ≈5% errors, ≈10% of steps finishing 30–120 s late, payloads 1–50 KB; the same seed gives the same output
- [X] T056 [US4] Implement the generator in `crates/tools/src/seed.rs`: research-agent-like step names (`plan`, `web_search`, `fetch_url`, `python_exec`, `summarize`, `llm`), LangSmith-shaped `post`/`patch` items with valid `dotted_order`, deterministic from `--seed`
- [X] T057 [US4] Wire `tracedb seed [--traces 100] [--seed 42] [--url http://localhost:7070]` in `crates/tracedb/src/cli.rs`: sends batches to `/runs/batch` (≤ 100 items), sends late `patch`es after their delay (or immediately with `--no-delay`), exit codes 0/1/2

**Checkpoint (demo)**: `tracedb seed --traces 100`, then click through the generated traces.

---

## Phase 7: Polish and cross-cutting

- [X] T058 [P] No-outbound check `scripts/no-outbound.sh`: run `tracedb serve` + `tracedb seed --traces 20 --no-delay` + contract replay under `strace -f -e trace=connect`; fail on any non-loopback address; add to CI (SC-05)
- [X] T059 [P] Throughput test `crates/tracedb/tests/throughput.rs` (`#[ignore]`): ≥ 5,000 events/s sustained for 60 s from 8 clients (SC-07)
- [X] T060 [P] Clean-container check in CI: build the release binary, run it in a minimal container without Node, `curl /healthz` (QR-002)
- [X] T061 [P] Add `examples/research_agent.py` (LangGraph agent with `web_search`, `fetch_url`, `python_exec`, `summarize`) and `examples/requirements.txt` pinning `langsmith==0.14.17`
- [X] T062 [P] `README.md`: install, `tracedb serve`, point an agent, `tracedb seed`, M1 limits (memory budget, no filters)
- [X] T063 Update the spec's restart target to 30 s in `specs/001-m1-see-agent-trace-live/spec.md` (Story 3, scenario 3) to match the assumed §11.4 answer
- [X] T064 Run [quickstart.md](quickstart.md) end to end and record the results (timings, counts) in `specs/001-m1-see-agent-trace-live/quickstart.md` under "Last verified"
- [X] T065 Update the master roadmap row for M1 in `specs/000-tracedb-master-plan/roadmap.md` (status, tasks done, exit check)

---

## Dependencies and execution order

```
Phase 1 Setup ──▶ T066 ──▶ Phase 2 Foundational (+ T067–T069 with the HTTP layer) ──┬──▶ US1 (P1, MVP) ──┬──▶ US2 (P1)   (US2 reuses US1's UI shell, api.ts, live.ts)
                                         │                    └──▶ US4 (P2)   (seed needs the LangSmith routes, T035)
                                         └──▶ US3 (P1)        (needs only the writer + WAL + server; can run alongside US1)
                                                        all ──▶ Phase 7 Polish
```
- **Within Phase 2**: T008–T014 (model) → T019 (store) → T022 (writer) → T024 (Ingest); T017 (WAL), T020, T021, T023 can go in parallel with the model.
- **Within each story**: tests first (they should fail), then implementation, then the checkpoint demo.

## Parallel opportunities
- **Phase 1**: T002, T003, T004, T005, T007 together.
- **Phase 2**: model files T008/T009/T010/T014 together; T015/T016 together; WAL T017 + T018 alongside the model; T020, T021, T023 together.
- **US1**: tests T030–T033 together; T036 (query) and T039/T041 (UI) alongside T034/T035 (HTTP).
- **US2**: T043/T044 together; T045 (API), T046 (tree model), T048 (JSON viewer) together.
- **US3**: T050–T052 together, alongside US1.
- **Polish**: T058–T062 together.

## Implementation strategy
1. **MVP = Phases 1–3 (US1)**: an unmodified agent streams into the live runs list. Stop and demo.
2. **Add US2**: the trace view and step panel; demo the 250-step trace.
3. **Add US3**: crash harness and disk-error handling; durability proven in CI.
4. **Add US4**: seed for anyone without an agent.
5. **Polish**: no-outbound, throughput, clean container, README, quickstart verified, roadmap updated.

PR mapping (plan.md "Delivery order"): PR1 = T001–T016 · PR1.5 = T066 · PR2 = T017–T025 · T067–T069 with PR3 · PR3 = T026–T035 · PR4 = T036–T038 ·
PR5 = T039–T042 · PR6 = T043–T049 · PR7 = T050–T065.
